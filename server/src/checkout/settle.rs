//! Settling a checkout's payment into escrow. Both the provider webhook and
//! the buyer's `GET /checkout/{id}` poll (the fallback when a webhook is lost)
//! come here; money is applied exactly once and only after the provider's API
//! confirms it.
//!
//! 1. Look the checkout up by its EXACT reference, without a lock. Unknown
//!    (or, for a webhook, owned by another provider) -> no outbound call.
//!    Already `paid`/`refunded`/`amount_mismatch` -> no outbound call.
//! 2. Pass the [`VerifyGate`](crate::payments::VerifyGate) (global concurrency
//!    cap, then the per-checkout cooldown), and call `verify_payment` on the
//!    checkout's stored provider (never the live `enabled_methods`), with a
//!    timeout. No transaction is open during the call.
//! 3. One transaction, locks in the global order: the checkout's variants (id
//!    order), the checkout, its orders. Status is re-checked under the locks.
//!    - amount, currency (`NGN`) or reference differ, or the fee is negative or
//!      above the total -> `amount_mismatch` + audit, a pending checkout's
//!      reservation released (the owner refunds it);
//!    - `pending` -> commit the reservation (`stock -= q`, `reserved -= q`), paid;
//!    - `expired` (reservation already released) -> if available stock covers
//!      every line, `stock -= q` and paid; otherwise the checkout is `refunded`:
//!      the payment is booked, then every order is refunded (stock untouched).
//!
//!    Paid: checkout and orders `paid`, deadlines snapshotted from the settings,
//!    provider fee split across orders, ledger `buyer_funds -T`, `escrow +T` per
//!    order, provider hold -> `provider_flagged`, the cart lines cleared.
//! 4. For a webhook, the event is marked processed in that same transaction, or
//!    after a confirmed no-op. Pending/unavailable answers leave it unprocessed,
//!    so the provider's retry is settled again.

use std::time::Duration;

use anyhow::Context;
use serde_json::json;
use sqlx::PgConnection;
use uuid::Uuid;

use super::service::{db_err, lock_reserved};
use crate::{
    AppState, audit,
    error::AppError,
    escrow::refund::{begin_refund, send_refund},
    ledger::{self, Account, Entry},
    money::split_proportional,
    payments::PaymentStatus,
    settings,
};

/// Longest wait for the provider's verify answer; past it the outcome is "retry later".
pub const VERIFY_TIMEOUT: Duration = Duration::from_secs(10);
const LOCK_TIMEOUT: &str = "SET LOCAL lock_timeout = '5s'";
const CURRENCY: &str = "NGN";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettleOutcome {
    /// This call moved the checkout to `paid`.
    Paid,
    /// Nothing to do: already paid, refunded or flagged as a mismatch.
    AlreadySettled,
    /// The provider says the payment is still pending.
    NotConfirmed,
    /// The provider says the payment failed; the checkout is left to expire.
    PaymentFailed,
    /// The verified amount, currency or reference differ from ours.
    AmountMismatch,
    /// Paid after expiry with too little stock left: booked and refunded.
    RefundedAfterExpiry,
    /// No checkout has this reference (for this provider).
    UnknownReference,
    /// Could not verify now (provider unavailable or timed out, cooldown,
    /// verify cap reached, lock contention); a webhook answers 503.
    RetryLater,
}

/// Where a settle request came from.
#[derive(Debug, Clone, Copy)]
pub enum Source<'a> {
    /// The buyer's poll, or an internal caller.
    Poll,
    /// A provider webhook whose `provider_events` row is marked processed.
    Webhook {
        provider: &'a str,
        event_id: &'a str,
    },
}

/// Settles the checkout with this reference (see the module docs).
pub async fn settle_payment(state: &AppState, reference: &str) -> Result<SettleOutcome, AppError> {
    settle(state, reference, Source::Poll).await
}

/// The poll fallback: settles the buyer's own checkout if it is still pending.
/// Errors are logged, never returned: the poll then just shows the current status.
pub async fn settle_for_buyer(state: &AppState, buyer: Uuid, checkout_id: Uuid) {
    let reference: Result<Option<String>, sqlx::Error> = sqlx::query_scalar(
        "SELECT reference FROM checkouts WHERE id = $1 AND buyer_id = $2 AND status = 'pending'",
    )
    .bind(checkout_id)
    .bind(buyer)
    .fetch_optional(&state.db)
    .await;
    match reference {
        Ok(Some(reference)) => {
            if let Err(e) = settle(state, &reference, Source::Poll).await {
                tracing::warn!(error = %e, "poll settle failed");
            }
        }
        Ok(None) => {}
        Err(e) => tracing::warn!(error = %e, "poll settle lookup failed"),
    }
}

