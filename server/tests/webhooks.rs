mod common;

use std::time::Duration;

use axum::http::Method;
use common::{Listing, TestApp, TestResponse};
use serde_json::{Value, json};
use server::{
    checkout::{SettleOutcome, settle_payment},
    error::AppError,
    escrow::refund::{refund_order, send_refund},
    ledger::{self, Account, Entry},
    limits::MAX_STOCK,
    payments::{
        PaymentStatus, ProviderError, VerifyGate,
        fake::{FAKE_SIGNATURE_HEADER, FAKE_WEBHOOK_SECRET},
    },
};
use sqlx::PgPool;
use tracing_test::traced_test;
use uuid::Uuid;

const METHOD: &str = "mono_directpay";
const BUYER: &str = "08022222221";
const OTHER: &str = "08022222222";

struct Bought {
    checkout_id: Uuid,
    reference: String,
    total_kobo: i64,
    buyer_token: String,
    listing: Listing,
}

/// One store, one product at `price`, `stock` on hand; the buyer checks out `qty`.
async fn bought(app: &TestApp, price: i64, stock: i32, qty: i64) -> Bought {
    let listing = app
        .make_active_store_with_product("08011111111", "Mama Put", price, stock)
        .await;
    let (_, buyer_token) = app.sign_up_verified(BUYER).await;
    assert_eq!(
        app.set_cart(&buyer_token, listing.variant_id, qty)
            .await
            .status,
        200
    );
    let res = app.checkout(&buyer_token, METHOD).await;
    assert_eq!(res.status, 201, "{}", res.json);
    Bought {
        checkout_id: res.json["checkout_id"].as_str().unwrap().parse().unwrap(),
        reference: res.json["reference"].as_str().unwrap().to_string(),
        total_kobo: res.json["total_kobo"].as_i64().unwrap(),
        buyer_token,
        listing,
    }
}

fn success(reference: &str, amount_kobo: i64, fee_kobo: i64) -> PaymentStatus {
    PaymentStatus::Successful {
        amount_kobo,
        fee_kobo,
        currency: "NGN".into(),
        held: false,
        reference: reference.into(),
    }
}

fn hook_body(event_id: &str, reference: &str) -> Vec<u8> {
    json!({ "event_id": event_id, "kind": "payment", "reference": reference })
        .to_string()
        .into_bytes()
}

async fn webhook(app: &TestApp, event_id: &str, reference: &str) -> TestResponse {
    app.send_raw(
        Method::POST,
        "/webhooks/fake",
        &[(FAKE_SIGNATURE_HEADER, FAKE_WEBHOOK_SECRET)],
        hook_body(event_id, reference),
    )
    .await
}

async fn checkout_status(app: &TestApp, id: Uuid) -> String {
    sqlx::query_scalar("SELECT status::text FROM checkouts WHERE id = $1")
        .bind(id)
        .fetch_one(&app.pool)
        .await
        .unwrap()
}

async fn order_statuses(app: &TestApp, checkout_id: Uuid) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT status::text FROM orders WHERE checkout_id = $1 ORDER BY items_total_kobo",
    )
    .bind(checkout_id)
    .fetch_all(&app.pool)
    .await
    .unwrap()
}

/// (order_id, account, amount) in insertion order.
async fn ledger_rows(app: &TestApp) -> Vec<(Option<Uuid>, String, i64)> {
    sqlx::query_as("SELECT order_id, account::text, amount_kobo FROM ledger_entries ORDER BY id")
        .fetch_all(&app.pool)
        .await
        .unwrap()
}

/// None: no row; Some(processed).
async fn event_processed(app: &TestApp, event_id: &str) -> Option<bool> {
    sqlx::query_scalar(
        "SELECT processed_at IS NOT NULL FROM provider_events \
         WHERE provider = 'fake' AND event_id = $1",
    )
    .bind(event_id)
    .fetch_optional(&app.pool)
    .await
    .unwrap()
}

async fn count(app: &TestApp, sql: &str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(&app.pool).await.unwrap()
}

async fn expire(app: &TestApp, checkout_id: Uuid) {
    let mut tx = app.pool.begin().await.unwrap();
    assert!(
        server::checkout::expire_and_restore(&mut tx, checkout_id)
            .await
            .unwrap()
    );
    tx.commit().await.unwrap();
}

fn assert_status(res: &TestResponse, status: u16, body_status: &str) {
    assert_eq!(res.status, status, "{}", res.json);
    assert_eq!(res.json["status"], body_status, "{}", res.json);
}

fn assert_err(res: &TestResponse, status: u16, code: &str) {
    assert_eq!(res.status, status, "{}", res.json);
    assert_eq!(res.json["error"]["code"], code, "{}", res.json);
}

