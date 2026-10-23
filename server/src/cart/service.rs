//! The buyer's cart: variant ids and quantities only. Prices shown here are a
//! preview; checkout recomputes everything from the locked variant rows.

use anyhow::Context;
use serde::Serialize;
use sqlx::PgPool;
use uuid::Uuid;

use crate::{error::AppError, limits::MAX_LINE_QUANTITY, money::checked_sum, products};

/// Most distinct lines one cart may hold.
pub const MAX_CART_LINES: i64 = 50;

/// SQL condition (over `p` products and `s` stores) for an item a buyer may buy.
pub(crate) const BUYABLE: &str = "p.deleted_at IS NULL AND p.is_published \
     AND s.deleted_at IS NULL AND s.status = 'active'";

#[derive(Debug, Serialize)]
pub struct CartLine {
    pub variant_id: Uuid,
    pub product_id: Uuid,
    pub product_name: String,
    pub attributes: serde_json::Value,
    pub store_id: Uuid,
    pub store_name: String,
    pub unit_price_kobo: i64,
    pub quantity: i32,
    pub line_total_kobo: i64,
    /// False when the product was unpublished or its store is not active; such
    /// lines stay in the cart but are left out of `total_kobo` and block checkout.
    pub available: bool,
}

#[derive(Debug, Serialize)]
pub struct Cart {
    pub items: Vec<CartLine>,
    /// Preview total of the available lines. The checkout response is authoritative.
    pub total_kobo: i64,
}

#[derive(sqlx::FromRow)]
struct LineRow {
    variant_id: Uuid,
    product_id: Uuid,
    product_name: String,
    attributes: serde_json::Value,
    store_id: Uuid,
    store_name: String,
    base_price_kobo: i64,
    price_kobo: Option<i64>,
    quantity: i32,
    available: bool,
}

fn too_large() -> AppError {
    AppError::Unprocessable("amount_too_large", "amount is too large".into())
}

pub async fn get(db: &PgPool, user: Uuid) -> Result<Cart, AppError> {
    let rows: Vec<LineRow> = sqlx::query_as(&format!(
        "SELECT c.variant_id, p.id AS product_id, p.name AS product_name, v.attributes, \
                s.id AS store_id, s.name AS store_name, p.base_price_kobo, v.price_kobo, \
                c.quantity, ({BUYABLE}) AS available \
         FROM cart_items c \
         JOIN product_variants v ON v.id = c.variant_id \
         JOIN products p ON p.id = v.product_id \
         JOIN stores s ON s.id = p.store_id \
         WHERE c.user_id = $1 ORDER BY c.added_at, c.variant_id"
    ))
    .bind(user)
    .fetch_all(db)
    .await
    .context("load cart")?;

    let mut items = Vec::with_capacity(rows.len());
    for r in rows {
        let unit = products::service::unit_price(r.base_price_kobo, r.price_kobo);
        let line_total = unit
            .checked_mul(i64::from(r.quantity))
            .ok_or_else(too_large)?;
        items.push(CartLine {
            variant_id: r.variant_id,
            product_id: r.product_id,
            product_name: r.product_name,
            attributes: r.attributes,
            store_id: r.store_id,
            store_name: r.store_name,
            unit_price_kobo: unit,
            quantity: r.quantity,
            line_total_kobo: line_total,
            available: r.available,
        });
    }
    let available: Vec<i64> = items
        .iter()
        .filter(|l| l.available)
        .map(|l| l.line_total_kobo)
        .collect();
    let total_kobo = checked_sum(&available)?;
    Ok(Cart { items, total_kobo })
}

/// Sets a line's quantity: 0 removes it, 1..=100 adds or replaces it. Adding
/// requires an item the buyer can see and buy, and never reserves stock.
pub async fn set_item(
    db: &PgPool,
    user: Uuid,
    variant_id: Uuid,
    quantity: i64,
) -> Result<Cart, AppError> {
    if !(0..=i64::from(MAX_LINE_QUANTITY)).contains(&quantity) {
        return Err(AppError::Validation(format!(
            "quantity must be 0 to {MAX_LINE_QUANTITY}"
        )));
    }
    if quantity == 0 {
        sqlx::query("DELETE FROM cart_items WHERE user_id = $1 AND variant_id = $2")
            .bind(user)
            .bind(variant_id)
            .execute(db)
            .await
            .context("remove cart line")?;
        return get(db, user).await;
    }

    let owner: Uuid = sqlx::query_scalar(&format!(
        "SELECT s.owner_id FROM product_variants v \
         JOIN products p ON p.id = v.product_id JOIN stores s ON s.id = p.store_id \
         WHERE v.id = $1 AND {BUYABLE}"
    ))
    .bind(variant_id)
    .fetch_optional(db)
    .await
    .context("check variant")?
    .ok_or(AppError::NotFound)?;
    if owner == user {
        return Err(own_store_item());
    }

    // The line cap is checked in the same statement; replacing an existing line always passes.
    let quantity = i32::try_from(quantity).context("quantity range")?;
    let written = sqlx::query(
        "INSERT INTO cart_items (user_id, variant_id, quantity) \
         SELECT $1, $2, $3 \
         WHERE (SELECT count(*) FROM cart_items WHERE user_id = $1 AND variant_id <> $2) < $4 \
         ON CONFLICT (user_id, variant_id) DO UPDATE SET quantity = EXCLUDED.quantity",
    )
    .bind(user)
    .bind(variant_id)
    .bind(quantity)
    .bind(MAX_CART_LINES)
    .execute(db)
    .await
    .map_err(|e| match &e {
        // The variant was deleted after the check above.
        sqlx::Error::Database(d) if d.is_foreign_key_violation() => AppError::NotFound,
        _ => AppError::Internal(anyhow::Error::new(e).context("upsert cart line")),
    })?
    .rows_affected();
    if written == 0 {
        return Err(AppError::Unprocessable(
            "cart_full",
            format!("A cart holds at most {MAX_CART_LINES} different items"),
        ));
    }
    get(db, user).await
}

pub async fn clear(db: &PgPool, user: Uuid) -> Result<(), AppError> {
    sqlx::query("DELETE FROM cart_items WHERE user_id = $1")
        .bind(user)
        .execute(db)
        .await
        .context("clear cart")?;
    Ok(())
}

pub(crate) fn own_store_item() -> AppError {
    AppError::Unprocessable(
        "own_store_item",
        "You cannot buy items from your own store".into(),
    )
}
