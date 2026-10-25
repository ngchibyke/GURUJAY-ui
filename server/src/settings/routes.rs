use axum::{Json, Router, extract::State, routing::get};

use super::service::{self, Settings, SettingsPatch};
use crate::{
    AppState,
    admin::Owner,
    auth::{AuthUser, Role},
    error::AppError,
    extract::AppJson,
};

pub fn router() -> Router<AppState> {
    Router::new().route("/admin/settings", get(read).patch(update))
}

async fn read(State(state): State<AppState>, user: AuthUser) -> Result<Json<Settings>, AppError> {
    user.require_role(Role::Support)?;
    Ok(Json(service::load(&state.db).await?))
}

/// Owner only; the role is checked before the body is read.
async fn update(
    State(state): State<AppState>,
    Owner(user): Owner,
    AppJson(patch): AppJson<SettingsPatch>,
) -> Result<Json<Settings>, AppError> {
    Ok(Json(service::update(&state.db, user.id, patch).await?))
}
