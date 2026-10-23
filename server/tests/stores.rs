mod common;

use axum::http::Method;
use common::TestApp;
use serde_json::{Value, json};
use sqlx::PgPool;

fn store_body(name: &str) -> Value {
    json!({
        "name": name,
        "description": "Fresh goods",
        "pickup_address": "12 Unique Street, Wuse 2",
        "pickup_state": "FCT",
        "delivery_courier": true,
        "delivery_self": false
    })
}

async fn create(app: &TestApp, token: &str, name: &str) -> common::TestResponse {
    app.request(Method::POST, "/stores", Some(store_body(name)), Some(token)).await
}

#[sqlx::test]
async fn seller_creates_store(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let (_, token) = app.make_seller("08011111111").await;
    let res = create(&app, &token, "Mama Put").await;
    assert_eq!(res.status, 201, "{}", res.json);
    assert_eq!(res.json["status"], "pending");
    let slug = res.json["slug"].as_str().unwrap();
    let got = app.request(Method::GET, &format!("/stores/{slug}"), None, Some(&token)).await;
    assert_eq!(got.status, 200);
    assert_eq!(got.json["name"], "Mama Put");
    assert!(got.json.get("pickup_address").is_none());
    // Pending stores are hidden from the public.
    let public = app.request(Method::GET, &format!("/stores/{slug}"), None, None).await;
    assert_eq!(public.status, 404);
    // The owner's own list includes the decrypted address.
    let mine = app.request(Method::GET, "/stores/mine", None, Some(&token)).await;
    assert_eq!(mine.status, 200);
    assert_eq!(mine.json[0]["pickup_address"], "12 Unique Street, Wuse 2");
}

#[sqlx::test]
async fn non_seller_cannot_create_store(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let (_, token) = app.sign_up_verified("08011111111").await;
    let res = create(&app, &token, "Nope").await;
    assert_eq!(res.status, 403);
    assert_eq!(res.json["error"]["code"], "not_a_seller");
}

#[sqlx::test]
async fn third_store_rejected(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let (_, token) = app.make_seller("08011111111").await;
    assert_eq!(create(&app, &token, "One").await.status, 201);
    assert_eq!(create(&app, &token, "Two").await.status, 201);
    let res = create(&app, &token, "Three").await;
    assert_eq!(res.status, 409);
    assert_eq!(res.json["error"]["code"], "store_limit_reached");
}

#[sqlx::test]
async fn deleted_store_frees_slot(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let (_, token) = app.make_seller("08011111111").await;
    let first = create(&app, &token, "One").await;
    assert_eq!(create(&app, &token, "Two").await.status, 201);
    let id = first.json["id"].as_str().unwrap();
    let del = app.request(Method::DELETE, &format!("/stores/{id}"), None, Some(&token)).await;
    assert_eq!(del.status, 204);
    assert_eq!(create(&app, &token, "Three").await.status, 201);
}

#[sqlx::test]
async fn concurrent_second_and_third_one_wins(pool: PgPool) {
    let app = TestApp::new(pool.clone()).await;
    let (_, token) = app.make_seller("08011111111").await;
    assert_eq!(create(&app, &token, "One").await.status, 201);
    let (a, b) = tokio::join!(create(&app, &token, "Two"), create(&app, &token, "Three"));
    let mut statuses = [a.status, b.status];
    statuses.sort();
    assert_eq!(statuses, [201, 409]);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM stores WHERE deleted_at IS NULL")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 2);
}

#[sqlx::test]
async fn non_fct_rejected(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let (_, token) = app.make_seller("08011111111").await;
    let mut body = store_body("Lagos Shop");
    body["pickup_state"] = json!("Lagos");
    let res = app.request(Method::POST, "/stores", Some(body), Some(&token)).await;
    assert_eq!(res.status, 422);
    assert_eq!(res.json["error"]["code"], "outside_service_area");
}

#[sqlx::test]
async fn needs_a_delivery_method(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let (_, token) = app.make_seller("08011111111").await;
    let mut body = store_body("No Delivery");
    body["delivery_courier"] = json!(false);
    body["delivery_self"] = json!(false);
    let res = app.request(Method::POST, "/stores", Some(body), Some(&token)).await;
    assert_eq!(res.status, 422);
    assert_eq!(res.json["error"]["code"], "validation_failed");
}

