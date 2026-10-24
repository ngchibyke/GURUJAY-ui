//! Background jobs (Task 9): one tick every 60 seconds runs, in order,
//!
//! 1. `expire_checkouts`: `pending` checkouts past their stored `expires_at`
//!    -> [`expire_and_restore`] (reservation released, orders cancelled).
//! 2. `reverify_checkouts`: the lost-webhook check. `expired` checkouts with a
//!    `provider_payment_id`, expired within the last 48h, are verified again
//!    through [`settle_payment`] (which passes the shared verify gate: its
//!    concurrency cap and per-checkout cooldown). Each claim pushes the
//!    checkout's own `next_reverify_at` back (5 min doubling, capped at 6h).
//! 3. `ship_deadline`: `paid` orders past their stored `ship_by`, not held,
//!    payment not `provider_flagged` ->
//!    a full refund (stock back on hand) plus a `seller_strikes` row, in one
//!    transaction, then the refund is sent. Capped per tick, and switched off
//!    by `platform_settings.jobs_ship_deadline_enabled` (owner kill switch).
//! 4. `auto_release`: `delivered` orders past their stored `release_at`, not
//!    held, whose checkout is not `provider_flagged` ->
//!    [`release_order`]`(AutoAfterDelivery)`.
//! 5. `backstop`: unfinished orders past their stored `backstop_at`, not yet
//!    flagged -> `backstop_flagged_at` + audit `order.backstop_flagged`.
//! 6. `payouts`: due payouts only. `scheduled`/`unknown` with `send_after`
//!    reached and `next_attempt_at` null or passed, and `sending` rows whose
//!    lease expired, for orders not held and payments not flagged ->
//!    [`send_payout`] (the 8-attempt cap lives there).
//!    `pending` payouts not polled for an hour -> [`settle_payout`] (the
//!    24h-pending -> `manual` rule lives there). A payout that ends `failed`
//!    goes to `manual` with an audit row.
//! 7. `refunds`: `requested` refunds not under a send claim, past their
//!    `next_attempt_at` -> [`send_refund`] with the same RF reference. After
//!    [`MAX_REFUND_ATTEMPTS`] sends the refund goes to `manual` with an audit
//!    row (the order stays `refund_pending`).
//!
//! **Time.** Every due comparison uses the database clock (`now()` in SQL),
//! and only the deadlines stored on each row: a settings change never moves an
//! existing deadline.
//!
//! **Claim, then act.** Each job selects its due ids with `FOR UPDATE SKIP
//! LOCKED LIMIT n` in a short transaction (that only proves no other instance
//! is claiming them right now) and commits; then it calls the engine function
//! per id, which takes its own locks and re-checks the state. No engine
//! function ever runs while the job holds a lock on the same row. Every
//! exclusion lives in the claim SQL, and a row the engine leaves unchanged is
//! pushed back by a durable column (`next_attempt_at`, `next_reverify_at`,
//! `last_polled_at`), so no row stays due forever and fills the batch. A row
//! whose engine call returns an error is pushed back in `job_row_failures`
//! (same backoff) and the claims skip it until then.
//!
//! **Provider calls.** Ids are processed one at a time, so the jobs make at
//! most one outbound call at a time, and each provider-calling job handles at
//! most [`PROVIDER_CALLS_PER_TICK`] rows per tick. Payment verifies also pass
//! the shared [`VerifyGate`](crate::payments::VerifyGate). These limits are
//! per process: with several replicas (SKIP LOCKED keeps them from processing
//! the same row), the provider sees up to that many times more calls. A
//! single replica is assumed when sizing the provider's rate limit.
//!
//! **Observability.** Each job upserts its row in `job_runs` (last run, last
//! run without a failure, counts) and the tick logs counts at info level.
//! Logs never carry personal data, references or account numbers.

use std::{future::Future, time::Duration};

use anyhow::Context;
use serde_json::json;
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{
    refund::{begin_refund, send_refund},
    release::{ReleaseReason, release_order, send_payout, settle_payout},
};
use crate::{
    AppState, audit,
    checkout::{SettleOutcome, service::expire_and_restore, settle_payment},
    error::AppError,
    settings,
};

