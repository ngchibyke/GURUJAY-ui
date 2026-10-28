//! Order routes: views, buyer cancel and buyer "received".
//! Pickup and delivery have no HTTP route yet (plan 3).

use anyhow::Context;
use axum::{
    Json, Router,
    body::Bytes,
    extract::{Path, State},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use super::views::{self, DEFAULT_LIMIT, MAX_LIMIT, OrderView, Page, StaffRole};
use crate::{
    AppState, audit,
    auth::{AuthUser, extractor::Role},
    error::AppError,
    escrow::{ReleaseReason, refund::refund_order, release_order},
    extract::AppQuery,
};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/orders", get(list))
        .route("/orders/{id}", get(show))
        .route("/orders/{id}/cancel", post(cancel))
        .route("/orders/{id}/received", post(received))
        .route("/stores/{key}/orders", get(store_orders))
}

fn parse_id(key: &str) -> Result<Uuid, AppError> {
    key.parse().map_err(|_| AppError::NotFound)
}

#[derive(Deserialize)]
struct PageQuery {
    limit: Option<i64>,
    /// Opaque cursor from the previous page's `next_cursor`.
    before: Option<String>,
}

impl PageQuery {
    fn limit(&self) -> Result<i64, AppError> {
        let limit = self.limit.unwrap_or(DEFAULT_LIMIT);
        if !(1..=MAX_LIMIT).contains(&limit) {
            return Err(AppError::Validation(format!(
                "limit must be between 1 and {MAX_LIMIT}"
            )));
        }
        Ok(limit)
    }
}

async fn list(
    State(state): State<AppState>,
    user: AuthUser,
    AppQuery(q): AppQuery<PageQuery>,
) -> Result<Json<Page<OrderView>>, AppError> {
    let limit = q.limit()?;
    Ok(Json(
        views::list_for_buyer(&state.db, user.id, limit, q.before.as_deref()).await?,
    ))
}

async fn show(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
) -> Result<Json<OrderView>, AppError> {
    let id = parse_id(&id)?;
    let staff = match user.role {
        Role::Owner => Some(StaffRole::Owner),
        Role::Support => Some(StaffRole::Support),
        Role::User => None,
    };
    Ok(Json(
        views::get_for_viewer(&state.db, user.id, staff, id).await?,
    ))
}

async fn store_orders(
    State(state): State<AppState>,
    user: AuthUser,
    Path(key): Path<String>,
    AppQuery(q): AppQuery<PageQuery>,
) -> Result<Json<Page<OrderView>>, AppError> {
    let store_id = parse_id(&key)?;
    let limit = q.limit()?;
    Ok(Json(
        views::list_for_store(&state.db, user.id, store_id, limit, q.before.as_deref()).await?,
    ))
}

/// (status, held, has a payout) if `user` is the order's buyer; anyone else gets 404.
async fn buyer_order(
    state: &AppState,
    user: &AuthUser,
    order_id: Uuid,
) -> Result<(String, bool, bool), AppError> {
    let row: Option<(String, bool, bool)> = sqlx::query_as(
        "SELECT o.status::text, o.held_at IS NOT NULL, \
                EXISTS (SELECT 1 FROM payouts p WHERE p.order_id = o.id) \
         FROM orders o WHERE o.id = $1 AND o.buyer_id = $2",
    )
    .bind(order_id)
    .bind(user.id)
    .fetch_optional(&state.db)
    .await
    .context("load buyer order")?;
    row.ok_or(AppError::NotFound)
}

/// Route-level audit row. The engines write the authoritative rows inside their
/// own transactions (`order.refund_requested`, `order.released`), so a failure
/// here is logged (error kind only, no PII) and never fails the request.
async fn record(
    state: &AppState,
    user: &AuthUser,
    action: &str,
    order_id: Uuid,
    prior_status: &str,
) {
    let result = async {
        let mut conn = state.db.acquire().await.map_err(|_| "acquire")?;
        audit::record(
            &mut conn,
            Some(user.id),
            action,
            "order",
            Some(order_id),
            json!({ "prior_status": prior_status }),
        )
        .await
        .map_err(|_| "insert")
    }
    .await;
    if let Err(kind) = result {
        tracing::error!(action, kind, "order audit write failed");
    }
}

async fn cancel(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
) -> Result<Json<OrderView>, AppError> {
    let id = parse_id(&id)?;
    let (status, _, _) = buyer_order(&state, &user, id).await?;
    if status != "paid" {
        return Err(AppError::Conflict("invalid_order_state"));
    }
    // A held order can still be cancelled before pickup. Stock goes back on hand.
    match refund_order(&state, id, true).await {
        Ok(_) => {}
        // Lost a race with pickup or release.
        Err(AppError::Conflict("not_refundable" | "already_released")) => {
            return Err(AppError::Conflict("invalid_order_state"));
        }
        Err(e) => return Err(e),
    }
    record(&state, &user, "order.cancelled_by_buyer", id, &status).await;
    Ok(Json(
        views::get_for_viewer(&state.db, user.id, None, id).await?,
    ))
}

/// The body must be exactly `{"confirm": true}`.
fn confirmed(body: &[u8]) -> bool {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Confirm {
        confirm: bool,
    }
    serde_json::from_slice::<Confirm>(body).is_ok_and(|c| c.confirm)
}

async fn received(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<String>,
    body: Bytes,
) -> Result<Json<OrderView>, AppError> {
    let id = parse_id(&id)?;
    // Ownership first, so a stranger gets 404 whatever the body says.
    let (status, held, has_payout) = buyer_order(&state, &user, id).await?;
    // Releasing the money is irreversible: the client must say so explicitly.
    if !confirmed(&body) {
        return Err(AppError::Unprocessable(
            "confirmation_required",
            "Only confirm once you hold the item. Never confirm because a seller, rider or chat asks.".into(),
        ));
    }
    match status.as_str() {
        // Repeating the call after release changes nothing.
        "completed" => {}
        "picked_up" | "delivered" if has_payout => {}
        "picked_up" | "delivered" => {
            if held {
                return Err(AppError::Conflict("order_held"));
            }
            release_order(&state, id, ReleaseReason::BuyerConfirmed).await?;
            record(&state, &user, "order.received", id, &status).await;
        }
        _ => return Err(AppError::Conflict("invalid_order_state")),
    }
    Ok(Json(
        views::get_for_viewer(&state.db, user.id, None, id).await?,
    ))
}
