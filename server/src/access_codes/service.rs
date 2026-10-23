use anyhow::Context;
use rand::{Rng, rngs::OsRng};
use serde::Serialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{audit, error::AppError};

const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
const CODE_LEN: usize = 10;
const MAX_FAILED_ATTEMPTS_PER_HOUR: i64 = 10;

/// Uppercases and strips dashes and whitespace.
fn normalize(input: &str) -> String {
    input
        .chars()
        .filter(|c| *c != '-' && !c.is_whitespace())
        .flat_map(char::to_uppercase)
        .collect()
}

fn hash(normalized: &str) -> Vec<u8> {
    Sha256::digest(normalized.as_bytes()).to_vec()
}

fn generate() -> String {
    let mut rng = OsRng;
    (0..CODE_LEN)
        .map(|_| ALPHABET[rng.gen_range(0..ALPHABET.len())] as char)
        .collect()
}

/// Creates a code; returns (id, display code `XXXXX-XXXXX`). The plain code is never stored.
pub async fn create(
    db: &PgPool,
    actor: Uuid,
    label: Option<String>,
    max_uses: Option<i32>,
    expires_at: Option<OffsetDateTime>,
) -> Result<(Uuid, String), AppError> {
    if matches!(max_uses, Some(n) if n < 1) {
        return Err(AppError::Validation("max_uses must be at least 1".into()));
    }
    if matches!(expires_at, Some(t) if t <= OffsetDateTime::now_utc()) {
        return Err(AppError::Validation(
            "expires_at must be in the future".into(),
        ));
    }
    let label = label
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty());
    if matches!(&label, Some(l) if l.chars().count() > 80) {
        return Err(AppError::Validation(
            "label must be at most 80 characters".into(),
        ));
    }

    let plain = generate();
    let last4 = plain[CODE_LEN - 4..].to_string();
    let mut tx = db.begin().await.context("begin")?;
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO access_codes (code_hash, code_last4, label, max_uses, expires_at, created_by) \
         VALUES ($1, $2, $3, $4, $5, $6) RETURNING id",
    )
    .bind(hash(&plain))
    .bind(&last4)
    .bind(label)
    .bind(max_uses)
    .bind(expires_at)
    .bind(actor)
    .fetch_one(&mut *tx)
    .await
    .context("insert access_code")?;
    audit::record(
        &mut tx,
        Some(actor),
        "access_code.created",
        "access_code",
        Some(id),
        json!({ "max_uses": max_uses, "has_expiry": expires_at.is_some() }),
    )
    .await?;
    tx.commit().await.context("commit")?;
    Ok((id, format!("{}-{}", &plain[..5], &plain[5..])))
}

#[derive(Serialize, sqlx::FromRow)]
pub struct AccessCodeSummary {
    pub id: Uuid,
    pub label: Option<String>,
    #[sqlx(rename = "code_last4")]
    pub last4: String,
    pub max_uses: Option<i32>,
    pub uses: i32,
    #[serde(with = "time::serde::rfc3339::option")]
    pub expires_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub revoked_at: Option<OffsetDateTime>,
}

pub async fn list(db: &PgPool) -> Result<Vec<AccessCodeSummary>, AppError> {
    Ok(sqlx::query_as(
        "SELECT id, label, code_last4, max_uses, uses, expires_at, revoked_at \
         FROM access_codes ORDER BY created_at DESC, id",
    )
    .fetch_all(db)
    .await
    .context("list access_codes")?)
}