/// [`settle_payment`] with the request's source.
pub async fn settle(
    state: &AppState,
    reference: &str,
    source: Source<'_>,
) -> Result<SettleOutcome, AppError> {
    let row: Option<(Uuid, String, String)> =
        sqlx::query_as("SELECT id, status::text, provider FROM checkouts WHERE reference = $1")
            .bind(reference)
            .fetch_optional(&state.db)
            .await
            .context("find checkout")?;
    let row = row.filter(|(_, _, provider)| match source {
        Source::Webhook { provider: p, .. } => p == provider,
        Source::Poll => true,
    });
    let Some((checkout_id, status, provider_name)) = row else {
        mark_processed(&state.db, source).await?;
        return Ok(SettleOutcome::UnknownReference);
    };
    if !matches!(status.as_str(), "pending" | "expired") {
        mark_processed(&state.db, source).await?;
        return Ok(SettleOutcome::AlreadySettled);
    }
    let Some(provider) = state.providers.by_name(&provider_name) else {
        tracing::warn!("checkout provider is not configured");
        return Ok(SettleOutcome::RetryLater);
    };

    // The cap first, so a saturated gate does not start a cooldown.
    let Some(permit) = state.verify_gate.try_permit() else {
        return Ok(SettleOutcome::RetryLater);
    };
    if !state.verify_gate.try_start(checkout_id) {
        return Ok(SettleOutcome::RetryLater);
    }
    let verified = tokio::time::timeout(VERIFY_TIMEOUT, provider.verify_payment(reference)).await;
    drop(permit);
    let verified = match verified {
        Ok(Ok(status)) => status,
        // Never applied on an unknown outcome: the retry verifies again.
        Ok(Err(e)) => {
            tracing::warn!(provider = provider.name(), error = %e, "payment verify failed");
            return Ok(SettleOutcome::RetryLater);
        }
        Err(_) => {
            tracing::warn!(provider = provider.name(), "payment verify timed out");
            return Ok(SettleOutcome::RetryLater);
        }
    };
    match verified {
        PaymentStatus::Pending => Ok(SettleOutcome::NotConfirmed),
        PaymentStatus::Failed => {
            mark_processed(&state.db, source).await?;
            Ok(SettleOutcome::PaymentFailed)
        }
        PaymentStatus::Successful {
            amount_kobo,
            fee_kobo,
            currency,
            held,
            reference: verified_reference,
        } => {
            let v = Verified {
                amount_kobo,
                fee_kobo,
                currency_ok: currency == CURRENCY,
                reference_ok: verified_reference == reference,
                held,
            };
            // Lock timeouts and deadlocks (`db_err`) roll back: retry later.
            match apply(state, checkout_id, reference, &v, source).await {
                Err(AppError::Conflict("checkout_busy" | "order_busy")) => {
                    Ok(SettleOutcome::RetryLater)
                }
                other => other,
            }
        }
    }
}

struct Verified {
    amount_kobo: i64,
    fee_kobo: i64,
    currency_ok: bool,
    reference_ok: bool,
    held: bool,
}

async fn mark_processed<'e, E>(db: E, source: Source<'_>) -> Result<(), AppError>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    if let Source::Webhook { provider, event_id } = source {
        sqlx::query(
            "UPDATE provider_events SET processed_at = now() \
             WHERE provider = $1 AND event_id = $2 AND processed_at IS NULL",
        )
        .bind(provider)
        .bind(event_id)
        .execute(db)
        .await
        .map_err(db_err("mark event processed"))?;
    }
    Ok(())
}

