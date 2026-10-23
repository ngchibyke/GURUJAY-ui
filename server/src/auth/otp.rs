use async_trait::async_trait;
use rand::{Rng, rngs::OsRng};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use uuid::Uuid;

use crate::error::AppError;

pub const OTP_MINUTES: i32 = 10;
pub const OTP_MAX_ATTEMPTS: i32 = 5;
pub const OTP_MIN_INTERVAL_SECS: f64 = 60.0;
pub const OTP_MAX_PER_HOUR: i64 = 5;

#[async_trait]
pub trait OtpSender: Send + Sync {
    async fn send(&self, phone_e164: &str, code: &str) -> Result<(), AppError>;
}

/// Development sender. Never logs the phone or the code; when `echo` is on
/// (OTP_DEV_ECHO=1, refused in production) the code alone is written to stdout.
pub struct LogOtpSender {
    pub echo: bool,
}

#[async_trait]
impl OtpSender for LogOtpSender {
    async fn send(&self, _phone_e164: &str, code: &str) -> Result<(), AppError> {
        if self.echo {
            println!("[dev] OTP code: {code}");
        }
        Ok(())
    }
}

fn hash_code(user_id: Uuid, code: &str) -> Vec<u8> {
    let mut h = Sha256::new();
    h.update(user_id.as_bytes());
    h.update(code.as_bytes());
    h.finalize().to_vec()
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Supersedes any live code for the user, stores a hash of a new 6-digit code and sends it.
pub async fn issue_otp(
    db: &PgPool,
    sender: &dyn OtpSender,
    user_id: Uuid,
    phone_e164: &str,
) -> Result<(), AppError> {
    let code = format!("{:06}", OsRng.gen_range(0..1_000_000u32));
    let mut tx = db.begin().await.map_err(anyhow::Error::from)?;
    // Serialise issuance per user so concurrent calls cannot slip past the throttle.
    sqlx::query("SELECT id FROM users WHERE id = $1 FOR UPDATE")
        .bind(user_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(anyhow::Error::from)?;
    let (recent, hourly): (i64, i64) = sqlx::query_as(
        "SELECT count(*) FILTER (WHERE created_at > now() - make_interval(secs => $2)), \
                count(*) FILTER (WHERE created_at > now() - interval '1 hour') \
         FROM otp_codes WHERE user_id = $1",
    )
    .bind(user_id)
    .bind(OTP_MIN_INTERVAL_SECS)
    .fetch_one(&mut *tx)
    .await
    .map_err(anyhow::Error::from)?;
    if recent > 0 || hourly >= OTP_MAX_PER_HOUR {
        // Throttled: callers keep their normal response, nothing new is sent.
        return Ok(());
    }
    sqlx::query("UPDATE otp_codes SET consumed_at = now() WHERE user_id = $1 AND consumed_at IS NULL")
        .bind(user_id)
        .execute(&mut *tx)
        .await
        .map_err(anyhow::Error::from)?;
    sqlx::query(
        "INSERT INTO otp_codes (user_id, code_hash, expires_at) \
         VALUES ($1, $2, now() + make_interval(mins => $3))",
    )
    .bind(user_id)
    .bind(hash_code(user_id, &code))
    .bind(OTP_MINUTES)
    .execute(&mut *tx)
    .await
    .map_err(anyhow::Error::from)?;
    tx.commit().await.map_err(anyhow::Error::from)?;

    sender.send(phone_e164, &code).await?;
    tracing::info!(user_id = %user_id, "OTP sent to user");
    Ok(())
}

fn invalid() -> AppError {
    AppError::Unprocessable("otp_invalid", "That code is not correct".into())
}

fn locked() -> AppError {
    AppError::Unprocessable("otp_locked", "Too many wrong codes. Request a new code".into())
}

/// Checks `code` against the user's live OTP. On success the code is consumed and
/// the phone is marked verified. Every guess counts toward the 5-attempt cap.
pub async fn verify_otp(db: &PgPool, user_id: Uuid, code: &str) -> Result<(), AppError> {
    let row: Option<(Uuid, Vec<u8>, i32, bool)> = sqlx::query_as(
        "SELECT id, code_hash, attempts, expires_at < now() FROM otp_codes \
         WHERE user_id = $1 AND consumed_at IS NULL ORDER BY created_at DESC LIMIT 1",
    )
    .bind(user_id)
    .fetch_optional(db)
    .await
    .map_err(anyhow::Error::from)?;
    let Some((otp_id, code_hash, attempts, expired)) = row else {
        return Err(invalid());
    };
    if attempts >= OTP_MAX_ATTEMPTS {
        return Err(locked());
    }
    if expired {
        return Err(AppError::Unprocessable("otp_expired", "That code has expired. Request a new code".into()));
    }
    // Atomically count this guess so a concurrent burst cannot exceed the cap.
    let counted: Option<i32> = sqlx::query_scalar(
        "UPDATE otp_codes SET attempts = attempts + 1 WHERE id = $1 AND attempts < $2 RETURNING attempts",
    )
    .bind(otp_id)
    .bind(OTP_MAX_ATTEMPTS)
    .fetch_optional(db)
    .await
    .map_err(anyhow::Error::from)?;
    if counted.is_none() {
        return Err(locked());
    }
    if !ct_eq(&hash_code(user_id, code), &code_hash) {
        return Err(invalid());
    }

    let mut tx = db.begin().await.map_err(anyhow::Error::from)?;
    let consumed = sqlx::query("UPDATE otp_codes SET consumed_at = now() WHERE id = $1 AND consumed_at IS NULL")
        .bind(otp_id)
        .execute(&mut *tx)
        .await
        .map_err(anyhow::Error::from)?
        .rows_affected();
    if consumed == 0 {
        return Err(invalid());
    }
    sqlx::query("UPDATE users SET phone_verified_at = COALESCE(phone_verified_at, now()) WHERE id = $1")
        .bind(user_id)
        .execute(&mut *tx)
        .await
        .map_err(anyhow::Error::from)?;
    tx.commit().await.map_err(anyhow::Error::from)?;
    Ok(())
}