/// How often the loop runs the jobs.
pub const TICK: Duration = Duration::from_secs(60);
/// Rows a job without provider calls handles per tick.
pub const BATCH: i64 = 100;
/// Rows a provider-calling job handles per tick (one call or so each).
pub const PROVIDER_CALLS_PER_TICK: i64 = 20;
/// Ship-deadline refunds per tick: a guard against a mass refund from one bad
/// condition (a wrong deadline backfill, say); the rest wait for later ticks.
pub const SHIP_DEADLINE_PER_TICK: i64 = 20;
/// A pending payout is polled at most this often.
const PENDING_POLL_EVERY: &str = "1 hour";
/// Refund sends (the inline one included) before a `requested` refund goes to
/// `manual` for the owner (same cap as payouts).
pub const MAX_REFUND_ATTEMPTS: i32 = 8;
// Job names (`job_runs.job`, `job_row_failures.job`).
const EXPIRE: &str = "expire_checkouts";
const REVERIFY: &str = "reverify_checkouts";
const SHIP_DEADLINE: &str = "ship_deadline";
const AUTO_RELEASE: &str = "auto_release";
const PAYOUTS: &str = "payouts";
const REFUNDS: &str = "refunds";
/// The refund sweeper leaves a refund this long after its last send.
const REFUND_GRACE: &str = "1 minute";
/// Expired checkouts are re-verified for this long after expiry.
const REVERIFY_WINDOW: &str = "48 hours";
const LOCK_TIMEOUT: &str = "SET LOCAL lock_timeout = '5s'";
/// Job backoff in SQL from an attempt count `a`: 5 minutes doubling, capped at
/// 6h, plus up to 20% jitter (also capped).
const BACKOFF_SQL: &str = "LEAST(300 * power(2, LEAST(GREATEST(a - 1, 0), 10)) \
                           * (1 + random() * 0.2), 21600)";

/// What one tick did. `failed` counts rows whose engine call returned an error
/// (and claims that failed); the row is tried again on a later tick.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct JobReport {
    pub expired: u32,
    pub reverified: u32,
    pub auto_cancelled: u32,
    pub released: u32,
    pub flagged: u32,
    pub payouts_processed: u32,
    pub refunds_retried: u32,
    pub failed: u32,
}

/// Rows a job handled in one tick, and how many of those failed.
#[derive(Debug, Default, Clone, Copy)]
struct Stats {
    done: u32,
    failed: u32,
}

/// Runs every job once (see the module docs). Never fails: each job has its
/// own error boundary, logs a sanitised error and lets the later jobs run.
pub async fn run_due_jobs(state: &AppState) -> JobReport {
    run_due_jobs_until(state, &CancellationToken::new()).await
}

/// [`run_due_jobs`], stopping between rows (never mid-call) once `cancel` fires.
pub async fn run_due_jobs_until(state: &AppState, cancel: &CancellationToken) -> JobReport {
    let mut report = JobReport::default();
    // Push-backs long past are no longer needed.
    if let Err(e) =
        sqlx::query("DELETE FROM job_row_failures WHERE next_try_at < now() - interval '7 days'")
            .execute(&state.db)
            .await
    {
        tracing::warn!(error = %e, "job push-backs not pruned");
    }
    let jobs: [(&str, Job); 7] = [
        (EXPIRE, Job::Expire),
        (REVERIFY, Job::Reverify),
        (SHIP_DEADLINE, Job::ShipDeadline),
        (AUTO_RELEASE, Job::AutoRelease),
        ("backstop", Job::Backstop),
        (PAYOUTS, Job::Payouts),
        (REFUNDS, Job::Refunds),
    ];
    for (name, job) in jobs {
        if cancel.is_cancelled() {
            break;
        }
        let stats = match job.run(state, cancel).await {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(job = name, error = %e, "job failed");
                Stats { done: 0, failed: 1 }
            }
        };
        record_run(state, name, stats).await;
        report.failed += stats.failed;
        let slot = match job {
            Job::Expire => &mut report.expired,
            Job::Reverify => &mut report.reverified,
            Job::ShipDeadline => &mut report.auto_cancelled,
            Job::AutoRelease => &mut report.released,
            Job::Backstop => &mut report.flagged,
            Job::Payouts => &mut report.payouts_processed,
            Job::Refunds => &mut report.refunds_retried,
        };
        *slot += stats.done;
    }
    report
}

