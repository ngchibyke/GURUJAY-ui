use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::post,
};
use serde::Deserialize;
use serde_json::{Value, json};
use time::OffsetDateTime;
use uuid::Uuid;

use super::service::{self, AccessCodeSummary};
use crate::{
    AppState,
    auth::{AuthUser, Role},
    error::AppError,
    extract::AppJson,
};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/admin/access-codes", post(create).get(list))
        .route("/admin/access-codes/{id}/revoke", post(revoke))
        .route("/seller/redeem", post(redeem))
}

fn parse_id(key: &str) -> Result<Uuid, AppError> {
    key.parse().map_err(|_| AppError::NotFound)
}

#[derive(Deserialize)]
struct CreateBody {
    label: Option<String>,
    max_uses: Option<i32>,
    #[serde(default, with = "time::serde::rfc3339::option")]
    expires_at: Option<OffsetDateTime>,
}

#[derive(Deserialize)]
struct RedeemBody {
    code: String,
}

async fn create(
    State(state): State<AppState>,
    user: AuthUser,
    AppJson(body): AppJson<CreateBody>,
) -> Result<(StatusCode, Json<Value>), AppError> {
    user.require_role(Role::Owner)?;
    let (id, code) = service::create(
        &state.db,
        user.id,
        body.label,
        body.max_uses,
        body.expires_at,
    )
    .await?;
    Ok((StatusCode::CREATED, Json(json!({ "id": id, "code": code }))))
}

async fn list(
    State(state): State<AppState>,
    user: AuthUser,
) -> Result<Json<Vec<AccessCodeSummary>>, AppError> {
    user.require_role(Role::Owner)?;
    Ok(Json(service::list(&state.db).await?))
}

async fn revoke(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
) -> Result<StatusCode, AppError> {
    user.require_role(Role::Owner)?;
    service::revoke(&state.db, user.id, parse_id(&id)?).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn redeem(
    State(state): State<AppState>,
    user: AuthUser,
    AppJson(body): AppJson<RedeemBody>,
) -> Result<Json<Value>, AppError> {
    if !user.phone_verified {
        return Err(AppError::Forbidden("phone_unverified"));
    }
    service::redeem(&state.db, user.id, &body.code).await?;
    Ok(Json(json!({ "is_seller": true })))
}
