//! Checkout: reserves stock (`reserved_quantity`; on-hand `stock` changes only
//! when the payment settles, see [`commit_reserved`]) and creates one checkout plus one order per store,
//! then asks the payment provider for a payment page.
//!
//! Locking follows the global order `users -> stores -> products ->
//! product_variants -> access_codes -> checkouts -> orders -> payouts`. The
//! create transaction locks the buyer's user row, then the variants (standalone
//! query, ordered by id); product and store data are read after that without
//! locks. Nothing holding a lock waits on the provider: `initiate` runs after
//! commit.
//!
//! Provider outcomes after commit:
//! - success with an allowed https `payment_url`: store `provider_payment_id`
//!   and `payment_url`, answer 201;
//! - any failure, an untrusted URL, or a timeout: [`expire_and_restore`] in a
//!   new transaction and answer 502. On a timeout the provider may have created
//!   the payment, but nothing is paid until verified; a later webhook for that
//!   reference goes through Task 6's paid-after-expiry path.

use std::collections::{BTreeMap, HashMap};

use anyhow::Context;
use serde::Serialize;
use serde_json::json;
use sqlx::{PgConnection, Postgres, Transaction};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{
    AppState, audit,
    cart::service::{BUYABLE, own_store_item},
    error::AppError,
    limits::MAX_CHECKOUT_TOTAL_KOBO,
    money::{checked_sum, cut_kobo},
    payments::{self, InitiatePayment, ProviderError, payment_url_allowed},
    products::service::unit_price,
    settings,
};

/// Most `pending` (unexpired) checkouts one buyer may hold: each one reserves
/// stock for up to the checkout TTL, so this bounds inventory hoarding (OWASP OAT-021).
pub const MAX_PENDING_CHECKOUTS: i64 = 3;
/// A checkout waiting longer than this for a row lock fails with 409 `checkout_busy`.
const LOCK_TIMEOUT: &str = "SET LOCAL lock_timeout = '5s'";
const REFERENCE_PREFIX: &str = "CK";
const REFERENCE_ATTEMPTS: usize = 5;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CreatedOrder {
    pub order_id: Uuid,
    pub store_name: String,
    pub items_total_kobo: i64,
}

/// The `POST /checkout` response body.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Created {
    pub checkout_id: Uuid,
    pub reference: String,
    pub payment_url: String,
    pub total_kobo: i64,
    pub orders: Vec<CreatedOrder>,
}

pub enum Outcome {
    /// A new checkout (201).
    Created(Created),
    /// An earlier checkout with the same idempotency key (also 201, same body).
    Replayed(Created),
}

fn too_large() -> AppError {
    AppError::Unprocessable("amount_too_large", "amount is too large".into())
}

/// Maps lock timeouts and deadlocks to 409 `checkout_busy`; anything else is internal.
fn db_err(ctx: &'static str) -> impl FnOnce(sqlx::Error) -> AppError {
    move |e| match &e {
        sqlx::Error::Database(d) if matches!(d.code().as_deref(), Some("55P03" | "40P01")) => {
            AppError::Conflict("checkout_busy")
        }
        _ => AppError::Internal(anyhow::Error::new(e).context(ctx)),
    }
}

#[derive(sqlx::FromRow)]
struct VariantRow {
    id: Uuid,
    product_id: Uuid,
    attributes: serde_json::Value,
    stock: i32,
    reserved_quantity: i32,
    price_kobo: Option<i64>,
}

#[derive(sqlx::FromRow)]
struct ProductRow {
    id: Uuid,
    name: String,
    base_price_kobo: i64,
    buyable: bool,
    store_id: Uuid,
    owner_id: Uuid,
}

struct Line {
    variant_id: Uuid,
    product_name: String,
    attributes: serde_json::Value,
    unit_price_kobo: i64,
    quantity: i32,
}

struct OrderDraft {
    lines: Vec<Line>,
    items_total_kobo: i64,
    platform_cut_kobo: i64,
}