#[sqlx::test]
async fn valid_webhook_marks_paid(pool: PgPool) {
    let app = TestApp::new(pool).await;
    // Two stores: the collection fee is split across the orders by order total.
    let a = app
        .make_active_store_with_product("08011111111", "Mama Put", 30_000, 5)
        .await;
    let b = app
        .make_active_store_with_product("08011111112", "Bukka Hut", 70_000, 5)
        .await;
    let (_, buyer) = app.sign_up_verified(BUYER).await;
    app.set_cart(&buyer, a.variant_id, 1).await;
    app.set_cart(&buyer, b.variant_id, 1).await;
    let res = app.checkout(&buyer, METHOD).await;
    assert_eq!(res.status, 201, "{}", res.json);
    let checkout_id: Uuid = res.json["checkout_id"].as_str().unwrap().parse().unwrap();
    let reference = res.json["reference"].as_str().unwrap().to_string();
    // A line added after checkout is not part of it and stays in the cart.
    let other_product = app
        .add_product(&a.seller_token, a.store_id, 5_000, &[(json!({}), 3)])
        .await;
    let other_variant = app.variant_ids(other_product).await[0];
    app.set_cart(&buyer, other_variant, 1).await;

    app.fake
        .set_payment(&reference, success(&reference, 100_000, 1_000));
    let res = webhook(&app, "evt-1", &reference).await;
    assert_status(&res, 200, "received");

    assert_eq!(checkout_status(&app, checkout_id).await, "paid");
    assert_eq!(
        order_statuses(&app, checkout_id).await,
        vec!["paid", "paid"]
    );
    let (fee, flagged, paid): (Option<i64>, bool, bool) = sqlx::query_as(
        "SELECT collection_fee_kobo, provider_flagged, paid_at IS NOT NULL FROM checkouts",
    )
    .fetch_one(&app.pool)
    .await
    .unwrap();
    assert_eq!((fee, flagged, paid), (Some(1_000), false, true));
    // Fee shares by order total; deadlines snapshotted from the settings.
    let orders: Vec<(i64, i64, bool, bool, bool)> = sqlx::query_as(
        "SELECT o.items_total_kobo, o.collection_fee_share_kobo, o.paid_at IS NOT NULL, \
                o.ship_by = o.paid_at + make_interval(hours => s.ship_deadline_hours), \
                o.backstop_at = o.paid_at + make_interval(days => s.backstop_days) \
         FROM orders o CROSS JOIN platform_settings s ORDER BY o.items_total_kobo",
    )
    .fetch_all(&app.pool)
    .await
    .unwrap();
    assert_eq!(
        orders,
        vec![
            (30_000, 300, true, true, true),
            (70_000, 700, true, true, true)
        ]
    );

    // Ledger: per order buyer_funds -T, escrow +T, one balanced transaction.
    let rows = ledger_rows(&app).await;
    assert_eq!(rows.len(), 4, "{rows:?}");
    let mut accounts: Vec<(String, i64)> = rows.iter().map(|r| (r.1.clone(), r.2)).collect();
    accounts.sort();
    assert_eq!(
        accounts,
        vec![
            ("buyer_funds".into(), -70_000),
            ("buyer_funds".into(), -30_000),
            ("escrow".into(), 30_000),
            ("escrow".into(), 70_000),
        ]
    );
    assert_eq!(
        count(
            &app,
            "SELECT count(*) FROM (SELECT order_id FROM ledger_entries \
             GROUP BY order_id HAVING sum(amount_kobo) <> 0) x"
        )
        .await,
        0
    );
    assert_eq!(
        count(&app, "SELECT count(DISTINCT txn_id) FROM ledger_entries").await,
        1
    );
    assert_eq!(
        count(
            &app,
            "SELECT count(*) FROM ledger_entries WHERE checkout_id IS NULL OR order_id IS NULL"
        )
        .await,
        0
    );

    // Reserved stock is committed: on hand and reserved both drop.
    for l in [&a, &b] {
        assert_eq!(app.stock(l.variant_id).await, 4);
        assert_eq!(app.reserved(l.variant_id).await, 0);
    }
    // Cart: the paid lines are gone, the later line stays.
    let cart = app.request(Method::GET, "/cart", None, Some(&buyer)).await;
    let items = cart.json["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "{}", cart.json);
    assert_eq!(items[0]["variant_id"], other_variant.to_string());

    assert_eq!(event_processed(&app, "evt-1").await, Some(true));
    assert_eq!(
        count(
            &app,
            "SELECT count(*) FROM audit_log WHERE action = 'checkout.paid'"
        )
        .await,
        1
    );
    assert_eq!(app.fake.calls("verify_payment"), 1);
}

#[sqlx::test]
async fn duplicate_event_applies_once(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let b = bought(&app, 1_000, 5, 2).await;
    app.fake
        .set_payment(&b.reference, success(&b.reference, b.total_kobo, 10));

    assert_status(&webhook(&app, "evt-1", &b.reference).await, 200, "received");
    assert_status(
        &webhook(&app, "evt-1", &b.reference).await,
        200,
        "duplicate",
    );
    assert_eq!(ledger_rows(&app).await.len(), 2);
    assert_eq!(app.stock(b.listing.variant_id).await, 3);
    assert_eq!(app.fake.calls("verify_payment"), 1);

    // A new event for a checkout that is already paid makes no outbound call.
    assert_status(&webhook(&app, "evt-2", &b.reference).await, 200, "received");
    assert_eq!(app.fake.calls("verify_payment"), 1);
    assert_eq!(event_processed(&app, "evt-2").await, Some(true));
    assert_eq!(ledger_rows(&app).await.len(), 2);
}

#[sqlx::test]
async fn wrong_secret_is_401_no_change(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let b = bought(&app, 1_000, 5, 1).await;
    app.fake
        .set_payment(&b.reference, success(&b.reference, b.total_kobo, 10));
    for headers in [
        vec![(FAKE_SIGNATURE_HEADER, "wrong-secret")],
        vec![(FAKE_SIGNATURE_HEADER, "test-secret-and-more")],
        vec![],
    ] {
        let res = app
            .send_raw(
                Method::POST,
                "/webhooks/fake",
                &headers,
                hook_body("evt-1", &b.reference),
            )
            .await;
        assert_err(&res, 401, "invalid_signature");
    }
    assert_eq!(count(&app, "SELECT count(*) FROM provider_events").await, 0);
    assert_eq!(checkout_status(&app, b.checkout_id).await, "pending");
    assert!(ledger_rows(&app).await.is_empty());
    assert_eq!(app.fake.calls("verify_payment"), 0);
}

#[sqlx::test]
async fn provider_not_confirming_changes_nothing(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let b = bought(&app, 1_000, 5, 1).await;
    // The fake's default verify answer is Pending.
    assert_status(&webhook(&app, "evt-1", &b.reference).await, 200, "received");
    assert_eq!(checkout_status(&app, b.checkout_id).await, "pending");
    assert_eq!(
        order_statuses(&app, b.checkout_id).await,
        vec!["pending_payment"]
    );
    assert!(ledger_rows(&app).await.is_empty());
    assert_eq!(app.reserved(b.listing.variant_id).await, 1);
    assert_eq!(app.stock(b.listing.variant_id).await, 5);
    // Not processed: a retry of this event is settled again.
    assert_eq!(event_processed(&app, "evt-1").await, Some(false));
}

#[sqlx::test]
async fn verify_failed_marks_event_processed(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let b = bought(&app, 1_000, 5, 1).await;
    app.fake.set_payment(&b.reference, PaymentStatus::Failed);
    assert_status(&webhook(&app, "evt-1", &b.reference).await, 200, "received");
    assert_eq!(checkout_status(&app, b.checkout_id).await, "pending");
    assert!(ledger_rows(&app).await.is_empty());
    assert_eq!(event_processed(&app, "evt-1").await, Some(true));
}

#[sqlx::test]
async fn amount_mismatch_flagged(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let b = bought(&app, 1_000, 5, 1).await;
    assert_eq!(b.total_kobo, 1_000);
    app.fake
        .set_payment(&b.reference, success(&b.reference, 999, 10));
    assert_status(&webhook(&app, "evt-1", &b.reference).await, 200, "received");
    assert_eq!(
        checkout_status(&app, b.checkout_id).await,
        "amount_mismatch"
    );
    // I2: the orders leave `pending_payment` (which has no other exit) in the
    // same transaction; the money is the owner's resolve/refund flow.
    assert_eq!(
        order_statuses(&app, b.checkout_id).await,
        vec!["cancelled"]
    );
    let cancelled: Vec<(Uuid, Value)> = sqlx::query_as(
        "SELECT subject_id, detail FROM audit_log WHERE action = 'order.cancelled' \
         AND subject_type = 'order'",
    )
    .fetch_all(&app.pool)
    .await
    .unwrap();
    assert_eq!(cancelled.len(), 1);
    assert_eq!(cancelled[0].1["reason"], "amount_mismatch");
    assert_eq!(cancelled[0].1["checkout_id"], b.checkout_id.to_string());
    let cancelled_at: bool = sqlx::query_scalar(
        "SELECT bool_and(cancelled_at IS NOT NULL) FROM orders WHERE checkout_id = $1",
    )
    .bind(b.checkout_id)
    .fetch_one(&app.pool)
    .await
    .unwrap();
    assert!(cancelled_at);
    assert!(ledger_rows(&app).await.is_empty());
    assert_eq!(app.stock(b.listing.variant_id).await, 5);
    // The reservation is released (the owner refunds it later).
    assert_eq!(app.reserved(b.listing.variant_id).await, 0);
    let detail: Value = sqlx::query_scalar(
        "SELECT detail FROM audit_log WHERE action = 'checkout.amount_mismatch' AND subject_id = $1",
    )
    .bind(b.checkout_id)
    .fetch_one(&app.pool)
    .await
    .unwrap();
    assert_eq!(detail["expected_kobo"], 1_000);
    assert_eq!(detail["received_kobo"], 999);
    assert_eq!(detail["reason"], "amount");
    assert_eq!(event_processed(&app, "evt-1").await, Some(true));
}

#[sqlx::test]
async fn wrong_currency_is_amount_mismatch(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let b = bought(&app, 1_000, 5, 1).await;
    app.fake.set_payment(
        &b.reference,
        PaymentStatus::Successful {
            amount_kobo: b.total_kobo,
            fee_kobo: 10,
            currency: "USD".into(),
            held: false,
            reference: b.reference.clone(),
        },
    );
    assert_status(&webhook(&app, "evt-1", &b.reference).await, 200, "received");
    assert_eq!(
        checkout_status(&app, b.checkout_id).await,
        "amount_mismatch"
    );
    assert!(ledger_rows(&app).await.is_empty());
}

#[sqlx::test]
async fn verify_for_other_reference_settles_nothing(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let b = bought(&app, 1_000, 5, 1).await;
    // Webhook names reference A; the provider's verify answer is about reference B.
    app.fake.set_payment(
        &b.reference,
        success("CKBBBBBBBBBBBBBBBBBBBB", b.total_kobo, 10),
    );
    assert_status(&webhook(&app, "evt-1", &b.reference).await, 200, "received");
    assert_ne!(checkout_status(&app, b.checkout_id).await, "paid");
    // Flagged as a mismatch: its orders are cancelled, never paid.
    assert_eq!(
        order_statuses(&app, b.checkout_id).await,
        vec!["cancelled"]
    );
    assert!(ledger_rows(&app).await.is_empty());
    assert_eq!(app.stock(b.listing.variant_id).await, 5);
}

#[sqlx::test]
async fn unknown_reference_makes_no_outbound_call(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let b = bought(&app, 1_000, 5, 1).await;
    // Exact match only: a lower-cased or unknown reference is unknown.
    for (evt, reference) in [
        ("evt-1", "CKNOSUCHREFERENCE00000".to_string()),
        ("evt-2", b.reference.to_lowercase()),
    ] {
        assert_status(&webhook(&app, evt, &reference).await, 200, "received");
        assert_eq!(event_processed(&app, evt).await, Some(true));
    }
    assert_eq!(app.fake.calls("verify_payment"), 0);
    assert_eq!(checkout_status(&app, b.checkout_id).await, "pending");
}

#[sqlx::test]
async fn held_payment_marks_paid_and_flags(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let b = bought(&app, 1_000, 5, 1).await;
    app.fake.set_payment(
        &b.reference,
        PaymentStatus::Successful {
            amount_kobo: b.total_kobo,
            fee_kobo: 10,
            currency: "NGN".into(),
            held: true,
            reference: b.reference.clone(),
        },
    );
    assert_status(&webhook(&app, "evt-1", &b.reference).await, 200, "received");
    assert_eq!(checkout_status(&app, b.checkout_id).await, "paid");
    let flagged: bool = sqlx::query_scalar("SELECT provider_flagged FROM checkouts")
        .fetch_one(&app.pool)
        .await
        .unwrap();
    assert!(flagged);
    assert_eq!(
        count(
            &app,
            "SELECT count(*) FROM audit_log WHERE action = 'checkout.provider_flagged'"
        )
        .await,
        1
    );
}

#[sqlx::test]
async fn paid_after_expiry_rereserves_when_stock_allows(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let b = bought(&app, 1_000, 5, 2).await;
    expire(&app, b.checkout_id).await;
    assert_eq!(
        (
            app.stock(b.listing.variant_id).await,
            app.reserved(b.listing.variant_id).await
        ),
        (5, 0)
    );
    app.fake
        .set_payment(&b.reference, success(&b.reference, b.total_kobo, 10));
    assert_status(&webhook(&app, "evt-1", &b.reference).await, 200, "received");

    assert_eq!(checkout_status(&app, b.checkout_id).await, "paid");
    let orders: Vec<(String, bool, bool)> = sqlx::query_as(
        "SELECT status::text, cancelled_at IS NULL, ship_by IS NOT NULL FROM orders",
    )
    .fetch_all(&app.pool)
    .await
    .unwrap();
    assert_eq!(orders, vec![("paid".into(), true, true)]);
    // Taken straight from on-hand stock; the released reservation is not touched again.
    assert_eq!(app.stock(b.listing.variant_id).await, 3);
    assert_eq!(app.reserved(b.listing.variant_id).await, 0);
    assert_eq!(ledger_rows(&app).await.len(), 2);
}

#[sqlx::test]
async fn paid_after_expiry_refunds_when_stock_gone(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let b = bought(&app, 1_000, 5, 2).await;
    expire(&app, b.checkout_id).await;
    // Someone else bought most of it meanwhile.
    sqlx::query("UPDATE product_variants SET stock = 1 WHERE id = $1")
        .bind(b.listing.variant_id)
        .execute(&app.pool)
        .await
        .unwrap();
    app.fake
        .set_payment(&b.reference, success(&b.reference, b.total_kobo, 10));
    assert_status(&webhook(&app, "evt-1", &b.reference).await, 200, "received");

    assert_eq!(checkout_status(&app, b.checkout_id).await, "refunded");
    // The fake does not support refunds: the owner refunds by hand, and the
    // order stays refund_pending until the owner marks the refund done.
    assert_eq!(
        order_statuses(&app, b.checkout_id).await,
        vec!["refund_pending"]
    );
    let (reference, amount, status): (String, i64, String) =
        sqlx::query_as("SELECT reference, amount_kobo, status::text FROM refunds")
            .fetch_one(&app.pool)
            .await
            .unwrap();
    assert!(
        reference.starts_with("RF") && reference.len() == 22,
        "{reference}"
    );
    assert_eq!((amount, status.as_str()), (2_000, "manual"));
    let sent = app.fake.refunds();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].reference, reference);
    assert_eq!(sent[0].payment_reference, b.reference);
    assert_eq!(sent[0].amount_kobo, 2_000);

    // Stock is neither committed nor restored.
    assert_eq!(app.stock(b.listing.variant_id).await, 1);
    assert_eq!(app.reserved(b.listing.variant_id).await, 0);
    // Payment in, then refund out.
    let rows: Vec<(String, i64)> = ledger_rows(&app)
        .await
        .into_iter()
        .map(|r| (r.1, r.2))
        .collect();
    assert_eq!(
        rows,
        vec![
            ("buyer_funds".into(), -2_000),
            ("escrow".into(), 2_000),
            ("escrow".into(), -2_000),
            ("refunds".into(), 2_000),
            // I3: the collection fee the platform absorbs on a refund.
            ("provider_fees".into(), 10),
            ("platform_revenue".into(), -10),
        ]
    );
    // The buyer did not get the items: the cart keeps them.
    let cart = app
        .request(Method::GET, "/cart", None, Some(&b.buyer_token))
        .await;
    assert_eq!(cart.json["items"].as_array().unwrap().len(), 1);
    assert_eq!(event_processed(&app, "evt-1").await, Some(true));
}

