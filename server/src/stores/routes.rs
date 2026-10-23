use axum::{
    Json, Router,
    extract::{FromRequestParts, Path, State},
    http::{StatusCode, request::Parts},
    routing::get,
};
use uuid::Uuid;

use super::service::{self, NewStore, OwnerStore, PublicStore, StorePatch};
use crate::{
    AppState,
    auth::{AuthUser, extractor::bearer_token},
    error::AppError,
    extract::AppJson,
};

// Path parameters share one name: `/stores/{key}` is a slug for GET and an id for PATCH/DELETE.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/stores", axum::routing::post(create))
        .route("/stores/mine", get(mine))
        .route("/stores/{key}", get(show).patch(update).delete(remove))
}

/// Optional authentication: no credentials means anonymous; bad credentials are still rejected.
struct MaybeAuth(Option<AuthUser>);

impl FromRequestParts<AppState> for MaybeAuth {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, AppError> {
        if bearer_token(&parts.headers).is_none() {
            return Ok(MaybeAuth(None));
        }
        Ok(MaybeAuth(Some(AuthUser::from_request_parts(parts, state).await?)))
    }
}

fn require_seller(user: &AuthUser) -> Result<(), AppError> {
    if user.is_seller { Ok(()) } else { Err(AppError::Forbidden("not_a_seller")) }
}

fn parse_id(key: &str) -> Result<Uuid, AppError> {
    key.parse().map_err(|_| AppError::NotFound)
}

async fn create(
    State(state): State<AppState>,
    user: AuthUser,
    AppJson(body): AppJson<NewStore>,
) -> Result<(StatusCode, Json<OwnerStore>), AppError> {
    require_seller(&user)?;
    let store = service::create_store(&state.db, &state.cipher, user.id, body).await?;
    Ok((StatusCode::CREATED, Json(store.owner_view(&state.cipher)?)))
}

async fn mine(State(state): State<AppState>, user: AuthUser) -> Result<Json<Vec<OwnerStore>>, AppError> {
    require_seller(&user)?;
    let stores = service::list_mine(&state.db, user.id).await?;
    Ok(Json(stores.iter().map(|s| s.owner_view(&state.cipher)).collect::<Result<_, _>>()?))
}

async fn show(
    State(state): State<AppState>,
    MaybeAuth(viewer): MaybeAuth,
    Path(slug): Path<String>,
) -> Result<Json<PublicStore>, AppError> {
    let store = service::get_by_slug(&state.db, &slug, viewer.map(|u| u.id)).await?;
    Ok(Json(store.public()))
}

async fn update(
    State(state): State<AppState>,
    user: AuthUser,
    Path(key): Path<String>,
    AppJson(patch): AppJson<StorePatch>,
) -> Result<Json<OwnerStore>, AppError> {
    require_seller(&user)?;
    let store = service::update_store(&state.db, &state.cipher, user.id, parse_id(&key)?, patch).await?;
    Ok(Json(store.owner_view(&state.cipher)?))
}

async fn remove(
    State(state): State<AppState>,
    user: AuthUser,
    Path(key): Path<String>,
) -> Result<StatusCode, AppError> {
    require_seller(&user)?;
    service::delete_store(&state.db, user.id, parse_id(&key)?).await?;
    Ok(StatusCode::NO_CONTENT)
}
