use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::{RngCore, rngs::OsRng};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use uuid::Uuid;

use crate::error::AppError;

pub const SESSION_DAYS: i32 = 30;

pub fn hash_token(token: &str) -> Vec<u8> {
    Sha256::digest(token.as_bytes()).to_vec()
}

/// Creates a session and returns the raw bearer token (never stored; only its SHA-256 is).
pub async fn create_session(db: &PgPool, user_id: Uuid) -> Result<String, AppError> {
    let mut raw = [0u8; 32];
    OsRng.fill_bytes(&mut raw);
    let token = URL_SAFE_NO_PAD.encode(raw);
    sqlx::query(
        "INSERT INTO sessions (user_id, token_hash, expires_at) \
         VALUES ($1, $2, now() + make_interval(days => $3))",
    )
    .bind(user_id)
    .bind(hash_token(&token))
    .bind(SESSION_DAYS)
    .execute(db)
    .await
    .map_err(anyhow::Error::from)?;
    Ok(token)
}

pub async fn revoke_session(db: &PgPool, token: &str) -> Result<(), AppError> {
    sqlx::query(
        "UPDATE sessions SET revoked_at = now() WHERE token_hash = $1 AND revoked_at IS NULL",
    )
    .bind(hash_token(token))
    .execute(db)
    .await
    .map_err(anyhow::Error::from)?;
    Ok(())
}
