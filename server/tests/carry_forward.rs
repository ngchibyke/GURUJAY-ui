mod common;

use axum::http::Method;
use common::TestApp;
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

async fn make_store(app: &TestApp, token: &str) -> Uuid {
    let res = app
        .request(
            Method::POST,
            "/stores",
            Some(json!({
                "name": "Mama Put",
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
    res.json["id"].as_str().unwrap().parse().unwrap()
}

fn product_body() -> Value {
    json!({
        "name": "Ankara Shirt",
        "description": "Handmade shirt",
        "category": "fashion",
        "condition": "new",
        "photo_keys": ["photos/a.jpg"],
        "weight_grams": 300,
        "base_price_kobo": 1_200_000,
        "variants": [
            { "attributes": { "size": "M" }, "stock": 4 },
            { "attributes": { "size": "L" }, "stock": 2 }
        ]
    })
}

async fn setup(app: &TestApp) -> (String, Uuid) {
    let (_, token) = app.make_seller("08011111111").await;
    let store_id = make_store(app, &token).await;
    (token, store_id)
}

async fn create(app: &TestApp, token: &str, store_id: Uuid, body: Value) -> common::TestResponse {
    app.request(
        Method::POST,
        &format!("/stores/{store_id}/products"),
        Some(body),
        Some(token),
    )
    .await
}

fn variant_by_size<'a>(variants: &'a [Value], size: &str) -> Option<&'a Value> {
    variants.iter().find(|v| v["attributes"]["size"] == size)
}

#[sqlx::test]
async fn variant_replace_keeps_ids_for_same_attributes(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let (token, store_id) = setup(&app).await;
    let created = create(&app, &token, store_id, product_body()).await;
    assert_eq!(created.status, 201, "{}", created.json);
    let id = created.json["id"].as_str().unwrap().to_string();
    let before = created.json["variants"].as_array().unwrap().clone();
    let m_id = variant_by_size(&before, "M").unwrap()["id"].clone();
    let l_id = variant_by_size(&before, "L").unwrap()["id"].clone();

    let res = app
        .request(
            Method::PUT,
            &format!("/products/{id}/variants"),
            Some(json!([
                { "attributes": { "size": "M" }, "stock": 9 },
                { "attributes": { "size": "XL" }, "stock": 3 }
            ])),
            Some(&token),
        )
        .await;
    assert_eq!(res.status, 200, "{}", res.json);
    let after = res.json["variants"].as_array().unwrap();
    assert_eq!(after.len(), 2);
    let m = variant_by_size(after, "M").unwrap();
    assert_eq!(m["id"], m_id);
    assert_eq!(m["stock"], 9);
    assert!(variant_by_size(after, "L").is_none());
    let xl = variant_by_size(after, "XL").unwrap();
    assert_ne!(xl["id"], m_id);
    assert_ne!(xl["id"], l_id);
    assert_eq!(xl["stock"], 3);
}

#[sqlx::test]
async fn price_above_cap_rejected(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let (token, store_id) = setup(&app).await;
    let mut body = product_body();
    body["base_price_kobo"] = json!(1_000_000_001_i64);
    let res = create(&app, &token, store_id, body).await;
    assert_eq!(res.status, 422, "{}", res.json);
    assert_eq!(res.json["error"]["code"], "validation_failed");

    // The cap itself is allowed, and a variant override above it is rejected.
    let mut at_cap = product_body();
    at_cap["base_price_kobo"] = json!(1_000_000_000_i64);
    let ok = create(&app, &token, store_id, at_cap).await;
    assert_eq!(ok.status, 201, "{}", ok.json);
    let id = ok.json["id"].as_str().unwrap();

    let res = app
        .request(
            Method::PUT,
            &format!("/products/{id}/variants"),
            Some(json!([{ "attributes": {}, "stock": 1, "price_kobo": 1_000_000_001_i64 }])),
            Some(&token),
        )
        .await;
    assert_eq!(res.status, 422, "{}", res.json);
    assert_eq!(res.json["error"]["code"], "validation_failed");

    let res = app
        .request(
            Method::PATCH,
            &format!("/products/{id}"),
            Some(json!({ "base_price_kobo": 1_000_000_001_i64 })),
            Some(&token),
        )
        .await;
    assert_eq!(res.status, 422, "{}", res.json);
}

#[sqlx::test]
async fn stock_above_cap_rejected(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let (token, store_id) = setup(&app).await;
    let mut body = product_body();
    body["variants"] = json!([{ "attributes": {}, "stock": 100_001 }]);
    let res = create(&app, &token, store_id, body).await;
    assert_eq!(res.status, 422, "{}", res.json);
    assert_eq!(res.json["error"]["code"], "validation_failed");

    let mut at_cap = product_body();
    at_cap["variants"] = json!([{ "attributes": {}, "stock": 100_000 }]);
    let ok = create(&app, &token, store_id, at_cap).await;
    assert_eq!(ok.status, 201, "{}", ok.json);
    let id = ok.json["id"].as_str().unwrap();
    let res = app
        .request(
            Method::PUT,
            &format!("/products/{id}/variants"),
            Some(json!([{ "attributes": {}, "stock": 100_001 }])),
            Some(&token),
        )
        .await;
    assert_eq!(res.status, 422, "{}", res.json);
}

#[sqlx::test]
async fn db_rejects_values_above_caps(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let (token, store_id) = setup(&app).await;
    let id: Uuid = create(&app, &token, store_id, product_body()).await.json["id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let price = sqlx::query("UPDATE products SET base_price_kobo = 1000000001 WHERE id = $1")
        .bind(id)
        .execute(&app.pool)
        .await;
    assert!(price.is_err());
    let stock = sqlx::query("UPDATE product_variants SET stock = 100001 WHERE product_id = $1")
        .bind(id)
        .execute(&app.pool)
        .await;
    assert!(stock.is_err());
    let vprice =
        sqlx::query("UPDATE product_variants SET price_kobo = 1000000001 WHERE product_id = $1")
            .bind(id)
            .execute(&app.pool)
            .await;
    assert!(vprice.is_err());
}

#[sqlx::test]
async fn variant_attributes_are_trimmed_and_matched(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let (token, store_id) = setup(&app).await;
    let created = create(&app, &token, store_id, product_body()).await;
    let id = created.json["id"].as_str().unwrap().to_string();
    let before = created.json["variants"].as_array().unwrap().clone();
    let m_id = variant_by_size(&before, "M").unwrap()["id"].clone();
    let path = format!("/products/{id}/variants");

    let res = app
        .request(
            Method::PUT,
            &path,
            Some(json!([{ "attributes": { " size ": " M " }, "stock": 7 }])),
            Some(&token),
        )
        .await;
    assert_eq!(res.status, 200, "{}", res.json);
    let v = &res.json["variants"][0];
    assert_eq!(v["id"], m_id);
    assert_eq!(v["attributes"], json!({ "size": "M" }));

    let dup = json!([
        { "attributes": { "size": "S" }, "stock": 1 },
        { "attributes": { "size": " S " }, "stock": 1 }
    ]);
    let res = app
        .request(Method::PUT, &path, Some(dup), Some(&token))
        .await;
    assert_eq!(res.status, 422, "{}", res.json);
    assert_eq!(res.json["error"]["code"], "duplicate_variant");
}

#[test]
fn migration_caps_match_limits_module() {
    let sql = include_str!("../migrations/0007_carry_forward.sql");
    let price = server::limits::MAX_PRICE_KOBO.to_string();
    let stock = server::limits::MAX_STOCK.to_string();
    assert!(sql.contains(&format!("base_price_kobo <= {price}")));
    assert!(sql.contains(&format!("price_kobo <= {price}")));
    assert!(sql.contains(&format!("stock <= {stock}")));
}
