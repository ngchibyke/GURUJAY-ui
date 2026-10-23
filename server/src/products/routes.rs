use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::{get, put},
};
use uuid::Uuid;

use super::service::{self, NewProduct, ProductPatch, ProductView, VariantInput};
use crate::{
    AppState,
    auth::AuthUser,
    error::AppError,
    extract::AppJson,
    stores::routes::{MaybeAuth, require_seller},
};

// `/stores/{key}` reuses the store routes' param name: the key is a store id for POST, a slug for GET.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/stores/{key}/products", get(list).post(create))
        .route("/products/{id}", get(show).patch(update).delete(remove))
        .route("/products/{id}/variants", put(replace_variants))
}

fn parse_id(key: &str) -> Result<Uuid, AppError> {
    key.parse().map_err(|_| AppError::NotFound)
}

async fn create(
    State(state): State<AppState>,
    user: AuthUser,
    Path(key): Path<String>,
    AppJson(body): AppJson<NewProduct>,
) -> Result<(StatusCode, Json<ProductView>), AppError> {
    require_seller(&user)?;
    let product = service::create_product(&state.db, user.id, parse_id(&key)?, body).await?;
    Ok((StatusCode::CREATED, Json(product)))
}

async fn list(
    State(state): State<AppState>,
    MaybeAuth(viewer): MaybeAuth,
    Path(slug): Path<String>,
) -> Result<Json<Vec<ProductView>>, AppError> {
    Ok(Json(
        service::list_store_products(&state.db, &slug, viewer.map(|u| u.id)).await?,
    ))
}

async fn show(
    State(state): State<AppState>,
    MaybeAuth(viewer): MaybeAuth,
    Path(id): Path<String>,
) -> Result<Json<ProductView>, AppError> {
    Ok(Json(
        service::get_product(&state.db, parse_id(&id)?, viewer.map(|u| u.id)).await?,
    ))
}

async fn update(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
    AppJson(patch): AppJson<ProductPatch>,
) -> Result<Json<ProductView>, AppError> {
    require_seller(&user)?;
    Ok(Json(
        service::update_product(&state.db, user.id, parse_id(&id)?, patch).await?,
    ))
}

async fn replace_variants(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
    AppJson(variants): AppJson<Vec<VariantInput>>,
) -> Result<Json<ProductView>, AppError> {
    require_seller(&user)?;
    Ok(Json(
        service::replace_variants(&state.db, user.id, parse_id(&id)?, variants).await?,
    ))
}

async fn remove(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
) -> Result<StatusCode, AppError> {
    require_seller(&user)?;
    service::delete_product(&state.db, user.id, parse_id(&id)?).await?;
    Ok(StatusCode::NO_CONTENT)
}
