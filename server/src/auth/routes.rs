use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use super::{
    extractor::{AuthUser, bearer_token},
    otp::{IssueReason, invalidate_otps, issue_otp, verify_otp},
    password::{blocking, dummy_verify, hash_password, validate_password, verify_password},
    sessions::{create_session, revoke_session},
};
use sqlx::PgConnection;

use crate::{
    AppState,
    crypto::blind_index,
    error::{AppError, Redacted},
    extract::AppJson,
    phone::normalize_ng_phone,
};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/auth/sign-up", post(sign_up))
        .route("/auth/verify-phone", post(verify_phone))
        .route("/auth/sign-in", post(sign_in))
        .route("/auth/sign-out", post(sign_out))
        .route("/me", get(me))
}

#[derive(Deserialize)]
struct SignUp {
    phone: String,
    password: String,
    display_name: String,
}

#[derive(Deserialize)]
struct VerifyPhone {
    verification_id: String,
    code: String,
}

#[derive(Deserialize)]
struct SignIn {
    phone: String,
    password: String,
}

/// Inserts a user, or reclaims an unverified one, inside the caller's transaction.
/// A reclaim retires every outstanding OTP and verification id of that user, so a code
/// issued for earlier credentials can never verify the account. Shared with `create-owner`.
pub async fn insert_user(
    conn: &mut PgConnection,
    state: &AppState,
    phone_e164: &str,
    display_name: &str,
    password_hash: &str,
) -> Result<Uuid, AppError> {
    // Single statement: a fresh phone inserts; an existing UNVERIFIED, undeleted account is
    // reclaimed (new credentials, same id); a verified/deleted one yields no row => conflict.
    // ON CONFLICT makes concurrent sign-ups safe (no duplicate rows, no unique-violation 500).
    let id: Option<Uuid> = sqlx::query_scalar(
        "INSERT INTO users (phone_enc, phone_idx, display_name, password_hash) \
         VALUES ($1, $2, $3, $4) \
         ON CONFLICT (phone_idx) DO UPDATE SET \
             phone_enc = EXCLUDED.phone_enc, \
             display_name = EXCLUDED.display_name, \
             password_hash = EXCLUDED.password_hash \
         WHERE users.phone_verified_at IS NULL AND users.deleted_at IS NULL \
         RETURNING id",
    )
    .bind(state.cipher.encrypt(phone_e164))
    .bind(blind_index(&state.index_key, phone_e164))
    .bind(display_name)
    .bind(password_hash)
    .fetch_optional(&mut *conn)
    .await
    .map_err(anyhow::Error::from)?;
    let id = id.ok_or(AppError::Conflict("phone_taken"))?;
    invalidate_otps(conn, id).await?;
    Ok(id)
}

pub fn validate_display_name(name: &str) -> Result<String, AppError> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 80 {
        return Err(AppError::Validation("Display name must be 1 to 80 characters".into()));
    }
    Ok(name.to_string())
}

async fn sign_up(
    State(state): State<AppState>,
    AppJson(req): AppJson<SignUp>,
) -> Result<(StatusCode, Json<Value>), AppError> {
    let phone = normalize_ng_phone(&req.phone)?;
    validate_password(&req.password)?;
    let display_name = validate_display_name(&req.display_name)?;

    let password = req.password;
    let hash = blocking(move || hash_password(&password)).await??;
    // Credentials and the verification they are bound to change together.
    let mut tx = state.db.begin().await.map_err(anyhow::Error::from)?;
    let user_id = insert_user(&mut tx, &state, &phone, &display_name, &hash).await?;
    let issued = issue_otp(&mut tx, user_id, IssueReason::NewCredentials).await?;
    tx.commit().await.map_err(anyhow::Error::from)?;
    issued.deliver(state.otp.as_ref(), user_id, &phone).await?;
    Ok((StatusCode::CREATED, Json(json!({ "user_id": user_id, "verification_id": issued.verification_id }))))
}

async fn verify_phone(State(state): State<AppState>, AppJson(req): AppJson<VerifyPhone>) -> Result<Json<Value>, AppError> {
    let user_id = verify_otp(&state.db, req.verification_id.trim(), req.code.trim()).await?;
    let token = create_session(&state.db, user_id).await?;
    Ok(Json(json!({ "token": token })))
}

async fn sign_in(State(state): State<AppState>, AppJson(req): AppJson<SignIn>) -> Result<Json<Value>, AppError> {
    let phone = normalize_ng_phone(&req.phone)?;
    let row: Option<(Uuid, String, bool)> = sqlx::query_as(
        "SELECT id, password_hash, phone_verified_at IS NOT NULL FROM users \
         WHERE phone_idx = $1 AND deleted_at IS NULL",
    )
    .bind(blind_index(&state.index_key, &phone))
    .fetch_optional(&state.db)
    .await
    .map_err(anyhow::Error::from)?;

    let password = req.password;
    let account = match row {
        Some((id, hash, verified)) => {
            let checked = hash.clone();
            blocking(move || verify_password(&password, &checked)).await?.then_some((id, hash, verified))
        }
        None => {
            blocking(move || dummy_verify(&password)).await?;
            None
        }
    };
    let Some((user_id, hash, verified)) = account else {
        return Err(AppError::InvalidCredentials);
    };

    if !verified {
        let mut tx = state.db.begin().await.map_err(anyhow::Error::from)?;
        // Only hand out a verification id if the credentials just checked are still current
        // (a concurrent reclaim may have replaced them).
        let current: Option<String> =
            sqlx::query_scalar("SELECT password_hash FROM users WHERE id = $1 AND deleted_at IS NULL FOR UPDATE")
                .bind(user_id)
                .fetch_optional(&mut *tx)
                .await
                .map_err(anyhow::Error::from)?;
        if current.as_deref() != Some(hash.as_str()) {
            return Err(AppError::InvalidCredentials);
        }
        let issued = issue_otp(&mut tx, user_id, IssueReason::Resend).await?;
        tx.commit().await.map_err(anyhow::Error::from)?;
        issued.deliver(state.otp.as_ref(), user_id, &phone).await?;
        return Err(AppError::PhoneUnverified(Redacted(issued.verification_id)));
    }
    let token = create_session(&state.db, user_id).await?;
    Ok(Json(json!({ "token": token })))
}

async fn sign_out(State(state): State<AppState>, _user: AuthUser, headers: HeaderMap) -> Result<StatusCode, AppError> {
    if let Some(token) = bearer_token(&headers) {
        revoke_session(&state.db, token).await?;
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn me(State(state): State<AppState>, user: AuthUser) -> Result<Json<Value>, AppError> {
    let (display_name, phone_enc): (String, Vec<u8>) =
        sqlx::query_as("SELECT display_name, phone_enc FROM users WHERE id = $1")
            .bind(user.id)
            .fetch_one(&state.db)
            .await
            .map_err(anyhow::Error::from)?;
    // Only ever decrypts the authenticated account's own phone.
    let phone = state.cipher.decrypt(&phone_enc)?;
    Ok(Json(json!({
        "id": user.id,
        "display_name": display_name,
        "phone": phone,
        "role": user.role,
        "is_seller": user.is_seller,
    })))
}