/// The production loop: [`run_due_jobs`] every [`TICK`] until `cancel` fires.
pub async fn run_loop(state: AppState, cancel: CancellationToken) {
    run_loop_every(state, cancel, TICK).await
}

/// [`run_loop`] with a custom period (tests).
pub async fn run_loop_every(state: AppState, cancel: CancellationToken, period: Duration) {
    run_every(period, cancel.clone(), move || {
        let state = state.clone();
        let cancel = cancel.clone();
        async move {
            let r = run_due_jobs_until(&state, &cancel).await;
            tracing::info!(
                expired = r.expired,
                reverified = r.reverified,
                auto_cancelled = r.auto_cancelled,
                released = r.released,
                flagged = r.flagged,
                payouts = r.payouts_processed,
                refunds = r.refunds_retried,
                failed = r.failed,
                "jobs tick"
            );
        }
    })
    .await
}

/// Calls `tick` every `period` until `cancel` fires. A slow tick delays the
/// next one (no catch-up burst). Each tick runs in its own task and is
/// awaited, so a panic is logged and the loop carries on; on cancel the tick
/// in flight is awaited (it stops between rows) before returning.
pub async fn run_every<F, Fut>(period: Duration, cancel: CancellationToken, tick: F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = ()> + Send + 'static,
{
    let mut interval = tokio::time::interval(period);
    interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = interval.tick() => {}
        }
        match tokio::spawn(tick()).await {
            Ok(()) => {}
            Err(e) if e.is_panic() => tracing::error!("jobs tick panicked; continuing"),
            Err(_) => tracing::error!("jobs tick was cancelled; continuing"),
        }
    }
    tracing::info!("jobs loop stopped");
}

#[derive(Clone, Copy)]
enum Job {
    Expire,
    Reverify,
    ShipDeadline,
    AutoRelease,
    Backstop,
    Payouts,
    Refunds,
}

impl Job {
    async fn run(self, state: &AppState, cancel: &CancellationToken) -> Result<Stats, AppError> {
        match self {
            Job::Expire => expire_checkouts(state, cancel).await,
            Job::Reverify => reverify_checkouts(state, cancel).await,
            Job::ShipDeadline => ship_deadline(state, cancel).await,
            Job::AutoRelease => auto_release(state, cancel).await,
            Job::Backstop => backstop(state).await,
            Job::Payouts => payouts(state, cancel).await,
            Job::Refunds => refunds(state, cancel).await,
        }
    }
}

/// Upserts the job's `job_runs` row; a failure here is only logged.
async fn record_run(state: &AppState, job: &str, stats: Stats) {
    let res = sqlx::query(
        "INSERT INTO job_runs (job, last_run_at, last_success_at, processed, failed) \
         VALUES ($1, now(), CASE WHEN $3 = 0 THEN now() END, $2, $3) \
         ON CONFLICT (job) DO UPDATE SET last_run_at = now(), \
             last_success_at = CASE WHEN $3 = 0 THEN now() ELSE job_runs.last_success_at END, \
             processed = $2, failed = $3",
    )
    .bind(job)
    .bind(i32::try_from(stats.done).unwrap_or(i32::MAX))
    .bind(i32::try_from(stats.failed).unwrap_or(i32::MAX))
    .execute(&state.db)
    .await;
    if let Err(e) = res {
        tracing::warn!(job, error = %e, "job run not recorded");
    }
}