/// Revokes a code (idempotent). Stores already created are unaffected.
pub async fn revoke(db: &PgPool, actor: Uuid, id: Uuid) -> Result<(), AppError> {
    let mut tx = db.begin().await.context("begin")?;
    let found: Option<Uuid> = sqlx::query_scalar(
        "UPDATE access_codes SET revoked_at = COALESCE(revoked_at, now()) WHERE id = $1 RETURNING id",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await
    .context("revoke access_code")?;
    if found.is_none() {
        return Err(AppError::NotFound);
    }
    audit::record(
        &mut tx,
        Some(actor),
        "access_code.revoked",
        "access_code",
        Some(id),
        json!({}),
    )
    .await?;
    tx.commit().await.context("commit")?;
    Ok(())
}

/// Redeems a code, making the user a seller. Already-sellers succeed without consuming a use.
pub async fn redeem(db: &PgPool, user_id: Uuid, code: &str) -> Result<(), AppError> {
    let result = redeem_tx(db, user_id, &normalize(code)).await;
    if let Err(AppError::Unprocessable(..)) = &result {
        // The lookup transaction was rolled back; record the failure on its own so it sticks.
        sqlx::query("INSERT INTO redeem_attempts (user_id) VALUES ($1)")
            .bind(user_id)
            .execute(db)
            .await
            .context("record redeem attempt")?;
    }
    result
}

async fn redeem_tx(db: &PgPool, user_id: Uuid, normalized: &str) -> Result<(), AppError> {
    let mut tx = db.begin().await.context("begin")?;

    // Lock the user first (consistent order: user, then code) so a double submit by the
    // same user serializes instead of hitting the unique redemption constraint.
    let is_seller: bool =
        sqlx::query_scalar("SELECT is_seller FROM users WHERE id = $1 FOR UPDATE")
            .bind(user_id)
            .fetch_one(&mut *tx)
            .await
            .context("lock user")?;
    if is_seller {
        return Ok(());
    }

    let failed: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM redeem_attempts WHERE user_id = $1 AND at > now() - interval '1 hour'",
    )
    .bind(user_id)
    .fetch_one(&mut *tx)
    .await
    .context("count attempts")?;
    if failed >= MAX_FAILED_ATTEMPTS_PER_HOUR {
        return Err(AppError::TooManyRequests("too_many_attempts"));
    }

    #[derive(sqlx::FromRow)]
    struct Row {
        id: Uuid,
        max_uses: Option<i32>,
        uses: i32,
        expired: bool,
        revoked: bool,
    }
    let row: Option<Row> = sqlx::query_as(
        "SELECT id, max_uses, uses, \
                COALESCE(expires_at <= now(), false) AS expired, \
                revoked_at IS NOT NULL AS revoked \
         FROM access_codes WHERE code_hash = $1 FOR UPDATE",
    )
    .bind(hash(normalized))
    .fetch_optional(&mut *tx)
    .await
    .context("lock access_code")?;

    let row = row.ok_or_else(|| {
        AppError::Unprocessable("code_invalid", "Access code is not valid".into())
    })?;
    if row.revoked {
        return Err(AppError::Unprocessable(
            "code_revoked",
            "Access code has been revoked".into(),
        ));
    }
    if row.expired {
        return Err(AppError::Unprocessable(
            "code_expired",
            "Access code has expired".into(),
        ));
    }
    if matches!(row.max_uses, Some(max) if row.uses >= max) {
        return Err(AppError::Unprocessable(
            "code_exhausted",
            "Access code has no uses left".into(),
        ));
    }

    sqlx::query("UPDATE access_codes SET uses = uses + 1 WHERE id = $1")
        .bind(row.id)
        .execute(&mut *tx)
        .await
        .context("increment uses")?;
    sqlx::query("INSERT INTO access_code_redemptions (code_id, user_id) VALUES ($1, $2)")
        .bind(row.id)
        .bind(user_id)
        .execute(&mut *tx)
        .await
        .context("insert redemption")?;
    sqlx::query("UPDATE users SET is_seller = true WHERE id = $1")
        .bind(user_id)
        .execute(&mut *tx)
        .await
        .context("set is_seller")?;
    audit::record(
        &mut tx,
        Some(user_id),
        "access_code.redeemed",
        "access_code",
        Some(row.id),
        json!({}),
    )
    .await?;
    tx.commit().await.context("commit")?;
    Ok(())
}
