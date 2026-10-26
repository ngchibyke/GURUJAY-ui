//! Owner money overrides: mark a payout paid or a refund refunded by hand,
//! re-queue a reversed payout, and clear a provider flag on a payment.
//!
//! Each is one state-conditional transition under the row locks, in the
//! global lock order (`checkouts -> orders -> payouts`; refunds after their
//! order), with its audit row in the same transaction. A request against any
//! other state is 409 naming the current one, so a retry never acts twice.
//! Amounts and destinations come from the stored rows only.

use anyhow::Context;
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

use super::LOCK_TIMEOUT;
use crate::{
    AppState, audit,
    checkout::service::busy_err,
    error::AppError,
    escrow::{
        SettlePayoutOutcome,
        release::{PAYOUT_CALL_TIMEOUT, may_have_paid, round_ref},
        send_payout, settle_payout,
    },
    ledger::{self, Account, Entry},
    payments::{self, PayoutState, ProviderError},
};

fn db_err(ctx: &'static str) -> impl FnOnce(sqlx::Error) -> AppError {
    busy_err("order_busy", ctx)
}

/// Payout statuses the owner may mark paid: never sent, given up on, or with
/// an outcome nobody could confirm. Never `sending`/`pending`/`scheduled`.
const MARK_PAID_FROM: &[&str] = &["manual", "failed", "unknown"];
/// Refund statuses the owner may mark refunded.
const MARK_REFUNDED_FROM: &[&str] = &["manual", "failed"];

fn payout_state(status: &str) -> AppError {
    AppError::ConflictDetail("invalid_payout_state", format!("payout is {status}"))
}

#[derive(sqlx::FromRow)]
struct LockedPayout {
    status: String,
    attempts: i32,
    updated_at: time::OffsetDateTime,
    round: i32,
    amount_kobo: i64,
    order_id: Uuid,
    checkout_id: Uuid,
    reference: String,
}

/// Locks the payout's order (share) then the payout; 404 when unknown.
async fn lock_payout(
    tx: &mut sqlx::PgConnection,
    payout_id: Uuid,
) -> Result<LockedPayout, AppError> {
    sqlx::query(LOCK_TIMEOUT)
        .execute(&mut *tx)
        .await
        .context("set lock_timeout")?;
    sqlx::query(
        "SELECT o.id FROM orders o JOIN payouts p ON p.order_id = o.id \
         WHERE p.id = $1 FOR SHARE OF o",
    )
    .bind(payout_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(db_err("lock order"))?
    .ok_or(AppError::NotFound)?;
    sqlx::query_as(
        "SELECT p.status::text AS status, p.attempts, p.updated_at, p.round, p.amount_kobo, \
                p.order_id, o.checkout_id, p.reference \
         FROM payouts p JOIN orders o ON o.id = p.order_id WHERE p.id = $1 FOR UPDATE OF p",
    )
    .bind(payout_id)
    .fetch_one(&mut *tx)
    .await
    .map_err(db_err("lock payout"))
}

#[derive(sqlx::FromRow)]
struct PaidCheck {
    status: String,
    reference: String,
    provider: String,
    provider_payout_id: Option<String>,
    last_error: Option<String>,
    attempts: i32,
    updated_at: time::OffsetDateTime,
}

/// Who settled the payout on a mark-paid request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkedPaid {
    /// The owner's hand payment was recorded.
    ByOwner,
    /// The provider had paid it: settled from the provider's answer instead.
    ByProvider,
}

fn provider_unavailable() -> AppError {
    AppError::Unavailable("provider_unavailable")
}

fn lookup_inconclusive() -> AppError {
    AppError::ConflictDetail(
        "provider_lookup_inconclusive",
        "The provider cannot tell whether this payout exists. Check the provider's dashboard, \
         then repeat with \"attest_provider_checked\": true"
            .into(),
    )
}

/// What the provider lookup before an owner override found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lookup {
    /// No lookup was needed (the payout cannot have been paid by the provider).
    NotNeeded,
    /// The provider has no such payout.
    Absent,
    /// The provider could not tell; the owner attested they checked.
    Inconclusive,
}

