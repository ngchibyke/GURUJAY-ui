use axum::{Json, Router, extract::State, routing::get};

use super::service::{self, Settings, SettingsPatch};
use crate::{
    AppState,
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

async fn update(
    State(state): State<AppState>,
    user: AuthUser,
    AppJson(patch): AppJson<SettingsPatch>,
) -> Result<Json<Settings>, AppError> {
    user.require_role(Role::Owner)?;
    Ok(Json(service::update(&state.db, user.id, patch).await?))
}
