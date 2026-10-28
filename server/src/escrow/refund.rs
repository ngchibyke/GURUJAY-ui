//! Full refunds of a paid order: items + delivery go back to the buyer, the
//! platform takes no cut (and absorbs the provider's collection fee).
//!
//! Two steps, so no provider call ever runs inside a DB transaction:
//! 1. [`begin_refund`] (inside the caller's transaction, under the order lock):
//!    order `paid -> refund_pending`, optional stock restore, a `refunds` row
//!    with a fresh `RF` reference (status `requested`), ledger `escrow -T`,
//!    `refunds +T` (key `("refund", id)`), and the absorbed collection fee
//!    `provider_fees +share`, `platform_revenue -share` (key
//!    `("refund_fee", id)`, none when the share is 0).
//! 2. [`send_refund`] (after commit): claims the row, calls `provider.refund`
//!    with the same reference every time, and records the answer:
//!    - Ok: refund `processing` (the buyer gets the money within about 10
//!      working days; not `completed`), order `refunded`;
//!    - NotSupported: refund `manual`, order stays `refund_pending` (the owner
//!      refunds by hand, then marks the refund done, which makes it `refunded`);
//!    - Rejected: refund `failed`, order stays `refund_pending` (owner follow-up);
//!    - Unavailable/Timeout: refund stays `requested` with `last_error`, order
//!      stays `refund_pending`; a later call retries with the SAME reference.
//!
//!    A refund that used up [`MAX_REFUND_ATTEMPTS`] sends is never claimed
//!    again (the sweeper moves it to `manual`), whoever calls: the job, a buyer
//!    re-cancel or an owner refund.

use std::time::Duration;

use anyhow::Context;
use serde_json::json;
use sqlx::PgConnection;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{
    AppState, audit,
    checkout::service::busy_err,
    error::AppError,
    ledger::{self, Account, Entry},
    limits::MAX_STOCK,
    payments::{self, ProviderError, RefundRequest},
};

const REFERENCE_PREFIX: &str = "RF";
const LOCK_TIMEOUT: &str = "SET LOCAL lock_timeout = '5s'";
/// Longest wait for the provider's refund answer. Well under the 2-minute
/// claim, so a slow call cannot outlive its claim; past it the outcome is a Timeout.
pub const REFUND_CALL_TIMEOUT: Duration = Duration::from_secs(30);
/// Refund sends (the inline one included) before a `requested` refund goes to
/// `manual` for the owner (same cap as payouts).
pub const MAX_REFUND_ATTEMPTS: i32 = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefundOutcome {
    pub refund_id: Uuid,
    pub reference: String,
    /// The refund's status after this call: requested, processing, completed, failed or manual.
    pub status: String,
}

fn db_err(ctx: &'static str) -> impl FnOnce(sqlx::Error) -> AppError {
    busy_err("order_busy", ctx)
}

/// The owner's refund from the admin route, audited with `actor` and `note`.
/// Beyond what [`refund_order`] accepts, it refunds a **held** `picked_up` or
/// `delivered` order (a complaint the owner decided for the buyer), without
/// restoring stock: the goods may be with the buyer (returns are plan 5). An
/// unheld shipped order is 409 `order_not_held` (hold it first, which stops
/// its release). Not a no-op on retry: an order already `refund_pending` or
/// `refunded` (or in any other state) is 409 `invalid_order_state` naming it;
/// a `completed` order is 409 `already_released`.
pub async fn owner_refund_order(
    state: &AppState,
    order_id: Uuid,
    actor: Uuid,
    note: &str,
) -> Result<RefundOutcome, AppError> {
    // Decides only whether to take the variant locks (they come before the
    // order lock); begin_refund restores stock only for an order still `paid`
    // under the lock.
    let status: String = sqlx::query_scalar("SELECT status::text FROM orders WHERE id = $1")
        .bind(order_id)
        .fetch_optional(&state.db)
        .await
        .context("load order")?
        .ok_or(AppError::NotFound)?;
    let mut tx = state.db.begin().await.context("begin")?;
    sqlx::query(LOCK_TIMEOUT)
        .execute(&mut *tx)
        .await
        .context("set lock_timeout")?;
    let begun = begin(&mut tx, order_id, status == "paid", "owner_decision", true).await?;
    audit::record(
        &mut tx,
        Some(actor),
        "admin.order_refunded",
        "order",
        Some(order_id),
        json!({
            "note": note,
            "prior_status": begun.prior_status,
            "refund_id": begun.refund_id,
            "restore_stock": begun.restored,
        }),
    )
    .await?;
    tx.commit().await.map_err(db_err("commit refund"))?;
    send_refund(state, begun.refund_id).await
}

