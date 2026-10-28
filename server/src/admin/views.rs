//! Admin read models and lists. Personal data: ids, amounts, statuses, our
//! own references and the last four digits of the account paid. Never an
//! account number, an account name or any buyer data. Support sees neither
//! the bank code nor any free text the owner wrote (hold reasons; notes are
//! only ever in the audit log): those fields are left out of its responses.
//! Every list read is audited (`admin.viewed`).

use anyhow::Context;
use serde::Serialize;
use serde_json::json;
use sqlx::{PgConnection, PgPool};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{audit, error::AppError};

/// Rows per list (each reconciliation section too).
pub const MAX_LIMIT: i64 = 100;
pub const DEFAULT_LIMIT: i64 = 50;
/// Payout statuses a list may filter on (`payout_status` enum).
const PAYOUT_STATUSES: &[&str] = &[
    "scheduled",
    "sending",
    "pending",
    "unknown",
    "successful",
    "failed",
    "manual",
];

pub fn limit(given: Option<i64>) -> Result<i64, AppError> {
    let limit = given.unwrap_or(DEFAULT_LIMIT);
    if !(1..=MAX_LIMIT).contains(&limit) {
        return Err(AppError::Validation(format!(
            "limit must be between 1 and {MAX_LIMIT}"
        )));
    }
    Ok(limit)
}

#[derive(Debug, Serialize)]
pub struct Items<T> {
    pub items: Vec<T>,
}

/// An order as the owner sees it (no buyer data).
#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct AdminOrderView {
    pub order_id: Uuid,
    pub store_id: Uuid,
    pub status: String,
    pub items_total_kobo: i64,
    pub delivery_fee_kobo: i64,
    pub held: bool,
    /// Owner only (None, and left out, for Support): free text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hold_reason: Option<String>,
    pub provider_flagged: bool,
    pub payout_status: Option<String>,
    pub refund_status: Option<String>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub paid_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub backstop_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub backstop_flagged_at: Option<OffsetDateTime>,
}

const ORDER_SELECT: &str = "SELECT o.id AS order_id, o.store_id, o.status::text AS status, \
        o.items_total_kobo, o.delivery_fee_kobo, o.held_at IS NOT NULL AS held, o.hold_reason, \
        c.provider_flagged, p.status::text AS payout_status, r.status::text AS refund_status, \
        o.paid_at, o.backstop_at, o.backstop_flagged_at \
    FROM orders o JOIN checkouts c ON c.id = o.checkout_id \
         LEFT JOIN payouts p ON p.order_id = o.id LEFT JOIN refunds r ON r.order_id = o.id";

pub async fn order(db: &PgPool, order_id: Uuid) -> Result<AdminOrderView, AppError> {
    sqlx::query_as::<_, AdminOrderView>(&format!("{ORDER_SELECT} WHERE o.id = $1"))
        .bind(order_id)
        .fetch_optional(db)
        .await
        .context("load admin order")?
        .ok_or(AppError::NotFound)
}

