//! Read models for orders. Two audiences, two structs, each an explicit
//! allowlist of fields (never serialise a database row):
//!
//! - [`BuyerOrderView`]: items, totals, cut, status, timestamps, held flag. No
//!   payout data.
//! - [`SellerOrderView`]: the same plus the buyer's display name only (no id,
//!   phone or address), the expected payout and the payout's status. Refund
//!   mechanics are hidden behind a coarse `cancelled` status.
//!
//! Every query puts the principal in the SQL, so a missing order and someone
//! else's order are indistinguishable (both `NotFound`).

use std::collections::HashMap;

use anyhow::Context;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::Serialize;
use serde_json::Value;
use sqlx::PgPool;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use uuid::Uuid;

use crate::{error::AppError, money, settings};

pub const DEFAULT_LIMIT: i64 = 20;
pub const MAX_LIMIT: i64 = 100;

#[derive(Debug, Serialize)]
pub struct ItemView {
    pub product_name: String,
    pub attributes: Value,
    pub unit_price_kobo: i64,
    pub quantity: i32,
}

#[derive(Debug, Serialize)]
pub struct PayoutView {
    pub status: String,
    pub amount_kobo: i64,
}

/// Fields common to both audiences.
#[derive(Debug, Serialize)]
pub struct OrderCore {
    pub id: Uuid,
    pub store_id: Uuid,
    pub status: String,
    pub items: Vec<ItemView>,
    pub items_total_kobo: i64,
    pub delivery_fee_kobo: i64,
    pub total_kobo: i64,
    pub platform_cut_kobo: i64,
    pub held: bool,
    #[serde(with = "time::serde::rfc3339::option")]
    pub paid_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub ship_by: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub delivered_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub release_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

#[derive(Debug, Serialize)]
pub struct BuyerOrderView {
    #[serde(flatten)]
    pub order: OrderCore,
}

#[derive(Debug, Serialize)]
pub struct SellerOrderView {
    #[serde(flatten)]
    pub order: OrderCore,
    pub buyer_display_name: String,
    pub expected_payout_kobo: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payout: Option<PayoutView>,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum OrderView {
    Buyer(BuyerOrderView),
    Seller(SellerOrderView),
}

impl OrderView {
    fn key(&self) -> (OffsetDateTime, Uuid) {
        let o = match self {
            OrderView::Buyer(b) => &b.order,
            OrderView::Seller(s) => &s.order,
        };
        (o.created_at, o.id)
    }
}

#[derive(Debug, Serialize)]
pub struct Page<T> {
    pub items: Vec<T>,
    /// Pass as `?before=` for the next page; null on the last page.
    pub next_cursor: Option<String>,
}

/// Keyset cursor `(created_at, id)`, opaque to clients.
struct Cursor(OffsetDateTime, Uuid);

impl Cursor {
    fn encode(&self) -> Result<String, AppError> {
        let ts = self.0.format(&Rfc3339).context("format cursor")?;
        Ok(URL_SAFE_NO_PAD.encode(format!("{ts}|{}", self.1)))
    }