struct Begun {
    refund_id: Uuid,
    prior_status: String,
    restored: bool,
}

/// Refunds a `paid` order in full, or retries the provider call of an order
/// already `refund_pending`. `restore_stock` puts the sold units back on hand
/// (false when they were never taken, e.g. paid after expiry with no stock).
/// A `completed` order is 409 `already_released` (clawback is plan 5); any
/// other status is 409 `not_refundable`. Idempotent: a refunded order returns
/// its refund unchanged.
pub async fn refund_order(
    state: &AppState,
    order_id: Uuid,
    restore_stock: bool,
) -> Result<RefundOutcome, AppError> {
    let mut tx = state.db.begin().await.context("begin")?;
    sqlx::query(LOCK_TIMEOUT)
        .execute(&mut *tx)
        .await
        .context("set lock_timeout")?;
    let refund_id = begin_refund(&mut tx, order_id, restore_stock, "refund").await?;
    tx.commit().await.map_err(db_err("commit refund"))?;
    send_refund(state, refund_id).await
}

/// Step 1 (see the module docs), inside the caller's transaction. Locks the
/// order's variants (only when restoring stock, id order), then the order.
/// Callers that already hold those locks (settlement) may call it directly.
/// Returns the refund id; for an order already `refund_pending`/`refunded`,
/// the existing refund's id with nothing changed.
pub async fn begin_refund(
    conn: &mut PgConnection,
    order_id: Uuid,
    restore_stock: bool,
    reason: &str,
) -> Result<Uuid, AppError> {
    Ok(begin(conn, order_id, restore_stock, reason, false)
        .await?
        .refund_id)
}