#[sqlx::test]
async fn poll_settles_without_webhook(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let b = bought(&app, 1_000, 5, 1).await;
    let path = format!("/checkout/{}", b.checkout_id);

    // Someone else's poll is 404 and triggers no provider call.
    let (_, other) = app.sign_up_verified(OTHER).await;
    app.fake
        .set_payment(&b.reference, success(&b.reference, b.total_kobo, 10));
    assert_eq!(
        app.request(Method::GET, &path, None, Some(&other))
            .await
            .status,
        404
    );
    assert_eq!(app.fake.calls("verify_payment"), 0);

    let res = app
        .request(Method::GET, &path, None, Some(&b.buyer_token))
        .await;
    assert_eq!(res.status, 200, "{}", res.json);
    assert_eq!(res.json["status"], "paid");
    assert_eq!(res.json["orders"][0]["status"], "paid");
    assert_eq!(ledger_rows(&app).await.len(), 2);
    assert_eq!(app.stock(b.listing.variant_id).await, 4);

    // Paid: later polls make no provider call.
    app.request(Method::GET, &path, None, Some(&b.buyer_token))
        .await;
    assert_eq!(app.fake.calls("verify_payment"), 1);
}

#[sqlx::test]
async fn verify_cooldown_limits_outbound_calls(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let b = bought(&app, 1_000, 5, 1).await;
    let path = format!("/checkout/{}", b.checkout_id);

    let res = app
        .request(Method::GET, &path, None, Some(&b.buyer_token))
        .await;
    assert_eq!(res.json["status"], "pending");
    assert_eq!(app.fake.calls("verify_payment"), 1);

    // Within the cooldown: the poll shows the current status, the webhook asks for a retry.
    app.fake
        .set_payment(&b.reference, success(&b.reference, b.total_kobo, 10));
    let res = app
        .request(Method::GET, &path, None, Some(&b.buyer_token))
        .await;
    assert_eq!(res.status, 200);
    assert_eq!(res.json["status"], "pending");
    assert_err(
        &webhook(&app, "evt-1", &b.reference).await,
        503,
        "verify_unavailable",
    );
    assert_eq!(app.fake.calls("verify_payment"), 1);
    assert_eq!(event_processed(&app, "evt-1").await, Some(false));

    app.state.verify_gate.clear();
    assert_status(&webhook(&app, "evt-1", &b.reference).await, 200, "received");
    assert_eq!(checkout_status(&app, b.checkout_id).await, "paid");
}

