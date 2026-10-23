mod common;

use axum::http::Method;
use common::{TestApp, TestResponse};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

/// Creates a (pending) store for `token`; returns (store id, slug).
async fn make_store(app: &TestApp, token: &str, name: &str) -> (Uuid, String) {
    let res = app
        .request(
            Method::POST,
            "/stores",
            Some(json!({
                "name": name,
                "description": "Fresh goods",
                "pickup_address": "12 Unique Street, Wuse 2",
                "pickup_state": "FCT",
                "delivery_courier": true,
                "delivery_self": false
            })),
            Some(token),
        )
        .await;
    assert_eq!(res.status, 201, "{}", res.json);
    (res.json["id"].as_str().unwrap().parse().unwrap(), res.json["slug"].as_str().unwrap().to_string())
}

async fn activate(app: &TestApp, store_id: Uuid) {
    sqlx::query("UPDATE stores SET status = 'active' WHERE id = $1")
        .bind(store_id)
        .execute(&app.pool)
        .await
        .unwrap();
}

fn product_body() -> Value {
    json!({
        "name": "Ankara Shirt",
        "description": "Handmade shirt",
        "category": "fashion",
        "brand": "Ada",
        "condition": "new",
        "photo_keys": ["photos/a.jpg"],
        "weight_grams": 300,
        "base_price_kobo": 1_200_000,
        "variants": [
            { "attributes": { "size": "M", "colour": "Black" }, "stock": 4 },
            { "attributes": { "size": "L", "colour": "Black" }, "stock": 2, "price_kobo": 1_500_000 }
        ]
    })
}

fn with(mut body: Value, key: &str, value: Value) -> Value {
    body[key] = value;
    body
}

async fn create(app: &TestApp, token: &str, store_id: Uuid, body: Value) -> TestResponse {
    app.request(Method::POST, &format!("/stores/{store_id}/products"), Some(body), Some(token)).await
}

async fn setup(app: &TestApp) -> (String, Uuid, String) {
    let (_, token) = app.make_seller("08011111111").await;
    let (store_id, slug) = make_store(app, &token, "Mama Put").await;
    (token, store_id, slug)
}

async fn audit_actions(app: &TestApp) -> Vec<String> {
    sqlx::query_scalar("SELECT action FROM audit_log WHERE action LIKE 'product.%' ORDER BY id")
        .fetch_all(&app.pool)
        .await
        .unwrap()
}

#[sqlx::test]
async fn create_product_with_variants(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let (token, store_id, _) = setup(&app).await;
    let res = create(&app, &token, store_id, product_body()).await;
    assert_eq!(res.status, 201, "{}", res.json);
    assert_eq!(res.json["is_published"], false);
    let id = res.json["id"].as_str().unwrap();
    let got = app.request(Method::GET, &format!("/products/{id}"), None, Some(&token)).await;
    assert_eq!(got.status, 200);
    let variants = got.json["variants"].as_array().unwrap();
    assert_eq!(variants.len(), 2);
    let m = variants.iter().find(|v| v["attributes"]["size"] == "M").unwrap();
    let l = variants.iter().find(|v| v["attributes"]["size"] == "L").unwrap();
    assert_eq!(m["stock"], 4);
    assert_eq!(l["stock"], 2);
    assert_eq!(l["price_kobo"], 1_500_000);
    assert_eq!(audit_actions(&app).await, vec!["product.created"]);
}

#[sqlx::test]
async fn simple_product_gets_default_variant(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let (token, store_id, _) = setup(&app).await;
    let body = with(product_body(), "variants", json!([{ "attributes": {}, "stock": 5 }]));
    let res = create(&app, &token, store_id, body).await;
    assert_eq!(res.status, 201, "{}", res.json);
    assert_eq!(res.json["variants"].as_array().unwrap().len(), 1);
    assert_eq!(res.json["variants"][0]["attributes"], json!({}));
    assert_eq!(res.json["variants"][0]["stock"], 5);
}

#[sqlx::test]
async fn empty_attribute_variant_mixed_with_others_rejected(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let (token, store_id, _) = setup(&app).await;
    let body = with(
        product_body(),
        "variants",
        json!([{ "attributes": {}, "stock": 1 }, { "attributes": { "size": "M" }, "stock": 1 }]),
    );
    let res = create(&app, &token, store_id, body).await;
    assert_eq!(res.status, 422);
    assert_eq!(res.json["error"]["code"], "validation_failed");
}

