mod common;

use std::collections::BTreeSet;

use axum::http::Method;
use common::TestApp;
use serde_json::{Value, json};
use server::{
    checkout::{SettleOutcome, settle_payment},
    orders::service::{mark_delivered, mark_picked_up},
    payments::PaymentStatus,
};
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

const METHOD: &str = "mono_directpay";
const SELLER: &str = "08011111111";
const BUYER: &str = "08022222221";
const STRANGER: &str = "08033333331";

struct Fixture {
    order_id: Uuid,
    store_id: Uuid,
    variant_id: Uuid,
    seller: String,
    buyer: String,
}

/// One paid order (one unit at 10,000 kobo) from a fresh store.
async fn paid_order(app: &TestApp) -> Fixture {
    let listing = app
        .make_active_store_with_product(SELLER, "Mama Put", 10_000, 5)
        .await;
    let (_, buyer) = app.sign_up_verified(BUYER).await;
    let order_id = buy(app, &buyer, listing.variant_id).await;
    Fixture {
        order_id,
        store_id: listing.store_id,
        variant_id: listing.variant_id,
        seller: listing.seller_token,
        buyer,
    }
}

/// Buys one unit of `variant_id`, pays through the fake provider, returns the order id.
async fn buy(app: &TestApp, buyer: &str, variant_id: Uuid) -> Uuid {
    assert_eq!(app.set_cart(buyer, variant_id, 1).await.status, 200);
    let res = app.checkout(buyer, METHOD).await;
    assert_eq!(res.status, 201, "{}", res.json);
    let reference = res.json["reference"].as_str().unwrap().to_string();
    let total = res.json["total_kobo"].as_i64().unwrap();
    let checkout_id: Uuid = res.json["checkout_id"].as_str().unwrap().parse().unwrap();
    app.fake.set_payment(
        &reference,
        PaymentStatus::Successful {
            amount_kobo: total,
            fee_kobo: 10,
            currency: "NGN".into(),
            held: false,
            reference: reference.clone(),
        },
    );
    assert_eq!(
        settle_payment(&app.state, &reference).await.unwrap(),
        SettleOutcome::Paid
    );
    sqlx::query_scalar("SELECT id FROM orders WHERE checkout_id = $1")
        .bind(checkout_id)
        .fetch_one(&app.pool)
        .await
        .unwrap()
}

async fn status_of(app: &TestApp, order_id: Uuid) -> String {
    sqlx::query_scalar("SELECT status::text FROM orders WHERE id = $1")
        .bind(order_id)
        .fetch_one(&app.pool)
        .await
        .unwrap()
}

async fn count(app: &TestApp, sql: &str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(&app.pool).await.unwrap()
}

fn keys(v: &Value) -> BTreeSet<String> {
    v.as_object().unwrap().keys().cloned().collect()
}

fn set(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|s| s.to_string()).collect()
}

async fn get(app: &TestApp, path: &str, token: &str) -> common::TestResponse {
    app.request(Method::GET, path, None, Some(token)).await
}

async fn cancel(app: &TestApp, id: Uuid, token: &str) -> common::TestResponse {
    app.request(
        Method::POST,
        &format!("/orders/{id}/cancel"),
        None,
        Some(token),
    )
    .await
}

async fn received(app: &TestApp, id: Uuid, token: &str) -> common::TestResponse {
    app.request(
        Method::POST,
        &format!("/orders/{id}/received"),
        Some(json!({ "confirm": true })),
        Some(token),
    )
    .await
}

const BUYER_KEYS: &[&str] = &[
    "id",
    "store_id",
    "status",
    "items",
    "items_total_kobo",
    "delivery_fee_kobo",
    "total_kobo",
    "platform_cut_kobo",
    "held",
    "paid_at",
    "ship_by",
    "delivered_at",
    "release_at",
    "created_at",
];

fn seller_keys(with_payout: bool) -> BTreeSet<String> {
    let mut k = set(BUYER_KEYS);
    k.insert("buyer_display_name".into());
    k.insert("expected_payout_kobo".into());
    if with_payout {
        k.insert("payout".into());
    }
    k
}

