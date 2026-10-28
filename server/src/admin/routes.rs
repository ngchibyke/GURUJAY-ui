//! Admin routes. Owner-only unless marked Staff (Owner or Support).

use anyhow::Context;
use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::{get, post, put},
};
use serde::{Deserialize, Serialize};

use super::{
    Owner, Staff, holidays, money, note, orders, parse_id, provider_reference,
    views::{self, AdminOrderView, CheckoutView, Items, PayoutView, Reconciliation, RefundView},
};
use crate::{
    AppState,
    error::AppError,
    escrow::{refund::owner_refund_order, release_by_owner},
    extract::{AppJson, AppQuery},
};

const NOTE_MAX: usize = 500;
const HOLD_REASON_MAX: usize = 200;
const HOLIDAY_NAME_MAX: usize = 100;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/admin/orders/backstop", get(backstop))
        .route("/admin/orders/{id}/release", post(release))
        .route("/admin/orders/{id}/refund", post(refund))
        .route("/admin/orders/{id}/hold", post(hold))
        .route("/admin/orders/{id}/unhold", post(unhold))
        .route("/admin/payouts", get(payouts))
        .route("/admin/payouts/{id}/mark-paid", post(mark_paid))
        .route("/admin/payouts/{id}/requeue", post(requeue))
        .route("/admin/refunds/{id}/mark-refunded", post(mark_refunded))
        .route("/admin/checkouts", get(checkouts))
        .route("/admin/checkouts/{id}/clear-flag", post(clear_flag))
        .route("/admin/checkouts/{id}/resolve", post(resolve_checkout))
        .route("/admin/reconciliation", get(reconciliation))
        .route("/admin/bank-holidays", get(list_holidays))
        .route(
            "/admin/bank-holidays/{day}",
            put(set_holiday).delete(remove_holiday),
        )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NoteBody {
    note: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HoldBody {
    reason: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OverrideBody {
    note: Option<String>,
    provider_reference: Option<String>,
    /// The owner checked the provider's dashboard: lets an inconclusive
    /// provider lookup proceed (recorded in the audit).
    attest_provider_checked: Option<bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RequeueBody {
    note: Option<String>,
    /// The owner checked the provider's dashboard and attests the old
    /// reference was not paid (only used when the lookup is inconclusive).
    attest_provider_checked: Option<bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HolidayBody {
    name: Option<String>,
}

#[derive(Deserialize)]
struct ListQuery {
    status: Option<String>,
    limit: Option<i64>,
}

/// `admin.viewed` for a list read. Reads are rare and owner-facing, so the
/// audit row is written before the response (a failure fails the read).
async fn viewed(
    state: &AppState,
    user: &crate::auth::AuthUser,
    view: &str,
    rows: usize,
) -> Result<(), AppError> {
    let mut conn = state.db.acquire().await.context("acquire")?;
    views::record_view(&mut conn, user.id, view, rows).await
}

// ------------------------------------------------------------------ orders

async fn backstop(
    State(state): State<AppState>,
    staff: Staff,
    AppQuery(q): AppQuery<ListQuery>,
) -> Result<Json<Items<AdminOrderView>>, AppError> {
    let items = views::backstop(&state.db, views::limit(q.limit)?, staff.is_owner()).await?;
    viewed(&state, &staff.0, "backstop", items.len()).await?;
    Ok(Json(Items { items }))
}

async fn release(
    State(state): State<AppState>,
    Owner(user): Owner,
    Path(id): Path<String>,
    AppJson(body): AppJson<NoteBody>,
) -> Result<Json<AdminOrderView>, AppError> {
    let id = parse_id(&id)?;
    let note = note("note", body.note.as_deref(), NOTE_MAX)?;
    release_by_owner(&state, id, user.id, &note).await?;
    Ok(Json(views::order(&state.db, id).await?))
}

async fn refund(
    State(state): State<AppState>,
    Owner(user): Owner,
    Path(id): Path<String>,
    AppJson(body): AppJson<NoteBody>,
) -> Result<Json<AdminOrderView>, AppError> {
    let id = parse_id(&id)?;
    let note = note("note", body.note.as_deref(), NOTE_MAX)?;
    owner_refund_order(&state, id, user.id, &note).await?;
    Ok(Json(views::order(&state.db, id).await?))
}

async fn hold(
    State(state): State<AppState>,
    Owner(user): Owner,
    Path(id): Path<String>,
    AppJson(body): AppJson<HoldBody>,
) -> Result<Json<AdminOrderView>, AppError> {
    let id = parse_id(&id)?;
    let reason = note("reason", body.reason.as_deref(), HOLD_REASON_MAX)?;
    orders::hold(&state.db, user.id, id, &reason).await?;
    Ok(Json(views::order(&state.db, id).await?))
}

async fn unhold(
    State(state): State<AppState>,
    Owner(user): Owner,
    Path(id): Path<String>,
    AppJson(body): AppJson<NoteBody>,
) -> Result<Json<AdminOrderView>, AppError> {
    let id = parse_id(&id)?;
    let note = note("note", body.note.as_deref(), NOTE_MAX)?;
    orders::unhold(&state.db, user.id, id, &note).await?;
    Ok(Json(views::order(&state.db, id).await?))
}

// ----------------------------------------------------------------- payouts

async fn payouts(
    State(state): State<AppState>,
    Owner(user): Owner,
    AppQuery(q): AppQuery<ListQuery>,
) -> Result<Json<Items<PayoutView>>, AppError> {
    let items = views::payouts(&state.db, q.status.as_deref(), views::limit(q.limit)?).await?;
    viewed(&state, &user, "payouts", items.len()).await?;
    Ok(Json(Items { items }))
}

async fn mark_paid(
    State(state): State<AppState>,
    Owner(user): Owner,
    Path(id): Path<String>,
    AppJson(body): AppJson<OverrideBody>,
) -> Result<Json<PayoutView>, AppError> {
    let id = parse_id(&id)?;
    let note = note("note", body.note.as_deref(), NOTE_MAX)?;
    let reference = provider_reference(body.provider_reference.as_deref())?;
    // The view's `settled_by` says whether the provider had paid it after all.
    let attested = body.attest_provider_checked.unwrap_or(false);
    money::mark_paid(&state, user.id, id, &note, &reference, attested).await?;
    Ok(Json(views::payout(&state.db, id).await?))
}

async fn requeue(
    State(state): State<AppState>,
    Owner(user): Owner,
    Path(id): Path<String>,
    AppJson(body): AppJson<RequeueBody>,
) -> Result<Json<PayoutView>, AppError> {
    let id = parse_id(&id)?;
    let note = note("note", body.note.as_deref(), NOTE_MAX)?;
    let attested = body.attest_provider_checked.unwrap_or(false);
    money::requeue(&state, user.id, id, &note, attested).await?;
    Ok(Json(views::payout(&state.db, id).await?))
}

// ----------------------------------------------------------------- refunds

/// A refund marked refunded, with what the provider was asked.
#[derive(Serialize)]
struct MarkedRefund {
    #[serde(flatten)]
    refund: RefundView,
    provider_attempts: i32,
    /// A send timed out: the provider may have refunded the buyer as well.
    provider_outcome_unknown: bool,
}

async fn mark_refunded(
    State(state): State<AppState>,
    Owner(user): Owner,
    Path(id): Path<String>,
    AppJson(body): AppJson<OverrideBody>,
) -> Result<Json<MarkedRefund>, AppError> {
    let id = parse_id(&id)?;
    let note = note("note", body.note.as_deref(), NOTE_MAX)?;
    let reference = provider_reference(body.provider_reference.as_deref())?;
    let marked = money::mark_refunded(&state.db, user.id, id, &note, &reference).await?;
    Ok(Json(MarkedRefund {
        refund: views::refund(&state.db, id).await?,
        provider_attempts: marked.provider_attempts,
        provider_outcome_unknown: marked.provider_outcome_unknown,
    }))
}

// --------------------------------------------------------------- checkouts

async fn checkouts(
    State(state): State<AppState>,
    Owner(user): Owner,
    AppQuery(q): AppQuery<ListQuery>,
) -> Result<Json<Items<CheckoutView>>, AppError> {
    let which = q.status.as_deref().unwrap_or("");
    let items = views::checkouts(&state.db, which, views::limit(q.limit)?).await?;
    viewed(&state, &user, "checkouts", items.len()).await?;
    Ok(Json(Items { items }))
}

async fn clear_flag(
    State(state): State<AppState>,
    Owner(user): Owner,
    Path(id): Path<String>,
    AppJson(body): AppJson<NoteBody>,
) -> Result<Json<CheckoutView>, AppError> {
    let id = parse_id(&id)?;
    let note = note("note", body.note.as_deref(), NOTE_MAX)?;
    money::clear_flag(&state.db, user.id, id, &note).await?;
    Ok(Json(views::checkout(&state.db, id).await?))
}

/// Records the owner's own handling of a mismatched or flagged checkout; moves
/// no money (see [`money::resolve_checkout`]).
async fn resolve_checkout(
    State(state): State<AppState>,
    Owner(user): Owner,
    Path(id): Path<String>,
    AppJson(body): AppJson<OverrideBody>,
) -> Result<Json<CheckoutView>, AppError> {
    let id = parse_id(&id)?;
    let note = note("note", body.note.as_deref(), NOTE_MAX)?;
    let reference = provider_reference(body.provider_reference.as_deref())?;
    money::resolve_checkout(&state.db, user.id, id, &note, &reference).await?;
    Ok(Json(views::checkout(&state.db, id).await?))
}

async fn reconciliation(
    State(state): State<AppState>,
    staff: Staff,
) -> Result<Json<Reconciliation>, AppError> {
    let r = views::reconciliation(&state.db, staff.is_owner()).await?;
    let rows = r.payouts.len()
        + r.refunds.len()
        + r.flagged_checkouts.len()
        + r.mismatched_checkouts.len()
        + r.late_provider_outcomes.len()
        + r.payout_amount_mismatches.len();
    viewed(&state, &staff.0, "reconciliation", rows).await?;
    Ok(Json(r))
}

// ----------------------------------------------------------- bank holidays

async fn list_holidays(
    State(state): State<AppState>,
    staff: Staff,
) -> Result<Json<Items<holidays::Holiday>>, AppError> {
    let items = holidays::list(&state.db).await?;
    viewed(&state, &staff.0, "bank_holidays", items.len()).await?;
    Ok(Json(Items { items }))
}

async fn set_holiday(
    State(state): State<AppState>,
    Owner(user): Owner,
    Path(day): Path<String>,
    AppJson(body): AppJson<HolidayBody>,
) -> Result<Json<holidays::Holiday>, AppError> {
    let day = holidays::parse_day(&day)?;
    let name = note("name", body.name.as_deref(), HOLIDAY_NAME_MAX)?;
    Ok(Json(holidays::set(&state.db, user.id, day, &name).await?))
}

async fn remove_holiday(
    State(state): State<AppState>,
    Owner(user): Owner,
    Path(day): Path<String>,
) -> Result<StatusCode, AppError> {
    let day = holidays::parse_day(&day)?;
    holidays::remove(&state.db, user.id, day).await?;
    Ok(StatusCode::NO_CONTENT)
}