    fn decode(s: &str) -> Result<Self, AppError> {
        let bad = || AppError::Validation("before is not a valid cursor".into());
        let raw = URL_SAFE_NO_PAD.decode(s).map_err(|_| bad())?;
        let raw = String::from_utf8(raw).map_err(|_| bad())?;
        let (ts, id) = raw.split_once('|').ok_or_else(bad)?;
        Ok(Cursor(
            OffsetDateTime::parse(ts, &Rfc3339).map_err(|_| bad())?,
            id.parse().map_err(|_| bad())?,
        ))
    }
}

#[derive(sqlx::FromRow)]
struct Row {
    id: Uuid,
    store_id: Uuid,
    buyer_id: Uuid,
    status: String,
    items_total_kobo: i64,
    delivery_fee_kobo: i64,
    platform_cut_kobo: i64,
    collection_fee_share_kobo: i64,
    held: bool,
    paid_at: Option<OffsetDateTime>,
    ship_by: Option<OffsetDateTime>,
    delivered_at: Option<OffsetDateTime>,
    release_at: Option<OffsetDateTime>,
    created_at: OffsetDateTime,
    buyer_display_name: String,
    payout_status: Option<String>,
    payout_amount_kobo: Option<i64>,
}

const SELECT: &str = "SELECT o.id, o.store_id, o.buyer_id, o.status::text AS status, \
        o.items_total_kobo, o.delivery_fee_kobo, o.platform_cut_kobo, \
        o.collection_fee_share_kobo, (o.held_at IS NOT NULL) AS held, \
        COALESCE(o.paid_at, c.paid_at) AS paid_at, o.ship_by, o.delivered_at, o.release_at, \
        o.created_at, u.display_name AS buyer_display_name, \
        p.status::text AS payout_status, p.amount_kobo AS payout_amount_kobo \
     FROM orders o \
     JOIN checkouts c ON c.id = o.checkout_id \
     JOIN users u ON u.id = o.buyer_id \
     JOIN stores s ON s.id = o.store_id \
     LEFT JOIN payouts p ON p.order_id = o.id";

async fn items_for(db: &PgPool, ids: &[Uuid]) -> Result<HashMap<Uuid, Vec<ItemView>>, AppError> {
    let rows: Vec<(Uuid, String, Value, i64, i32)> = sqlx::query_as(
        "SELECT order_id, product_name, attributes, unit_price_kobo, quantity \
         FROM order_items WHERE order_id = ANY($1) ORDER BY order_id, product_name, variant_id",
    )
    .bind(ids)
    .fetch_all(db)
    .await
    .context("load order items")?;
    let mut map: HashMap<Uuid, Vec<ItemView>> = HashMap::new();
    for (order_id, product_name, attributes, unit_price_kobo, quantity) in rows {
        map.entry(order_id).or_default().push(ItemView {
            product_name,
            attributes,
            unit_price_kobo,
            quantity,
        });
    }
    Ok(map)
}

fn core(r: &Row, items: Vec<ItemView>, seller: bool) -> Result<OrderCore, AppError> {
    let total = r
        .items_total_kobo
        .checked_add(r.delivery_fee_kobo)
        .context("order total overflow")?;
    // Sellers get a coarse status: refund mechanics stay internal.
    let status = match (seller, r.status.as_str()) {
        (true, "refund_pending" | "refunded") => "cancelled".to_string(),
        _ => r.status.clone(),
    };
    Ok(OrderCore {
        id: r.id,
        store_id: r.store_id,
        status,
        items,
        items_total_kobo: r.items_total_kobo,
        delivery_fee_kobo: r.delivery_fee_kobo,
        total_kobo: total,
        platform_cut_kobo: r.platform_cut_kobo,
        held: r.held,
        paid_at: r.paid_at,
        ship_by: r.ship_by,
        delivered_at: r.delivered_at,
        release_at: r.release_at,
        created_at: r.created_at,
    })
}

async fn build(db: &PgPool, rows: Vec<Row>, seller: bool) -> Result<Vec<OrderView>, AppError> {
    let ids: Vec<Uuid> = rows.iter().map(|r| r.id).collect();
    let mut items = items_for(db, &ids).await?;
    let schedule = if seller {
        Some(settings::load(db).await?.fee_schedule())
    } else {
        None
    };
    rows.into_iter()
        .map(|r| {
            let order = core(&r, items.remove(&r.id).unwrap_or_default(), seller)?;
            let Some(schedule) = &schedule else {
                return Ok(OrderView::Buyer(BuyerOrderView { order }));
            };
            let expected = match r.payout_amount_kobo {
                Some(amount) => amount,
                None => {
                    let before_fee = r
                        .items_total_kobo
                        .saturating_sub(r.platform_cut_kobo)
                        .saturating_sub(r.collection_fee_share_kobo);
                    before_fee - money::payout_fee_kobo(before_fee, schedule)
                }
            };
            Ok(OrderView::Seller(SellerOrderView {
                order,
                buyer_display_name: r.buyer_display_name,
                expected_payout_kobo: expected,
                payout: r.payout_status.map(|status| PayoutView {
                    status,
                    amount_kobo: expected,
                }),
            }))
        })
        .collect()
}

async fn page(
    db: &PgPool,
    mut rows: Vec<Row>,
    limit: i64,
    seller: bool,
) -> Result<Page<OrderView>, AppError> {
    let more = rows.len() as i64 > limit;
    rows.truncate(usize::try_from(limit).unwrap_or(0));
    let items = build(db, rows, seller).await?;
    let next_cursor = match (more, items.last()) {
        (true, Some(last)) => {
            let (ts, id) = last.key();
            Some(Cursor(ts, id).encode()?)
        }
        _ => None,
    };
    Ok(Page { items, next_cursor })
}

/// The buyer's own orders, newest first, keyset-paged on `(created_at, id)`.
pub async fn list_for_buyer(
    db: &PgPool,
    buyer: Uuid,
    limit: i64,
    before: Option<&str>,
) -> Result<Page<OrderView>, AppError> {
    let cur = before.map(Cursor::decode).transpose()?;
    let rows: Vec<Row> = sqlx::query_as(&format!(
        "{SELECT} WHERE o.buyer_id = $1 \
         AND ($2::timestamptz IS NULL OR (o.created_at, o.id) < ($2, $3::uuid)) \
         ORDER BY o.created_at DESC, o.id DESC LIMIT $4"
    ))
    .bind(buyer)
    .bind(cur.as_ref().map(|c| c.0))
    .bind(cur.as_ref().map(|c| c.1))
    .bind(limit + 1)
    .fetch_all(db)
    .await
    .context("list buyer orders")?;
    page(db, rows, limit, false).await
}

/// A store's orders. The owner is part of the query: a store that is missing
/// or not `owner`'s yields `NotFound`.
pub async fn list_for_store(
    db: &PgPool,
    owner: Uuid,
    store_id: Uuid,
    limit: i64,
    before: Option<&str>,
) -> Result<Page<OrderView>, AppError> {
    let cur = before.map(Cursor::decode).transpose()?;
    let owned: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM stores WHERE id = $1 AND owner_id = $2 AND deleted_at IS NULL",
    )
    .bind(store_id)
    .bind(owner)
    .fetch_optional(db)
    .await
    .context("check store owner")?;
    owned.ok_or(AppError::NotFound)?;
    let rows: Vec<Row> = sqlx::query_as(&format!(
        "{SELECT} WHERE o.store_id = $1 AND s.owner_id = $5 \
         AND o.status <> 'pending_payment' \
         AND ($2::timestamptz IS NULL OR (o.created_at, o.id) < ($2, $3::uuid)) \
         ORDER BY o.created_at DESC, o.id DESC LIMIT $4"
    ))
    .bind(store_id)
    .bind(cur.as_ref().map(|c| c.0))
    .bind(cur.as_ref().map(|c| c.1))
    .bind(limit + 1)
    .bind(owner)
    .fetch_all(db)
    .await
    .context("list store orders")?;
    page(db, rows, limit, true).await
}

/// One order for `viewer`: the buyer sees the buyer view; the store's owner and
/// owner/support admins see the seller view. The principal is part of the
/// query, so a missing order and someone else's order are both `NotFound`.
pub async fn get_for_viewer(
    db: &PgPool,
    viewer: Uuid,
    is_admin: bool,
    order_id: Uuid,
) -> Result<OrderView, AppError> {
    let row: Option<Row> = sqlx::query_as(&format!(
        "{SELECT} WHERE o.id = $1 AND (o.buyer_id = $2 OR s.owner_id = $2 OR $3)"
    ))
    .bind(order_id)
    .bind(viewer)
    .bind(is_admin)
    .fetch_optional(db)
    .await
    .context("load order")?;
    let row = row.ok_or(AppError::NotFound)?;
    let seller = row.buyer_id != viewer;
    build(db, vec![row], seller)
        .await?
        .pop()
        .ok_or(AppError::NotFound)
}