#[sqlx::test]
async fn buyer_sees_own_orders_only(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let f = paid_order(&app).await;
    let (_, other) = app.sign_up_verified(STRANGER).await;

    let res = get(&app, "/orders", &f.buyer).await;
    assert_eq!(res.status, 200, "{}", res.json);
    let items = res.json["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["id"], f.order_id.to_string());
    assert_eq!(keys(&items[0]), set(BUYER_KEYS));
    assert_eq!(items[0]["items"][0]["product_name"], "Ankara Shirt");
    assert_eq!(items[0]["items"][0]["unit_price_kobo"], 10_000);
    assert_eq!(items[0]["held"], false);
    assert!(res.json["next_cursor"].is_null());

    let res = get(&app, "/orders", &other).await;
    assert_eq!(res.json["items"].as_array().unwrap().len(), 0);

    let res = get(&app, &format!("/orders/{}", f.order_id), &f.buyer).await;
    assert_eq!(res.status, 200);
    assert_eq!(keys(&res.json), set(BUYER_KEYS));
    // Stranger and missing id look the same.
    let stranger = get(&app, &format!("/orders/{}", f.order_id), &other).await;
    let missing = get(&app, &format!("/orders/{}", Uuid::new_v4()), &other).await;
    assert_eq!(stranger.status, 404);
    assert_eq!(missing.status, 404);
    assert_eq!(stranger.json, missing.json);
}

#[sqlx::test]
async fn seller_sees_store_orders_without_contact_details(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let f = paid_order(&app).await;

    let res = get(&app, &format!("/stores/{}/orders", f.store_id), &f.seller).await;
    assert_eq!(res.status, 200, "{}", res.json);
    let items = res.json["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    // Exact key set: a new field cannot leak buyer data by accident.
    assert_eq!(keys(&items[0]), seller_keys(false));
    assert_eq!(items[0]["buyer_display_name"], "Ada");
    assert!(items[0]["expected_payout_kobo"].as_i64().unwrap() > 0);
    let text = res.json.to_string();
    assert!(!text.contains(BUYER));
    assert!(!text.contains("2348022222221"));
    assert!(!text.contains("buyer_id"));
    assert!(!text.contains("address"));

    // The store owner reads one order, with the same allowlist.
    let one = get(&app, &format!("/orders/{}", f.order_id), &f.seller).await;
    assert_eq!(one.status, 200);
    assert_eq!(keys(&one.json), seller_keys(false));

    // Another seller cannot see this store's orders or this order.
    let (_, s2) = app.make_seller("08044444441").await;
    assert_eq!(
        get(&app, &format!("/stores/{}/orders", f.store_id), &s2)
            .await
            .status,
        404
    );
    assert_eq!(
        get(&app, &format!("/orders/{}", f.order_id), &s2)
            .await
            .status,
        404
    );
    // A buyer cannot list a store's orders.
    assert_eq!(
        get(&app, &format!("/stores/{}/orders", f.store_id), &f.buyer)
            .await
            .status,
        404
    );

    // Owner/support admins can view the order.
    let owner = app.owner_token().await;
    assert_eq!(
        get(&app, &format!("/orders/{}", f.order_id), &owner)
            .await
            .status,
        200
    );
    let support = app.make_support().await;
    assert_eq!(
        get(&app, &format!("/orders/{}", f.order_id), &support)
            .await
            .status,
        200
    );
}

#[sqlx::test]
async fn cancel_before_pickup_starts_refund(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let f = paid_order(&app).await;
    assert_eq!(app.stock(f.variant_id).await, 4);

    let res = cancel(&app, f.order_id, &f.buyer).await;
    assert_eq!(res.status, 200, "{}", res.json);
    let status = status_of(&app, f.order_id).await;
    assert!(
        matches!(status.as_str(), "refund_pending" | "refunded"),
        "{status}"
    );
    assert_eq!(count(&app, "SELECT count(*) FROM refunds").await, 1);
    assert_eq!(
        count(
            &app,
            "SELECT count(*) FROM audit_log WHERE action = 'order.cancelled_by_buyer'"
        )
        .await,
        1
    );
    assert_eq!(app.stock(f.variant_id).await, 5, "stock restored");
    // Cancelling again is a conflict, not a second refund.
    assert_eq!(cancel(&app, f.order_id, &f.buyer).await.status, 409);
    assert_eq!(count(&app, "SELECT count(*) FROM refunds").await, 1);
}

#[sqlx::test]
async fn held_order_can_be_cancelled_before_pickup(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let f = paid_order(&app).await;
    sqlx::query("UPDATE orders SET held_at = now(), hold_reason = 'complaint'")
        .execute(&app.pool)
        .await
        .unwrap();
    assert_eq!(cancel(&app, f.order_id, &f.buyer).await.status, 200);
}

#[sqlx::test]
async fn cancel_after_pickup_rejected(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let f = paid_order(&app).await;
    mark_picked_up(&app.pool, f.order_id, OffsetDateTime::now_utc())
        .await
        .unwrap();
    let res = cancel(&app, f.order_id, &f.buyer).await;
    assert_eq!(res.status, 409);
    assert_eq!(res.json["error"]["code"], "invalid_order_state");
    assert_eq!(status_of(&app, f.order_id).await, "picked_up");
    assert_eq!(count(&app, "SELECT count(*) FROM refunds").await, 0);
}

#[sqlx::test]
async fn received_triggers_release(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let f = paid_order(&app).await;
    mark_picked_up(&app.pool, f.order_id, OffsetDateTime::now_utc())
        .await
        .unwrap();
    let res = received(&app, f.order_id, &f.buyer).await;
    assert_eq!(res.status, 200, "{}", res.json);
    assert_eq!(count(&app, "SELECT count(*) FROM payouts").await, 1);
    assert_eq!(
        count(
            &app,
            "SELECT count(*) FROM audit_log WHERE action = 'order.received'"
        )
        .await,
        1
    );
    // The seller now sees the payout; the buyer's view still has no payout.
    let seller = get(&app, &format!("/orders/{}", f.order_id), &f.seller).await;
    assert_eq!(keys(&seller.json), seller_keys(true));
    let buyer = get(&app, &format!("/orders/{}", f.order_id), &f.buyer).await;
    assert_eq!(keys(&buyer.json), set(BUYER_KEYS));
}

#[sqlx::test]
async fn received_from_delivered_works(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let f = paid_order(&app).await;
    let now = OffsetDateTime::now_utc();
    mark_picked_up(&app.pool, f.order_id, now).await.unwrap();
    mark_delivered(&app.pool, f.order_id, now).await.unwrap();
    assert_eq!(received(&app, f.order_id, &f.buyer).await.status, 200);
    assert_eq!(count(&app, "SELECT count(*) FROM payouts").await, 1);
}

#[sqlx::test]
async fn received_twice_one_payout(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let f = paid_order(&app).await;
    mark_picked_up(&app.pool, f.order_id, OffsetDateTime::now_utc())
        .await
        .unwrap();
    assert_eq!(received(&app, f.order_id, &f.buyer).await.status, 200);
    assert_eq!(received(&app, f.order_id, &f.buyer).await.status, 200);
    assert_eq!(count(&app, "SELECT count(*) FROM payouts").await, 1);
    assert_eq!(
        count(
            &app,
            "SELECT count(*) FROM audit_log WHERE action = 'order.received'"
        )
        .await,
        1
    );
}

#[sqlx::test]
async fn held_order_cannot_be_received(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let f = paid_order(&app).await;
    mark_picked_up(&app.pool, f.order_id, OffsetDateTime::now_utc())
        .await
        .unwrap();
    sqlx::query("UPDATE orders SET held_at = now(), hold_reason = 'complaint'")
        .execute(&app.pool)
        .await
        .unwrap();
    let res = received(&app, f.order_id, &f.buyer).await;
    assert_eq!(res.status, 409);
    assert_eq!(res.json["error"]["code"], "order_held");
    assert_eq!(status_of(&app, f.order_id).await, "picked_up");
    assert_eq!(count(&app, "SELECT count(*) FROM payouts").await, 0);
}

#[sqlx::test]
async fn received_requires_explicit_confirmation(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let f = paid_order(&app).await;
    mark_picked_up(&app.pool, f.order_id, OffsetDateTime::now_utc())
        .await
        .unwrap();
    let path = format!("/orders/{}/received", f.order_id);
    for body in [
        None,
        Some(json!({})),
        Some(json!({ "confirm": false })),
        Some(json!({ "confirm": "yes" })),
        Some(json!({ "confirm": true, "extra": 1 })),
    ] {
        let res = app
            .request(Method::POST, &path, body.clone(), Some(&f.buyer))
            .await;
        assert_eq!(res.status, 422, "{body:?}");
        assert_eq!(res.json["error"]["code"], "confirmation_required");
    }
    assert_eq!(count(&app, "SELECT count(*) FROM payouts").await, 0);
}

#[sqlx::test]
async fn invalid_transition_rejected(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let f = paid_order(&app).await;
    // Not picked up yet.
    let res = received(&app, f.order_id, &f.buyer).await;
    assert_eq!(res.status, 409);
    assert_eq!(res.json["error"]["code"], "invalid_order_state");
    // The service refuses skipping a step, with the same code.
    let err = mark_delivered(&app.pool, f.order_id, OffsetDateTime::now_utc())
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        server::error::AppError::Conflict("invalid_order_state")
    ));
    assert_eq!(status_of(&app, f.order_id).await, "paid");
}