impl Lookup {
    fn as_str(self) -> &'static str {
        match self {
            Lookup::NotNeeded => "not_needed",
            Lookup::Absent => "absent",
            Lookup::Inconclusive => "inconclusive",
        }
    }
}

/// The owner paid the seller outside the provider. Books `seller_payable ->
/// seller_paid` under the round's success key (none for a zero payout);
/// provider answers no longer change the payout afterwards.
///
/// A payout that may have been paid by the provider (`unknown`, or `manual`
/// after the provider accepted it or its outcome stayed unknown) is checked
/// with the provider first, outside any transaction, so the seller is never
/// paid twice:
/// - the provider has no such payout: the owner's payment is recorded;
/// - the provider has it: it is settled from the provider's answer
///   ([`settle_payout`]). Successful -> settled by the provider (real fee
///   booked), returned as [`MarkedPaid::ByProvider`]; failed/reversed -> 409
///   naming the new status (the owner may then mark it paid); still pending
///   -> 409 `payout_pending_at_provider`;
/// - the provider cannot answer: 503 `provider_unavailable`, nothing changed;
/// - the provider answers but cannot tell (inconclusive lookup): 409
///   `provider_lookup_inconclusive`, unless `attested` (the owner checked the
///   provider's dashboard), when the owner's payment is recorded and the audit
///   says `attested: true, lookup: "inconclusive"`.
///
/// The transaction then re-checks that the payout did not change since it
/// was read (409 `payout_changed`), so a send that claimed it meanwhile wins.
pub async fn mark_paid(
    state: &AppState,
    actor: Uuid,
    payout_id: Uuid,
    note: &str,
    provider_reference: &str,
    attested: bool,
) -> Result<MarkedPaid, AppError> {
    let before: PaidCheck = sqlx::query_as(
        "SELECT status::text AS status, reference, provider, provider_payout_id, last_error, \
                attempts, updated_at \
         FROM payouts WHERE id = $1",
    )
    .bind(payout_id)
    .fetch_optional(&state.db)
    .await
    .context("load payout")?
    .ok_or(AppError::NotFound)?;
    if !MARK_PAID_FROM.contains(&before.status.as_str()) {
        return Err(payout_state(&before.status));
    }
    let lookup = if may_have_paid(
        &before.status,
        before.provider_payout_id.is_some(),
        before.last_error.as_deref(),
    ) {
        match provider_has_it(state, actor, payout_id, &before, attested).await? {
            None => return Ok(MarkedPaid::ByProvider),
            Some(lookup) => lookup,
        }
    } else {
        Lookup::NotNeeded
    };

    let mut tx = state.db.begin().await.context("begin")?;
    let p = lock_payout(&mut tx, payout_id).await?;
    if !MARK_PAID_FROM.contains(&p.status.as_str()) {
        return Err(payout_state(&p.status));
    }
    if p.attempts != before.attempts || p.updated_at != before.updated_at {
        return Err(AppError::Conflict("payout_changed"));
    }
    sqlx::query(
        "UPDATE payouts SET status = 'successful', settled_by = 'owner', settled_at = now(), \
                manual_reference = $2, last_error = NULL, next_attempt_at = NULL, \
                sending_until = NULL, reversed_at = NULL, updated_at = now() \
         WHERE id = $1",
    )
    .bind(payout_id)
    .bind(provider_reference)
    .execute(&mut *tx)
    .await
    .context("mark payout paid")?;
    if p.amount_kobo > 0 {
        const KIND: &str = "payout_success";
        let entry = |account, amount_kobo| Entry {
            order_id: Some(p.order_id),
            checkout_id: Some(p.checkout_id),
            account,
            amount_kobo,
            kind: KIND,
            reference: Some(p.reference.clone()),
        };
        let posted = ledger::post(
            &mut tx,
            (KIND, &round_ref(payout_id, p.round)),
            &[
                entry(Account::SellerPayable, -p.amount_kobo),
                entry(Account::SellerPaid, p.amount_kobo),
            ],
        )
        .await?;
        if !posted {
            // This round's success is already booked: nothing is owed.
            return Err(AppError::Conflict("already_settled"));
        }
    }
    audit::record(
        &mut tx,
        Some(actor),
        "admin.payout_marked_paid",
        "payout",
        Some(payout_id),
        json!({
            "prior_status": p.status,
            "note": note,
            "provider_reference": provider_reference,
            "amount_kobo": p.amount_kobo,
            "round": p.round,
            "attested": attested && lookup == Lookup::Inconclusive,
            "lookup": lookup.as_str(),
        }),
    )
    .await?;
    tx.commit().await.map_err(db_err("commit mark paid"))?;
    Ok(MarkedPaid::ByOwner)
}