/// [`begin_refund`]; `owner` widens and tightens it as [`owner_refund_order`]
/// documents.
async fn begin(
    conn: &mut PgConnection,
    order_id: Uuid,
    restore_stock: bool,
    reason: &str,
    owner: bool,
) -> Result<Begun, AppError> {
    // (variant, quantity, on-hand stock under the lock)
    let items: Vec<(Uuid, i32, i32)> = if restore_stock {
        let items: Vec<(Uuid, i32)> = sqlx::query_as(
            "SELECT variant_id, quantity FROM order_items WHERE order_id = $1 ORDER BY variant_id",
        )
        .bind(order_id)
        .fetch_all(&mut *conn)
        .await
        .context("load order items")?;
        let ids: Vec<Uuid> = items.iter().map(|(id, _)| *id).collect();
        let stocks: Vec<(Uuid, i32)> = sqlx::query_as(
            "SELECT id, stock FROM product_variants WHERE id = ANY($1) \
             ORDER BY id FOR NO KEY UPDATE",
        )
        .bind(&ids)
        .fetch_all(&mut *conn)
        .await
        .map_err(db_err("lock variants"))?;
        items
            .into_iter()
            .map(|(id, q)| {
                let stock = stocks.iter().find(|(v, _)| *v == id).map_or(0, |(_, s)| *s);
                (id, q, stock)
            })
            .collect()
    } else {
        Vec::new()
    };

    let (status, items_total, delivery_fee, fee_share, checkout_id, provider, held): (
        String,
        i64,
        i64,
        i64,
        Uuid,
        String,
        bool,
    ) = sqlx::query_as(
        "SELECT o.status::text, o.items_total_kobo, o.delivery_fee_kobo, \
                    o.collection_fee_share_kobo, o.checkout_id, c.provider, \
                    o.held_at IS NOT NULL \
             FROM orders o JOIN checkouts c ON c.id = o.checkout_id \
             WHERE o.id = $1 FOR UPDATE OF o",
    )
    .bind(order_id)
    .fetch_optional(&mut *conn)
    .await
    .map_err(db_err("lock order"))?
    .ok_or(AppError::NotFound)?;
    let state_err =
        || AppError::ConflictDetail("invalid_order_state", format!("order is {status}"));
    match status.as_str() {
        "paid" => {}
        "picked_up" | "delivered" if owner && held => {}
        "picked_up" | "delivered" if owner => return Err(AppError::Conflict("order_not_held")),
        "refund_pending" | "refunded" if owner => return Err(state_err()),
        "refund_pending" | "refunded" => {
            let id: Uuid = sqlx::query_scalar("SELECT id FROM refunds WHERE order_id = $1")
                .bind(order_id)
                .fetch_one(&mut *conn)
                .await
                .context("load refund")?;
            return Ok(Begun {
                refund_id: id,
                prior_status: status,
                restored: false,
            });
        }
        "completed" => return Err(AppError::Conflict("already_released")),
        _ if owner => return Err(state_err()),
        _ => return Err(AppError::Conflict("not_refundable")),
    }
    // Only goods still with the seller go back on hand.
    let restore = restore_stock && status == "paid";
    let amount = items_total
        .checked_add(delivery_fee)
        .ok_or_else(|| anyhow::anyhow!("refund amount overflow"))?;

    sqlx::query("UPDATE orders SET status = 'refund_pending' WHERE id = $1")
        .bind(order_id)
        .execute(&mut *conn)
        .await
        .context("mark refund pending")?;
    // Units back on hand, clamped at the stock cap (the seller may have
    // restocked meanwhile); the clamped units are recorded in the audit row.
    let mut clamped_units: i64 = 0;
    for (variant_id, quantity, stock) in items.iter().filter(|_| restore) {
        let wanted = i64::from(*stock) + i64::from(*quantity);
        clamped_units += (wanted - i64::from(MAX_STOCK)).max(0);
        sqlx::query("UPDATE product_variants SET stock = LEAST(stock + $2, $3) WHERE id = $1")
            .bind(variant_id)
            .bind(quantity)
            .bind(MAX_STOCK)
            .execute(&mut *conn)
            .await
            .map_err(db_err("restore stock"))?;
    }
    let reference = payments::new_reference(REFERENCE_PREFIX);
    let refund_id: Uuid = sqlx::query_scalar(
        "INSERT INTO refunds (order_id, reference, provider, amount_kobo, reason) \
         VALUES ($1, $2, $3, $4, $5) RETURNING id",
    )
    .bind(order_id)
    .bind(&reference)
    .bind(&provider)
    .bind(amount)
    .bind(reason)
    .fetch_one(&mut *conn)
    .await
    .context("insert refund")?;
    let entry = |account, amount_kobo| Entry {
        order_id: Some(order_id),
        checkout_id: Some(checkout_id),
        account,
        amount_kobo,
        kind: "refund",
        reference: Some(reference.clone()),
    };
    ledger::post(
        conn,
        ("refund", &refund_id.to_string()),
        &[
            entry(Account::Escrow, -amount),
            entry(Account::Refunds, amount),
        ],
    )
    .await?;
    // The provider kept its collection fee on the payment and the buyer gets
    // the full amount back, so the platform absorbs the fee (by ruling). It
    // is booked here, as release books it, so `provider_fees` matches cash.
    if fee_share > 0 {
        const FEE_KIND: &str = "refund_fee";
        let fee_entry = |account, amount_kobo| Entry {
            kind: FEE_KIND,
            ..entry(account, amount_kobo)
        };
        ledger::post(
            conn,
            (FEE_KIND, &refund_id.to_string()),
            &[
                fee_entry(Account::ProviderFees, fee_share),
                fee_entry(Account::PlatformRevenue, -fee_share),
            ],
        )
        .await?;
    }
    audit::record(
        conn,
        None,
        "order.refund_requested",
        "order",
        Some(order_id),
        json!({
            "refund_id": refund_id,
            "amount_kobo": amount,
            "absorbed_fee_kobo": fee_share,
            "restore_stock": restore,
            "clamped_units": clamped_units,
        }),
    )
    .await?;
    Ok(Begun {
        refund_id,
        prior_status: status,
        restored: restore,
    })
}

#[derive(sqlx::FromRow)]
struct Claimed {
    order_id: Uuid,
    reference: String,
    provider: String,
    amount_kobo: i64,
    reason: String,
    /// Fencing token: the outcome is written only while the claim still holds it.
    claimed_until: OffsetDateTime,
    payment_reference: String,
}