#[sqlx::test]
async fn stranger_cannot_cancel_or_receive(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let f = paid_order(&app).await;
    let (_, other) = app.sign_up_verified(STRANGER).await;
    let missing = cancel(&app, Uuid::new_v4(), &other).await;
    assert_eq!(missing.status, 404);

    // Even in a state where the buyer would get 409, a stranger gets 404.
    for (token, who) in [(&other, "stranger"), (&f.seller, "seller")] {
        let res = cancel(&app, f.order_id, token).await;
        assert_eq!(res.status, 404, "{who}");
        assert_eq!(res.json, missing.json, "{who}");
        assert_eq!(received(&app, f.order_id, token).await.status, 404, "{who}");
    }
    assert_eq!(status_of(&app, f.order_id).await, "paid");
    assert_eq!(count(&app, "SELECT count(*) FROM refunds").await, 0);

    mark_picked_up(&app.pool, f.order_id, OffsetDateTime::now_utc())
        .await
        .unwrap();
    assert_eq!(cancel(&app, f.order_id, &other).await.status, 404);
    assert_eq!(received(&app, f.order_id, &f.seller).await.status, 404);
    assert_eq!(count(&app, "SELECT count(*) FROM payouts").await, 0);
    assert_eq!(status_of(&app, f.order_id).await, "picked_up");
}

