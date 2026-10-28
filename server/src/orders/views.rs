//! Read models for orders. Two audiences, two structs, each an explicit
//! allowlist of fields (never serialise a database row):
//!
//! - [`BuyerOrderView`]: items, totals, status, timestamps, held flag. No
//!   payout data, and no platform cut (as at checkout).
//! - [`SellerOrderView`]: the same plus the platform cut, the buyer's display
//!   name only (no id, phone or address), the expected payout and the
//!   payout's status. Refund mechanics are hidden behind a coarse `cancelled`
//!   status. The expected payout uses the fee schedule snapshotted on the
//!   order when it was paid (the live one only for orders paid before the
//!   snapshot existed), is never below 0, and is left out once the order is
//!   cancelled or refunded.
//!
//! Every query puts the principal in the SQL, so a missing order and someone
//! else's order are indistinguishable (both `NotFound`).
//!
//! **Staff reads.** Owner and Support may read any paid order through
//! [`get_for_viewer`]; when the order is not their own (as buyer or store
//! owner), the read writes an `admin.viewed` audit row (`view: "order"`), like
//! every admin list. Support gets the seller view without the buyer's display
//! name; the Owner keeps it (pending DPO review).

use std::collections::HashMap;

use anyhow::Context;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::PgPool;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use uuid::Uuid;

use crate::{
    audit,
    error::AppError,
    money::{self, FeeSchedule},
    settings,
};

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
    pub platform_cut_kobo: i64,
    /// Left out for Support (see the module docs).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub buyer_display_name: Option<String>,
    /// Left out once the order is cancelled or refunded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_payout_kobo: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payout: Option<PayoutView>,
}

/// Staff reading an order through [`get_for_viewer`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StaffRole {
    Owner,
    Support,
}

/// Which view `build` makes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Audience {
    Buyer,
    /// The seller view; `false` leaves out the buyer's display name.
    Seller { buyer_name: bool },
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
    store_owner_id: Uuid,
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
    fee_small_kobo: Option<i64>,
    fee_large_kobo: Option<i64>,
    fee_threshold_kobo: Option<i64>,
    stamp_duty_kobo: Option<i64>,
    stamp_duty_threshold_kobo: Option<i64>,
}

impl Row {
    /// The payout fee schedule snapshotted when the order was paid, if any.
    fn fee_snapshot(&self) -> Option<FeeSchedule> {
        Some(FeeSchedule {
            small_fee: self.fee_small_kobo?,
            large_fee: self.fee_large_kobo?,
            threshold: self.fee_threshold_kobo?,
            stamp_duty_kobo: self.stamp_duty_kobo?,
            stamp_duty_threshold: self.stamp_duty_threshold_kobo?,
        })
    }

    /// What the seller should expect to be paid (see the module docs): the
    /// payout's own amount once released, else the sum release makes. None
    /// once the order is cancelled or refunded.
    fn expected_payout(&self, live: Option<&FeeSchedule>) -> Option<i64> {
        if matches!(
            self.status.as_str(),
            "cancelled" | "refund_pending" | "refunded"
        ) {
            return None;
        }
        if let Some(amount) = self.payout_amount_kobo {
            return Some(amount.max(0));
        }
        let snapshot = self.fee_snapshot();
        let schedule = snapshot.as_ref().or(live)?;
        let before_fee = self
            .items_total_kobo
            .saturating_sub(self.platform_cut_kobo)
            .saturating_sub(self.collection_fee_share_kobo);
        Some(
            before_fee
                .saturating_sub(money::payout_fee_kobo(before_fee, schedule))
                .max(0),
        )
    }
}