/// Step 2 (see the module docs). Never call it inside an open transaction.
/// Only one caller at a time sends a given refund (a 2-minute claim on the row,
/// and the provider call is cut off after [`REFUND_CALL_TIMEOUT`]); anyone else
/// gets the refund's current status. The outcome is written only if the claim
/// is still ours (`sending_until` unchanged), so a late answer never
/// overwrites a newer sender's.
pub async fn send_refund(state: &AppState, refund_id: Uuid) -> Result<RefundOutcome, AppError> {
    let claimed: Option<Claimed> = sqlx::query_as(
        "UPDATE refunds r SET sending_until = now() + interval '2 minutes', \
                attempts = attempts + 1, updated_at = now() \
         FROM orders o JOIN checkouts c ON c.id = o.checkout_id \
         WHERE r.id = $1 AND o.id = r.order_id AND r.status = 'requested' \
           AND r.attempts < $2 \
           AND (r.sending_until IS NULL OR r.sending_until < now()) \
         RETURNING r.order_id, r.reference, r.provider, r.amount_kobo, r.reason, \
                   r.sending_until AS claimed_until, c.reference AS payment_reference",
    )
    .bind(refund_id)
    .bind(MAX_REFUND_ATTEMPTS)
    .fetch_optional(&state.db)
    .await
    .context("claim refund")?;
    let Some(c) = claimed else {
        return current(&state.db, refund_id).await;
    };

    let result = match state.providers.by_name(&c.provider) {
        Some(provider) => {
            let call = provider.refund(RefundRequest {
                reference: c.reference.clone(),
                payment_reference: c.payment_reference.clone(),
                amount_kobo: c.amount_kobo,
                reason: c.reason.clone(),
            });
            tokio::time::timeout(REFUND_CALL_TIMEOUT, call)
                .await
                .unwrap_or(Err(ProviderError::Timeout))
        }
        None => Err(ProviderError::Unavailable(anyhow::anyhow!(
            "provider not configured"
        ))),
    };
    // (refund status, provider refund id, order refunded?, sanitised error)
    let (status, provider_id, done, error): (&str, Option<String>, bool, Option<&str>) =
        match result {
            Ok(h) => ("processing", Some(h.provider_refund_id), true, None),
            // The buyer is not paid back until the owner refunds by hand.
            Err(ProviderError::NotSupported) => ("manual", None, false, None),
            Err(ProviderError::Rejected { .. }) => ("failed", None, false, Some("rejected")),
            Err(ProviderError::Timeout) => ("requested", None, false, Some("timed out")),
            Err(_) => ("requested", None, false, Some("unavailable")),
        };
    tracing::info!(
        provider = c.provider.as_str(),
        outcome = status,
        "refund sent"
    );

    let mut tx = state.db.begin().await.context("begin")?;
    // Lock order: orders before refunds.
    sqlx::query("SELECT id FROM orders WHERE id = $1 FOR UPDATE")
        .bind(c.order_id)
        .execute(&mut *tx)
        .await
        .map_err(db_err("lock order"))?;
    // Fenced on our claim: if it expired and another sender claimed the row,
    // that sender owns the outcome and this late answer is dropped.
    let fenced: Option<Uuid> = sqlx::query_scalar(
        "UPDATE refunds SET status = $2::refund_status, \
                provider_refund_id = COALESCE($3, provider_refund_id), \
                last_error = $4, sending_until = NULL, updated_at = now(), \
                timeouts = timeouts + CASE WHEN $4 = 'timed out' THEN 1 ELSE 0 END \
         WHERE id = $1 AND status = 'requested' AND sending_until = $5 RETURNING id",
    )
    .bind(refund_id)
    .bind(status)
    .bind(&provider_id)
    .bind(error)
    .bind(c.claimed_until)
    .fetch_optional(&mut *tx)
    .await
    .context("record refund outcome")?;
    if fenced.is_none() {
        drop(tx);
        tracing::warn!(
            provider = c.provider.as_str(),
            outcome = status,
            "refund claim lost to a newer sender; late answer not recorded"
        );
        return current(&state.db, refund_id).await;
    }
    if done {
        sqlx::query(
            "UPDATE orders SET status = 'refunded' WHERE id = $1 AND status = 'refund_pending'",
        )
        .bind(c.order_id)
        .execute(&mut *tx)
        .await
        .context("mark order refunded")?;
    }
    if done {
        audit::record(
            &mut tx,
            None,
            "order.refunded",
            "order",
            Some(c.order_id),
            json!({ "refund_id": refund_id, "refund_status": status }),
        )
        .await?;
    } else if status == "manual" {
        audit::record(
            &mut tx,
            None,
            "refund.manual_required",
            "refund",
            Some(refund_id),
            json!({ "order_id": c.order_id, "reason": "not_supported" }),
        )
        .await?;
    }
    tx.commit().await.context("commit")?;
    Ok(RefundOutcome {
        refund_id,
        reference: c.reference,
        status: status.to_string(),
    })
}

async fn current(db: &sqlx::PgPool, refund_id: Uuid) -> Result<RefundOutcome, AppError> {
    let (reference, status): (String, String) =
        sqlx::query_as("SELECT reference, status::text FROM refunds WHERE id = $1")
            .bind(refund_id)
            .fetch_optional(db)
            .await
            .context("load refund")?
            .ok_or(AppError::NotFound)?;
    Ok(RefundOutcome {
        refund_id,
        reference,
        status,
    })
}