/// Orders the backstop job flagged (unreleased 30 days after payment) that are
/// still unfinished, oldest flag first. `owner`: false hides hold reasons.
pub async fn backstop(
    db: &PgPool,
    limit: i64,
    owner: bool,
) -> Result<Vec<AdminOrderView>, AppError> {
    let mut items = sqlx::query_as::<_, AdminOrderView>(&format!(
        "{ORDER_SELECT} WHERE o.backstop_flagged_at IS NOT NULL \
           AND o.status NOT IN ('completed', 'refunded', 'cancelled') \
         ORDER BY o.backstop_flagged_at, o.id LIMIT $1"
    ))
    .bind(limit)
    .fetch_all(db)
    .await
    .context("list backstop orders")?;
    if !owner {
        for o in &mut items {
            o.hold_reason = None;
        }
    }
    Ok(items)
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct PayoutView {
    pub id: Uuid,
    pub order_id: Uuid,
    pub store_id: Uuid,
    pub reference: String,
    pub status: String,
    pub amount_kobo: i64,
    pub fee_kobo: i64,
    pub attempts: i32,
    pub last_error: Option<String>,
    /// Owner only (None, and left out, for Support).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bank_code: Option<String>,
    pub account_last4: Option<String>,
    pub round: i32,
    pub reversed: bool,
    pub settled_by: Option<String>,
    pub release_reason: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

const PAYOUT_SELECT: &str = "SELECT p.id, p.order_id, o.store_id, p.reference, \
        p.status::text AS status, p.amount_kobo, p.fee_kobo, p.attempts, p.last_error, \
        p.bank_code, p.account_last4, p.round, p.reversed_at IS NOT NULL AS reversed, \
        p.settled_by, p.release_reason, p.created_at, p.updated_at \
    FROM payouts p JOIN orders o ON o.id = p.order_id";

pub async fn payout(db: &PgPool, payout_id: Uuid) -> Result<PayoutView, AppError> {
    sqlx::query_as::<_, PayoutView>(&format!("{PAYOUT_SELECT} WHERE p.id = $1"))
        .bind(payout_id)
        .fetch_optional(db)
        .await
        .context("load payout")?
        .ok_or(AppError::NotFound)
}

/// Payouts in `status` (any when None), oldest change first.
pub async fn payouts(
    db: &PgPool,
    status: Option<&str>,
    limit: i64,
) -> Result<Vec<PayoutView>, AppError> {
    if let Some(s) = status
        && !PAYOUT_STATUSES.contains(&s)
    {
        return Err(AppError::Validation("status is not a payout status".into()));
    }
    Ok(sqlx::query_as::<_, PayoutView>(&format!(
        "{PAYOUT_SELECT} WHERE ($1::text IS NULL OR p.status::text = $1) \
         ORDER BY p.updated_at, p.id LIMIT $2"
    ))
    .bind(status)
    .bind(limit)
    .fetch_all(db)
    .await
    .context("list payouts")?)
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct RefundView {
    pub id: Uuid,
    pub order_id: Uuid,
    pub reference: String,
    pub status: String,
    pub amount_kobo: i64,
    pub attempts: i32,
    pub last_error: Option<String>,
    pub settled_by: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

const REFUND_SELECT: &str = "SELECT id, order_id, reference, status::text AS status, \
        amount_kobo, attempts, last_error, settled_by, created_at, updated_at FROM refunds";

pub async fn refund(db: &PgPool, refund_id: Uuid) -> Result<RefundView, AppError> {
    sqlx::query_as::<_, RefundView>(&format!("{REFUND_SELECT} WHERE id = $1"))
        .bind(refund_id)
        .fetch_optional(db)
        .await
        .context("load refund")?
        .ok_or(AppError::NotFound)
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct CheckoutView {
    pub id: Uuid,
    pub reference: String,
    pub status: String,
    pub provider: String,
    pub total_kobo: i64,
    pub collection_fee_kobo: Option<i64>,
    pub provider_flagged: bool,
    /// The owner recorded handling it outside the system.
    pub resolved: bool,
    #[serde(with = "time::serde::rfc3339::option")]
    pub paid_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

const CHECKOUT_SELECT: &str = "SELECT id, reference, status::text AS status, provider, \
        total_kobo, collection_fee_kobo, provider_flagged, resolved_at IS NOT NULL AS resolved, \
        paid_at, created_at FROM checkouts";

pub async fn checkout(db: &PgPool, checkout_id: Uuid) -> Result<CheckoutView, AppError> {
    sqlx::query_as::<_, CheckoutView>(&format!("{CHECKOUT_SELECT} WHERE id = $1"))
        .bind(checkout_id)
        .fetch_optional(db)
        .await
        .context("load checkout")?
        .ok_or(AppError::NotFound)
}

/// Checkouts needing the owner: `amount_mismatch` (paid the wrong amount,
/// to refund by hand) or `flagged` (the provider held the payment), not yet
/// resolved by the owner.
pub async fn checkouts(
    db: &PgPool,
    which: &str,
    limit: i64,
) -> Result<Vec<CheckoutView>, AppError> {
    let filter = match which {
        "amount_mismatch" => {
            "status = 'amount_mismatch' AND resolved_at IS NULL ORDER BY created_at, id"
        }
        "flagged" => "provider_flagged AND resolved_at IS NULL ORDER BY paid_at, id",
        _ => {
            return Err(AppError::Validation(
                "status must be amount_mismatch or flagged".into(),
            ));
        }
    };
    Ok(
        sqlx::query_as::<_, CheckoutView>(&format!("{CHECKOUT_SELECT} WHERE {filter} LIMIT $1"))
            .bind(limit)
            .fetch_all(db)
            .await
            .context("list checkouts")?,
    )
}

#[derive(Debug, Serialize)]
pub struct Reconciliation {
    /// `unknown`, `manual` and `failed` payouts.
    pub payouts: Vec<PayoutView>,
    /// `manual` and `failed` refunds.
    pub refunds: Vec<RefundView>,
    pub flagged_checkouts: Vec<CheckoutView>,
    pub mismatched_checkouts: Vec<CheckoutView>,
    /// Provider outcomes on references that no longer take them (a payout
    /// the owner settled by hand, a reversed attempt re-queued under a new
    /// reference): a success there may be a double payment. The last
    /// [`ALERT_WINDOW_DAYS`] days, newest first.
    pub late_provider_outcomes: Vec<LateProviderOutcome>,
    /// Payouts the provider reported paying a different amount for. The last
    /// [`ALERT_WINDOW_DAYS`] days, newest first.
    pub payout_amount_mismatches: Vec<PayoutAmountMismatch>,
    /// Rows per section; a full section has more behind it.
    pub limit: i64,
}

/// How far back the audit-log alert sections of [`Reconciliation`] look.
/// The alerts have no acknowledgement: they age out instead.
pub const ALERT_WINDOW_DAYS: i32 = 30;

/// A `payout.provider_outcome_after_close` alert.
#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct LateProviderOutcome {
    pub payout_id: Option<Uuid>,
    /// `owner_settled` or `retired_attempt`.
    pub reference_kind: Option<String>,
    /// `successful`, `reversed` or `unconfirmed`.
    pub outcome: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
}

/// A `payout.amount_mismatch` alert.
#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct PayoutAmountMismatch {
    pub payout_id: Option<Uuid>,
    pub expected_kobo: Option<i64>,
    pub reported_kobo: Option<i64>,
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
}

/// Everything waiting on the owner, oldest first, [`MAX_LIMIT`] per section.
pub async fn reconciliation(db: &PgPool, owner: bool) -> Result<Reconciliation, AppError> {
    let mut payouts = sqlx::query_as::<_, PayoutView>(&format!(
        "{PAYOUT_SELECT} WHERE p.status IN ('unknown', 'manual', 'failed') \
         ORDER BY p.updated_at, p.id LIMIT $1"
    ))
    .bind(MAX_LIMIT)
    .fetch_all(db)
    .await
    .context("list attention payouts")?;
    if !owner {
        for p in &mut payouts {
            p.bank_code = None;
        }
    }
    let refunds = sqlx::query_as::<_, RefundView>(&format!(
        "{REFUND_SELECT} WHERE status IN ('manual', 'failed') ORDER BY updated_at, id LIMIT $1"
    ))
    .bind(MAX_LIMIT)
    .fetch_all(db)
    .await
    .context("list attention refunds")?;
    let late_provider_outcomes = sqlx::query_as::<_, LateProviderOutcome>(
        "SELECT subject_id AS payout_id, detail->>'reference_kind' AS reference_kind, \
                detail->>'outcome' AS outcome, at \
         FROM audit_log \
         WHERE action = 'payout.provider_outcome_after_close' \
           AND at > now() - make_interval(days => $2) \
         ORDER BY at DESC, id DESC LIMIT $1",
    )
    .bind(MAX_LIMIT)
    .bind(ALERT_WINDOW_DAYS)
    .fetch_all(db)
    .await
    .context("list late provider outcomes")?;
    let payout_amount_mismatches = sqlx::query_as::<_, PayoutAmountMismatch>(
        "SELECT subject_id AS payout_id, (detail->>'expected_kobo')::bigint AS expected_kobo, \
                (detail->>'reported_kobo')::bigint AS reported_kobo, at \
         FROM audit_log \
         WHERE action = 'payout.amount_mismatch' \
           AND at > now() - make_interval(days => $2) \
         ORDER BY at DESC, id DESC LIMIT $1",
    )
    .bind(MAX_LIMIT)
    .bind(ALERT_WINDOW_DAYS)
    .fetch_all(db)
    .await
    .context("list payout amount mismatches")?;
    Ok(Reconciliation {
        payouts,
        refunds,
        flagged_checkouts: checkouts(db, "flagged", MAX_LIMIT).await?,
        mismatched_checkouts: checkouts(db, "amount_mismatch", MAX_LIMIT).await?,
        late_provider_outcomes,
        payout_amount_mismatches,
        limit: MAX_LIMIT,
    })
}

/// Audit row for an admin read (`admin.viewed`): which view and how many rows.
pub async fn record_view(
    conn: &mut PgConnection,
    actor: Uuid,
    view: &str,
    rows: usize,
) -> Result<(), AppError> {
    audit::record(
        conn,
        Some(actor),
        "admin.viewed",
        "admin_view",
        None,
        json!({ "view": view, "rows": rows }),
    )
    .await
}