/// Step 3 of the module docs: the state change, in one transaction.
async fn apply(
    state: &AppState,
    checkout_id: Uuid,
    reference: &str,
    v: &Verified,
    source: Source<'_>,
) -> Result<SettleOutcome, AppError> {
    let settings = settings::load(&state.db).await?;
    let mut tx = state.db.begin().await.context("begin")?;
    sqlx::query(LOCK_TIMEOUT)
        .execute(&mut *tx)
        .await
        .map_err(db_err("set lock_timeout"))?;

    // product_variants (id order) -> checkouts -> orders.
    let items = lock_reserved(&mut tx, checkout_id).await?;
    let (status, total_kobo, buyer_id): (String, i64, Uuid) = sqlx::query_as(
        "SELECT status::text, total_kobo, buyer_id FROM checkouts WHERE id = $1 FOR UPDATE",
    )
    .bind(checkout_id)
    .fetch_one(&mut *tx)
    .await
    .map_err(db_err("lock checkout"))?;
    let orders: Vec<(Uuid, i64, i64)> = sqlx::query_as(
        "SELECT id, items_total_kobo, delivery_fee_kobo FROM orders \
         WHERE checkout_id = $1 ORDER BY id FOR UPDATE",
    )
    .bind(checkout_id)
    .fetch_all(&mut *tx)
    .await
    .map_err(db_err("lock orders"))?;

    let after_expiry = match status.as_str() {
        "pending" => false,
        "expired" => true,
        _ => {
            mark_processed(&mut *tx, source).await?;
            tx.commit().await.map_err(db_err("commit"))?;
            return Ok(SettleOutcome::AlreadySettled);
        }
    };

    let mismatch = if v.amount_kobo != total_kobo {
        Some("amount")
    } else if !v.currency_ok {
        Some("currency")
    } else if !v.reference_ok {
        Some("reference")
    } else if v.fee_kobo < 0 || v.fee_kobo > total_kobo {
        Some("fee_out_of_range")
    } else {
        None
    };
    if let Some(reason) = mismatch {
        // Nothing is applied. A pending checkout's reservation is released here,
        // under the variant locks; the owner refunds the payment (Task 10).
        if !after_expiry {
            release_reserved(&mut tx, &items).await?;
        }
        sqlx::query("UPDATE checkouts SET status = 'amount_mismatch' WHERE id = $1")
            .bind(checkout_id)
            .execute(&mut *tx)
            .await
            .map_err(db_err("flag amount mismatch"))?;
        audit::record(
            &mut tx,
            None,
            "checkout.amount_mismatch",
            "checkout",
            Some(checkout_id),
            json!({
                "reason": reason,
                "expected_kobo": total_kobo,
                "received_kobo": v.amount_kobo,
                "currency_ok": v.currency_ok,
                "reference_ok": v.reference_ok,
                "fee_kobo": v.fee_kobo,
                "after_expiry": after_expiry,
                "reservation_released": !after_expiry,
            }),
        )
        .await?;
        mark_processed(&mut *tx, source).await?;
        tx.commit().await.map_err(db_err("commit"))?;
        tracing::warn!("payment does not match its checkout; flagged amount_mismatch");
        return Ok(SettleOutcome::AmountMismatch);
    }

    // Stock: commit the reservation, or (after expiry) take it from what is available.
    let mut refund = false;
    if after_expiry {
        let ids: Vec<Uuid> = items.iter().map(|(id, _)| *id).collect();
        let rows: Vec<(Uuid, i32, i32)> = sqlx::query_as(
            "SELECT id, stock, reserved_quantity FROM product_variants WHERE id = ANY($1)",
        )
        .bind(&ids)
        .fetch_all(&mut *tx)
        .await
        .map_err(db_err("load variants"))?;
        let covered = items.iter().all(|(id, q)| {
            rows.iter()
                .any(|(vid, stock, reserved)| vid == id && stock - reserved >= *q)
        });
        if covered {
            for (variant_id, quantity) in &items {
                sqlx::query("UPDATE product_variants SET stock = stock - $2 WHERE id = $1")
                    .bind(variant_id)
                    .bind(quantity)
                    .execute(&mut *tx)
                    .await
                    .map_err(db_err("take stock"))?;
            }
        } else {
            refund = true;
        }
    } else {
        super::commit_reserved(&mut tx, checkout_id).await?;
    }

    // In range: checked with the mismatch rules above.
    let fee = v.fee_kobo;
    sqlx::query(
        "UPDATE checkouts SET status = $2::checkout_status, paid_at = now(), \
                collection_fee_kobo = $3, provider_flagged = $4 WHERE id = $1",
    )
    .bind(checkout_id)
    .bind(if refund { "refunded" } else { "paid" })
    .bind(fee)
    .bind(v.held)
    .execute(&mut *tx)
    .await
    .map_err(db_err("mark checkout paid"))?;

    let totals = orders
        .iter()
        .map(|(_, items, delivery)| {
            items
                .checked_add(*delivery)
                .ok_or_else(|| anyhow::anyhow!("order total overflow"))
        })
        .collect::<Result<Vec<i64>, _>>()?;
    let shares = split_proportional(fee, &totals);
    let mut entries = Vec::with_capacity(orders.len() * 2);
    for (((order_id, _, _), total), share) in orders.iter().zip(&totals).zip(&shares) {
        sqlx::query(
            "UPDATE orders SET status = 'paid', paid_at = now(), cancelled_at = NULL, \
                    collection_fee_share_kobo = $2, \
                    ship_by = now() + make_interval(hours => $3), \
                    backstop_at = now() + make_interval(days => $4) \
             WHERE id = $1",
        )
        .bind(order_id)
        .bind(share)
        .bind(settings.ship_deadline_hours)
        .bind(settings.backstop_days)
        .execute(&mut *tx)
        .await
        .map_err(db_err("mark order paid"))?;
        for (account, amount_kobo) in [(Account::BuyerFunds, -total), (Account::Escrow, *total)] {
            entries.push(Entry {
                order_id: Some(*order_id),
                checkout_id: Some(checkout_id),
                account,
                amount_kobo,
                kind: "payment",
                reference: Some(reference.to_string()),
            });
        }
    }
    ledger::post(&mut tx, &entries).await?;

    if v.held {
        audit::record(
            &mut tx,
            None,
            "checkout.provider_flagged",
            "checkout",
            Some(checkout_id),
            json!({}),
        )
        .await?;
    }

    let mut refunds = Vec::new();
    if refund {
        for (order_id, _, _) in &orders {
            refunds.push(begin_refund(&mut tx, *order_id, false, "paid_after_expiry").await?);
        }
        audit::record(
            &mut tx,
            None,
            "checkout.refunded_after_expiry",
            "checkout",
            Some(checkout_id),
            json!({ "orders": orders.len(), "total_kobo": total_kobo }),
        )
        .await?;
    } else {
        clear_cart_lines(&mut tx, buyer_id, &items).await?;
        audit::record(
            &mut tx,
            None,
            "checkout.paid",
            "checkout",
            Some(checkout_id),
            json!({ "orders": orders.len(), "total_kobo": total_kobo, "after_expiry": after_expiry }),
        )
        .await?;
    }
    mark_processed(&mut *tx, source).await?;
    tx.commit().await.map_err(db_err("commit settlement"))?;

    if !refund {
        return Ok(SettleOutcome::Paid);
    }
    // After commit: the provider calls. A failure leaves the refund `requested`
    // for a retry with the same reference; the money is already booked.
    for refund_id in refunds {
        if let Err(e) = send_refund(state, refund_id).await {
            tracing::warn!(error = %e, "refund after expiry not sent");
        }
    }
    Ok(SettleOutcome::RefundedAfterExpiry)
}

/// Releases a pending checkout's reservation (`reserved_quantity -= q`); the
/// variants are already locked by the caller.
async fn release_reserved(conn: &mut PgConnection, items: &[(Uuid, i32)]) -> Result<(), AppError> {
    for (variant_id, quantity) in items {
        sqlx::query(
            "UPDATE product_variants SET reserved_quantity = reserved_quantity - $2 WHERE id = $1",
        )
        .bind(variant_id)
        .bind(quantity)
        .execute(&mut *conn)
        .await
        .map_err(db_err("release reservation"))?;
    }
    Ok(())
}

/// Removes the paid variants from the buyer's cart (lines added since stay).
async fn clear_cart_lines(
    conn: &mut PgConnection,
    buyer: Uuid,
    items: &[(Uuid, i32)],
) -> Result<(), AppError> {
    let ids: Vec<Uuid> = items.iter().map(|(id, _)| *id).collect();
    sqlx::query("DELETE FROM cart_items WHERE user_id = $1 AND variant_id = ANY($2)")
        .bind(buyer)
        .bind(&ids)
        .execute(&mut *conn)
        .await
        .map_err(db_err("clear cart lines"))?;
    Ok(())
}