/// Runs a claim query (it must return ids) in its own short transaction.
async fn claim(state: &AppState, sql: &str, limit: i64) -> Result<Vec<Uuid>, AppError> {
    let mut tx = state.db.begin().await.context("begin claim")?;
    let ids: Vec<Uuid> = sqlx::query_scalar(sql)
        .bind(limit)
        .fetch_all(&mut *tx)
        .await
        .context("claim due rows")?;
    tx.commit().await.context("commit claim")?;
    Ok(ids)
}

/// Counts one engine call's result into `stats`; `Ok(true)` means it did
/// something. A row whose call failed is pushed back in `job_row_failures`
/// (the jobs' usual backoff), so it cannot be reclaimed at the head of the
/// batch every tick and starve newer due rows.
async fn tally(
    state: &AppState,
    stats: &mut Stats,
    job: &'static str,
    row_id: Uuid,
    result: Result<bool, AppError>,
) {
    match result {
        Ok(true) => stats.done += 1,
        Ok(false) => {}
        Err(e) => {
            stats.failed += 1;
            tracing::warn!(job, error = %e, "job row failed; retried after a backoff");
            push_back(state, job, row_id).await;
        }
    }
}

async fn push_back(state: &AppState, job: &str, row_id: Uuid) {
    let sql = format!(
        "INSERT INTO job_row_failures (job, row_id, failures, next_try_at) \
         VALUES ($1, $2, 1, now() + make_interval(secs => \
             (SELECT {BACKOFF_SQL} FROM (SELECT 1 AS a) x))) \
         ON CONFLICT (job, row_id) DO UPDATE SET failures = job_row_failures.failures + 1, \
             next_try_at = now() + make_interval(secs => \
                 (SELECT {BACKOFF_SQL} FROM (SELECT job_row_failures.failures + 1 AS a) x))"
    );
    if let Err(e) = sqlx::query(&sql)
        .bind(job)
        .bind(row_id)
        .execute(&state.db)
        .await
    {
        tracing::warn!(job, error = %e, "failed row not pushed back");
    }
}

/// SQL condition: the row `row` (an id expression) is not pushed back for `job`.
fn not_pushed_back(job: &str, row: &str) -> String {
    format!(
        "NOT EXISTS (SELECT 1 FROM job_row_failures f \
                     WHERE f.job = '{job}' AND f.row_id = {row} AND f.next_try_at > now())"
    )
}

// ------------------------------------------------------------------ 1. expiry

async fn expire_checkouts(state: &AppState, cancel: &CancellationToken) -> Result<Stats, AppError> {
    let sql = format!(
        "SELECT c.id FROM checkouts c WHERE c.status = 'pending' AND c.expires_at < now() \
           AND {} ORDER BY c.expires_at, c.id LIMIT $1 FOR UPDATE SKIP LOCKED",
        not_pushed_back(EXPIRE, "c.id")
    );
    let ids = claim(state, &sql, BATCH).await?;
    let mut stats = Stats::default();
    for id in ids {
        if cancel.is_cancelled() {
            break;
        }
        let result = expire_one(state, id).await;
        tally(state, &mut stats, EXPIRE, id, result).await;
    }
    Ok(stats)
}

/// No checkout lock is held here: `expire_and_restore` locks the variants,
/// then the checkout, then its orders (the global order).
async fn expire_one(state: &AppState, checkout_id: Uuid) -> Result<bool, AppError> {
    let mut tx = state.db.begin().await.context("begin")?;
    sqlx::query(LOCK_TIMEOUT)
        .execute(&mut *tx)
        .await
        .context("set lock_timeout")?;
    let moved = expire_and_restore(&mut tx, checkout_id).await?;
    tx.commit().await.context("commit expiry")?;
    Ok(moved)
}

// ------------------------------------------------------------ 2. lost webhooks

