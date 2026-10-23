use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    routing::{get, post},
};
use serde::Deserialize;
use uuid::Uuid;

use super::service::{self, CheckoutView, Created, Outcome};
use crate::{AppState, auth::AuthUser, error::AppError, extract::AppJson};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/checkout", post(create))
        .route("/checkout/{id}", get(show))
}

const IDEMPOTENCY_HEADER: &str = "idempotency-key";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckoutBody {
    method: String,
}

fn parse_id(key: &str) -> Result<Uuid, AppError> {
    key.parse().map_err(|_| AppError::NotFound)
}

/// The optional `Idempotency-Key`: 1 to 64 of `[A-Za-z0-9_-]` (clients should use a
/// random UUID; never personal data).
fn idempotency_key(headers: &HeaderMap) -> Result<Option<String>, AppError> {
    let Some(value) = headers.get(IDEMPOTENCY_HEADER) else {
        return Ok(None);
    };
    let invalid = || {
        AppError::Validation("Idempotency-Key must be 1 to 64 letters, digits, '-' or '_'".into())
    };
    let key = value.to_str().map_err(|_| invalid())?;
    if key.is_empty()
        || key.len() > 64
        || !key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(invalid());
    }
    Ok(Some(key.to_string()))
}

async fn create(
    State(state): State<AppState>,
    user: AuthUser,
    headers: HeaderMap,
    AppJson(body): AppJson<CheckoutBody>,
) -> Result<(StatusCode, Json<Created>), AppError> {
    let key = idempotency_key(&headers)?;
    match service::create(&state, user.id, &body.method, key.as_deref()).await? {
        Outcome::Created(c) => Ok((StatusCode::CREATED, Json(c))),
        Outcome::Replayed(c) => Ok((StatusCode::OK, Json(c))),
    }
}

async fn show(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
) -> Result<Json<CheckoutView>, AppError> {
    Ok(Json(
        service::get(&state.db, user.id, parse_id(&id)?).await?,
    ))
}
