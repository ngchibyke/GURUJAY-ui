//! Order delivery transitions. Task 7 exposes them through routes (with the
//! caller's authorisation); here they are plain state changes.
//!
//! - [`mark_picked_up`]: `paid -> picked_up` (sets `picked_up_at`).
//! - [`mark_delivered`]: `picked_up -> delivered` (sets `delivered_at`, and
//!   `release_at = delivered_at + release_after_hours`, the setting read at
//!   that moment, so a later settings change never moves the deadline).
//!
//! Repeating a transition on an order already in its target state is a no-op;
//! any other status is 409 `invalid_order_state`.

use anyhow::Context;
use serde_json::json;
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{audit, checkout::service::busy_err, error::AppError};

const LOCK_TIMEOUT: &str = "SET LOCAL lock_timeout = '5s'";

/// `paid -> picked_up` at `at`.
pub async fn mark_picked_up(
    db: &PgPool,
    order_id: Uuid,
    at: OffsetDateTime,
) -> Result<(), AppError> {
    transition(db, order_id, at, "paid", "picked_up").await
}

/// `picked_up -> delivered` at `at`, snapshotting the auto-release deadline.
pub async fn mark_delivered(
    db: &PgPool,
    order_id: Uuid,
    at: OffsetDateTime,
) -> Result<(), AppError> {
    transition(db, order_id, at, "picked_up", "delivered").await
}

async fn transition(
    db: &PgPool,
    order_id: Uuid,
    at: OffsetDateTime,
    from: &'static str,
    to: &'static str,
) -> Result<(), AppError> {
    let mut tx = db.begin().await.context("begin")?;
    sqlx::query(LOCK_TIMEOUT)
        .execute(&mut *tx)
        .await
        .context("set lock_timeout")?;
    let status: String =
        sqlx::query_scalar("SELECT status::text FROM orders WHERE id = $1 FOR UPDATE")
            .bind(order_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(busy_err("order_busy", "lock order"))?
            .ok_or(AppError::NotFound)?;
    if status == to {
        return Ok(());
    }
    if status != from {
        return Err(AppError::Conflict("invalid_order_state"));
    }
    if to == "picked_up" {
        sqlx::query("UPDATE orders SET status = 'picked_up', picked_up_at = $2 WHERE id = $1")
            .bind(order_id)
            .bind(at)
            .execute(&mut *tx)
            .await
            .context("mark picked up")?;
    } else {
        sqlx::query(
            "UPDATE orders SET status = 'delivered', delivered_at = $2, \
                    release_at = $2 + make_interval(hours => \
                        (SELECT release_after_hours FROM platform_settings WHERE id)) \
             WHERE id = $1",
        )
        .bind(order_id)
        .bind(at)
        .execute(&mut *tx)
        .await
        .context("mark delivered")?;
    }
    audit::record(
        &mut tx,
        None,
        &format!("order.{to}"),
        "order",
        Some(order_id),
        json!({}),
    )
    .await?;
    tx.commit().await.context("commit")?;
    Ok(())
}