async fn reverify_checkouts(
    state: &AppState,
    cancel: &CancellationToken,
) -> Result<Stats, AppError> {
    // The claim itself pushes `next_reverify_at` back, so a checkout the
    // provider still reports unpaid (or cannot answer for) waits its backoff.
    let sql = format!(
        "WITH due AS ( \
             SELECT id FROM checkouts \
             WHERE status = 'expired' AND provider_payment_id IS NOT NULL \
               AND expires_at > now() - interval '{REVERIFY_WINDOW}' \
               AND (next_reverify_at IS NULL OR next_reverify_at <= now()) \
             ORDER BY COALESCE(next_reverify_at, expires_at), id \
             LIMIT $1 FOR UPDATE SKIP LOCKED) \
         UPDATE checkouts c SET reverify_attempts = c.reverify_attempts + 1, \
             next_reverify_at = now() + make_interval(secs => \
                 (SELECT {BACKOFF_SQL} FROM (SELECT c.reverify_attempts + 1 AS a) x)) \
         FROM due WHERE c.id = due.id RETURNING c.id, c.reference"
    );
    let mut tx = state.db.begin().await.context("begin claim")?;
    let references: Vec<(Uuid, String)> = sqlx::query_as(&sql)
        .bind(PROVIDER_CALLS_PER_TICK)
        .fetch_all(&mut *tx)
        .await
        .context("claim expired checkouts")?;
    tx.commit().await.context("commit claim")?;

    let mut stats = Stats::default();
    for (id, reference) in references {
        if cancel.is_cancelled() {
            break;
        }
        let result = settle_payment(state, &reference).await.map(|outcome| {
            matches!(
                outcome,
                SettleOutcome::Paid | SettleOutcome::RefundedAfterExpiry
            )
        });
        tally(state, &mut stats, REVERIFY, id, result).await;
    }
    Ok(stats)
}

// ------------------------------------------------------------ 3. ship deadline

async fn ship_deadline(state: &AppState, cancel: &CancellationToken) -> Result<Stats, AppError> {
    if !settings::load(&state.db).await?.jobs_ship_deadline_enabled {
        return Ok(Stats::default());
    }
    let sql = format!(
        "SELECT o.id FROM orders o JOIN checkouts c ON c.id = o.checkout_id \
         WHERE o.status = 'paid' AND o.ship_by < now() AND o.held_at IS NULL \
           AND NOT c.provider_flagged AND {} \
         ORDER BY o.ship_by, o.id LIMIT $1 FOR UPDATE OF o SKIP LOCKED",
        not_pushed_back(SHIP_DEADLINE, "o.id")
    );
    let ids = claim(state, &sql, SHIP_DEADLINE_PER_TICK).await?;
    let mut stats = Stats::default();
    for id in ids {
        if cancel.is_cancelled() {
            break;
        }
        let result = refund_missed_ship_deadline(state, id).await;
        tally(state, &mut stats, SHIP_DEADLINE, id, result).await;
    }
    Ok(stats)
}