#[sqlx::test]
async fn zero_or_negative_price_rejected(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let (token, store_id, _) = setup(&app).await;
    for price in [0, -100] {
        let res = create(&app, &token, store_id, with(product_body(), "base_price_kobo", json!(price))).await;
        assert_eq!(res.status, 422, "price {price}");
        assert_eq!(res.json["error"]["code"], "validation_failed");
    }
    let body = with(product_body(), "variants", json!([{ "attributes": {}, "stock": 1, "price_kobo": 0 }]));
    assert_eq!(create(&app, &token, store_id, body).await.status, 422);
}

#[sqlx::test]
async fn negative_stock_rejected(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let (token, store_id, _) = setup(&app).await;
    let body = with(product_body(), "variants", json!([{ "attributes": {}, "stock": -1 }]));
    let res = create(&app, &token, store_id, body).await;
    assert_eq!(res.status, 422);
    assert_eq!(res.json["error"]["code"], "validation_failed");
}

#[sqlx::test]
async fn photo_count_bounds(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let (token, store_id, _) = setup(&app).await;
    let photos = |n: usize| json!((0..n).map(|i| format!("photos/{i}.jpg")).collect::<Vec<_>>());
    for n in [0, 11] {
        let res = create(&app, &token, store_id, with(product_body(), "photo_keys", photos(n))).await;
        assert_eq!(res.status, 422, "{n} photos");
        assert_eq!(res.json["error"]["code"], "validation_failed");
    }
    assert_eq!(create(&app, &token, store_id, with(product_body(), "photo_keys", photos(10))).await.status, 201);
}

#[sqlx::test]
async fn empty_variants_rejected(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let (token, store_id, _) = setup(&app).await;
    let res = create(&app, &token, store_id, with(product_body(), "variants", json!([]))).await;
    assert_eq!(res.status, 422);
    assert_eq!(res.json["error"]["code"], "validation_failed");
}

#[sqlx::test]
async fn variant_count_and_attribute_limits(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let (token, store_id, _) = setup(&app).await;
    let many = |n: usize| json!((0..n).map(|i| json!({ "attributes": { "n": i.to_string() }, "stock": 1 })).collect::<Vec<_>>());
    assert_eq!(create(&app, &token, store_id, with(product_body(), "variants", many(51))).await.status, 422);
    assert_eq!(create(&app, &token, store_id, with(product_body(), "variants", many(50))).await.status, 201);
    let six = json!([{ "attributes": { "a": "1", "b": "1", "c": "1", "d": "1", "e": "1", "f": "1" }, "stock": 1 }]);
    assert_eq!(create(&app, &token, store_id, with(product_body(), "variants", six)).await.status, 422);
    let long = json!([{ "attributes": { "size": "x".repeat(41) }, "stock": 1 }]);
    assert_eq!(create(&app, &token, store_id, with(product_body(), "variants", long)).await.status, 422);
}

#[sqlx::test]
async fn duplicate_variant_attributes_rejected(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let (token, store_id, _) = setup(&app).await;
    let body = with(
        product_body(),
        "variants",
        json!([{ "attributes": { "size": "M" }, "stock": 1 }, { "attributes": { "size": "M" }, "stock": 2 }]),
    );
    let res = create(&app, &token, store_id, body).await;
    assert_eq!(res.status, 422);
    assert_eq!(res.json["error"]["code"], "duplicate_variant");
}

#[sqlx::test]
async fn condition_must_be_known(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let (token, store_id, _) = setup(&app).await;
    let res = create(&app, &token, store_id, with(product_body(), "condition", json!("fake"))).await;
    assert_eq!(res.status, 422);
    assert_eq!(res.json["error"]["code"], "validation_failed");
    for ok in ["new", "used", "refurbished"] {
        assert_eq!(create(&app, &token, store_id, with(product_body(), "condition", json!(ok))).await.status, 201);
    }
}

#[sqlx::test]
async fn only_store_owner_can_add(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let (_, store_id, _) = setup(&app).await;
    let (_, other) = app.make_seller("08022222222").await;
    assert_eq!(create(&app, &other, store_id, product_body()).await.status, 404);
    let missing = create(&app, &other, Uuid::new_v4(), product_body()).await;
    assert_eq!(missing.status, 404);
}

#[sqlx::test]
async fn unpublished_hidden_from_public(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let (token, store_id, slug) = setup(&app).await;
    activate(&app, store_id).await;
    let id = create(&app, &token, store_id, product_body()).await.json["id"].as_str().unwrap().to_string();
    let path = format!("/products/{id}");
    assert_eq!(app.request(Method::GET, &path, None, None).await.status, 404);
    let list = app.request(Method::GET, &format!("/stores/{slug}/products"), None, None).await;
    assert_eq!(list.status, 200);
    assert_eq!(list.json, json!([]));
    // The owner still sees it.
    assert_eq!(app.request(Method::GET, &path, None, Some(&token)).await.status, 200);

    let patched = app.request(Method::PATCH, &path, Some(json!({ "is_published": true })), Some(&token)).await;
    assert_eq!(patched.status, 200, "{}", patched.json);
    assert_eq!(patched.json["is_published"], true);
    assert_eq!(app.request(Method::GET, &path, None, None).await.status, 200);
    let list = app.request(Method::GET, &format!("/stores/{slug}/products"), None, None).await;
    assert_eq!(list.json.as_array().unwrap().len(), 1);
}