/// Creates a checkout for the buyer's whole cart. See the module docs.
pub async fn create(
    state: &AppState,
    buyer: Uuid,
    method: &str,
    idempotency_key: Option<&str>,
) -> Result<Outcome, AppError> {
    let settings = settings::load(&state.db).await?;
    let mut tx = state.db.begin().await.context("begin")?;
    sqlx::query(LOCK_TIMEOUT)
        .execute(&mut *tx)
        .await
        .context("set lock_timeout")?;

    // users: serialises this buyer's checkouts (idempotency lookup, pending cap).
    let display_name: String = sqlx::query_scalar(
        "SELECT display_name FROM users WHERE id = $1 AND deleted_at IS NULL FOR NO KEY UPDATE",
    )
    .bind(buyer)
    .fetch_optional(&mut *tx)
    .await
    .map_err(db_err("lock buyer"))?
    .ok_or(AppError::Unauthorized)?;

    if let Some(key) = idempotency_key
        && let Some(found) = replay(&mut tx, buyer, key, method).await?
    {
        tx.commit().await.context("commit")?;
        return Ok(Outcome::Replayed(found));
    }

    if !settings.enabled_methods.iter().any(|m| m == method) {
        return Err(AppError::Unprocessable(
            "payment_method_disabled",
            "This payment method is not available".into(),
        ));
    }
    let provider = state
        .providers
        .for_method(method)
        .ok_or(AppError::ProviderUnavailable)?;

    let pending: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM checkouts \
         WHERE buyer_id = $1 AND status = 'pending' AND expires_at > now()",
    )
    .bind(buyer)
    .fetch_one(&mut *tx)
    .await
    .context("count pending checkouts")?;
    if pending >= MAX_PENDING_CHECKOUTS {
        return Err(AppError::TooManyRequests("too_many_pending_checkouts"));
    }

    // The cart is read, not locked: cart_items rows cascade from variant deletes, and
    // locking them before the variants would invert the lock order.
    let cart: Vec<(Uuid, i32)> = sqlx::query_as(
        "SELECT variant_id, quantity FROM cart_items WHERE user_id = $1 ORDER BY variant_id",
    )
    .bind(buyer)
    .fetch_all(&mut *tx)
    .await
    .context("load cart")?;
    if cart.is_empty() {
        return Err(AppError::Unprocessable(
            "cart_empty",
            "Your cart is empty".into(),
        ));
    }
    let ids: Vec<Uuid> = cart.iter().map(|(id, _)| *id).collect();

    // product_variants: one standalone statement, always in id order.
    let variants: HashMap<Uuid, VariantRow> = sqlx::query_as::<_, VariantRow>(
        "SELECT id, product_id, attributes, stock, reserved_quantity, price_kobo \
         FROM product_variants WHERE id = ANY($1) ORDER BY id FOR NO KEY UPDATE",
    )
    .bind(&ids)
    .fetch_all(&mut *tx)
    .await
    .map_err(db_err("lock variants"))?
    .into_iter()
    .map(|v| (v.id, v))
    .collect();

    let product_ids: Vec<Uuid> = variants.values().map(|v| v.product_id).collect();
    let products: HashMap<Uuid, ProductRow> = sqlx::query_as::<_, ProductRow>(&format!(
        "SELECT p.id, p.name, p.base_price_kobo, ({BUYABLE}) AS buyable, \
                s.id AS store_id, s.owner_id \
         FROM products p JOIN stores s ON s.id = p.store_id WHERE p.id = ANY($1)"
    ))
    .bind(&product_ids)
    .fetch_all(&mut *tx)
    .await
    .context("load products")?
    .into_iter()
    .map(|p| (p.id, p))
    .collect();

    let unavailable = |id: Uuid| {
        AppError::Unprocessable(
            "item_unavailable",
            format!("Item {id} is no longer available"),
        )
    };
    let mut by_store: BTreeMap<Uuid, Vec<Line>> = BTreeMap::new();
    for &(variant_id, quantity) in &cart {
        let v = variants
            .get(&variant_id)
            .ok_or_else(|| unavailable(variant_id))?;
        let p = products
            .get(&v.product_id)
            .filter(|p| p.buyable)
            .ok_or_else(|| unavailable(variant_id))?;
        if p.owner_id == buyer {
            return Err(own_store_item());
        }
        // Available = on hand minus what other pending checkouts hold, read from the locked row.
        if v.stock - v.reserved_quantity < quantity {
            return Err(AppError::ConflictDetail(
                "out_of_stock",
                format!("Item {variant_id} does not have enough stock"),
            ));
        }
        by_store.entry(p.store_id).or_default().push(Line {
            variant_id,
            product_name: p.name.clone(),
            attributes: v.attributes.clone(),
            unit_price_kobo: unit_price(p.base_price_kobo, v.price_kobo),
            quantity,
        });
    }

    let mut orders: Vec<(Uuid, OrderDraft)> = Vec::with_capacity(by_store.len());
    for (store_id, lines) in by_store {
        let totals = lines
            .iter()
            .map(|l| {
                l.unit_price_kobo
                    .checked_mul(i64::from(l.quantity))
                    .ok_or_else(too_large)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let items_total_kobo = checked_sum(&totals)?;
        orders.push((
            store_id,
            OrderDraft {
                platform_cut_kobo: cut_kobo(items_total_kobo, settings.platform_cut_bps),
                items_total_kobo,
                lines,
            },
        ));
    }
    // Delivery is 0 in this plan, so the checkout total is the sum of the items totals.
    let order_totals: Vec<i64> = orders.iter().map(|(_, o)| o.items_total_kobo).collect();
    let total_kobo = checked_sum(&order_totals)?;
    if total_kobo > MAX_CHECKOUT_TOTAL_KOBO {
        return Err(too_large());
    }

    // The reservation: on-hand stock is untouched until the payment settles (commit_reserved).
    for (_, order) in &orders {
        for l in &order.lines {
            sqlx::query(
                "UPDATE product_variants SET reserved_quantity = reserved_quantity + $2 WHERE id = $1",
            )
                .bind(l.variant_id)
                .bind(l.quantity)
                .execute(&mut *tx)
                .await
                .context("reserve stock")?;
        }
    }

    let mut created = None;
    for _ in 0..REFERENCE_ATTEMPTS {
        let reference = payments::new_reference(REFERENCE_PREFIX);
        let id: Option<Uuid> = sqlx::query_scalar(
            "INSERT INTO checkouts (buyer_id, reference, method, provider, total_kobo, \
                                    idempotency_key, expires_at) \
             VALUES ($1, $2, $3, $4, $5, $6, now() + make_interval(mins => $7)) \
             ON CONFLICT (reference) DO NOTHING RETURNING id",
        )
        .bind(buyer)
        .bind(&reference)
        .bind(method)
        .bind(provider.name())
        .bind(total_kobo)
        .bind(idempotency_key)
        .bind(settings.checkout_ttl_minutes)
        .fetch_optional(&mut *tx)
        .await
        .context("insert checkout")?;
        if let Some(id) = id {
            created = Some((id, reference));
            break;
        }
    }
    let (checkout_id, reference) =
        created.ok_or_else(|| anyhow::anyhow!("could not allocate a checkout reference"))?;

    for (store_id, order) in &orders {
        let order_id: Uuid = sqlx::query_scalar(
            "INSERT INTO orders (checkout_id, store_id, buyer_id, items_total_kobo, \
                                 platform_cut_bps, platform_cut_kobo) \
             VALUES ($1, $2, $3, $4, $5, $6) RETURNING id",
        )
        .bind(checkout_id)
        .bind(store_id)
        .bind(buyer)
        .bind(order.items_total_kobo)
        .bind(settings.platform_cut_bps)
        .bind(order.platform_cut_kobo)
        .fetch_one(&mut *tx)
        .await
        .context("insert order")?;
        for l in &order.lines {
            sqlx::query(
                "INSERT INTO order_items (order_id, variant_id, product_name, attributes, \
                                          unit_price_kobo, quantity) \
                 VALUES ($1, $2, $3, $4, $5, $6)",
            )
            .bind(order_id)
            .bind(l.variant_id)
            .bind(&l.product_name)
            .bind(&l.attributes)
            .bind(l.unit_price_kobo)
            .bind(l.quantity)
            .execute(&mut *tx)
            .await
            .context("insert order item")?;
        }
    }
    audit::record(
        &mut tx,
        Some(buyer),
        "checkout.created",
        "checkout",
        Some(checkout_id),
        json!({ "orders": orders.len(), "total_kobo": total_kobo, "method": method }),
    )
    .await?;
    tx.commit().await.map_err(db_err("commit checkout"))?;

    // After commit, outside any transaction. Spawned so the provider call and the
    // follow-up (store the URL, or expire and restore stock) finish even if the
    // client disconnects and the handler future is dropped.
    let state = state.clone();
    let req = InitiatePayment {
        reference: reference.clone(),
        amount_kobo: total_kobo,
        description: format!("Marketplace order {reference}"),
        // Data minimisation: display name only; users have no email, no phone/address is sent.
        customer_name: display_name,
        customer_email: None,
        redirect_url: state.payment_redirect_url.clone(),
    };
    let db = state.db.clone();
    let task = tokio::spawn(async move {
        let failure = match provider.initiate(req).await {
            Ok(p) if payment_url_allowed(&p.payment_url, provider.payment_url_hosts()) => {
                match store_payment(&state.db, checkout_id, &p).await {
                    Ok(created) => return Ok(created),
                    Err(e) => {
                        tracing::error!(error = %e, "could not store the payment page");
                        "could not store the payment page"
                    }
                }
            }
            Ok(_) => "untrusted payment url",
            // The outcome is unknown; see the module docs.
            Err(ProviderError::Timeout) => "timed out",
            Err(_) => "failed",
        };
        tracing::warn!(
            provider = provider.name(),
            outcome = failure,
            "payment initiate did not succeed; expiring checkout"
        );
        expire_now(&state.db, checkout_id).await?;
        Err(AppError::ProviderUnavailable)
    });
    match task.await {
        Ok(result) => result.map(Outcome::Created),
        // The task panicked: never leave the checkout pending with stock reserved.
        Err(e) => {
            tracing::error!(error = %e, "checkout initiate task failed; expiring checkout");
            expire_now(&db, checkout_id).await?;
            Err(AppError::ProviderUnavailable)
        }
    }
}

/// Saves the provider's payment id and page, then builds the 201 body.
async fn store_payment(
    db: &sqlx::PgPool,
    checkout_id: Uuid,
    p: &payments::InitiatedPayment,
) -> Result<Created, AppError> {
    sqlx::query("UPDATE checkouts SET provider_payment_id = $2, payment_url = $3 WHERE id = $1")
        .bind(checkout_id)
        .bind(&p.provider_payment_id)
        .bind(&p.payment_url)
        .execute(db)
        .await
        .context("store payment url")?;
    let mut conn = db.acquire().await.context("acquire")?;
    load_created(&mut conn, checkout_id).await
}

/// [`expire_and_restore`] in a transaction of its own.
async fn expire_now(db: &sqlx::PgPool, checkout_id: Uuid) -> Result<(), AppError> {
    let mut tx = db.begin().await.context("begin")?;
    expire_and_restore(&mut tx, checkout_id).await?;
    tx.commit().await.context("commit")?;
    Ok(())
}

/// Looks up the buyer's checkout with this idempotency key. `Ok(Some)` replays a
/// pending checkout that has a payment page (201, same body). A pending one still
/// being created is 409 `checkout_in_progress`; one no longer pending (expired,
/// failed, paid, ...) is 409 `checkout_not_pending`, so a dead payment page is never
/// handed out; a different method under the same key is 422.
async fn replay(
    tx: &mut Transaction<'_, Postgres>,
    buyer: Uuid,
    key: &str,
    method: &str,
) -> Result<Option<Created>, AppError> {
    let row: Option<(Uuid, String, String, Option<String>)> = sqlx::query_as(
        "SELECT id, method, status::text, payment_url FROM checkouts \
         WHERE buyer_id = $1 AND idempotency_key = $2",
    )
    .bind(buyer)
    .bind(key)
    .fetch_optional(&mut **tx)
    .await
    .context("find idempotent checkout")?;
    let Some((id, used_method, status, url)) = row else {
        return Ok(None);
    };
    if used_method != method {
        return Err(AppError::Unprocessable(
            "idempotency_key_mismatch",
            "This Idempotency-Key was used for a different request".into(),
        ));
    }
    match (status.as_str(), url) {
        ("pending", Some(_)) => Ok(Some(load_created(tx, id).await?)),
        ("pending", None) => Err(AppError::Conflict("checkout_in_progress")),
        _ => Err(AppError::Conflict("checkout_not_pending")),
    }
}

async fn load_created(conn: &mut PgConnection, checkout_id: Uuid) -> Result<Created, AppError> {
    let (reference, url, total_kobo): (String, Option<String>, i64) =
        sqlx::query_as("SELECT reference, payment_url, total_kobo FROM checkouts WHERE id = $1")
            .bind(checkout_id)
            .fetch_one(&mut *conn)
            .await
            .context("load checkout")?;
    let orders: Vec<(Uuid, String, i64)> = sqlx::query_as(
        "SELECT o.id, s.name, o.items_total_kobo FROM orders o JOIN stores s ON s.id = o.store_id \
         WHERE o.checkout_id = $1 ORDER BY s.name, o.id",
    )
    .bind(checkout_id)
    .fetch_all(&mut *conn)
    .await
    .context("load orders")?;
    Ok(Created {
        checkout_id,
        reference,
        payment_url: url.ok_or_else(|| anyhow::anyhow!("checkout has no payment url"))?,
        total_kobo,
        orders: orders
            .into_iter()
            .map(|(order_id, store_name, items_total_kobo)| CreatedOrder {
                order_id,
                store_name,
                items_total_kobo,
            })
            .collect(),
    })
}

/// The checkout's reserved quantities per variant (from the snapshotted
/// `order_items`, never the live cart), with those variants locked in id order.
/// A lock timeout or deadlock is 409 `checkout_busy`.
pub(crate) async fn lock_reserved(
    conn: &mut PgConnection,
    checkout_id: Uuid,
) -> Result<Vec<(Uuid, i32)>, AppError> {
    let items: Vec<(Uuid, i64)> = sqlx::query_as(
        "SELECT oi.variant_id, sum(oi.quantity)::bigint FROM order_items oi \
         JOIN orders o ON o.id = oi.order_id WHERE o.checkout_id = $1 \
         GROUP BY oi.variant_id ORDER BY oi.variant_id",
    )
    .bind(checkout_id)
    .fetch_all(&mut *conn)
    .await
    .context("load reserved items")?;
    let ids: Vec<Uuid> = items.iter().map(|(id, _)| *id).collect();
    sqlx::query("SELECT id FROM product_variants WHERE id = ANY($1) ORDER BY id FOR NO KEY UPDATE")
        .bind(&ids)
        .fetch_all(&mut *conn)
        .await
        .map_err(db_err("lock variants"))?;
    items
        .into_iter()
        .map(|(id, q)| Ok((id, i32::try_from(q).context("quantity range")?)))
        .collect()
}

/// Turns a checkout's reservation into a sale, inside the caller's transaction:
/// `stock -= q` and `reserved_quantity -= q` for every line, under the variant
/// locks (id order). Task 6 calls this when the payment settles, in the same
/// transaction that moves the checkout `pending -> paid`; that transition is the
/// gate, so this must run only when it succeeded (never twice).
pub async fn commit_reserved(conn: &mut PgConnection, checkout_id: Uuid) -> Result<(), AppError> {
    for (variant_id, quantity) in lock_reserved(conn, checkout_id).await? {
        sqlx::query(
            "UPDATE product_variants \
             SET stock = stock - $2, reserved_quantity = reserved_quantity - $2 WHERE id = $1",
        )
        .bind(variant_id)
        .bind(quantity)
        .execute(&mut *conn)
        .await
        .context("commit reserved stock")?;
    }
    Ok(())
}

/// Expires a `pending` checkout and releases its reserved stock, inside the
/// caller's transaction. Returns whether it did anything.
///
/// The reservation is released only when this call moves the checkout out of
/// `pending` (`UPDATE ... WHERE status = 'pending'`), so running it twice, or
/// racing the expiry job (Task 9) or a paid webhook (Task 6), never releases
/// twice. Locks the variants (id order), then the checkout, then its orders.
/// On-hand `stock` is not touched: seller edits made meanwhile stand.
pub async fn expire_and_restore(
    conn: &mut PgConnection,
    checkout_id: Uuid,
) -> Result<bool, AppError> {
    let items = lock_reserved(conn, checkout_id).await?;

    let moved: Option<Uuid> = sqlx::query_scalar(
        "UPDATE checkouts SET status = 'expired' WHERE id = $1 AND status = 'pending' RETURNING id",
    )
    .bind(checkout_id)
    .fetch_optional(&mut *conn)
    .await
    .context("expire checkout")?;
    if moved.is_none() {
        return Ok(false);
    }
    for (variant_id, quantity) in &items {
        sqlx::query(
            "UPDATE product_variants SET reserved_quantity = reserved_quantity - $2 WHERE id = $1",
        )
        .bind(variant_id)
        .bind(quantity)
        .execute(&mut *conn)
        .await
        .context("release reservation")?;
    }
    sqlx::query(
        "UPDATE orders SET status = 'cancelled', cancelled_at = now() \
         WHERE checkout_id = $1 AND status = 'pending_payment'",
    )
    .bind(checkout_id)
    .execute(&mut *conn)
    .await
    .context("cancel orders")?;
    audit::record(
        conn,
        None,
        "checkout.expired",
        "checkout",
        Some(checkout_id),
        json!({}),
    )
    .await?;
    Ok(true)
}

#[derive(Debug, Serialize)]
pub struct ItemView {
    pub variant_id: Uuid,
    pub product_name: String,
    pub attributes: serde_json::Value,
    pub unit_price_kobo: i64,
    pub quantity: i32,
}

#[derive(Debug, Serialize)]
pub struct OrderView {
    pub order_id: Uuid,
    pub store_id: Uuid,
    pub store_name: String,
    pub status: String,
    pub items_total_kobo: i64,
    pub delivery_fee_kobo: i64,
    pub items: Vec<ItemView>,
}

#[derive(Debug, Serialize)]
pub struct CheckoutView {
    pub checkout_id: Uuid,
    pub reference: String,
    pub status: String,
    pub method: String,
    pub total_kobo: i64,
    pub payment_url: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub expires_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    pub paid_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    pub orders: Vec<OrderView>,
}

#[derive(sqlx::FromRow)]
struct CheckoutRow {
    id: Uuid,
    reference: String,
    status: String,
    method: String,
    total_kobo: i64,
    payment_url: Option<String>,
    expires_at: OffsetDateTime,
    paid_at: Option<OffsetDateTime>,
    created_at: OffsetDateTime,
}

#[derive(sqlx::FromRow)]
struct OrderRow {
    id: Uuid,
    store_id: Uuid,
    store_name: String,
    status: String,
    items_total_kobo: i64,
    delivery_fee_kobo: i64,
}

#[derive(sqlx::FromRow)]
struct ItemRow {
    order_id: Uuid,
    variant_id: Uuid,
    product_name: String,
    attributes: serde_json::Value,
    unit_price_kobo: i64,
    quantity: i32,
}

/// The buyer's own checkout; anyone else gets NotFound. The platform cut is not shown.
pub async fn get(db: &sqlx::PgPool, buyer: Uuid, id: Uuid) -> Result<CheckoutView, AppError> {
    let c: CheckoutRow = sqlx::query_as(
        "SELECT id, reference, status::text AS status, method, total_kobo, payment_url, \
                expires_at, paid_at, created_at \
         FROM checkouts WHERE id = $1 AND buyer_id = $2",
    )
    .bind(id)
    .bind(buyer)
    .fetch_optional(db)
    .await
    .context("load checkout")?
    .ok_or(AppError::NotFound)?;
    let orders: Vec<OrderRow> = sqlx::query_as(
        "SELECT o.id, o.store_id, s.name AS store_name, o.status::text AS status, \
                o.items_total_kobo, o.delivery_fee_kobo \
         FROM orders o JOIN stores s ON s.id = o.store_id \
         WHERE o.checkout_id = $1 ORDER BY s.name, o.id",
    )
    .bind(id)
    .fetch_all(db)
    .await
    .context("load orders")?;
    let items: Vec<ItemRow> = sqlx::query_as(
        "SELECT oi.order_id, oi.variant_id, oi.product_name, oi.attributes, \
                oi.unit_price_kobo, oi.quantity \
         FROM order_items oi JOIN orders o ON o.id = oi.order_id \
         WHERE o.checkout_id = $1 ORDER BY oi.product_name, oi.variant_id",
    )
    .bind(id)
    .fetch_all(db)
    .await
    .context("load order items")?;
    let mut by_order: HashMap<Uuid, Vec<ItemView>> = HashMap::new();
    for i in items {
        by_order.entry(i.order_id).or_default().push(ItemView {
            variant_id: i.variant_id,
            product_name: i.product_name,
            attributes: i.attributes,
            unit_price_kobo: i.unit_price_kobo,
            quantity: i.quantity,
        });
    }
    Ok(CheckoutView {
        checkout_id: c.id,
        reference: c.reference,
        status: c.status,
        method: c.method,
        total_kobo: c.total_kobo,
        payment_url: c.payment_url,
        expires_at: c.expires_at,
        paid_at: c.paid_at,
        created_at: c.created_at,
        orders: orders
            .into_iter()
            .map(|o| OrderView {
                items: by_order.remove(&o.id).unwrap_or_default(),
                order_id: o.id,
                store_id: o.store_id,
                store_name: o.store_name,
                status: o.status,
                items_total_kobo: o.items_total_kobo,
                delivery_fee_kobo: o.delivery_fee_kobo,
            })
            .collect(),
    })
}
