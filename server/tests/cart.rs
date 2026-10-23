mod common;

use axum::http::Method;
use common::TestApp;
use serde_json::json;
use server::limits::MAX_LINE_QUANTITY;
use sqlx::PgPool;
use uuid::Uuid;

const BUYER: &str = "08022222221";

#[sqlx::test]
async fn cart_add_update_remove(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let l = app
        .make_active_store_with_product("08011111111", "Mama Put", 150_000, 5)
        .await;
    let (_, buyer) = app.sign_up_verified(BUYER).await;

    let empty = app.request(Method::GET, "/cart", None, Some(&buyer)).await;
    assert_eq!(empty.status, 200, "{}", empty.json);
    assert_eq!(empty.json["items"].as_array().unwrap().len(), 0);
    assert_eq!(empty.json["total_kobo"], 0);

    let res = app.set_cart(&buyer, l.variant_id, 2).await;
    assert_eq!(res.status, 200, "{}", res.json);
    let cart = app.request(Method::GET, "/cart", None, Some(&buyer)).await;
    let items = cart.json["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["variant_id"], l.variant_id.to_string());
    assert_eq!(items[0]["quantity"], 2);
    assert_eq!(items[0]["unit_price_kobo"], 150_000);
    assert_eq!(items[0]["line_total_kobo"], 300_000);
    assert_eq!(items[0]["store_name"], "Mama Put");
    assert_eq!(items[0]["available"], true);
    assert_eq!(cart.json["total_kobo"], 300_000);

    // Update replaces the quantity.
    let res = app.set_cart(&buyer, l.variant_id, 3).await;
    assert_eq!(res.json["items"][0]["quantity"], 3);
    assert_eq!(res.json["total_kobo"], 450_000);

    // Adding to the cart does not reserve stock.
    assert_eq!(app.stock(l.variant_id).await, 5);

    // 0 removes the line.
    let res = app.set_cart(&buyer, l.variant_id, 0).await;
    assert_eq!(res.status, 200, "{}", res.json);
    assert_eq!(res.json["items"].as_array().unwrap().len(), 0);

    // DELETE /cart clears everything.
    app.set_cart(&buyer, l.variant_id, 1).await;
    let res = app
        .request(Method::DELETE, "/cart", None, Some(&buyer))
        .await;
    assert_eq!(res.status, 204);
    let cart = app.request(Method::GET, "/cart", None, Some(&buyer)).await;
    assert_eq!(cart.json["items"].as_array().unwrap().len(), 0);

    // Carts are private.
    assert_eq!(
        app.request(Method::GET, "/cart", None, None).await.status,
        401
    );
}

#[sqlx::test]
async fn quantity_cap(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let l = app
        .make_active_store_with_product("08011111111", "Mama Put", 100, 500)
        .await;
    let (_, buyer) = app.sign_up_verified(BUYER).await;

    let res = app
        .set_cart(&buyer, l.variant_id, i64::from(MAX_LINE_QUANTITY) + 1)
        .await;
    assert_eq!(res.status, 422, "{}", res.json);
    assert_eq!(res.json["error"]["code"], "validation_failed");
    let res = app.set_cart(&buyer, l.variant_id, -1).await;
    assert_eq!(res.status, 422, "{}", res.json);

    let res = app
        .set_cart(&buyer, l.variant_id, i64::from(MAX_LINE_QUANTITY))
        .await;
    assert_eq!(res.status, 200, "{}", res.json);
    assert_eq!(res.json["items"][0]["quantity"], MAX_LINE_QUANTITY);
}

#[sqlx::test]
async fn unknown_or_hidden_variant_not_found(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let l = app
        .make_active_store_with_product("08011111111", "Mama Put", 100, 5)
        .await;
    let (_, buyer) = app.sign_up_verified(BUYER).await;

    assert_eq!(app.set_cart(&buyer, Uuid::new_v4(), 1).await.status, 404);

    // An unpublished product cannot be added.
    app.request(
        Method::PATCH,
        &format!("/products/{}", l.product_id),
        Some(json!({ "is_published": false })),
        Some(&l.seller_token),
    )
    .await;
    let res = app.set_cart(&buyer, l.variant_id, 1).await;
    assert_eq!(res.status, 404, "{}", res.json);

    // Removing a line is always allowed, even for an item that is no longer visible.
    assert_eq!(app.set_cart(&buyer, l.variant_id, 0).await.status, 200);

    // Malformed body.
    let res = app
        .request(
            Method::PUT,
            "/cart/items",
            Some(json!({ "variant_id": "nope", "quantity": 1 })),
            Some(&buyer),
        )
        .await;
    assert_eq!(res.status, 422);
}

#[sqlx::test]
async fn unavailable_item_stays_in_cart_marked(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let l = app
        .make_active_store_with_product("08011111111", "Mama Put", 100, 5)
        .await;
    let (_, buyer) = app.sign_up_verified(BUYER).await;
    app.set_cart(&buyer, l.variant_id, 1).await;
    sqlx::query("UPDATE stores SET status = 'suspended' WHERE id = $1")
        .bind(l.store_id)
        .execute(&app.pool)
        .await
        .unwrap();
    let cart = app.request(Method::GET, "/cart", None, Some(&buyer)).await;
    assert_eq!(cart.json["items"][0]["available"], false);
    // Unavailable lines do not count towards the preview total.
    assert_eq!(cart.json["total_kobo"], 0);
}

#[sqlx::test]
async fn own_store_item_cannot_be_added(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let l = app
        .make_active_store_with_product("08011111111", "Mama Put", 100, 5)
        .await;
    let res = app.set_cart(&l.seller_token, l.variant_id, 1).await;
    assert_eq!(res.status, 422, "{}", res.json);
    assert_eq!(res.json["error"]["code"], "own_store_item");
}