#[sqlx::test]
async fn cancel_races_pickup_exactly_one_wins(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let f = paid_order(&app).await;
    let app = std::sync::Arc::new(app);
    let (a1, pool, order_id, buyer) = (app.clone(), app.pool.clone(), f.order_id, f.buyer.clone());
    let cancel_task = tokio::spawn(async move { cancel(&a1, order_id, &buyer).await });
    let pickup_task =
        tokio::spawn(
            async move { mark_picked_up(&pool, order_id, OffsetDateTime::now_utc()).await },
        );
    let (c, p) = (cancel_task.await.unwrap(), pickup_task.await.unwrap());
    let status = status_of(&app, f.order_id).await;
    match (c.status, p.is_ok()) {
        (200, false) => assert!(matches!(status.as_str(), "refund_pending" | "refunded")),
        (409, true) => assert_eq!(status, "picked_up"),
        other => panic!("both or neither won: {other:?} status {status}"),
    }
    let refunds = count(&app, "SELECT count(*) FROM refunds").await;
    assert_eq!(refunds, i64::from(c.status == 200));
}

#[sqlx::test]
async fn pending_payment_order_hidden_from_seller_and_admin(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let f = paid_order(&app).await;
    sqlx::query("UPDATE orders SET status = 'pending_payment'")
        .execute(&app.pool)
        .await
        .unwrap();
    let path = format!("/orders/{}", f.order_id);
    assert_eq!(get(&app, &path, &f.seller).await.status, 404);
    let owner = app.owner_token().await;
    assert_eq!(get(&app, &path, &owner).await.status, 404);
    assert_eq!(get(&app, &path, &f.buyer).await.status, 200);
}

#[sqlx::test]
async fn bad_query_strings_use_the_json_envelope(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let f = paid_order(&app).await;
    for path in [
        "/orders?limit=abc".to_string(),
        format!("/stores/{}/orders?limit=abc", f.store_id),
    ] {
        let token = if path.starts_with("/stores") {
            &f.seller
        } else {
            &f.buyer
        };
        let res = get(&app, &path, token).await;
        assert_eq!(res.status, 422, "{path}");
        assert_eq!(res.json["error"]["code"], "validation_failed");
    }
}

#[sqlx::test]
async fn orders_paginate_with_keyset_cursor(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let f = paid_order(&app).await;
    let mut ids = vec![f.order_id];
    for _ in 0..4 {
        ids.push(buy(&app, &f.buyer, f.variant_id).await);
    }
    // Force ties on created_at across the page boundary.
    sqlx::query("UPDATE orders SET created_at = '2026-01-01T00:00:00Z'")
        .execute(&app.pool)
        .await
        .unwrap();

    let mut seen: Vec<String> = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..5 {
        let path = match &cursor {
            Some(c) => format!("/orders?limit=2&before={c}"),
            None => "/orders?limit=2".to_string(),
        };
        let res = get(&app, &path, &f.buyer).await;
        assert_eq!(res.status, 200, "{}", res.json);
        for it in res.json["items"].as_array().unwrap() {
            seen.push(it["id"].as_str().unwrap().to_string());
        }
        cursor = res.json["next_cursor"].as_str().map(str::to_string);
        if cursor.is_none() {
            break;
        }
    }
    let unique: BTreeSet<_> = seen.iter().cloned().collect();
    assert_eq!(seen.len(), 5, "no row skipped or repeated: {seen:?}");
    assert_eq!(unique.len(), 5);
    for id in ids {
        assert!(unique.contains(&id.to_string()));
    }

    for bad in [
        "/orders?limit=0",
        "/orders?limit=101",
        "/orders?before=notacursor",
    ] {
        assert_eq!(get(&app, bad, &f.buyer).await.status, 422, "{bad}");
    }
}
