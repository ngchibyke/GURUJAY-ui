use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    routing::{get, put},
};
use serde::Deserialize;
use uuid::Uuid;

use super::service::{self, Cart};
use crate::{AppState, auth::AuthUser, error::AppError, extract::AppJson};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/cart", get(show).delete(clear))
        .route("/cart/items", put(set_item))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SetItem {
    variant_id: Uuid,
    quantity: i64,
}

async fn show(State(state): State<AppState>, user: AuthUser) -> Result<Json<Cart>, AppError> {
    Ok(Json(service::get(&state.db, user.id).await?))
}

async fn set_item(
    State(state): State<AppState>,
    user: AuthUser,
    AppJson(body): AppJson<SetItem>,
) -> Result<Json<Cart>, AppError> {
    Ok(Json(
        service::set_item(&state.db, user.id, body.variant_id, body.quantity).await?,
    ))
}

async fn clear(State(state): State<AppState>, user: AuthUser) -> Result<StatusCode, AppError> {
    service::clear(&state.db, user.id).await?;
    Ok(StatusCode::NO_CONTENT)
}