const SELECT: &str = "SELECT o.id, o.store_id, o.buyer_id, s.owner_id AS store_owner_id, \
        o.status::text AS status, \
        o.items_total_kobo, o.delivery_fee_kobo, o.platform_cut_kobo, \
        o.collection_fee_share_kobo, (o.held_at IS NOT NULL) AS held, \
        COALESCE(o.paid_at, c.paid_at) AS paid_at, o.ship_by, o.delivered_at, o.release_at, \
        o.created_at, u.display_name AS buyer_display_name, \
        p.status::text AS payout_status, p.amount_kobo AS payout_amount_kobo, \
        o.fee_small_kobo, o.fee_large_kobo, o.fee_threshold_kobo, \
        o.stamp_duty_kobo, o.stamp_duty_threshold_kobo \
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
        held: r.held,
        paid_at: r.paid_at,
        ship_by: r.ship_by,
        delivered_at: r.delivered_at,
        release_at: r.release_at,
        created_at: r.created_at,
    })
}

async fn build(
    db: &PgPool,
    rows: Vec<Row>,
    audience: Audience,
) -> Result<Vec<OrderView>, AppError> {
    let ids: Vec<Uuid> = rows.iter().map(|r| r.id).collect();
    let mut items = items_for(db, &ids).await?;
    let Audience::Seller { buyer_name } = audience else {
        return rows
            .into_iter()
            .map(|r| {
                let order = core(&r, items.remove(&r.id).unwrap_or_default(), false)?;
                Ok(OrderView::Buyer(BuyerOrderView { order }))
            })
            .collect();
    };
    // The live schedule only for orders paid before the snapshot existed.
    let live = if rows.iter().any(|r| r.fee_snapshot().is_none()) {
        Some(settings::load(db).await?.fee_schedule())
    } else {
        None
    };
    rows.into_iter()
        .map(|r| {
            let order = core(&r, items.remove(&r.id).unwrap_or_default(), true)?;
            Ok(OrderView::Seller(SellerOrderView {
                order,
                platform_cut_kobo: r.platform_cut_kobo,
                expected_payout_kobo: r.expected_payout(live.as_ref()),
                buyer_display_name: buyer_name.then_some(r.buyer_display_name),
                payout: r.payout_status.map(|status| PayoutView { status }),
            }))
        })
        .collect()
}

async fn page(
    db: &PgPool,
    mut rows: Vec<Row>,
    limit: i64,
    audience: Audience,
) -> Result<Page<OrderView>, AppError> {
    let more = rows.len() as i64 > limit;
    rows.truncate(usize::try_from(limit).unwrap_or(0));
    let items = build(db, rows, audience).await?;
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
    page(db, rows, limit, Audience::Buyer).await
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
    page(db, rows, limit, Audience::Seller { buyer_name: true }).await
}

/// One order for `viewer`: the buyer sees the buyer view; the store's owner and
/// owner/support staff (`staff`) see the seller view. The principal is part of
/// the query, so a missing order and someone else's order are both
/// `NotFound`. A staff read of an order that is not the viewer's own is
/// audited (`admin.viewed`, written before the response, so a failure fails
/// the read), and Support gets no buyer display name (see the module docs).
pub async fn get_for_viewer(
    db: &PgPool,
    viewer: Uuid,
    staff: Option<StaffRole>,
    order_id: Uuid,
) -> Result<OrderView, AppError> {
    let row: Option<Row> = sqlx::query_as(&format!(
        "{SELECT} WHERE o.id = $1 AND (o.buyer_id = $2 OR \
         ((s.owner_id = $2 OR $3) AND o.status <> 'pending_payment'))"
    ))
    .bind(order_id)
    .bind(viewer)
    .bind(staff.is_some())
    .fetch_optional(db)
    .await
    .context("load order")?;
    let row = row.ok_or(AppError::NotFound)?;
    let audience = if row.buyer_id == viewer {
        Audience::Buyer
    } else if row.store_owner_id == viewer {
        Audience::Seller { buyer_name: true }
    } else {
        // Only staff get here: the query admits no one else.
        let mut conn = db.acquire().await.context("acquire")?;
        audit::record(
            &mut conn,
            Some(viewer),
            "admin.viewed",
            "order",
            Some(order_id),
            json!({ "view": "order" }),
        )
        .await?;
        Audience::Seller {
            buyer_name: staff == Some(StaffRole::Owner),
        }
    };
    build(db, vec![row], audience)
        .await?
        .pop()
        .ok_or(AppError::NotFound)
}