/// The ship-deadline action for one order (the job calls it per claimed id):
/// refund + strike in one transaction, then the provider call. `Ok(false)`,
/// with nothing changed, when the order is no longer due: shipped, held, its
/// payment flagged, or already refunded for another reason.
pub async fn refund_missed_ship_deadline(
    state: &AppState,
    order_id: Uuid,
) -> Result<bool, AppError> {
    const REASON: &str = "ship_deadline";
    let mut tx = state.db.begin().await.context("begin")?;
    sqlx::query(LOCK_TIMEOUT)
        .execute(&mut *tx)
        .await
        .context("set lock_timeout")?;
    // begin_refund locks the variants, then the order, and refunds only a
    // `paid` order; other states are a conflict, i.e. no longer due.
    let refund_id = match begin_refund(&mut tx, order_id, true, REASON).await {
        Ok(id) => id,
        Err(AppError::Conflict("not_refundable" | "already_released")) => return Ok(false),
        Err(e) => return Err(e),
    };
    // Re-checked under the order lock begin_refund now holds: still due, not
    // held, payment not flagged, and the refund is the one this transaction
    // created for the deadline. An order that left `paid` between the claim
    // and the lock (a buyer cancel makes it refund_pending, and begin_refund
    // then returns that existing refund) fails the reason / created_at check
    // and gets no strike.
    let (due, store_id): (bool, Uuid) = sqlx::query_as(
        "SELECT o.held_at IS NULL AND o.ship_by < now() AND NOT c.provider_flagged \
                AND r.reason = $2 AND r.created_at = now(), o.store_id \
         FROM orders o JOIN refunds r ON r.order_id = o.id \
              JOIN checkouts c ON c.id = o.checkout_id WHERE o.id = $1",
    )
    .bind(order_id)
    .bind(REASON)
    .fetch_one(&mut *tx)
    .await
    .context("recheck ship deadline")?;
    if !due {
        // Rolls back anything begin_refund changed.
        return Ok(false);
    }
    sqlx::query(
        "INSERT INTO seller_strikes (store_id, order_id, reason) VALUES ($1, $2, $3) \
         ON CONFLICT (order_id, reason) DO NOTHING",
    )
    .bind(store_id)
    .bind(order_id)
    .bind(REASON)
    .execute(&mut *tx)
    .await
    .context("insert seller strike")?;
    audit::record(
        &mut tx,
        None,
        "order.ship_deadline_missed",
        "order",
        Some(order_id),
        json!({ "refund_id": refund_id, "store_id": store_id }),
    )
    .await?;
    tx.commit().await.context("commit ship deadline")?;
    // The refund is booked; a failed send stays `requested` for the sweeper.
    if let Err(e) = send_refund(state, refund_id).await {
        tracing::warn!(error = %e, "ship-deadline refund not sent; the sweeper retries");
    }
    Ok(true)
}

// ------------------------------------------------------------- 4. auto-release

async fn auto_release(state: &AppState, cancel: &CancellationToken) -> Result<Stats, AppError> {
    let sql = format!(
        "SELECT o.id FROM orders o JOIN checkouts c ON c.id = o.checkout_id \
         WHERE o.status = 'delivered' AND o.delivered_at IS NOT NULL \
           AND o.release_at <= now() AND o.held_at IS NULL AND NOT c.provider_flagged \
           AND {} \
         ORDER BY o.release_at, o.id LIMIT $1 FOR UPDATE OF o SKIP LOCKED",
        not_pushed_back(AUTO_RELEASE, "o.id")
    );
    let ids = claim(state, &sql, PROVIDER_CALLS_PER_TICK).await?;
    let mut stats = Stats::default();
    for id in ids {
        if cancel.is_cancelled() {
            break;
        }
        let result = match release_order(state, id, ReleaseReason::AutoAfterDelivery).await {
            // Count only a release this call made (a concurrent buyer
            // confirmation wins the race and leaves this a no-op).
            Ok(()) => sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM payouts WHERE order_id = $1 \
                                AND release_reason = 'auto_after_delivery')",
            )
            .bind(id)
            .fetch_one(&state.db)
            .await
            .context("check release")
            .map_err(AppError::from),
            Err(e) => Err(e),
        };
        tally(state, &mut stats, AUTO_RELEASE, id, result).await;
    }
    Ok(stats)
}

// ----------------------------------------------------------------- 5. backstop

/// No engine function: claimed and flagged in one statement.
async fn backstop(state: &AppState) -> Result<Stats, AppError> {
    let mut tx = state.db.begin().await.context("begin")?;
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "WITH due AS ( \
             SELECT id FROM orders \
             WHERE backstop_at <= now() AND backstop_flagged_at IS NULL \
               AND status NOT IN ('completed', 'refunded', 'cancelled') \
             ORDER BY backstop_at, id LIMIT $1 FOR UPDATE SKIP LOCKED) \
         UPDATE orders o SET backstop_flagged_at = now() FROM due \
         WHERE o.id = due.id RETURNING o.id",
    )
    .bind(BATCH)
    .fetch_all(&mut *tx)
    .await
    .context("flag backstop orders")?;
    for id in &ids {
        audit::record(
            &mut tx,
            None,
            "order.backstop_flagged",
            "order",
            Some(*id),
            json!({}),
        )
        .await?;
    }
    tx.commit().await.context("commit backstop")?;
    Ok(Stats {
        done: u32::try_from(ids.len()).unwrap_or(u32::MAX),
        failed: 0,
    })
}