#[sqlx::test]
async fn verify_unavailable_then_retry_applies(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let b = bought(&app, 1_000, 5, 1).await;
    app.fake
        .set_payment(&b.reference, success(&b.reference, b.total_kobo, 10));
    app.fake.verify_unavailable_n(1);

    assert_err(
        &webhook(&app, "evt-1", &b.reference).await,
        503,
        "verify_unavailable",
    );
    assert_eq!(event_processed(&app, "evt-1").await, Some(false));
    assert_eq!(checkout_status(&app, b.checkout_id).await, "pending");

    // The provider retries the same event later (after the cooldown).
    app.state.verify_gate.clear();
    assert_status(&webhook(&app, "evt-1", &b.reference).await, 200, "received");
    assert_eq!(checkout_status(&app, b.checkout_id).await, "paid");
    assert_eq!(event_processed(&app, "evt-1").await, Some(true));
    assert_eq!(ledger_rows(&app).await.len(), 2);
}

#[sqlx::test]
async fn verify_concurrency_cap_returns_503(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let b = bought(&app, 1_000, 5, 1).await;
    app.fake
        .set_payment(&b.reference, success(&b.reference, b.total_kobo, 10));
    let permits: Vec<_> = (0..8)
        .map(|_| app.state.verify_gate.try_permit().expect("free permit"))
        .collect();
    assert!(app.state.verify_gate.try_permit().is_none());

    assert_err(
        &webhook(&app, "evt-1", &b.reference).await,
        503,
        "verify_unavailable",
    );
    assert_eq!(app.fake.calls("verify_payment"), 0);
    assert_eq!(event_processed(&app, "evt-1").await, Some(false));

    drop(permits);
    assert_status(&webhook(&app, "evt-1", &b.reference).await, 200, "received");
    assert_eq!(checkout_status(&app, b.checkout_id).await, "paid");
}

