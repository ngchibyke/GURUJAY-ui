use anyhow::Context;
use serde_json::Value;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::error::AppError;

/// Appends an entry to the append-only audit log.
///
/// Pass `&mut *tx` to write inside the caller's transaction.
///
/// `detail` must never contain phone numbers, passwords, codes or addresses
/// (or any other personal data); reference subjects by id instead.
pub async fn record(
    tx: &mut PgConnection,
    actor: Option<Uuid>,
    action: &str,
    subject_type: &str,
    subject_id: Option<Uuid>,
    detail: Value,
) -> Result<(), AppError> {
    sqlx::query(
        "INSERT INTO audit_log (actor_id, action, subject_type, subject_id, detail) \
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(actor)
    .bind(action)
    .bind(subject_type)
    .bind(subject_id)
    .bind(detail)
    .execute(tx)
    .await
    .context("insert audit_log")?;
    Ok(())
}