// ------------------------------------------------------------------ 6. payouts

async fn payouts(state: &AppState, cancel: &CancellationToken) -> Result<Stats, AppError> {
    // A claimed `pending` row gets `last_polled_at = now()` here, so a poll the
    // provider cannot answer still waits an hour (settle_payout records the
    // poll again when it gets an answer). Sends move `next_attempt_at` or
    // the status themselves.
    let sql = format!(
        "WITH due AS ( \
             SELECT p.id, p.status FROM payouts p \
                 JOIN orders o ON o.id = p.order_id JOIN checkouts c ON c.id = o.checkout_id \
             WHERE ((((p.status IN ('scheduled', 'unknown') \
                       AND (p.next_attempt_at IS NULL OR p.next_attempt_at <= now()) \
                       AND (p.send_after IS NULL OR p.send_after <= now())) \
                      OR (p.status = 'sending' \
                          AND (p.sending_until IS NULL OR p.sending_until < now()))) \
                     AND o.held_at IS NULL AND NOT c.provider_flagged) \
                    OR (p.status = 'pending' \
                        AND COALESCE(p.last_polled_at, p.pending_since, p.updated_at) \
                            <= now() - interval '{PENDING_POLL_EVERY}')) \
               AND {not_pushed} \
             ORDER BY CASE p.status \
                 WHEN 'pending' THEN COALESCE(p.last_polled_at, p.pending_since, p.updated_at) \
                 WHEN 'sending' THEN p.sending_until \
                 ELSE COALESCE(p.next_attempt_at, p.send_after, p.created_at) END \
                 NULLS FIRST, p.id \
             LIMIT $1 FOR UPDATE OF p SKIP LOCKED) \
         UPDATE payouts p SET last_polled_at = CASE WHEN due.status = 'pending' \
                                                    THEN now() ELSE p.last_polled_at END \
         FROM due WHERE p.id = due.id \
         RETURNING p.id, p.reference, due.status::text",
        not_pushed = not_pushed_back(PAYOUTS, "p.id")
    );
    let mut tx = state.db.begin().await.context("begin claim")?;
    let due: Vec<(Uuid, String, String)> = sqlx::query_as(&sql)
        .bind(PROVIDER_CALLS_PER_TICK)
        .fetch_all(&mut *tx)
        .await
        .context("claim due payouts")?;
    tx.commit().await.context("commit claim")?;

    let mut stats = Stats::default();
    for (id, reference, status) in due {
        if cancel.is_cancelled() {
            break;
        }
        let result = if status == "pending" {
            settle_payout(state, &reference).await.map(|_| ())
        } else {
            send_payout(state, id).await.map(|_| ())
        };
        let result = match result {
            Ok(()) => escalate_failed(state, id).await.map(|_| true),
            Err(e) => Err(e),
        };
        tally(state, &mut stats, PAYOUTS, id, result).await;
    }
    Ok(stats)
}

/// A payout the job's call left `failed` (rejected by the provider, or failed
/// or reversed when polled) goes to `manual` for the owner, with an audit row.
async fn escalate_failed(state: &AppState, payout_id: Uuid) -> Result<bool, AppError> {
    let mut tx = state.db.begin().await.context("begin")?;
    let reason: Option<Option<String>> = sqlx::query_scalar(
        "UPDATE payouts SET status = 'manual', updated_at = now() \
         WHERE id = $1 AND status = 'failed' RETURNING last_error",
    )
    .bind(payout_id)
    .fetch_optional(&mut *tx)
    .await
    .context("escalate failed payout")?;
    let Some(reason) = reason else {
        return Ok(false);
    };
    audit::record(
        &mut tx,
        None,
        "payout.escalated_manual",
        "payout",
        Some(payout_id),
        json!({ "from": "failed", "reason": reason }),
    )
    .await?;
    tx.commit().await.context("commit escalation")?;
    tracing::warn!("payout failed; moved to manual");
    Ok(true)
}