#[sqlx::test]
async fn concurrent_webhooks_and_poll_apply_once(pool: PgPool) {
    let app = TestApp::with_verify_gate(pool, VerifyGate::new(Duration::ZERO, 8)).await;
    let b = bought(&app, 1_000, 5, 2).await;
    app.fake
        .set_payment(&b.reference, success(&b.reference, b.total_kobo, 10));

    let (r1, r2, settled) = tokio::join!(
        webhook(&app, "evt-1", &b.reference),
        webhook(&app, "evt-2", &b.reference),
        settle_payment(&app.state, &b.reference),
    );
    assert_eq!(r1.status, 200, "{}", r1.json);
    assert_eq!(r2.status, 200, "{}", r2.json);
    assert!(
        matches!(
            settled,
            Ok(SettleOutcome::Paid | SettleOutcome::AlreadySettled)
        ),
        "{settled:?}"
    );
    assert_eq!(checkout_status(&app, b.checkout_id).await, "paid");
    assert_eq!(ledger_rows(&app).await.len(), 2);
    assert_eq!(app.stock(b.listing.variant_id).await, 3);
    assert_eq!(app.reserved(b.listing.variant_id).await, 0);
    assert_eq!(
        count(
            &app,
            "SELECT count(*) FROM audit_log WHERE action = 'checkout.paid'"
        )
        .await,
        1
    );
}

#[sqlx::test]
async fn disabled_method_still_settles(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let b = bought(&app, 1_000, 5, 1).await;
    // The owner switches the checkout's method off after the buyer paid.
    sqlx::query("UPDATE platform_settings SET enabled_methods = '{paystack_card}'")
        .execute(&app.pool)
        .await
        .unwrap();
    app.fake
        .set_payment(&b.reference, success(&b.reference, b.total_kobo, 10));
    assert_status(&webhook(&app, "evt-1", &b.reference).await, 200, "received");
    assert_eq!(checkout_status(&app, b.checkout_id).await, "paid");
}

#[sqlx::test]
async fn unknown_provider_is_404(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let res = app
        .send_raw(
            Method::POST,
            "/webhooks/nosuch",
            &[(FAKE_SIGNATURE_HEADER, FAKE_WEBHOOK_SECRET)],
            hook_body("evt-1", "CK1"),
        )
        .await;
    assert_err(&res, 404, "not_found");
    assert_eq!(count(&app, "SELECT count(*) FROM provider_events").await, 0);
}

#[sqlx::test]
async fn oversized_body_is_413_and_stores_nothing(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let b = bought(&app, 1_000, 5, 1).await;
    app.fake
        .set_payment(&b.reference, success(&b.reference, b.total_kobo, 10));
    let mut body = json!({
        "event_id": "evt-1", "kind": "payment", "reference": b.reference,
        "padding": "x".repeat(70 * 1024)
    })
    .to_string()
    .into_bytes();
    let res = app
        .send_raw(
            Method::POST,
            "/webhooks/fake",
            &[(FAKE_SIGNATURE_HEADER, FAKE_WEBHOOK_SECRET)],
            body.clone(),
        )
        .await;
    assert_err(&res, 413, "payload_too_large");
    // Also when the declared length is honest but large.
    let len = body.len().to_string();
    let res = app
        .send_raw(
            Method::POST,
            "/webhooks/fake",
            &[
                (FAKE_SIGNATURE_HEADER, FAKE_WEBHOOK_SECRET),
                ("content-length", &len),
            ],
            std::mem::take(&mut body),
        )
        .await;
    assert_err(&res, 413, "payload_too_large");
    assert_eq!(count(&app, "SELECT count(*) FROM provider_events").await, 0);
    assert_eq!(app.fake.calls("verify_webhook"), 0);
    assert_eq!(checkout_status(&app, b.checkout_id).await, "pending");
}

#[sqlx::test]
async fn malformed_or_other_events_are_ignored(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let signed = [(FAKE_SIGNATURE_HEADER, FAKE_WEBHOOK_SECRET)];
    for body in [
        b"not json".to_vec(),
        json!({"event_id": "evt-1", "kind": "payment"})
            .to_string()
            .into_bytes(),
        json!({"event_id": "evt-2", "kind": "other"})
            .to_string()
            .into_bytes(),
    ] {
        let res = app
            .send_raw(Method::POST, "/webhooks/fake", &signed, body)
            .await;
        assert_status(&res, 200, "ignored");
    }
    assert_eq!(count(&app, "SELECT count(*) FROM provider_events").await, 0);
    assert_eq!(app.fake.calls("verify_payment"), 0);
}

