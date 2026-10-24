//! Full refunds of a paid order: items + delivery go back to the buyer, the
//! platform takes no cut (and absorbs the provider's collection fee).
//!
//! Two steps, so no provider call ever runs inside a DB transaction:
//! 1. [`begin_refund`] (inside the caller's transaction, under the order lock):
//!    order `paid -> refund_pending`, optional stock restore, a `refunds` row
//!    with a fresh `RF` reference (status `requested`), ledger `escrow -T`,
//!    `refunds +T`.
//! 2. [`send_refund`] (after commit): claims the row, calls `provider.refund`
//!    with the same reference every time, and records the answer:
//!    - Ok: refund `processing` (the buyer gets the money within about 10
//!      working days; not `completed`), order `refunded`;
//!    - NotSupported: refund `manual` (the owner refunds by hand), order `refunded`;
//!    - Rejected: refund `failed`, order stays `refund_pending` (owner follow-up);
//!    - Unavailable/Timeout: refund stays `requested` with `last_error`, order
//!      stays `refund_pending`; a later call retries with the SAME reference.

use anyhow::Context;
use serde_json::json;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::{
    AppState, audit,
    error::AppError,
    ledger::{self, Account, Entry},
    payments::{self, ProviderError, RefundRequest},
};

const REFERENCE_PREFIX: &str = "RF";
const LOCK_TIMEOUT: &str = "SET LOCAL lock_timeout = '5s'";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefundOutcome {
    pub refund_id: Uuid,
    pub reference: String,
    /// The refund's status after this call: requested, processing, completed, failed or manual.
    pub status: String,
}

fn db_err(ctx: &'static str) -> impl FnOnce(sqlx::Error) -> AppError {
    move |e| match &e {
        sqlx::Error::Database(d) if matches!(d.code().as_deref(), Some("55P03" | "40P01")) => {
            AppError::Conflict("order_busy")
        }
        _ => AppError::Internal(anyhow::Error::new(e).context(ctx)),
    }
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
    let items: Vec<(Uuid, i32)> = if restore_stock {
        let items: Vec<(Uuid, i32)> = sqlx::query_as(
            "SELECT variant_id, quantity FROM order_items WHERE order_id = $1 ORDER BY variant_id",
        )
        .bind(order_id)
        .fetch_all(&mut *conn)
        .await
        .context("load order items")?;
        let ids: Vec<Uuid> = items.iter().map(|(id, _)| *id).collect();
        sqlx::query(
            "SELECT id FROM product_variants WHERE id = ANY($1) ORDER BY id FOR NO KEY UPDATE",
        )
        .bind(&ids)
        .fetch_all(&mut *conn)
        .await
        .map_err(db_err("lock variants"))?;
        items
    } else {
        Vec::new()
    };

    let (status, items_total, delivery_fee, checkout_id, provider): (
        String,
        i64,
        i64,
        Uuid,
        String,
    ) = sqlx::query_as(
        "SELECT o.status::text, o.items_total_kobo, o.delivery_fee_kobo, o.checkout_id, \
                    c.provider \
             FROM orders o JOIN checkouts c ON c.id = o.checkout_id \
             WHERE o.id = $1 FOR UPDATE OF o",
    )
    .bind(order_id)
    .fetch_optional(&mut *conn)
    .await
    .map_err(db_err("lock order"))?
    .ok_or(AppError::NotFound)?;
    match status.as_str() {
        "paid" => {}
        "refund_pending" | "refunded" => {
            let id: Uuid = sqlx::query_scalar("SELECT id FROM refunds WHERE order_id = $1")
                .bind(order_id)
                .fetch_one(&mut *conn)
                .await
                .context("load refund")?;
            return Ok(id);
        }
        "completed" => return Err(AppError::Conflict("already_released")),
        _ => return Err(AppError::Conflict("not_refundable")),
    }
    let amount = items_total
        .checked_add(delivery_fee)
        .ok_or_else(|| anyhow::anyhow!("refund amount overflow"))?;

    sqlx::query("UPDATE orders SET status = 'refund_pending' WHERE id = $1")
        .bind(order_id)
        .execute(&mut *conn)
        .await
        .context("mark refund pending")?;
    for (variant_id, quantity) in &items {
        sqlx::query("UPDATE product_variants SET stock = stock + $2 WHERE id = $1")
            .bind(variant_id)
            .bind(quantity)
            .execute(&mut *conn)
            .await
            .context("restore stock")?;
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
        &[
            entry(Account::Escrow, -amount),
            entry(Account::Refunds, amount),
        ],
    )
    .await?;
    audit::record(
        conn,
        None,
        "order.refund_requested",
        "order",
        Some(order_id),
        json!({ "refund_id": refund_id, "amount_kobo": amount, "restore_stock": restore_stock }),
    )
    .await?;
    Ok(refund_id)
}

#[derive(sqlx::FromRow)]
struct Claimed {
    order_id: Uuid,
    reference: String,
    provider: String,
    amount_kobo: i64,
    reason: String,
    payment_reference: String,
}

/// Step 2 (see the module docs). Never call it inside an open transaction.
/// Only one caller at a time sends a given refund (a 2-minute claim on the row);
/// anyone else gets the refund's current status.
pub async fn send_refund(state: &AppState, refund_id: Uuid) -> Result<RefundOutcome, AppError> {
    let claimed: Option<Claimed> = sqlx::query_as(
        "UPDATE refunds r SET sending_until = now() + interval '2 minutes', \
                attempts = attempts + 1, updated_at = now() \
         FROM orders o JOIN checkouts c ON c.id = o.checkout_id \
         WHERE r.id = $1 AND o.id = r.order_id AND r.status = 'requested' \
           AND (r.sending_until IS NULL OR r.sending_until < now()) \
         RETURNING r.order_id, r.reference, r.provider, r.amount_kobo, r.reason, \
                   c.reference AS payment_reference",
    )
    .bind(refund_id)
    .fetch_optional(&state.db)
    .await
    .context("claim refund")?;
    let Some(c) = claimed else {
        return current(&state.db, refund_id).await;
    };

    let result = match state.providers.by_name(&c.provider) {
        Some(provider) => {
            provider
                .refund(RefundRequest {
                    reference: c.reference.clone(),
                    payment_reference: c.payment_reference.clone(),
                    amount_kobo: c.amount_kobo,
                    reason: c.reason.clone(),
                })
                .await
        }
        None => Err(ProviderError::Unavailable(anyhow::anyhow!(
            "provider not configured"
        ))),
    };
    // (refund status, provider refund id, order refunded?, sanitised error)
    let (status, provider_id, done, error): (&str, Option<String>, bool, Option<&str>) =
        match result {
            Ok(h) => ("processing", Some(h.provider_refund_id), true, None),
            Err(ProviderError::NotSupported) => ("manual", None, true, None),
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
    if done {
        sqlx::query(
            "UPDATE orders SET status = 'refunded' WHERE id = $1 AND status = 'refund_pending'",
        )
        .bind(c.order_id)
        .execute(&mut *tx)
        .await
        .context("mark order refunded")?;
    }
    sqlx::query(
        "UPDATE refunds SET status = $2::refund_status, \
                provider_refund_id = COALESCE($3, provider_refund_id), \
                last_error = $4, sending_until = NULL, updated_at = now() \
         WHERE id = $1",
    )
    .bind(refund_id)
    .bind(status)
    .bind(&provider_id)
    .bind(error)
    .execute(&mut *tx)
    .await
    .context("record refund outcome")?;
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