#[sqlx::test]
async fn pending_store_products_hidden_from_public(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let (token, store_id, slug) = setup(&app).await;
    let id = create(&app, &token, store_id, with(product_body(), "variants", json!([{ "attributes": {}, "stock": 1 }])))
        .await
        .json["id"]
        .as_str()
        .unwrap()
        .to_string();
    let path = format!("/products/{id}");
    app.request(Method::PATCH, &path, Some(json!({ "is_published": true })), Some(&token)).await;

    let list_path = format!("/stores/{slug}/products");
    let public = app.request(Method::GET, &list_path, None, None).await;
    assert_eq!(public.status, 200);
    assert_eq!(public.json, json!([]));
    assert_eq!(app.request(Method::GET, &path, None, None).await.status, 404);

    let owner = app.request(Method::GET, &list_path, None, Some(&token)).await;
    assert_eq!(owner.json.as_array().unwrap().len(), 1);
}

#[sqlx::test]
async fn patch_requires_ownership_and_validates(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let (token, store_id, _) = setup(&app).await;
    let (_, other) = app.make_seller("08022222222").await;
    let id = create(&app, &token, store_id, product_body()).await.json["id"].as_str().unwrap().to_string();
    let path = format!("/products/{id}");
    assert_eq!(app.request(Method::PATCH, &path, Some(json!({ "name": "Hacked" })), Some(&other)).await.status, 404);
    assert_eq!(app.request(Method::DELETE, &path, None, Some(&other)).await.status, 404);
    let bad = app.request(Method::PATCH, &path, Some(json!({ "base_price_kobo": 0 })), Some(&token)).await;
    assert_eq!(bad.status, 422);
    let ok = app.request(Method::PATCH, &path, Some(json!({ "name": "Better Shirt" })), Some(&token)).await;
    assert_eq!(ok.status, 200);
    assert_eq!(ok.json["name"], "Better Shirt");
    assert_eq!(ok.json["brand"], "Ada");
}

#[sqlx::test]
async fn replace_variants_swaps_whole_set(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let (token, store_id, _) = setup(&app).await;
    let id = create(&app, &token, store_id, product_body()).await.json["id"].as_str().unwrap().to_string();
    let path = format!("/products/{id}/variants");

    let dup = json!([{ "attributes": { "size": "S" }, "stock": 1 }, { "attributes": { "size": "S" }, "stock": 1 }]);
    assert_eq!(app.request(Method::PUT, &path, Some(dup), Some(&token)).await.json["error"]["code"], "duplicate_variant");
    // A rejected replacement leaves the old set intact.
    let still = app.request(Method::GET, &format!("/products/{id}"), None, Some(&token)).await;
    assert_eq!(still.json["variants"].as_array().unwrap().len(), 2);

    let res = app
        .request(Method::PUT, &path, Some(json!([{ "attributes": { "size": "S" }, "stock": 9 }])), Some(&token))
        .await;
    assert_eq!(res.status, 200, "{}", res.json);
    assert_eq!(res.json["variants"].as_array().unwrap().len(), 1);
    assert_eq!(res.json["variants"][0]["stock"], 9);
    assert_eq!(app.request(Method::PUT, &path, Some(json!([])), Some(&token)).await.status, 422);
    assert!(audit_actions(&app).await.contains(&"product.variants_replaced".to_string()));
}

#[sqlx::test]
async fn deleted_product_is_gone(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let (token, store_id, _) = setup(&app).await;
    let id = create(&app, &token, store_id, product_body()).await.json["id"].as_str().unwrap().to_string();
    let path = format!("/products/{id}");
    assert_eq!(app.request(Method::DELETE, &path, None, Some(&token)).await.status, 204);
    assert_eq!(app.request(Method::GET, &path, None, Some(&token)).await.status, 404);
    assert_eq!(app.request(Method::DELETE, &path, None, Some(&token)).await.status, 404);
    assert!(audit_actions(&app).await.contains(&"product.deleted".to_string()));
}

#[sqlx::test]
async fn cannot_add_to_deleted_store(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let (token, store_id, _) = setup(&app).await;
    let del = app.request(Method::DELETE, &format!("/stores/{store_id}"), None, Some(&token)).await;
    assert_eq!(del.status, 204);
    assert_eq!(create(&app, &token, store_id, product_body()).await.status, 404);
}