#[traced_test]
#[sqlx::test]
async fn webhook_never_logs_body(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let b = bought(&app, 1_000, 5, 1).await;
    app.fake
        .set_payment(&b.reference, success(&b.reference, b.total_kobo, 10));
    let body = json!({
        "event_id": "evt-SENTINELEVT", "kind": "payment", "reference": b.reference,
        "account": "SENTINEL0123456789"
    })
    .to_string()
    .into_bytes();
    for secret in ["wrong", FAKE_WEBHOOK_SECRET] {
        app.send_raw(
            Method::POST,
            "/webhooks/fake",
            &[(FAKE_SIGNATURE_HEADER, secret)],
            body.clone(),
        )
        .await;
    }
    assert_eq!(checkout_status(&app, b.checkout_id).await, "paid");
    assert!(logs_contain("webhook"));
    assert!(!logs_contain("SENTINEL"));
    assert!(!logs_contain(&b.reference));
}

#[sqlx::test]
async fn ledger_is_append_only(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let b = bought(&app, 1_000, 5, 1).await;
    app.fake
        .set_payment(&b.reference, success(&b.reference, b.total_kobo, 10));
    assert_status(&webhook(&app, "evt-1", &b.reference).await, 200, "received");
    assert_eq!(ledger_rows(&app).await.len(), 2);

    for sql in [
        "UPDATE ledger_entries SET amount_kobo = amount_kobo * 2",
        "DELETE FROM ledger_entries",
        "TRUNCATE ledger_entries",
    ] {
        let err = sqlx::query(sql).execute(&app.pool).await.unwrap_err();
        assert!(err.to_string().contains("append-only"), "{sql}: {err}");
    }
    // ALWAYS triggers: replication mode does not switch them off.
    let mut conn = app.pool.acquire().await.unwrap();
    sqlx::query("SET session_replication_role = replica")
        .execute(&mut *conn)
        .await
        .unwrap();
    let res = sqlx::query("DELETE FROM ledger_entries")
        .execute(&mut *conn)
        .await;
    sqlx::query("SET session_replication_role = origin")
        .execute(&mut *conn)
        .await
        .unwrap();
    assert!(res.is_err());
    assert_eq!(ledger_rows(&app).await.len(), 2);
}

#[sqlx::test]
async fn unbalanced_raw_insert_fails_at_commit(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let txn = Uuid::new_v4();
    let mut tx = app.pool.begin().await.unwrap();
    // The check is deferred: the insert itself succeeds, the commit fails.
    sqlx::query(
        "INSERT INTO ledger_entries (txn_id, account, amount_kobo, kind) \
         VALUES ($1, 'escrow', 100, 'test')",
    )
    .bind(txn)
    .execute(&mut *tx)
    .await
    .unwrap();
    let err = tx.commit().await.unwrap_err();
    assert!(err.to_string().contains("does not balance"), "{err}");
    assert!(ledger_rows(&app).await.is_empty());

    // A balanced raw set commits.
    let mut tx = app.pool.begin().await.unwrap();
    sqlx::query(
        "INSERT INTO ledger_entries (txn_id, account, amount_kobo, kind) \
         VALUES ($1, 'escrow', 100, 'test'), ($1, 'buyer_funds', -100, 'test')",
    )
    .bind(txn)
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(ledger_rows(&app).await.len(), 2);
}

#[sqlx::test]
async fn ledger_post_rejects_unbalanced_and_empty(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let entry = |account, amount_kobo| Entry {
        order_id: None,
        checkout_id: None,
        account,
        amount_kobo,
        kind: "test",
        reference: None,
    };
    let mut tx = app.pool.begin().await.unwrap();
    assert!(ledger::post(&mut tx, ("test", "k1"), &[]).await.is_err());
    assert!(
        ledger::post(
            &mut tx,
            ("test", "k2"),
            &[entry(Account::Escrow, 100), entry(Account::BuyerFunds, -99)]
        )
        .await
        .is_err()
    );
    assert!(
        ledger::post(
            &mut tx,
            ("test", "k3"),
            &[entry(Account::Escrow, i64::MAX), entry(Account::Escrow, 1)]
        )
        .await
        .is_err()
    );
    ledger::post(
        &mut tx,
        ("test", "k4"),
        &[
            entry(Account::Escrow, 100),
            entry(Account::BuyerFunds, -100),
        ],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(ledger_rows(&app).await.len(), 2);
}

/// A paid single-order checkout; returns (bought, order id).
async fn paid_order(app: &TestApp, qty: i64) -> (Bought, Uuid) {
    let b = bought(app, 1_000, 5, qty).await;
    app.fake
        .set_payment(&b.reference, success(&b.reference, b.total_kobo, 10));
    assert_status(
        &webhook(app, "evt-pay", &b.reference).await,
        200,
        "received",
    );
    let order_id: Uuid = sqlx::query_scalar("SELECT id FROM orders WHERE checkout_id = $1")
        .bind(b.checkout_id)
        .fetch_one(&app.pool)
        .await
        .unwrap();
    (b, order_id)
}

async fn refund_row(app: &TestApp) -> (String, String, Option<String>, i32) {
    sqlx::query_as("SELECT reference, status::text, last_error, attempts FROM refunds")
        .fetch_one(&app.pool)
        .await
        .unwrap()
}

async fn order_status(app: &TestApp, order_id: Uuid) -> String {
    sqlx::query_scalar("SELECT status::text FROM orders WHERE id = $1")
        .bind(order_id)
        .fetch_one(&app.pool)
        .await
        .unwrap()
}

#[sqlx::test]
async fn refund_order_restores_stock_and_takes_no_cut(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let (b, order_id) = paid_order(&app, 2).await;
    assert_eq!(app.stock(b.listing.variant_id).await, 3);
    app.fake.set_refund_supported(true);

    let out = refund_order(&app.state, order_id, true).await.unwrap();
    assert_eq!(out.status, "processing");
    assert_eq!(order_status(&app, order_id).await, "refunded");
    let (reference, status, _, _) = refund_row(&app).await;
    assert_eq!(status, "processing");
    assert_eq!(out.reference, reference);
    let provider_id: Option<String> = sqlx::query_scalar("SELECT provider_refund_id FROM refunds")
        .fetch_one(&app.pool)
        .await
        .unwrap();
    assert_eq!(provider_id, Some(format!("fakerf_{}", b.reference)));
    // Items + delivery, nothing kept back.
    let sent = app.fake.refunds();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].amount_kobo, 2_000);
    assert_eq!(app.stock(b.listing.variant_id).await, 5);
    let rows: Vec<(String, i64)> = ledger_rows(&app)
        .await
        .into_iter()
        .map(|r| (r.1, r.2))
        .collect();
    assert_eq!(
        rows[2..],
        [
            ("escrow".to_string(), -2_000),
            ("refunds".to_string(), 2_000),
            // I3: the provider kept its 10 kobo fee; the platform absorbs it.
            ("provider_fees".to_string(), 10),
            ("platform_revenue".to_string(), -10),
        ]
    );
    assert_eq!(
        count(
            &app,
            "SELECT count(*) FROM (SELECT order_id FROM ledger_entries \
             GROUP BY order_id HAVING sum(amount_kobo) <> 0) x"
        )
        .await,
        0
    );

    // Refunding again is a no-op.
    let again = refund_order(&app.state, order_id, true).await.unwrap();
    assert_eq!(again.status, "processing");
    assert_eq!(app.fake.calls("refund"), 1);
    assert_eq!(app.stock(b.listing.variant_id).await, 5);
    // Payment, refund and the absorbed fee: two entries each.
    assert_eq!(ledger_rows(&app).await.len(), 6);
}

