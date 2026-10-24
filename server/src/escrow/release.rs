//! Releasing an order's escrow to the seller, and paying the seller out.
//!
//! Three steps, and no provider call ever runs inside a DB transaction:
//!
//! 1. [`release_order`] (one transaction, under the order lock): an order
//!    `picked_up`/`delivered` with no payout row is booked out of escrow,
//!    marked `completed`, and gets a payout row `scheduled` (or `manual` when
//!    the payout would be ≤ 0). Ledger, key `("release", order_id)`:
//!    `escrow −(items + delivery)`, `platform_revenue +(cut + delivery)`,
//!    `provider_fees +collection_fee_share`, `platform_revenue +payout_fee`,
//!    `seller_payable +payout`. Delivery money sits in `platform_revenue`
//!    until plan 3 routes it to the courier or the self-delivering seller.
//!    The payout fee is fixed here, from the fee schedule at release.
//!    The payout waits for the provider's settlement of the buyer's payment:
//!    `send_after = max(now, settlement_time(paid_at) + buffer)`.
//! 2. [`send_payout`] (inline after release when due, else the Task 9 job):
//!    re-checks the gates (order not held, checkout not flagged, payout account
//!    verified and outside the 48h change hold, `send_after` and the backoff
//!    passed), claims the row (`scheduled|unknown -> sending`, a 2-minute
//!    lease, an account snapshot), commits, then calls the provider with the
//!    payout's one and only reference. An `unknown` payout (a timeout, or an
//!    expired lease) is looked up with `find_payout` first and never
//!    re-created blindly. The outcome is written only if the claim still holds.
//! 3. [`settle_payout`] (payout webhook, or the job's poll): applies the
//!    provider's `payout_status`. Success, key `("payout_success", id)`:
//!    `seller_payable −payout`, `seller_paid +payout`, and the actual provider
//!    fee `provider_fees +fee`, `platform_revenue −fee`. A failure after
//!    success (a reversal), key `("payout_reversal", id)`, posts the exact
//!    reverse and marks the payout `failed`. A payout still pending 24h after
//!    the provider accepted it goes `manual`: never `failed`, never re-sent.
//!
//! Personal data: the seller's account number is decrypted only for the
//! provider call and never logged, audited or returned.

use std::{collections::HashSet, time::Duration};

use anyhow::Context;
use rand::Rng;
use serde_json::json;
use sqlx::{PgConnection, PgPool};
use time::{Date, OffsetDateTime, PrimitiveDateTime, Time, UtcOffset, Weekday};
use uuid::Uuid;

use crate::{
    AppState, audit,
    checkout::service::busy_err,
    error::AppError,
    ledger::{self, Account, Entry},
    money,
    payments::{self, PayoutHandle, PayoutRequest, PayoutState, ProviderError},
    settings,
};

const REFERENCE_PREFIX: &str = "PO";
const LOCK_TIMEOUT: &str = "SET LOCAL lock_timeout = '5s'";
/// Longest wait for a provider payout call. Well under the 2-minute lease, so
/// a slow call cannot outlive its claim; past it the outcome is unknown.
pub const PAYOUT_CALL_TIMEOUT: Duration = Duration::from_secs(30);
/// Unavailable answers allowed before a payout goes to `manual`.
pub const MAX_SEND_ATTEMPTS: i32 = 8;
/// No payout within this long of a change that replaced an earlier account.
pub const ACCOUNT_CHANGE_HOLD: time::Duration = time::Duration::hours(48);
/// A payout accepted but still pending this long goes to `manual` (NIP
/// reversals can follow the NIBSS daily report up to 24h later).
pub const PENDING_MANUAL_AFTER: time::Duration = time::Duration::hours(24);
/// When a gate (hold, flag, unverified account) blocks a send, look again after this.
const GATE_RECHECK: time::Duration = time::Duration::hours(1);
const BACKOFF_FIRST_SECS: i64 = 5 * 60;
const BACKOFF_CAP_SECS: i64 = 6 * 60 * 60;
const SETTLEMENT_HOUR: u8 = 19;
const NARRATION: &str = "Marketplace order payout";

