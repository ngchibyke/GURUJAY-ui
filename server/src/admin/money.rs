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
        release::{PAYOUT_CALL_TIMEOUT, round_ref},
        send_payout,
    },
    ledger::{self, Account, Entry},
    payments::{self, PayoutState},
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
        "SELECT p.status::text AS status, p.round, p.amount_kobo, p.order_id, o.checkout_id, \
                p.reference \
         FROM payouts p JOIN orders o ON o.id = p.order_id WHERE p.id = $1 FOR UPDATE OF p",
    )
    .bind(payout_id)
    .fetch_one(&mut *tx)
    .await
    .map_err(db_err("lock payout"))
}

/// The owner paid the seller outside the provider (or confirmed with the
/// provider that it paid). Books `seller_payable -> seller_paid` under the
/// round's success key (none for a zero payout); provider answers no longer
/// change the payout afterwards.
pub async fn mark_paid(
    db: &PgPool,
    actor: Uuid,
    payout_id: Uuid,
    note: &str,
    provider_reference: &str,
) -> Result<(), AppError> {
    let mut tx = db.begin().await.context("begin")?;
    let p = lock_payout(&mut tx, payout_id).await?;
    if !MARK_PAID_FROM.contains(&p.status.as_str()) {
        return Err(payout_state(&p.status));
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
        }),
    )
    .await?;
    tx.commit().await.map_err(db_err("commit mark paid"))?;
    Ok(())
}

/// The owner refunded the buyer by hand. The money left escrow when the
/// refund was requested (key `("refund", id)`), so nothing more is booked;
/// the refund becomes `completed` and its order `refunded`.
pub async fn mark_refunded(
    db: &PgPool,
    actor: Uuid,
    refund_id: Uuid,
    note: &str,
    provider_reference: &str,
) -> Result<(), AppError> {
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
    let (status, amount): (String, i64) =
        sqlx::query_as("SELECT status::text, amount_kobo FROM refunds WHERE id = $1 FOR UPDATE")
            .bind(refund_id)
            .fetch_one(&mut *tx)
            .await
            .map_err(db_err("lock refund"))?;
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
    sqlx::query(
        "UPDATE orders SET status = 'refunded' WHERE id = $1 AND status = 'refund_pending'",
    )
    .bind(order_id)
    .execute(&mut *tx)
    .await
    .context("mark order refunded")?;
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
        }),
    )
    .await?;
    tx.commit().await.context("commit mark refunded")?;
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
/// `payout_not_confirmed_reversed`; 502 when it cannot answer). The provider
/// call runs before the transaction; the transaction re-checks that the
/// payout did not change meanwhile. The account is re-snapshotted, and the
/// payout is sent at once (a failed send is left to the job).
pub async fn requeue(
    state: &AppState,
    actor: Uuid,
    payout_id: Uuid,
    note: &str,
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
    match answer {
        Ok(Ok(PayoutState::Failed { .. } | PayoutState::Reversed { .. })) => {}
        Ok(Ok(_)) => return Err(AppError::Conflict("payout_not_confirmed_reversed")),
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
    }

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