#[sqlx::test]
async fn refund_not_supported_goes_manual(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let (_, order_id) = paid_order(&app, 1).await;
    let out = refund_order(&app.state, order_id, false).await.unwrap();
    assert_eq!(out.status, "manual");
    // The buyer has not been paid back yet: the owner refunds by hand and then
    // marks the refund done (Task 10), which completes the order.
    assert_eq!(order_status(&app, order_id).await, "refund_pending");
    assert_eq!(refund_row(&app).await.1, "manual");
}

#[sqlx::test]
async fn send_refund_refuses_a_refund_out_of_attempts(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let (_, order_id) = paid_order(&app, 1).await;
    app.fake.set_refund_supported(true);
    app.fake
        .fail_refund_with(ProviderError::Unavailable(anyhow::anyhow!("down")));
    let first = refund_order(&app.state, order_id, false).await.unwrap();
    assert_eq!(first.status, "requested");
    // Used up its sends (the sweeper escalates it to manual on its next tick).
    sqlx::query("UPDATE refunds SET attempts = 8")
        .execute(&app.pool)
        .await
        .unwrap();
    app.fake.clear_refund_failure();
    let again = send_refund(&app.state, first.refund_id).await.unwrap();
    assert_eq!(again.status, "requested");
    assert_eq!(app.fake.calls("refund"), 1, "no 9th provider call");
    assert_eq!(refund_row(&app).await.3, 8);
    // A buyer re-cancel goes through the same claim.
    let again = refund_order(&app.state, order_id, false).await.unwrap();
    assert_eq!(again.status, "requested");
    assert_eq!(app.fake.calls("refund"), 1);
}

#[sqlx::test]
async fn refund_unavailable_retries_with_same_reference(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let (b, order_id) = paid_order(&app, 2).await;
    app.fake.set_refund_supported(true);
    app.fake
        .fail_refund_with(ProviderError::Unavailable(anyhow::anyhow!("down")));

    let out = refund_order(&app.state, order_id, true).await.unwrap();
    assert_eq!(out.status, "requested");
    assert_eq!(order_status(&app, order_id).await, "refund_pending");
    let (reference, status, last_error, attempts) = refund_row(&app).await;
    assert_eq!(status, "requested");
    assert!(last_error.is_some());
    assert_eq!(attempts, 1);
    assert_eq!(app.stock(b.listing.variant_id).await, 5);

    app.fake.clear_refund_failure();
    let out = refund_order(&app.state, order_id, true).await.unwrap();
    assert_eq!(out.status, "processing");
    assert_eq!(order_status(&app, order_id).await, "refunded");
    let sent = app.fake.refunds();
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0].reference, reference);
    assert_eq!(sent[1].reference, reference);
    // Booked and restocked once.
    assert_eq!(app.stock(b.listing.variant_id).await, 5);
    // Payment, refund and the absorbed fee: two entries each.
    assert_eq!(ledger_rows(&app).await.len(), 6);
    assert_eq!(count(&app, "SELECT count(*) FROM refunds").await, 1);
}

