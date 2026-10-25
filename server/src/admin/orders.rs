//! Owner order actions: hold and unhold. (Owner release and refund live in
//! the escrow engine: `release_by_owner`, `owner_refund_order`.)
//!
//! A hold stops the order's auto-release, ship-deadline refund, buyer
//! confirmation and payout send until the owner lifts it (or releases or
//! refunds the order). Each change locks the order, is state-conditional and
//! is audited with the owner and the reason.

use anyhow::Context;
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

use super::LOCK_TIMEOUT;
use crate::{audit, checkout::service::busy_err, error::AppError};

/// Statuses a hold makes sense on: money still in escrow, or a payout that
/// may not have gone yet.
const HOLDABLE: &[&str] = &["paid", "picked_up", "delivered", "completed"];

/// Locks the order; returns (status, held, hold_reason).
async fn lock_order(
    tx: &mut sqlx::PgConnection,
    order_id: Uuid,
) -> Result<(String, bool, Option<String>), AppError> {
    sqlx::query(LOCK_TIMEOUT)
        .execute(&mut *tx)
        .await
        .context("set lock_timeout")?;
    sqlx::query_as(
        "SELECT status::text, held_at IS NOT NULL, hold_reason FROM orders \
         WHERE id = $1 FOR UPDATE",
    )
    .bind(order_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(busy_err("order_busy", "lock order"))?
    .ok_or(AppError::NotFound)
}

/// `reason` is validated by the caller (no personal data).
pub async fn hold(db: &PgPool, actor: Uuid, order_id: Uuid, reason: &str) -> Result<(), AppError> {
    let mut tx = db.begin().await.context("begin")?;
    let (status, held, _) = lock_order(&mut tx, order_id).await?;
    if held {
        return Err(AppError::Conflict("already_held"));
    }
    if !HOLDABLE.contains(&status.as_str()) {
        return Err(AppError::ConflictDetail(
            "invalid_order_state",
            format!("order is {status}"),
        ));
    }
    sqlx::query("UPDATE orders SET held_at = now(), hold_reason = $2 WHERE id = $1")
        .bind(order_id)
        .bind(reason)
        .execute(&mut *tx)
        .await
        .context("hold order")?;
    audit::record(
        &mut tx,
        Some(actor),
        "admin.order_held",
        "order",
        Some(order_id),
        json!({ "reason": reason, "prior_status": status }),
    )
    .await?;
    tx.commit().await.context("commit")?;
    Ok(())
}

pub async fn unhold(db: &PgPool, actor: Uuid, order_id: Uuid, note: &str) -> Result<(), AppError> {
    let mut tx = db.begin().await.context("begin")?;
    let (status, held, reason) = lock_order(&mut tx, order_id).await?;
    if !held {
        return Err(AppError::Conflict("not_held"));
    }
    sqlx::query("UPDATE orders SET held_at = NULL, hold_reason = NULL WHERE id = $1")
        .bind(order_id)
        .execute(&mut *tx)
        .await
        .context("unhold order")?;
    audit::record(
        &mut tx,
        Some(actor),
        "admin.order_unheld",
        "order",
        Some(order_id),
        json!({ "note": note, "hold_reason": reason, "status": status }),
    )
    .await?;
    tx.commit().await.context("commit")?;
    Ok(())
}