/// Africa/Lagos: UTC+1 all year (no DST).
pub const LAGOS: UtcOffset = match UtcOffset::from_hms(1, 0, 0) {
    Ok(o) => o,
    Err(_) => panic!("valid offset"),
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReleaseReason {
    BuyerConfirmed,
    AutoAfterDelivery,
    OwnerDecision,
}

impl ReleaseReason {
    pub fn as_str(self) -> &'static str {
        match self {
            ReleaseReason::BuyerConfirmed => "buyer_confirmed",
            ReleaseReason::AutoAfterDelivery => "auto_after_delivery",
            ReleaseReason::OwnerDecision => "owner_decision",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PayoutOutcome {
    pub payout_id: Uuid,
    pub reference: String,
    /// The payout's status after this call.
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettlePayoutOutcome {
    /// The payout's status after applying the provider's answer.
    Settled { status: String },
    /// No payout has this reference (for this provider).
    UnknownReference,
    /// The provider could not be asked now; a webhook answers 503.
    RetryLater,
}

fn db_err(ctx: &'static str) -> impl FnOnce(sqlx::Error) -> AppError {
    busy_err("order_busy", ctx)
}

fn too_large() -> AppError {
    AppError::Unprocessable("amount_too_large", "amount is too large".into())
}

/// When the provider settles a payment made at `paid_at`: the next working
/// day after its Lagos date, at 19:00 Lagos time, returned in UTC. Saturdays,
/// Sundays and `holidays` (Lagos dates) are not working days, so Friday to
/// Sunday payments settle on Monday (Mono: "Fri–Sun settle Monday 7 PM").
pub fn settlement_time(paid_at: OffsetDateTime, holidays: &HashSet<Date>) -> OffsetDateTime {
    let mut day = paid_at.to_offset(LAGOS).date();
    // Bounded: a year of consecutive holidays would be a data error.
    for _ in 0..366 {
        day = day.next_day().expect("date in range");
        let weekend = matches!(day.weekday(), Weekday::Saturday | Weekday::Sunday);
        if !weekend && !holidays.contains(&day) {
            break;
        }
    }
    let at = Time::from_hms(SETTLEMENT_HOUR, 0, 0).expect("valid time");
    PrimitiveDateTime::new(day, at)
        .assume_offset(LAGOS)
        .to_offset(UtcOffset::UTC)
}

/// Bank holidays from `from` onwards (the owner maintains the table).
pub async fn load_bank_holidays(
    conn: &mut PgConnection,
    from: Date,
) -> Result<HashSet<Date>, AppError> {
    let days: Vec<Date> = sqlx::query_scalar(
        "SELECT day FROM bank_holidays WHERE day >= $1 AND day <= $1 + 400 ORDER BY day",
    )
    .bind(from)
    .fetch_all(conn)
    .await
    .context("load bank holidays")?;
    Ok(days.into_iter().collect())
}

/// Delay before retry `attempt` (1-based) after an Unavailable answer: 5
/// minutes, doubling, plus up to 20% jitter (`jitter` in 0..=1), capped at 6h.
pub fn backoff_delay(attempt: i32, jitter: f64) -> time::Duration {
    let doublings = u32::try_from(attempt.max(1) - 1).unwrap_or(0).min(20);
    let base = (BACKOFF_FIRST_SECS << doublings).min(BACKOFF_CAP_SECS);
    let extra = (base as f64 * 0.2 * jitter.clamp(0.0, 1.0)) as i64;
    time::Duration::seconds((base + extra).min(BACKOFF_CAP_SECS))
}

fn backoff(attempt: i32) -> time::Duration {
    backoff_delay(attempt, rand::thread_rng().r#gen::<f64>())
}

#[derive(sqlx::FromRow)]
struct ReleaseRow {
    status: String,
    items_total_kobo: i64,
    delivery_fee_kobo: i64,
    platform_cut_kobo: i64,
    collection_fee_share_kobo: i64,
    checkout_id: Uuid,
    paid_at: Option<OffsetDateTime>,
    has_payout: bool,
}

/// Step 1 (see the module docs). Idempotent: an order that is not
/// `picked_up`/`delivered`, or already has a payout, is left as it is (Ok).
/// Unknown order -> 404. After commit, a payout that is already due is sent
/// inline; a send failure is only logged (the job retries).
pub async fn release_order(
    state: &AppState,
    order_id: Uuid,
    reason: ReleaseReason,
) -> Result<(), AppError> {
    // The fee schedule and payout provider at release are fixed on the payout.
    let settings = settings::load(&state.db).await?;
    let mut tx = state.db.begin().await.context("begin")?;
    sqlx::query(LOCK_TIMEOUT)
        .execute(&mut *tx)
        .await
        .map_err(db_err("set lock_timeout"))?;
    let row: ReleaseRow = sqlx::query_as(
        "SELECT o.status::text AS status, o.items_total_kobo, o.delivery_fee_kobo, \
                o.platform_cut_kobo, o.collection_fee_share_kobo, o.checkout_id, c.paid_at, \
                EXISTS (SELECT 1 FROM payouts p WHERE p.order_id = o.id) AS has_payout \
         FROM orders o JOIN checkouts c ON c.id = o.checkout_id \
         WHERE o.id = $1 FOR UPDATE OF o",
    )
    .bind(order_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(db_err("lock order"))?
    .ok_or(AppError::NotFound)?;
    if !matches!(row.status.as_str(), "picked_up" | "delivered") || row.has_payout {
        return Ok(());
    }

    let items = row.items_total_kobo;
    let delivery = row.delivery_fee_kobo;
    let cut = row.platform_cut_kobo;
    let share = row.collection_fee_share_kobo;
    let before_fee = items
        .checked_sub(cut)
        .and_then(|x| x.checked_sub(share))
        .ok_or_else(too_large)?;
    // The fee tier and the stamp-duty threshold are compared against the
    // payout BEFORE the fee (Task 1 ruling): a payout just over ₦10,000 pays
    // the duty even if the net lands just under it.
    let payout_fee = money::payout_fee_kobo(before_fee, &settings.fee_schedule());
    let payout = before_fee.checked_sub(payout_fee).ok_or_else(too_large)?;
    let gross = items.checked_add(delivery).ok_or_else(too_large)?;
    let cut_and_delivery = cut.checked_add(delivery).ok_or_else(too_large)?;

    let now = OffsetDateTime::now_utc();
    let paid_at = row.paid_at.unwrap_or(now);
    let holidays = load_bank_holidays(&mut tx, paid_at.to_offset(LAGOS).date()).await?;
    let buffer = time::Duration::hours(i64::from(settings.payout_settlement_buffer_hours));
    let send_after = (settlement_time(paid_at, &holidays) + buffer).max(now);
    let (status, last_error) = if payout <= 0 {
        ("manual", Some("non_positive_payout"))
    } else {
        ("scheduled", None)
    };

    let reference = payments::new_reference(REFERENCE_PREFIX);
    let entry = |account, amount_kobo| Entry {
        order_id: Some(order_id),
        checkout_id: Some(row.checkout_id),
        account,
        amount_kobo,
        kind: "release",
        reference: Some(reference.clone()),
    };
    ledger::post(
        &mut tx,
        ("release", &order_id.to_string()),
        &[
            entry(Account::Escrow, -gross),
            entry(Account::PlatformRevenue, cut_and_delivery),
            entry(Account::ProviderFees, share),
            // The payout fee charged to the seller is platform revenue; the
            // provider's real fee is booked when settle_payout learns it.
            entry(Account::PlatformRevenue, payout_fee),
            entry(Account::SellerPayable, payout),
        ],
    )
    .await?;
    let payout_id: Uuid = sqlx::query_scalar(
        "INSERT INTO payouts (order_id, reference, provider, amount_kobo, fee_kobo, status, \
                              send_after, release_reason, last_error) \
         VALUES ($1, $2, $3, $4, $5, $6::payout_status, $7, $8, $9) RETURNING id",
    )
    .bind(order_id)
    .bind(&reference)
    .bind(&settings.payout_provider)
    .bind(payout)
    .bind(payout_fee)
    .bind(status)
    .bind(send_after)
    .bind(reason.as_str())
    .bind(last_error)
    .fetch_one(&mut *tx)
    .await
    .context("insert payout")?;
    sqlx::query("UPDATE orders SET status = 'completed', completed_at = now() WHERE id = $1")
        .bind(order_id)
        .execute(&mut *tx)
        .await
        .context("complete order")?;
    audit::record(
        &mut tx,
        None,
        "order.released",
        "order",
        Some(order_id),
        json!({
            "payout_id": payout_id,
            "reason": reason.as_str(),
            "payout_kobo": payout,
            "payout_fee_kobo": payout_fee,
            "payout_status": status,
            // Fraud signal: a buyer confirming minutes after paying.
            "minutes_since_paid": (now - paid_at).whole_minutes(),
        }),
    )
    .await?;
    tx.commit().await.map_err(db_err("commit release"))?;

    if status == "scheduled"
        && send_after <= OffsetDateTime::now_utc()
        && let Err(e) = send_payout(state, payout_id).await
    {
        tracing::warn!(error = %e, "payout after release not sent; the job retries");
    }
    Ok(())
}

#[derive(sqlx::FromRow)]
struct SendRow {
    status: String,
    sending_until: Option<OffsetDateTime>,
    send_after: Option<OffsetDateTime>,
    next_attempt_at: Option<OffsetDateTime>,
    reference: String,
    provider: String,
    amount_kobo: i64,
    store_id: Uuid,
    held: bool,
    flagged: bool,
    now: OffsetDateTime,
}

#[derive(sqlx::FromRow)]
struct AccountRow {
    status: String,
    changed_at: OffsetDateTime,
    previous_account_existed: bool,
    bank_code: String,
    account_number_enc: Vec<u8>,
    account_last4: String,
}

#[derive(sqlx::FromRow)]
struct Claim {
    sending_until: OffsetDateTime,
    attempts: i32,
    bank_code: String,
    account_number_enc: Vec<u8>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// `scheduled`: nothing was sent yet.
    Create,
    /// `unknown` or an expired `sending` lease: something may have been sent.
    Lookup,
}

/// Step 2 (see the module docs). Never call it inside an open transaction.
/// Returns the payout's status after the call; a payout that is not due,
/// blocked by a gate, or claimed by another sender is left as it is.
pub async fn send_payout(state: &AppState, payout_id: Uuid) -> Result<PayoutOutcome, AppError> {
    let mut tx = state.db.begin().await.context("begin")?;
    sqlx::query(LOCK_TIMEOUT)
        .execute(&mut *tx)
        .await
        .map_err(db_err("set lock_timeout"))?;
    // Lock order: orders before payouts.
    sqlx::query(
        "SELECT o.id FROM orders o JOIN payouts p ON p.order_id = o.id \
         WHERE p.id = $1 FOR SHARE OF o",
    )
    .bind(payout_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(db_err("lock order"))?
    .ok_or(AppError::NotFound)?;
    let row: SendRow = sqlx::query_as(
        "SELECT p.status::text AS status, p.sending_until, p.send_after, p.next_attempt_at, \
                p.reference, p.provider, p.amount_kobo, o.store_id, \
                o.held_at IS NOT NULL AS held, c.provider_flagged AS flagged, now() AS now \
         FROM payouts p JOIN orders o ON o.id = p.order_id JOIN checkouts c ON c.id = o.checkout_id \
         WHERE p.id = $1 FOR UPDATE OF p",
    )
    .bind(payout_id)
    .fetch_one(&mut *tx)
    .await
    .map_err(db_err("lock payout"))?;
    let now = row.now;
    let outcome = |status: &str| PayoutOutcome {
        payout_id,
        reference: row.reference.clone(),
        status: status.to_string(),
    };
    let mode = match row.status.as_str() {
        "scheduled" => Mode::Create,
        "unknown" => Mode::Lookup,
        "sending" if row.sending_until.is_none_or(|u| u < now) => Mode::Lookup,
        other => return Ok(outcome(other)),
    };
    // Where a blocked payout waits: an expired lease becomes `unknown`.
    let waiting = if mode == Mode::Lookup {
        "unknown"
    } else {
        "scheduled"
    };
    if row.next_attempt_at.is_some_and(|t| t > now) {
        if row.status == "sending" {
            defer(&mut tx, payout_id, waiting, row.next_attempt_at, None).await?;
            tx.commit().await.context("commit")?;
        }
        return Ok(outcome(if row.status == "sending" {
            waiting
        } else {
            &row.status
        }));
    }

    let account: Option<AccountRow> = sqlx::query_as(
        "SELECT status::text AS status, changed_at, previous_account_existed, bank_code, \
                account_number_enc, account_last4 \
         FROM payout_accounts WHERE store_id = $1 FOR SHARE",
    )
    .bind(row.store_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(db_err("load payout account"))?;
    let blocked: Option<(OffsetDateTime, &str)> = if let Some(after) =
        row.send_after.filter(|t| *t > now)
    {
        Some((after, "awaiting_settlement"))
    } else if row.held {
        Some((now + GATE_RECHECK, "order_held"))
    } else if row.flagged {
        Some((now + GATE_RECHECK, "provider_flagged"))
    } else {
        match &account {
            Some(a) if a.status != "verified" => Some((now + GATE_RECHECK, "account_not_verified")),
            None => Some((now + GATE_RECHECK, "account_not_verified")),
            Some(a) if a.previous_account_existed && a.changed_at + ACCOUNT_CHANGE_HOLD > now => {
                Some((a.changed_at + ACCOUNT_CHANGE_HOLD, "account_change_hold"))
            }
            Some(_) => None,
        }
    };
    if let Some((until, why)) = blocked {
        defer(&mut tx, payout_id, waiting, Some(until), Some(why)).await?;
        tx.commit().await.context("commit")?;
        tracing::info!(reason = why, "payout not sent yet");
        return Ok(outcome(waiting));
    }
    let account = account.expect("checked above");

    // Claim with a 2-minute lease and snapshot the account paid. A payout that
    // may already be in flight (Lookup) keeps its earlier snapshot, so a later
    // account change never redirects it.
    let keep_snapshot = mode == Mode::Lookup;
    let claim: Claim = sqlx::query_as(
        "UPDATE payouts SET status = 'sending', sending_until = now() + interval '2 minutes', \
                attempts = attempts + 1, updated_at = now(), \
                bank_code = CASE WHEN $2 AND bank_code IS NOT NULL THEN bank_code ELSE $3 END, \
                account_number_enc = CASE WHEN $2 AND account_number_enc IS NOT NULL \
                                     THEN account_number_enc ELSE $4 END, \
                account_last4 = CASE WHEN $2 AND account_last4 IS NOT NULL \
                                THEN account_last4 ELSE $5 END \
         WHERE id = $1 \
         RETURNING sending_until, attempts, bank_code, account_number_enc",
    )
    .bind(payout_id)
    .bind(keep_snapshot)
    .bind(&account.bank_code)
    .bind(&account.account_number_enc)
    .bind(&account.account_last4)
    .fetch_one(&mut *tx)
    .await
    .context("claim payout")?;
    tx.commit().await.map_err(db_err("commit claim"))?;

    let result = call_provider(state, &row, &claim, mode).await;
    let (status, provider_id, error, next_attempt): (
        &str,
        Option<String>,
        Option<&str>,
        Option<OffsetDateTime>,
    ) = match result {
        Sent::Accepted(h) => ("pending", Some(h.provider_payout_id), None, None),
        Sent::Rejected => ("failed", None, Some("rejected"), None),
        Sent::NotSupported => ("manual", None, Some("not_supported"), None),
        Sent::Unavailable if claim.attempts >= MAX_SEND_ATTEMPTS => {
            ("manual", None, Some("unavailable"), None)
        }
        Sent::Unavailable => (
            "scheduled",
            None,
            Some("unavailable"),
            Some(now + backoff(claim.attempts)),
        ),
        Sent::Unknown(why) => (
            "unknown",
            None,
            Some(why),
            Some(now + backoff(claim.attempts)),
        ),
        Sent::Misconfigured(why) => ("manual", None, Some(why), None),
    };
    tracing::info!(
        provider = row.provider.as_str(),
        outcome = status,
        "payout sent"
    );

    let mut tx = state.db.begin().await.context("begin")?;
    sqlx::query(
        "SELECT o.id FROM orders o JOIN payouts p ON p.order_id = o.id \
         WHERE p.id = $1 FOR SHARE OF o",
    )
    .bind(payout_id)
    .execute(&mut *tx)
    .await
    .map_err(db_err("lock order"))?;
    // Fenced on our claim: a newer sender (after our lease expired) owns the outcome.
    let fenced: Option<Uuid> = sqlx::query_scalar(
        "UPDATE payouts SET status = $2::payout_status, \
                provider_payout_id = COALESCE($3, provider_payout_id), \
                last_error = $4, next_attempt_at = $5, sending_until = NULL, \
                pending_since = CASE WHEN $2 = 'pending' THEN now() ELSE pending_since END, \
                updated_at = now() \
         WHERE id = $1 AND status = 'sending' AND sending_until = $6 RETURNING id",
    )
    .bind(payout_id)
    .bind(status)
    .bind(&provider_id)
    .bind(error)
    .bind(next_attempt)
    .bind(claim.sending_until)
    .fetch_optional(&mut *tx)
    .await
    .context("record payout outcome")?;
    if fenced.is_none() {
        drop(tx);
        tracing::warn!(
            provider = row.provider.as_str(),
            outcome = status,
            "payout claim lost; late answer not recorded"
        );
        return current(&state.db, payout_id).await;
    }
    audit::record(
        &mut tx,
        None,
        "payout.send_attempt",
        "payout",
        Some(payout_id),
        json!({ "status": status, "attempt": claim.attempts, "error": error }),
    )
    .await?;
    tx.commit().await.context("commit")?;
    Ok(outcome(status))
}

async fn defer(
    conn: &mut PgConnection,
    payout_id: Uuid,
    status: &str,
    next_attempt_at: Option<OffsetDateTime>,
    error: Option<&str>,
) -> Result<(), AppError> {
    sqlx::query(
        "UPDATE payouts SET status = $2::payout_status, next_attempt_at = $3, \
                last_error = COALESCE($4, last_error), sending_until = NULL, updated_at = now() \
         WHERE id = $1",
    )
    .bind(payout_id)
    .bind(status)
    .bind(next_attempt_at)
    .bind(error)
    .execute(conn)
    .await
    .context("defer payout")?;
    Ok(())
}

enum Sent {
    Accepted(PayoutHandle),
    Rejected,
    NotSupported,
    /// Definitely not sent: retry later with the same reference.
    Unavailable,
    /// May have been sent: look it up before anything else.
    Unknown(&'static str),
    Misconfigured(&'static str),
}

/// The provider calls for a claimed payout; no transaction is open.
async fn call_provider(state: &AppState, row: &SendRow, claim: &Claim, mode: Mode) -> Sent {
    let Some(provider) = state.providers.by_name(&row.provider) else {
        return Sent::Unavailable;
    };
    if mode == Mode::Lookup {
        let found =
            tokio::time::timeout(PAYOUT_CALL_TIMEOUT, provider.find_payout(&row.reference)).await;
        match found {
            Ok(Ok(Some(handle))) => return Sent::Accepted(handle),
            // Confirmed absent: safe to create with the same reference.
            Ok(Ok(None)) => {}
            // Could not tell: never create on a guess.
            Ok(Err(_)) | Err(_) => return Sent::Unknown("lookup_failed"),
        }
    }
    let account_number = match state.cipher.decrypt(&claim.account_number_enc) {
        Ok(n) => n,
        Err(_) => return Sent::Misconfigured("account_decrypt_failed"),
    };
    let request = PayoutRequest {
        reference: row.reference.clone(),
        amount_kobo: row.amount_kobo,
        bank_code: claim.bank_code.clone(),
        account_number,
        narration: NARRATION.to_string(),
    };
    let created = tokio::time::timeout(PAYOUT_CALL_TIMEOUT, provider.create_payout(request)).await;
    // Every error kind is matched explicitly: a Timeout must become `unknown`,
    // never be propagated with `?` or retried as a fresh create.
    match created {
        Ok(Ok(handle)) => Sent::Accepted(handle),
        Ok(Err(ProviderError::Rejected { .. })) => Sent::Rejected,
        Ok(Err(ProviderError::Unavailable(_))) => Sent::Unavailable,
        Ok(Err(ProviderError::Timeout)) | Err(_) => Sent::Unknown("timed_out"),
        Ok(Err(ProviderError::NotSupported)) => Sent::NotSupported,
        Ok(Err(ProviderError::InvalidSignature)) => Sent::Unknown("unexpected_answer"),
    }
}

async fn current(db: &PgPool, payout_id: Uuid) -> Result<PayoutOutcome, AppError> {
    let (reference, status): (String, String) =
        sqlx::query_as("SELECT reference, status::text FROM payouts WHERE id = $1")
            .bind(payout_id)
            .fetch_optional(db)
            .await
            .context("load payout")?
            .ok_or(AppError::NotFound)?;
    Ok(PayoutOutcome {
        payout_id,
        reference,
        status,
    })
}

#[derive(sqlx::FromRow)]
struct SettleRow {
    id: Uuid,
    status: String,
    amount_kobo: i64,
    provider_fee_kobo: Option<i64>,
    pending_since: Option<OffsetDateTime>,
    order_id: Uuid,
    checkout_id: Uuid,
    now: OffsetDateTime,
}

/// Whether a payout in `status` can still change on the provider's answer.
fn settleable(status: &str, has_provider_id: bool) -> bool {
    match status {
        "pending" | "unknown" | "sending" | "successful" => true,
        // Pending too long, but the provider did accept it.
        "manual" => has_provider_id,
        _ => false,
    }
}

/// Step 3 (see the module docs). Idempotent: status transitions are checked
/// under the payout lock and every ledger post has its own key.
pub async fn settle_payout(
    state: &AppState,
    reference: &str,
) -> Result<SettlePayoutOutcome, AppError> {
    settle_payout_from(state, reference, None).await
}

/// [`settle_payout`] for a webhook from `provider`: a payout owned by another
/// provider counts as unknown.
pub async fn settle_payout_from(
    state: &AppState,
    reference: &str,
    from_provider: Option<&str>,
) -> Result<SettlePayoutOutcome, AppError> {
    let row: Option<(String, String, Option<String>)> = sqlx::query_as(
        "SELECT status::text, provider, provider_payout_id FROM payouts WHERE reference = $1",
    )
    .bind(reference)
    .fetch_optional(&state.db)
    .await
    .context("find payout")?;
    let Some((status, provider_name, provider_id)) =
        row.filter(|(_, p, _)| from_provider.is_none_or(|f| f == p))
    else {
        return Ok(SettlePayoutOutcome::UnknownReference);
    };
    if !settleable(&status, provider_id.is_some()) {
        return Ok(SettlePayoutOutcome::Settled { status });
    }
    let Some(provider) = state.providers.by_name(&provider_name) else {
        tracing::warn!("payout provider is not configured");
        return Ok(SettlePayoutOutcome::RetryLater);
    };
    let answer = tokio::time::timeout(
        PAYOUT_CALL_TIMEOUT,
        provider.payout_status(reference, provider_id.as_deref()),
    )
    .await;
    let answer = match answer {
        Ok(Ok(a)) => a,
        Ok(Err(e)) => {
            tracing::warn!(provider = provider.name(), error = %e, "payout status failed");
            return Ok(SettlePayoutOutcome::RetryLater);
        }
        Err(_) => {
            tracing::warn!(provider = provider.name(), "payout status timed out");
            return Ok(SettlePayoutOutcome::RetryLater);
        }
    };

    let mut tx = state.db.begin().await.context("begin")?;
    sqlx::query(LOCK_TIMEOUT)
        .execute(&mut *tx)
        .await
        .map_err(db_err("set lock_timeout"))?;
    sqlx::query(
        "SELECT o.id FROM orders o JOIN payouts p ON p.order_id = o.id \
         WHERE p.reference = $1 FOR SHARE OF o",
    )
    .bind(reference)
    .execute(&mut *tx)
    .await
    .map_err(db_err("lock order"))?;
    let p: SettleRow = sqlx::query_as(
        "SELECT p.id, p.status::text AS status, p.amount_kobo, p.provider_fee_kobo, \
                p.pending_since, p.order_id, o.checkout_id, now() AS now \
         FROM payouts p JOIN orders o ON o.id = p.order_id \
         WHERE p.reference = $1 FOR UPDATE OF p",
    )
    .bind(reference)
    .fetch_one(&mut *tx)
    .await
    .map_err(db_err("lock payout"))?;
    let entry = |account, amount_kobo, kind| Entry {
        order_id: Some(p.order_id),
        checkout_id: Some(p.checkout_id),
        account,
        amount_kobo,
        kind,
        reference: Some(reference.to_string()),
    };
    let not_final = matches!(
        p.status.as_str(),
        "pending" | "unknown" | "sending" | "manual"
    );

    let new_status: String = match answer {
        PayoutState::Pending => {
            let overdue = p.status == "pending"
                && p.pending_since
                    .is_some_and(|t| t + PENDING_MANUAL_AFTER <= p.now);
            if overdue {
                sqlx::query(
                    "UPDATE payouts SET status = 'manual', last_error = 'pending_over_24h', \
                            last_polled_at = now(), updated_at = now() WHERE id = $1",
                )
                .bind(p.id)
                .execute(&mut *tx)
                .await
                .context("payout to manual")?;
                tracing::warn!("payout pending over 24h; moved to manual");
                "manual".into()
            } else {
                sqlx::query("UPDATE payouts SET last_polled_at = now() WHERE id = $1")
                    .bind(p.id)
                    .execute(&mut *tx)
                    .await
                    .context("record poll")?;
                p.status.clone()
            }
        }
        PayoutState::Successful { fee_kobo } if not_final => {
            // A negative fee is bad provider data; book none.
            let fee = fee_kobo.max(0);
            const KIND: &str = "payout_success";
            ledger::post(
                &mut tx,
                (KIND, &p.id.to_string()),
                &[
                    entry(Account::SellerPayable, -p.amount_kobo, KIND),
                    entry(Account::SellerPaid, p.amount_kobo, KIND),
                    entry(Account::ProviderFees, fee, KIND),
                    entry(Account::PlatformRevenue, -fee, KIND),
                ],
            )
            .await?;
            sqlx::query(
                "UPDATE payouts SET status = 'successful', provider_fee_kobo = $2, \
                        settled_at = now(), last_error = NULL, sending_until = NULL, \
                        next_attempt_at = NULL, last_polled_at = now(), updated_at = now() \
                 WHERE id = $1",
            )
            .bind(p.id)
            .bind(fee)
            .execute(&mut *tx)
            .await
            .context("payout successful")?;
            "successful".into()
        }
        PayoutState::Successful { .. } => p.status.clone(),
        PayoutState::Failed { .. } | PayoutState::Reversed { .. } if p.status == "successful" => {
            // Reversed after success: undo exactly what the success booked.
            let fee = p.provider_fee_kobo.unwrap_or(0);
            const KIND: &str = "payout_reversal";
            ledger::post(
                &mut tx,
                (KIND, &p.id.to_string()),
                &[
                    entry(Account::SellerPaid, -p.amount_kobo, KIND),
                    entry(Account::SellerPayable, p.amount_kobo, KIND),
                    entry(Account::ProviderFees, -fee, KIND),
                    entry(Account::PlatformRevenue, fee, KIND),
                ],
            )
            .await?;
            mark_failed(&mut tx, p.id, "reversed").await?;
            tracing::warn!("payout reversed after success");
            "failed".into()
        }
        PayoutState::Failed { .. } | PayoutState::Reversed { .. } if not_final => {
            // Never paid: seller_payable still holds the money; the owner re-queues.
            mark_failed(&mut tx, p.id, "provider_failed").await?;
            "failed".into()
        }
        PayoutState::Failed { .. } | PayoutState::Reversed { .. } => p.status.clone(),
    };
    if new_status != p.status {
        audit::record(
            &mut tx,
            None,
            "payout.settled",
            "payout",
            Some(p.id),
            json!({ "from": p.status, "to": new_status }),
        )
        .await?;
    }
    tx.commit().await.map_err(db_err("commit payout settle"))?;
    Ok(SettlePayoutOutcome::Settled { status: new_status })
}

async fn mark_failed(conn: &mut PgConnection, id: Uuid, error: &str) -> Result<(), AppError> {
    sqlx::query(
        "UPDATE payouts SET status = 'failed', last_error = $2, sending_until = NULL, \
                next_attempt_at = NULL, last_polled_at = now(), updated_at = now() \
         WHERE id = $1",
    )
    .bind(id)
    .bind(error)
    .execute(conn)
    .await
    .context("payout failed")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::Month;

    fn day(d: u8) -> Date {
        Date::from_calendar_date(2026, Month::October, d).unwrap()
    }

    /// `hh:mm` Lagos time on 2026-10-`d` (19 = Monday).
    fn wat(d: u8, h: u8, m: u8) -> OffsetDateTime {
        PrimitiveDateTime::new(day(d), Time::from_hms(h, m, 0).unwrap()).assume_offset(LAGOS)
    }

    fn none() -> HashSet<Date> {
        HashSet::new()
    }

    #[test]
    fn settlement_monday_morning_is_tuesday_7pm() {
        let s = settlement_time(wat(19, 10, 0), &none());
        assert_eq!(s, wat(20, 19, 0));
        assert_eq!(s.offset(), UtcOffset::UTC);
    }

    #[test]
    fn settlement_thursday_evening_is_friday_7pm() {
        assert_eq!(settlement_time(wat(22, 20, 0), &none()), wat(23, 19, 0));
    }

    #[test]
    fn settlement_friday_is_monday() {
        assert_eq!(settlement_time(wat(23, 10, 0), &none()), wat(26, 19, 0));
        assert_eq!(settlement_time(wat(24, 12, 0), &none()), wat(26, 19, 0));
    }

    #[test]
    fn settlement_sunday_late_is_monday() {
        assert_eq!(settlement_time(wat(25, 23, 0), &none()), wat(26, 19, 0));
        // 23:30 UTC on Sunday is already Monday 00:30 in Lagos: Tuesday.
        let sunday_utc = wat(26, 0, 30).to_offset(UtcOffset::UTC);
        assert_eq!(sunday_utc.day(), 25);
        assert_eq!(settlement_time(sunday_utc, &none()), wat(27, 19, 0));
    }

    #[test]
    fn settlement_skips_a_holiday_on_the_settlement_day() {
        let holidays: HashSet<Date> = [day(20)].into();
        assert_eq!(settlement_time(wat(19, 10, 0), &holidays), wat(21, 19, 0));
        // Friday with a Monday holiday: Tuesday.
        let holidays: HashSet<Date> = [day(26)].into();
        assert_eq!(settlement_time(wat(23, 10, 0), &holidays), wat(27, 19, 0));
    }

    #[test]
    fn backoff_starts_at_5_minutes_doubles_and_caps_at_6_hours() {
        assert_eq!(backoff_delay(1, 0.0), time::Duration::minutes(5));
        assert_eq!(backoff_delay(2, 0.0), time::Duration::minutes(10));
        assert_eq!(backoff_delay(3, 0.0), time::Duration::minutes(20));
        assert_eq!(backoff_delay(7, 0.0), time::Duration::minutes(320));
        assert_eq!(backoff_delay(8, 0.0), time::Duration::hours(6));
        assert_eq!(backoff_delay(40, 0.0), time::Duration::hours(6));
        assert_eq!(backoff_delay(0, 0.0), time::Duration::minutes(5));
        // Jitter adds up to 20%, still capped.
        assert_eq!(backoff_delay(1, 1.0), time::Duration::minutes(6));
        assert_eq!(backoff_delay(9, 1.0), time::Duration::hours(6));
    }

    #[test]
    fn release_reason_names() {
        assert_eq!(ReleaseReason::BuyerConfirmed.as_str(), "buyer_confirmed");
        assert_eq!(
            ReleaseReason::AutoAfterDelivery.as_str(),
            "auto_after_delivery"
        );
        assert_eq!(ReleaseReason::OwnerDecision.as_str(), "owner_decision");
    }
}