#[sqlx::test]
async fn refund_rejects_released_or_unpaid_orders(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let (_, order_id) = paid_order(&app, 1).await;
    sqlx::query("UPDATE orders SET status = 'completed' WHERE id = $1")
        .bind(order_id)
        .execute(&app.pool)
        .await
        .unwrap();
    let err = refund_order(&app.state, order_id, false).await.unwrap_err();
    assert!(
        matches!(err, AppError::Conflict("already_released")),
        "{err:?}"
    );

    let pending: Uuid = {
        let (_, buyer) = app.sign_up_verified(OTHER).await;
        let variant: Uuid = sqlx::query_scalar("SELECT id FROM product_variants")
            .fetch_one(&app.pool)
            .await
            .unwrap();
        app.set_cart(&buyer, variant, 1).await;
        let res = app.checkout(&buyer, METHOD).await;
        assert_eq!(res.status, 201, "{}", res.json);
        res.json["orders"][0]["order_id"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap()
    };
    let err = refund_order(&app.state, pending, false).await.unwrap_err();
    assert!(
        matches!(err, AppError::Conflict("not_refundable")),
        "{err:?}"
    );
    assert!(matches!(
        refund_order(&app.state, Uuid::new_v4(), false).await,
        Err(AppError::NotFound)
    ));
    assert_eq!(count(&app, "SELECT count(*) FROM refunds").await, 0);
    assert_eq!(app.fake.calls("refund"), 0);
}

#[sqlx::test]
async fn fee_out_of_range_is_amount_mismatch(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let b = bought(&app, 1_000, 5, 1).await;
    // The amount matches, but the provider claims a fee larger than the payment.
    app.fake
        .set_payment(&b.reference, success(&b.reference, b.total_kobo, 1_001));
    assert_status(&webhook(&app, "evt-1", &b.reference).await, 200, "received");
    assert_eq!(
        checkout_status(&app, b.checkout_id).await,
        "amount_mismatch"
    );
    assert!(ledger_rows(&app).await.is_empty());
    assert_eq!(app.reserved(b.listing.variant_id).await, 0);
    let reason: String = sqlx::query_scalar(
        "SELECT detail->>'reason' FROM audit_log WHERE action = 'checkout.amount_mismatch'",
    )
    .fetch_one(&app.pool)
    .await
    .unwrap();
    assert_eq!(reason, "fee_out_of_range");
}

#[sqlx::test]
async fn negative_fee_is_amount_mismatch(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let b = bought(&app, 1_000, 5, 1).await;
    app.fake
        .set_payment(&b.reference, success(&b.reference, b.total_kobo, -1));
    assert_status(&webhook(&app, "evt-1", &b.reference).await, 200, "received");
    assert_eq!(
        checkout_status(&app, b.checkout_id).await,
        "amount_mismatch"
    );
    assert!(ledger_rows(&app).await.is_empty());
}

#[sqlx::test]
async fn concurrent_same_event_applies_once(pool: PgPool) {
    let app = TestApp::with_verify_gate(pool, VerifyGate::new(Duration::ZERO, 8)).await;
    let b = bought(&app, 1_000, 5, 2).await;
    app.fake
        .set_payment(&b.reference, success(&b.reference, b.total_kobo, 10));
    let (r1, r2) = tokio::join!(
        webhook(&app, "evt-1", &b.reference),
        webhook(&app, "evt-1", &b.reference),
    );
    assert_eq!(r1.status, 200, "{}", r1.json);
    assert_eq!(r2.status, 200, "{}", r2.json);
    assert_eq!(checkout_status(&app, b.checkout_id).await, "paid");
    assert_eq!(ledger_rows(&app).await.len(), 2);
    assert_eq!(app.stock(b.listing.variant_id).await, 3);
    assert_eq!(count(&app, "SELECT count(*) FROM provider_events").await, 1);
    assert_eq!(event_processed(&app, "evt-1").await, Some(true));
    assert_eq!(
        count(
            &app,
            "SELECT count(*) FROM audit_log WHERE action = 'checkout.paid'"
        )
        .await,
        1
    );
}

#[sqlx::test]
async fn refund_restore_clamps_at_stock_cap(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let (b, order_id) = paid_order(&app, 2).await;
    // The seller restocked to one below the cap meanwhile.
    sqlx::query("UPDATE product_variants SET stock = $2 WHERE id = $1")
        .bind(b.listing.variant_id)
        .bind(MAX_STOCK - 1)
        .execute(&app.pool)
        .await
        .unwrap();
    let out = refund_order(&app.state, order_id, true).await.unwrap();
    assert_eq!(out.status, "manual");
    assert_eq!(app.stock(b.listing.variant_id).await, MAX_STOCK);
    let clamped: i64 = sqlx::query_scalar(
        "SELECT (detail->>'clamped_units')::bigint FROM audit_log \
         WHERE action = 'order.refund_requested'",
    )
    .fetch_one(&app.pool)
    .await
    .unwrap();
    assert_eq!(clamped, 1);
    // Payment, refund and the absorbed fee: two entries each.
    assert_eq!(ledger_rows(&app).await.len(), 6);
}

#[sqlx::test]
async fn refund_double_claim_cannot_overwrite(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let (_, order_id) = paid_order(&app, 1).await;
    app.fake.set_refund_supported(true);
    app.fake
        .fail_refund_with(ProviderError::Unavailable(anyhow::anyhow!("down")));
    let first = refund_order(&app.state, order_id, false).await.unwrap();
    assert_eq!(first.status, "requested");

    // Sender A claims and gets a slow "unavailable" answer.
    app.fake.delay_next_refund(Duration::from_millis(800));
    let state = app.state.clone();
    let refund_id = first.refund_id;
    let a = tokio::spawn(async move { send_refund(&state, refund_id).await });
    let mut claimed = false;
    for _ in 0..200 {
        tokio::time::sleep(Duration::from_millis(10)).await;
        if app.fake.calls("refund") == 2 {
            claimed = true;
            break;
        }
    }
    assert!(claimed, "sender A never called the provider");

    // A's claim expires; sender B claims afresh and succeeds.
    sqlx::query("UPDATE refunds SET sending_until = now() - interval '1 second'")
        .execute(&app.pool)
        .await
        .unwrap();
    app.fake.clear_refund_failure();
    let b = send_refund(&app.state, refund_id).await.unwrap();
    assert_eq!(b.status, "processing");

    // A's late answer does not overwrite B's.
    let a = a.await.unwrap().unwrap();
    assert_eq!(a.status, "processing");
    let (status, last_error, provider_id): (String, Option<String>, Option<String>) =
        sqlx::query_as("SELECT status::text, last_error, provider_refund_id FROM refunds")
            .fetch_one(&app.pool)
            .await
            .unwrap();
    assert_eq!(status, "processing");
    assert_eq!(last_error, None);
    assert!(provider_id.is_some());
    assert_eq!(order_status(&app, order_id).await, "refunded");
}

#[sqlx::test]
async fn ledger_post_rejects_unbalanced_order(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let b = bought(&app, 1_000, 5, 1).await;
    let o1: Option<Uuid> = sqlx::query_scalar("SELECT id FROM orders WHERE checkout_id = $1")
        .bind(b.checkout_id)
        .fetch_optional(&app.pool)
        .await
        .unwrap();
    let o2: Option<Uuid> = None;
    let entry = |order_id, account, amount_kobo| Entry {
        order_id,
        checkout_id: None,
        account,
        amount_kobo,
        kind: "test",
        reference: None,
    };
    // Balanced per transaction, but the order on its own is not.
    let mut tx = app.pool.begin().await.unwrap();
    let res = ledger::post(
        &mut tx,
        ("test", "k5"),
        &[
            entry(o1, Account::Escrow, 100),
            entry(o2, Account::BuyerFunds, -100),
        ],
    )
    .await;
    assert!(res.is_err());
    drop(tx);
    assert!(ledger_rows(&app).await.is_empty());
}
