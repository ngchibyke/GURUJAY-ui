use std::collections::BTreeMap;

use anyhow::Context;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::{PgPool, Postgres, Transaction};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{audit, error::AppError};

const MAX_VARIANTS: usize = 50;
const MAX_ATTRIBUTES: usize = 5;
const CONDITIONS: &[&str] = &["new", "used", "refurbished"];

#[derive(Debug, Clone, Deserialize)]
pub struct VariantInput {
    #[serde(default)]
    pub attributes: BTreeMap<String, String>,
    pub stock: i32,
    pub price_kobo: Option<i64>,
    pub sku: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct NewProduct {
    pub name: String,
    pub description: String,
    pub category: String,
    pub brand: Option<String>,
    pub condition: String,
    pub photo_keys: Vec<String>,
    pub weight_grams: i32,
    pub length_cm: Option<i32>,
    pub width_cm: Option<i32>,
    pub height_cm: Option<i32>,
    pub base_price_kobo: i64,
    pub variants: Vec<VariantInput>,
}

#[derive(Debug, Default, Deserialize)]
pub struct ProductPatch {
    pub name: Option<String>,
    pub description: Option<String>,
    pub category: Option<String>,
    pub brand: Option<String>,
    pub condition: Option<String>,
    pub photo_keys: Option<Vec<String>>,
    pub weight_grams: Option<i32>,
    pub length_cm: Option<i32>,
    pub width_cm: Option<i32>,
    pub height_cm: Option<i32>,
    pub base_price_kobo: Option<i64>,
    pub is_published: Option<bool>,
}

#[derive(Debug, Clone, sqlx::FromRow, Serialize)]
pub struct Product {
    pub id: Uuid,
    pub store_id: Uuid,
    pub name: String,
    pub description: String,
    pub category: String,
    pub brand: Option<String>,
    pub condition: String,
    pub photo_keys: Vec<String>,
    pub weight_grams: i32,
    pub length_cm: Option<i32>,
    pub width_cm: Option<i32>,
    pub height_cm: Option<i32>,
    pub base_price_kobo: i64,
    pub is_published: bool,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

const COLUMNS: &str = "id, store_id, name, description, category, brand, condition::text AS condition, \
                       photo_keys, weight_grams, length_cm, width_cm, height_cm, base_price_kobo, \
                       is_published, created_at, updated_at";

/// `COLUMNS` qualified for queries that join `stores` (which also has `id`, `name`, ...).
const PRODUCT_COLUMNS: &str = "p.id, p.store_id, p.name, p.description, p.category, p.brand, \
                       p.condition::text AS condition, p.photo_keys, p.weight_grams, p.length_cm, \
                       p.width_cm, p.height_cm, p.base_price_kobo, p.is_published, p.created_at, p.updated_at";

#[derive(Debug, Clone, Serialize)]
pub struct Variant {
    pub id: Uuid,
    pub attributes: BTreeMap<String, String>,
    pub stock: i32,
    pub price_kobo: Option<i64>,
    pub sku: Option<String>,
}

#[derive(Debug, sqlx::FromRow)]
struct VariantRow {
    id: Uuid,
    attributes: serde_json::Value,
    stock: i32,
    price_kobo: Option<i64>,
    sku: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ProductView {
    #[serde(flatten)]
    pub product: Product,
    pub variants: Vec<Variant>,
}

/// The price a buyer pays for `variant`: its own override, else the product's base price.
pub fn effective_price(product: &Product, variant: &Variant) -> i64 {
    variant.price_kobo.unwrap_or(product.base_price_kobo)
}

fn invalid(msg: &str) -> AppError {
    AppError::Validation(msg.to_string())
}

fn duplicate_variant() -> AppError {
    AppError::Unprocessable(
        "duplicate_variant",
        "Two variants share the same attributes".into(),
    )
}

fn validate_text(value: &str, max: usize, field: &str) -> Result<(), AppError> {
    let n = value.trim().chars().count();
    if n == 0 || n > max {
        return Err(AppError::Validation(format!(
            "{field} must be 1 to {max} characters"
        )));
    }
    Ok(())
}

fn validate_brand(brand: &Option<String>) -> Result<(), AppError> {
    match brand {
        Some(b) if b.chars().count() > 60 => Err(invalid("brand must be at most 60 characters")),
        _ => Ok(()),
    }
}

fn validate_condition(condition: &str) -> Result<(), AppError> {
    if CONDITIONS.contains(&condition) {
        Ok(())
    } else {
        Err(invalid("condition must be new, used or refurbished"))
    }
}

fn validate_photos(keys: &[String]) -> Result<(), AppError> {
    if keys.is_empty() || keys.len() > 10 {
        return Err(invalid("photo_keys must contain 1 to 10 entries"));
    }
    if keys.iter().any(|k| k.trim().is_empty()) {
        return Err(invalid("photo_keys must not contain empty entries"));
    }
    Ok(())
}

fn validate_positive(value: i64, field: &str) -> Result<(), AppError> {
    if value > 0 {
        Ok(())
    } else {
        Err(AppError::Validation(format!(
            "{field} must be greater than 0"
        )))
    }
}

fn validate_dimensions(l: Option<i32>, w: Option<i32>, h: Option<i32>) -> Result<(), AppError> {
    for (v, f) in [(l, "length_cm"), (w, "width_cm"), (h, "height_cm")] {
        if let Some(v) = v {
            validate_positive(v.into(), f)?;
        }
    }
    Ok(())
}

/// Validates a full variant set; duplicates are detected here before any insert.
fn validate_variants(variants: &[VariantInput]) -> Result<(), AppError> {
    if variants.is_empty() || variants.len() > MAX_VARIANTS {
        return Err(invalid("variants must contain 1 to 50 entries"));
    }
    for v in variants {
        if v.stock < 0 {
            return Err(invalid("variant stock must not be negative"));
        }
        if let Some(p) = v.price_kobo {
            validate_positive(p, "variant price_kobo")?;
        }
        if v.attributes.len() > MAX_ATTRIBUTES {
            return Err(invalid("a variant can have at most 5 attributes"));
        }
        for (k, val) in &v.attributes {
            validate_text(k, 40, "variant attribute key")?;
            validate_text(val, 40, "variant attribute value")?;
        }
    }
    if variants.len() > 1 && variants.iter().any(|v| v.attributes.is_empty()) {
        return Err(invalid(
            "a variant without attributes cannot be combined with other variants",
        ));
    }
    for (i, v) in variants.iter().enumerate() {
        if variants[..i].iter().any(|o| o.attributes == v.attributes) {
            return Err(duplicate_variant());
        }
    }
    Ok(())
}

async fn insert_variants(
    tx: &mut Transaction<'_, Postgres>,
    product_id: Uuid,
    variants: &[VariantInput],
) -> Result<(), AppError> {
    for v in variants {
        let attrs = serde_json::to_value(&v.attributes).context("serialise attributes")?;
        sqlx::query(
            "INSERT INTO product_variants (product_id, attributes, stock, price_kobo, sku) \
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(product_id)
        .bind(attrs)
        .bind(v.stock)
        .bind(v.price_kobo)
        .bind(&v.sku)
        .execute(&mut **tx)
        .await
        .map_err(|e| match &e {
            sqlx::Error::Database(db) if db.is_unique_violation() => duplicate_variant(),
            _ => AppError::Internal(anyhow::Error::new(e).context("insert variant")),
        })?;
    }
    Ok(())
}

async fn load_variants(db: &PgPool, product_id: Uuid) -> Result<Vec<Variant>, AppError> {
    let rows: Vec<VariantRow> = sqlx::query_as(
        "SELECT id, attributes, stock, price_kobo, sku FROM product_variants \
         WHERE product_id = $1 ORDER BY attributes::text, id",
    )
    .bind(product_id)
    .fetch_all(db)
    .await
    .context("load variants")?;
    rows.into_iter()
        .map(|r| {
            Ok(Variant {
                id: r.id,
                attributes: serde_json::from_value(r.attributes).context("decode attributes")?,
                stock: r.stock,
                price_kobo: r.price_kobo,
                sku: r.sku,
            })
        })
        .collect::<Result<_, AppError>>()
}

async fn view(db: &PgPool, product: Product) -> Result<ProductView, AppError> {
    let variants = load_variants(db, product.id).await?;
    Ok(ProductView { product, variants })
}

/// Locks a live product whose live store belongs to `owner`; anything else is NotFound.
async fn lock_owned(
    tx: &mut Transaction<'_, Postgres>,
    owner: Uuid,
    id: Uuid,
) -> Result<Product, AppError> {
    sqlx::query_as(&format!(
        "SELECT {PRODUCT_COLUMNS} FROM products p WHERE p.id = $1 AND p.deleted_at IS NULL \
         AND EXISTS (SELECT 1 FROM stores s WHERE s.id = p.store_id AND s.owner_id = $2 AND s.deleted_at IS NULL) \
         FOR UPDATE OF p"
    ))
    .bind(id)
    .bind(owner)
    .fetch_optional(&mut **tx)
    .await
    .context("lock product")?
    .ok_or(AppError::NotFound)
}

pub async fn create_product(
    db: &PgPool,
    owner: Uuid,
    store_id: Uuid,
    input: NewProduct,
) -> Result<ProductView, AppError> {
    validate_text(&input.name, 120, "name")?;
    validate_text(&input.description, 5000, "description")?;
    validate_text(&input.category, 60, "category")?;
    validate_brand(&input.brand)?;
    validate_condition(&input.condition)?;
    validate_photos(&input.photo_keys)?;
    validate_positive(input.weight_grams.into(), "weight_grams")?;
    validate_dimensions(input.length_cm, input.width_cm, input.height_cm)?;
    validate_positive(input.base_price_kobo, "base_price_kobo")?;
    validate_variants(&input.variants)?;

    let mut tx = db.begin().await.context("begin")?;
    // The store must be the caller's and not deleted; otherwise it does not exist for them.
    sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM stores WHERE id = $1 AND owner_id = $2 AND deleted_at IS NULL FOR SHARE",
    )
    .bind(store_id)
    .bind(owner)
    .fetch_optional(&mut *tx)
    .await
    .context("check store")?
    .ok_or(AppError::NotFound)?;

    let product: Product = sqlx::query_as(&format!(
        "INSERT INTO products (store_id, name, description, category, brand, condition, photo_keys, \
                               weight_grams, length_cm, width_cm, height_cm, base_price_kobo) \
         VALUES ($1, $2, $3, $4, $5, $6::product_condition, $7, $8, $9, $10, $11, $12) RETURNING {COLUMNS}"
    ))
    .bind(store_id)
    .bind(input.name.trim())
    .bind(&input.description)
    .bind(input.category.trim())
    .bind(&input.brand)
    .bind(&input.condition)
    .bind(&input.photo_keys)
    .bind(input.weight_grams)
    .bind(input.length_cm)
    .bind(input.width_cm)
    .bind(input.height_cm)
    .bind(input.base_price_kobo)
    .fetch_one(&mut *tx)
    .await
    .context("insert product")?;
    insert_variants(&mut tx, product.id, &input.variants).await?;
    audit::record(
        &mut tx,
        Some(owner),
        "product.created",
        "product",
        Some(product.id),
        json!({}),
    )
    .await?;
    tx.commit().await.context("commit")?;
    view(db, product).await
}

pub async fn update_product(
    db: &PgPool,
    owner: Uuid,
    id: Uuid,
    patch: ProductPatch,
) -> Result<ProductView, AppError> {
    let mut tx = db.begin().await.context("begin")?;
    let cur = lock_owned(&mut tx, owner, id).await?;

    let name = patch.name.unwrap_or(cur.name);
    let description = patch.description.unwrap_or(cur.description);
    let category = patch.category.unwrap_or(cur.category);
    let brand = patch.brand.or(cur.brand);
    let condition = patch.condition.unwrap_or(cur.condition);
    let photo_keys = patch.photo_keys.unwrap_or(cur.photo_keys);
    let weight = patch.weight_grams.unwrap_or(cur.weight_grams);
    let (l, w, h) = (
        patch.length_cm.or(cur.length_cm),
        patch.width_cm.or(cur.width_cm),
        patch.height_cm.or(cur.height_cm),
    );
    let base = patch.base_price_kobo.unwrap_or(cur.base_price_kobo);
    let published = patch.is_published.unwrap_or(cur.is_published);

    validate_text(&name, 120, "name")?;
    validate_text(&description, 5000, "description")?;
    validate_text(&category, 60, "category")?;
    validate_brand(&brand)?;
    validate_condition(&condition)?;
    validate_photos(&photo_keys)?;
    validate_positive(weight.into(), "weight_grams")?;
    validate_dimensions(l, w, h)?;
    validate_positive(base, "base_price_kobo")?;

    let product: Product = sqlx::query_as(&format!(
        "UPDATE products SET name = $2, description = $3, category = $4, brand = $5, \
                condition = $6::product_condition, photo_keys = $7, weight_grams = $8, length_cm = $9, \
                width_cm = $10, height_cm = $11, base_price_kobo = $12, is_published = $13, updated_at = now() \
         WHERE id = $1 RETURNING {COLUMNS}"
    ))
    .bind(id)
    .bind(name.trim())
    .bind(&description)
    .bind(category.trim())
    .bind(&brand)
    .bind(&condition)
    .bind(&photo_keys)
    .bind(weight)
    .bind(l)
    .bind(w)
    .bind(h)
    .bind(base)
    .bind(published)
    .fetch_one(&mut *tx)
    .await
    .context("update product")?;
    audit::record(
        &mut tx,
        Some(owner),
        "product.updated",
        "product",
        Some(id),
        json!({}),
    )
    .await?;
    tx.commit().await.context("commit")?;
    view(db, product).await
}

pub async fn replace_variants(
    db: &PgPool,
    owner: Uuid,
    id: Uuid,
    variants: Vec<VariantInput>,
) -> Result<ProductView, AppError> {
    validate_variants(&variants)?;
    let mut tx = db.begin().await.context("begin")?;
    let product = lock_owned(&mut tx, owner, id).await?;
    sqlx::query("DELETE FROM product_variants WHERE product_id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await
        .context("clear variants")?;
    insert_variants(&mut tx, id, &variants).await?;
    sqlx::query("UPDATE products SET updated_at = now() WHERE id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await
        .context("touch product")?;
    audit::record(
        &mut tx,
        Some(owner),
        "product.variants_replaced",
        "product",
        Some(id),
        json!({}),
    )
    .await?;
    tx.commit().await.context("commit")?;
    view(db, product).await
}

pub async fn delete_product(db: &PgPool, owner: Uuid, id: Uuid) -> Result<(), AppError> {
    let mut tx = db.begin().await.context("begin")?;
    lock_owned(&mut tx, owner, id).await?;
    sqlx::query("UPDATE products SET deleted_at = now(), updated_at = now() WHERE id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await
        .context("soft delete product")?;
    audit::record(
        &mut tx,
        Some(owner),
        "product.deleted",
        "product",
        Some(id),
        json!({}),
    )
    .await?;
    tx.commit().await.context("commit")?;
    Ok(())
}

/// Public when published and in an active store; the store owner also sees their own products.
pub async fn get_product(
    db: &PgPool,
    id: Uuid,
    viewer: Option<Uuid>,
) -> Result<ProductView, AppError> {
    let product: Product = sqlx::query_as(&format!(
        "SELECT {PRODUCT_COLUMNS} FROM products p WHERE p.id = $1 AND p.deleted_at IS NULL AND EXISTS ( \
           SELECT 1 FROM stores s WHERE s.id = p.store_id AND s.deleted_at IS NULL AND ( \
             s.owner_id = $2 OR (p.is_published AND s.status = 'active')))"
    ))
    .bind(id)
    .bind(viewer)
    .fetch_optional(db)
    .await
    .context("get product")?
    .ok_or(AppError::NotFound)?;
    view(db, product).await
}

/// Lists a store's products by slug. Anyone who cannot see the store gets an empty list.
pub async fn list_store_products(
    db: &PgPool,
    slug: &str,
    viewer: Option<Uuid>,
) -> Result<Vec<ProductView>, AppError> {
    let products: Vec<Product> = sqlx::query_as(&format!(
        "SELECT {PRODUCT_COLUMNS} FROM products p JOIN stores s ON s.id = p.store_id \
         WHERE s.slug = $1 AND s.deleted_at IS NULL AND p.deleted_at IS NULL \
           AND (s.owner_id = $2 OR (p.is_published AND s.status = 'active')) \
         ORDER BY p.created_at, p.id"
    ))
    .bind(slug)
    .bind(viewer)
    .fetch_all(db)
    .await
    .context("list products")?;
    let mut out = Vec::with_capacity(products.len());
    for p in products {
        out.push(view(db, p).await?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn product(base: i64) -> Product {
        let now = OffsetDateTime::now_utc();
        Product {
            id: Uuid::nil(),
            store_id: Uuid::nil(),
            name: "n".into(),
            description: "d".into(),
            category: "c".into(),
            brand: None,
            condition: "new".into(),
            photo_keys: vec!["k".into()],
            weight_grams: 1,
            length_cm: None,
            width_cm: None,
            height_cm: None,
            base_price_kobo: base,
            is_published: false,
            created_at: now,
            updated_at: now,
        }
    }

    fn variant(price: Option<i64>) -> Variant {
        Variant {
            id: Uuid::nil(),
            attributes: BTreeMap::new(),
            stock: 1,
            price_kobo: price,
            sku: None,
        }
    }

    #[test]
    fn effective_price_uses_override() {
        let p = product(1_200_000);
        assert_eq!(effective_price(&p, &variant(Some(1_500_000))), 1_500_000);
        assert_eq!(effective_price(&p, &variant(None)), 1_200_000);
    }
}