/// Asks the provider about a payout before the owner settles it by hand
/// (outside any transaction). `Ok(Some(lookup))`: go on (the provider has no
/// such payout, or could not tell and the owner attested). `Ok(None)`: the
/// provider paid it and it is now settled. Anything else is the error to
/// return. A lookup that reached the provider's record is audited
/// (`admin.payout_provider_checked`).
async fn provider_has_it(
    state: &AppState,
    actor: Uuid,
    payout_id: Uuid,
    p: &PaidCheck,
    attested: bool,
) -> Result<Option<Lookup>, AppError> {
    let provider = state
        .providers
        .by_name(&p.provider)
        .ok_or_else(provider_unavailable)?;
    if p.provider_payout_id.is_none() {
        let found =
            tokio::time::timeout(PAYOUT_CALL_TIMEOUT, provider.find_payout(&p.reference)).await;
        match found {
            Ok(Ok(None)) => return Ok(Some(Lookup::Absent)),
            Ok(Ok(Some(_))) => {}
            Ok(Err(ProviderError::Inconclusive)) if attested => {
                return Ok(Some(Lookup::Inconclusive));
            }
            Ok(Err(ProviderError::Inconclusive)) => return Err(lookup_inconclusive()),
            Ok(Err(e)) => {
                tracing::warn!(provider = provider.name(), error = %e, "payout lookup for mark-paid failed");
                return Err(provider_unavailable());
            }
            Err(_) => {
                tracing::warn!(
                    provider = provider.name(),
                    "payout lookup for mark-paid timed out"
                );
                return Err(provider_unavailable());
            }
        }
    }
    let outcome = settle_payout(state, &p.reference).await?;
    let label = match &outcome {
        SettlePayoutOutcome::Settled { status } => status.as_str(),
        SettlePayoutOutcome::UnknownReference => "reference_changed",
        SettlePayoutOutcome::RetryLater => "provider_unavailable",
        SettlePayoutOutcome::Inconclusive => "inconclusive",
    };
    let mut conn = state.db.acquire().await.context("acquire")?;
    audit::record(
        &mut conn,
        Some(actor),
        "admin.payout_provider_checked",
        "payout",
        Some(payout_id),
        json!({ "prior_status": p.status, "outcome": label }),
    )
    .await?;
    match outcome {
        SettlePayoutOutcome::Settled { status } if status == "successful" => Ok(None),
        SettlePayoutOutcome::Settled { status } if status == "failed" => Err(payout_state(&status)),
        SettlePayoutOutcome::Settled { status } => Err(AppError::ConflictDetail(
            "payout_pending_at_provider",
            format!("the provider has this payout and it is not final (payout is {status})"),
        )),
        SettlePayoutOutcome::UnknownReference => Err(AppError::Conflict("payout_changed")),
        SettlePayoutOutcome::RetryLater => Err(provider_unavailable()),
        SettlePayoutOutcome::Inconclusive if attested => Ok(Some(Lookup::Inconclusive)),
        SettlePayoutOutcome::Inconclusive => Err(lookup_inconclusive()),
    }
}

/// What the provider was asked before a refund was marked refunded by hand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefundMarked {
    /// Refund sends made to the provider.
    pub provider_attempts: i32,
    /// At least one send timed out: the provider may have paid the buyer too.
    /// The provider trait has no refund-status lookup to rule it out (Task 11).
    pub provider_outcome_unknown: bool,
}