// ------------------------------------------------------------------ 7. refunds

async fn refunds(state: &AppState, cancel: &CancellationToken) -> Result<Stats, AppError> {
    escalate_exhausted_refunds(state).await?;
    // The claim pushes `next_attempt_at` back from the refund's attempt count;
    // send_refund's own 2-minute claim keeps two senders apart. A refund sent
    // (or created) within the last minute is left to its own caller first, so
    // an inline send that just failed is not repeated in the same tick.
    let sql = format!(
        "WITH due AS ( \
             SELECT id FROM refunds \
             WHERE status = 'requested' AND attempts < $2 \
               AND (sending_until IS NULL OR sending_until < now()) \
               AND updated_at <= now() - interval '{REFUND_GRACE}' \
               AND (next_attempt_at IS NULL OR next_attempt_at <= now()) \
             ORDER BY COALESCE(next_attempt_at, created_at), id \
             LIMIT $1 FOR UPDATE SKIP LOCKED) \
         UPDATE refunds r SET next_attempt_at = now() + make_interval(secs => \
                 (SELECT {BACKOFF_SQL} FROM (SELECT r.attempts + 1 AS a) x)) \
         FROM due WHERE r.id = due.id RETURNING r.id"
    );
    let mut tx = state.db.begin().await.context("begin claim")?;
    let ids: Vec<Uuid> = sqlx::query_scalar(&sql)
        .bind(PROVIDER_CALLS_PER_TICK)
        .bind(MAX_REFUND_ATTEMPTS)
        .fetch_all(&mut *tx)
        .await
        .context("claim requested refunds")?;
    tx.commit().await.context("commit claim")?;
    let mut stats = Stats::default();
    for id in ids {
        if cancel.is_cancelled() {
            break;
        }
        let result = send_refund(state, id).await.map(|_| true);
        tally(state, &mut stats, REFUNDS, id, result).await;
    }
    Ok(stats)
}

/// `requested` refunds that used up [`MAX_REFUND_ATTEMPTS`] sends (and are not
/// being sent right now) go to `manual` for the owner, each with an audit row,
/// in one transaction fenced on `status = 'requested'`. The order stays
/// `refund_pending`: the buyer has not been paid back yet.
async fn escalate_exhausted_refunds(state: &AppState) -> Result<(), AppError> {
    let mut tx = state.db.begin().await.context("begin")?;
    let rows: Vec<(Uuid, Option<String>)> = sqlx::query_as(
        "UPDATE refunds SET status = 'manual', updated_at = now() \
         WHERE status = 'requested' AND attempts >= $1 \
           AND (sending_until IS NULL OR sending_until < now()) \
         RETURNING id, last_error",
    )
    .bind(MAX_REFUND_ATTEMPTS)
    .fetch_all(&mut *tx)
    .await
    .context("escalate exhausted refunds")?;
    for (id, reason) in &rows {
        audit::record(
            &mut tx,
            None,
            "refund.escalated_manual",
            "refund",
            Some(*id),
            json!({ "from": "requested", "reason": reason }),
        )
        .await?;
    }
    tx.commit().await.context("commit refund escalation")?;
    if !rows.is_empty() {
        tracing::warn!(
            count = rows.len(),
            "refunds still unsent after max attempts; moved to manual"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use super::*;

    #[tokio::test]
    async fn loop_survives_a_panicking_tick_and_stops_on_cancel() {
        let ticks = Arc::new(AtomicUsize::new(0));
        let cancel = CancellationToken::new();
        let handle = {
            let ticks = ticks.clone();
            let cancel = cancel.clone();
            tokio::spawn(run_every(Duration::from_millis(5), cancel, move || {
                let ticks = ticks.clone();
                async move {
                    if ticks.fetch_add(1, Ordering::SeqCst) == 0 {
                        panic!("first tick fails");
                    }
                }
            }))
        };
        while ticks.load(Ordering::SeqCst) < 3 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("stops after cancel")
            .expect("the loop itself never panics");
    }
}