#[sqlx::test]
async fn only_owner_can_edit(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let (_, token) = app.make_seller("08011111111").await;
    let (_, other) = app.make_seller("08022222222").await;
    let store = create(&app, &token, "Mine").await;
    let id = store.json["id"].as_str().unwrap();
    let patch = app
        .request(Method::PATCH, &format!("/stores/{id}"), Some(json!({ "name": "Stolen" })), Some(&other))
        .await;
    assert_eq!(patch.status, 404);
    let del = app.request(Method::DELETE, &format!("/stores/{id}"), None, Some(&other)).await;
    assert_eq!(del.status, 404);
    // Owner can edit.
    let ok = app
        .request(Method::PATCH, &format!("/stores/{id}"), Some(json!({ "name": "Renamed" })), Some(&token))
        .await;
    assert_eq!(ok.status, 200, "{}", ok.json);
    assert_eq!(ok.json["name"], "Renamed");
    let bad = app
        .request(Method::PATCH, &format!("/stores/{id}"), Some(json!({ "pickup_state": "Kano" })), Some(&token))
        .await;
    assert_eq!(bad.status, 422);
    assert_eq!(bad.json["error"]["code"], "outside_service_area");
}

#[sqlx::test]
async fn pickup_address_encrypted(pool: PgPool) {
    let app = TestApp::new(pool.clone()).await;
    let (_, token) = app.make_seller("08011111111").await;
    assert_eq!(create(&app, &token, "Secret").await.status, 201);
    let raw: Vec<u8> = sqlx::query_scalar("SELECT pickup_address_enc FROM stores").fetch_one(&pool).await.unwrap();
    let needle = b"Unique Street";
    assert!(!raw.windows(needle.len()).any(|w| w == needle));
}

#[sqlx::test]
async fn slug_collision_suffixed(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let (_, a) = app.make_seller("08011111111").await;
    let (_, b) = app.make_seller("08022222222").await;
    assert_eq!(create(&app, &a, "Mama Put").await.json["slug"], "mama-put");
    assert_eq!(create(&app, &b, "Mama  Put!").await.json["slug"], "mama-put-2");
}

#[sqlx::test]
async fn revoked_code_keeps_existing_store(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let owner = app.owner_token().await;
    let (code_id, code) = app.create_access_code(&owner, json!({})).await;
    let (_, token) = app.sign_up_verified("08011111111").await;
    assert_eq!(app.redeem(&token, &code).await.status, 200);
    let store = create(&app, &token, "Survivor").await;
    assert_eq!(store.status, 201);
    let revoke = app
        .request(Method::POST, &format!("/admin/access-codes/{code_id}/revoke"), None, Some(&owner))
        .await;
    assert_eq!(revoke.status, 204);
    let slug = store.json["slug"].as_str().unwrap();
    let got = app.request(Method::GET, &format!("/stores/{slug}"), None, Some(&token)).await;
    assert_eq!(got.status, 200);
    assert!(got.json.get("pickup_address").is_none());
}

#[sqlx::test]
async fn store_creation_audited(pool: PgPool) {
    let app = TestApp::new(pool.clone()).await;
    let (user, token) = app.make_seller("08011111111").await;
    let store = create(&app, &token, "Audited").await;
    let id: uuid::Uuid = store.json["id"].as_str().unwrap().parse().unwrap();
    let row: (Option<uuid::Uuid>, Option<uuid::Uuid>, String) = sqlx::query_as(
        "SELECT actor_id, subject_id, detail::text FROM audit_log WHERE action = 'store.created'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(row.0, Some(user));
    assert_eq!(row.1, Some(id));
    assert!(!row.2.contains("Unique Street"));
}

#[sqlx::test]
async fn reserved_slug_is_skipped(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let (_, token) = app.make_seller("08011111111").await;
    let res = create(&app, &token, "Mine").await;
    assert_eq!(res.status, 201, "{}", res.json);
    assert_eq!(res.json["slug"], "mine-2");
    let got = app.request(Method::GET, "/stores/mine-2", None, Some(&token)).await;
    assert_eq!(got.status, 200);
    assert_eq!(got.json["name"], "Mine");
}