/// The owner refunded the buyer by hand. The money left escrow when the
/// refund was requested (key `("refund", id)`), so nothing more is booked;
/// the refund becomes `completed` and its order `refunded` (409
/// `order_state_changed` if the order is no longer `refund_pending`).
pub async fn mark_refunded(
    db: &PgPool,
    actor: Uuid,
    refund_id: Uuid,
    note: &str,
    provider_reference: &str,
) -> Result<RefundMarked, AppError> {
    let mut tx = db.begin().await.context("begin")?;
    sqlx::query(LOCK_TIMEOUT)
        .execute(&mut *tx)
        .await
        .context("set lock_timeout")?;
    // Lock order: orders before refunds.
    let order_id: Uuid = sqlx::query_scalar(
        "SELECT o.id FROM orders o JOIN refunds r ON r.order_id = o.id \
         WHERE r.id = $1 FOR UPDATE OF o",
    )
    .bind(refund_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(db_err("lock order"))?
    .ok_or(AppError::NotFound)?;
    let (status, amount, attempts, timeouts): (String, i64, i32, i32) = sqlx::query_as(
        "SELECT status::text, amount_kobo, attempts, timeouts FROM refunds \
         WHERE id = $1 FOR UPDATE",
    )
    .bind(refund_id)
    .fetch_one(&mut *tx)
    .await
    .map_err(db_err("lock refund"))?;
    let marked = RefundMarked {
        provider_attempts: attempts,
        provider_outcome_unknown: timeouts > 0,
    };
    if !MARK_REFUNDED_FROM.contains(&status.as_str()) {
        return Err(AppError::ConflictDetail(
            "invalid_refund_state",
            format!("refund is {status}"),
        ));
    }
    sqlx::query(
        "UPDATE refunds SET status = 'completed', settled_by = 'owner', manual_reference = $2, \
                last_error = NULL, sending_until = NULL, next_attempt_at = NULL, \
                updated_at = now() \
         WHERE id = $1",
    )
    .bind(refund_id)
    .bind(provider_reference)
    .execute(&mut *tx)
    .await
    .context("mark refund completed")?;
    let order = sqlx::query(
        "UPDATE orders SET status = 'refunded' WHERE id = $1 AND status = 'refund_pending'",
    )
    .bind(order_id)
    .execute(&mut *tx)
    .await
    .context("mark order refunded")?;
    if order.rows_affected() != 1 {
        // Dropping the transaction rolls the refund update back.
        return Err(AppError::Conflict("order_state_changed"));
    }
    audit::record(
        &mut tx,
        Some(actor),
        "admin.refund_marked_refunded",
        "refund",
        Some(refund_id),
        json!({
            "prior_status": status,
            "note": note,
            "provider_reference": provider_reference,
            "order_id": order_id,
            "amount_kobo": amount,
            "provider_attempts": marked.provider_attempts,
            "provider_outcome_unknown": marked.provider_outcome_unknown,
        }),
    )
    .await?;
    tx.commit().await.context("commit mark refunded")?;
    Ok(marked)
}

/// The owner handled an `amount_mismatch` or provider-flagged checkout outside
/// the system (refunded the buyer by hand, settled it with the provider) and
/// records that, with the reference they checked. **This moves no money and
/// writes no ledger entry**: it only records the owner's handling, so the
/// checkout leaves the attention lists. A provider flag still gates payouts
/// until it is cleared (`clear-flag`). 409 `nothing_to_resolve` for a
/// checkout that needs neither, `already_resolved` on a retry.
pub async fn resolve_checkout(
    db: &PgPool,
    actor: Uuid,
    checkout_id: Uuid,
    note: &str,
    provider_reference: &str,
) -> Result<(), AppError> {
    let mut tx = db.begin().await.context("begin")?;
    sqlx::query(LOCK_TIMEOUT)
        .execute(&mut *tx)
        .await
        .context("set lock_timeout")?;
    let (status, flagged, resolved): (String, bool, bool) = sqlx::query_as(
        "SELECT status::text, provider_flagged, resolved_at IS NOT NULL FROM checkouts \
         WHERE id = $1 FOR UPDATE",
    )
    .bind(checkout_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(busy_err("checkout_busy", "lock checkout"))?
    .ok_or(AppError::NotFound)?;
    if resolved {
        return Err(AppError::Conflict("already_resolved"));
    }
    if status != "amount_mismatch" && !flagged {
        return Err(AppError::Conflict("nothing_to_resolve"));
    }
    sqlx::query(
        "UPDATE checkouts SET resolved_at = now(), resolved_by = $2, \
                resolution_reference = $3 \
         WHERE id = $1",
    )
    .bind(checkout_id)
    .bind(actor)
    .bind(provider_reference)
    .execute(&mut *tx)
    .await
    .context("resolve checkout")?;
    audit::record(
        &mut tx,
        Some(actor),
        "admin.checkout_resolved",
        "checkout",
        Some(checkout_id),
        json!({
            "prior_status": status,
            "provider_flagged": flagged,
            "note": note,
            "provider_reference": provider_reference,
        }),
    )
    .await?;
    tx.commit().await.context("commit resolve")?;
    Ok(())
}

/// The owner checked the held payment with the provider and lets it through:
/// the orders' payouts are no longer gated on the flag.
pub async fn clear_flag(
    db: &PgPool,
    actor: Uuid,
    checkout_id: Uuid,
    note: &str,
) -> Result<(), AppError> {
    let mut tx = db.begin().await.context("begin")?;
    sqlx::query(LOCK_TIMEOUT)
        .execute(&mut *tx)
        .await
        .context("set lock_timeout")?;
    let flagged: bool =
        sqlx::query_scalar("SELECT provider_flagged FROM checkouts WHERE id = $1 FOR UPDATE")
            .bind(checkout_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(busy_err("checkout_busy", "lock checkout"))?
            .ok_or(AppError::NotFound)?;
    if !flagged {
        return Err(AppError::Conflict("not_flagged"));
    }
    sqlx::query("UPDATE checkouts SET provider_flagged = false WHERE id = $1")
        .bind(checkout_id)
        .execute(&mut *tx)
        .await
        .context("clear flag")?;
    audit::record(
        &mut tx,
        Some(actor),
        "admin.checkout_flag_cleared",
        "checkout",
        Some(checkout_id),
        json!({ "note": note }),
    )
    .await?;
    tx.commit().await.context("commit")?;
    Ok(())
}

#[derive(sqlx::FromRow)]
struct RequeueRow {
    status: String,
    reversed: bool,
    reference: String,
    provider: String,
    provider_payout_id: Option<String>,
    store_id: Uuid,
    updated_at: time::OffsetDateTime,
}

const REQUEUE_SELECT: &str = "SELECT p.status::text AS status, \
        p.reversed_at IS NOT NULL AS reversed, p.reference, p.provider, p.provider_payout_id, \
        o.store_id, p.updated_at \
    FROM payouts p JOIN orders o ON o.id = p.order_id WHERE p.id = $1";

#[derive(sqlx::FromRow)]
struct VerifiedAccount {
    bank_code: String,
    account_number_enc: Vec<u8>,
    account_last4: String,
}

/// Re-queues a payout whose success the bank reversed, under a NEW reference
/// (the old one stays in `payout_attempts` and is never sent again).
///
/// Allowed only from `failed`/`manual` with a reversal recorded (the job moves
/// failed payouts to `manual`), once the seller's payout account has been
/// re-verified (409 `payout_account_unverified`), and once the provider
/// confirms the old reference was not paid (409
/// `payout_not_confirmed_reversed`; 502 when it cannot answer; 409
/// `provider_lookup_inconclusive` when it answers but cannot tell, unless
/// `attested`: the owner checked the provider's dashboard and attests the old
/// reference was not paid, which the audit records). The provider
/// call runs before the transaction; the transaction re-checks that the
/// payout did not change meanwhile. The account is re-snapshotted, and the
/// payout is sent at once (a failed send is left to the job).
pub async fn requeue(
    state: &AppState,
    actor: Uuid,
    payout_id: Uuid,
    note: &str,
    attested: bool,
) -> Result<(), AppError> {
    let before: RequeueRow = sqlx::query_as(REQUEUE_SELECT)
        .bind(payout_id)
        .fetch_optional(&state.db)
        .await
        .context("load payout")?
        .ok_or(AppError::NotFound)?;
    check_requeueable(&before)?;
    if verified_account(&state.db, before.store_id)
        .await?
        .is_none()
    {
        return Err(AppError::Conflict("payout_account_unverified"));
    }
    // Outside any transaction: the old reference must be confirmed unpaid.
    let provider = state
        .providers
        .by_name(&before.provider)
        .ok_or(AppError::ProviderUnavailable)?;
    let answer = tokio::time::timeout(
        PAYOUT_CALL_TIMEOUT,
        provider.payout_status(&before.reference, before.provider_payout_id.as_deref()),
    )
    .await;
    let lookup = match answer {
        Ok(Ok(PayoutState::Failed { .. } | PayoutState::Reversed { .. })) => "confirmed",
        Ok(Ok(_)) => return Err(AppError::Conflict("payout_not_confirmed_reversed")),
        Ok(Err(ProviderError::Inconclusive)) if attested => "inconclusive",
        Ok(Err(ProviderError::Inconclusive)) => return Err(lookup_inconclusive()),
        Ok(Err(e)) => {
            tracing::warn!(provider = provider.name(), error = %e, "payout status for re-queue failed");
            return Err(AppError::ProviderUnavailable);
        }
        Err(_) => {
            tracing::warn!(
                provider = provider.name(),
                "payout status for re-queue timed out"
            );
            return Err(AppError::ProviderUnavailable);
        }
    };

    let mut tx = state.db.begin().await.context("begin")?;
    lock_payout(&mut tx, payout_id).await?;
    let now: RequeueRow = sqlx::query_as(REQUEUE_SELECT)
        .bind(payout_id)
        .fetch_one(&mut *tx)
        .await
        .context("reload payout")?;
    check_requeueable(&now)?;
    if now.updated_at != before.updated_at {
        return Err(AppError::Conflict("payout_changed"));
    }
    let account = verified_account(&mut *tx, now.store_id)
        .await?
        .ok_or(AppError::Conflict("payout_account_unverified"))?;
    let reference = payments::new_reference("PO");
    sqlx::query(
        "UPDATE payouts SET reference = $2, status = 'scheduled', attempts = 0, \
                provider_payout_id = NULL, provider_fee_kobo = NULL, next_attempt_at = NULL, \
                send_after = now(), sending_until = NULL, pending_since = NULL, \
                last_polled_at = NULL, last_error = NULL, reversed_at = NULL, \
                settled_at = NULL, settled_by = NULL, \
                bank_code = $3, account_number_enc = $4, account_last4 = $5, \
                updated_at = now() \
         WHERE id = $1",
    )
    .bind(payout_id)
    .bind(&reference)
    .bind(&account.bank_code)
    .bind(&account.account_number_enc)
    .bind(&account.account_last4)
    .execute(&mut *tx)
    .await
    .context("requeue payout")?;
    audit::record(
        &mut tx,
        Some(actor),
        "admin.payout_requeued",
        "payout",
        Some(payout_id),
        json!({
            "prior_status": now.status,
            "note": note,
            "previous_reference": now.reference,
            "reference": reference,
            "attested": attested && lookup == "inconclusive",
            "lookup": lookup,
        }),
    )
    .await?;
    tx.commit().await.map_err(db_err("commit requeue"))?;
    if let Err(e) = send_payout(state, payout_id).await {
        tracing::warn!(error = %e, "re-queued payout not sent; the job retries");
    }
    Ok(())
}

fn check_requeueable(p: &RequeueRow) -> Result<(), AppError> {
    if !matches!(p.status.as_str(), "failed" | "manual") {
        return Err(payout_state(&p.status));
    }
    if !p.reversed {
        // A payout that never succeeded is not re-sent: the owner checks the
        // account and pays by hand (mark-paid).
        return Err(AppError::Conflict("not_reversed"));
    }
    Ok(())
}

/// The store's payout account if it is verified (read under FOR SHARE when
/// `conn` is a transaction; after payouts in the lock order).
async fn verified_account<'e, E>(
    conn: E,
    store_id: Uuid,
) -> Result<Option<VerifiedAccount>, AppError>
where
    E: sqlx::PgExecutor<'e>,
{
    sqlx::query_as(
        "SELECT bank_code, account_number_enc, account_last4 FROM payout_accounts \
         WHERE store_id = $1 AND status = 'verified' FOR SHARE",
    )
    .bind(store_id)
    .fetch_optional(conn)
    .await
    .map_err(db_err("load payout account"))
}
