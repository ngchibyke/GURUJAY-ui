use axum::{
    Json, Router,
    extract::{Path, State},
    routing::{get, post},
};
use uuid::Uuid;

use super::service::{self, PayoutAccountView, SetPayoutAccount, VerifyPayoutAccount};
use crate::{
    AppState,
    auth::{AuthUser, Role},
    error::AppError,
    extract::AppJson,
    stores::routes::require_seller,
};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/stores/{key}/payout-account", get(show).put(set))
        .route("/admin/stores/{id}/payout-account", get(admin_show))
        .route("/admin/stores/{id}/payout-account/verify", post(verify))
}

fn parse_id(key: &str) -> Result<Uuid, AppError> {
    key.parse().map_err(|_| AppError::NotFound)
}

async fn set(
    State(state): State<AppState>,
    user: AuthUser,
    Path(key): Path<String>,
    AppJson(body): AppJson<SetPayoutAccount>,
) -> Result<Json<PayoutAccountView>, AppError> {
    require_seller(&user)?;
    Ok(Json(
        service::set_account(&state, user.id, parse_id(&key)?, body).await?,
    ))
}

async fn show(
    State(state): State<AppState>,
    user: AuthUser,
    Path(key): Path<String>,
) -> Result<Json<PayoutAccountView>, AppError> {
    require_seller(&user)?;
    Ok(Json(
        service::get_for_owner(&state, user.id, parse_id(&key)?).await?,
    ))
}

async fn admin_show(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
) -> Result<Json<PayoutAccountView>, AppError> {
    user.require_role(Role::Owner)?;
    Ok(Json(
        service::get_for_admin(&state, user.id, parse_id(&id)?).await?,
    ))
}

async fn verify(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
    AppJson(body): AppJson<VerifyPayoutAccount>,
) -> Result<Json<PayoutAccountView>, AppError> {
    user.require_role(Role::Owner)?;
    Ok(Json(
        service::owner_verify(&state, user.id, parse_id(&id)?, body).await?,
    ))
}
