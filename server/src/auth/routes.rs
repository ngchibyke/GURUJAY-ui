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
    otp::{issue_otp, verify_otp},
    password::{blocking, dummy_verify, hash_password, validate_password, verify_password},
    sessions::{create_session, revoke_session},
};
use crate::{AppState, crypto::blind_index, error::AppError, extract::AppJson, phone::normalize_ng_phone};

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
    user_id: Uuid,
    code: String,
}

#[derive(Deserialize)]
struct SignIn {
    phone: String,
    password: String,
}

/// Inserts a user. Shared with the `create-owner` CLI command.
pub async fn insert_user(
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
    .fetch_optional(&state.db)
    .await
    .map_err(anyhow::Error::from)?;
    id.ok_or(AppError::Conflict("phone_taken"))
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
    let user_id = insert_user(&state, &phone, &display_name, &hash).await?;
    issue_otp(&state.db, state.otp.as_ref(), user_id, &phone).await?;
    Ok((StatusCode::CREATED, Json(json!({ "user_id": user_id }))))
}

async fn verify_phone(State(state): State<AppState>, AppJson(req): AppJson<VerifyPhone>) -> Result<Json<Value>, AppError> {
    verify_otp(&state.db, req.user_id, req.code.trim()).await?;
    let token = create_session(&state.db, req.user_id).await?;
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
            blocking(move || verify_password(&password, &hash)).await?.then_some((id, verified))
        }
        None => {
            blocking(move || dummy_verify(&password)).await?;
            None
        }
    };
    let Some((user_id, verified)) = account else {
        return Err(AppError::InvalidCredentials);
    };

    if !verified {
        issue_otp(&state.db, state.otp.as_ref(), user_id, &phone).await?;
        return Err(AppError::Forbidden("phone_unverified"));
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
