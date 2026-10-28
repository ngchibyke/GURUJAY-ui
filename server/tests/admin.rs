//! Owner admin endpoints (Task 10): backstop, release/refund/hold, manual money
//! overrides, re-queue after a reversal, reconciliation and bank holidays.

mod common;

use axum::http::Method;
use common::{TestApp, TestResponse};
use serde_json::{Value, json};
use server::{
    checkout::{SettleOutcome, settle_payment},
    escrow::{SettlePayoutOutcome, settle_payout},
    orders::service::{mark_delivered, mark_picked_up},
    payments::{
        PaymentStatus, PayoutState, ProviderError,
        fake::{FAKE_SIGNATURE_HEADER, FAKE_WEBHOOK_SECRET},
    },
};
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

const METHOD: &str = "mono_directpay";
const SELLER: &str = "08011111111";
const BUYER: &str = "08022222221";
const BUYER_2: &str = "08022222223";
const PRICE: i64 = 1_000_000;
/// The account number `make_active_store_with_product` sets.
const ACCOUNT_NUMBER: &str = "0123456785";
/// 1,000,000 - 50,000 cut - 1,500 fee share = 948,500 -> fee 3,500.
const PAYOUT: i64 = 945_000;

struct Fixture {
    order_id: Uuid,
    checkout_id: Uuid,
    store_id: Uuid,
    variant_id: Uuid,
}

async fn exec(app: &TestApp, sql: &str) {
    sqlx::query(sql).execute(&app.pool).await.unwrap();
}

async fn count(app: &TestApp, sql: &str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(&app.pool).await.unwrap()
}

async fn text(app: &TestApp, sql: &str) -> String {
    sqlx::query_scalar(sql).fetch_one(&app.pool).await.unwrap()
}

async fn order_status(app: &TestApp, id: Uuid) -> String {
    sqlx::query_scalar("SELECT status::text FROM orders WHERE id = $1")
        .bind(id)
        .fetch_one(&app.pool)
        .await
        .unwrap()
}

async fn balance(app: &TestApp, account: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT COALESCE(sum(amount_kobo), 0)::bigint FROM ledger_entries \
         WHERE account = $1::ledger_account",
    )
    .bind(account)
    .fetch_one(&app.pool)
    .await
    .unwrap()
}

async fn assert_orders_balance(app: &TestApp) {
    assert_eq!(
        count(
            app,
            "SELECT count(*) FROM (SELECT order_id FROM ledger_entries \
             GROUP BY order_id HAVING sum(amount_kobo) <> 0) x"
        )
        .await,
        0
    );
}

async fn owner_id(app: &TestApp) -> Uuid {
    sqlx::query_scalar("SELECT id FROM users WHERE role = 'owner'")
        .fetch_one(&app.pool)
        .await
        .unwrap()
}

/// One paid order for one unit at PRICE (provider fee 1,500), payment back-dated
/// 10 days so a payout released from it is sent at once. `flagged`: the
/// provider held the payment.
async fn paid_order(app: &TestApp, flagged: bool) -> Fixture {
    let listing = app
        .make_active_store_with_product(SELLER, "Mama Put", PRICE, 5)
        .await;
    let (_, buyer) = app.sign_up_verified(BUYER).await;
    assert_eq!(
        app.set_cart(&buyer, listing.variant_id, 1).await.status,
        200
    );
    let res = app.checkout(&buyer, METHOD).await;
    assert_eq!(res.status, 201, "{}", res.json);
    let reference = res.json["reference"].as_str().unwrap().to_string();
    let checkout_id: Uuid = res.json["checkout_id"].as_str().unwrap().parse().unwrap();
    app.fake.set_payment(
        &reference,
        PaymentStatus::Successful {
            amount_kobo: PRICE,
            fee_kobo: 1_500,
            currency: "NGN".into(),
            held: flagged,
            reference: reference.clone(),
        },
    );
    assert_eq!(
        settle_payment(&app.state, &reference).await.unwrap(),
        SettleOutcome::Paid
    );
    let order_id: Uuid = sqlx::query_scalar("SELECT id FROM orders WHERE checkout_id = $1")
        .bind(checkout_id)
        .fetch_one(&app.pool)
        .await
        .unwrap();
    exec(
        app,
        &format!(
            "UPDATE checkouts SET paid_at = now() - interval '10 days' WHERE id = '{checkout_id}'"
        ),
    )
    .await;
    Fixture {
        order_id,
        checkout_id,
        store_id: listing.store_id,
        variant_id: listing.variant_id,
    }
}

async fn delivered_order(app: &TestApp) -> Fixture {
    let f = paid_order(app, false).await;
    mark_picked_up(&app.pool, f.order_id, OffsetDateTime::now_utc())
        .await
        .unwrap();
    mark_delivered(&app.pool, f.order_id, OffsetDateTime::now_utc())
        .await
        .unwrap();
    f
}

async fn post(app: &TestApp, token: &str, path: &str, body: Value) -> TestResponse {
    app.request(Method::POST, path, Some(body), Some(token))
        .await
}

async fn get(app: &TestApp, token: &str, path: &str) -> TestResponse {
    app.request(Method::GET, path, None, Some(token)).await
}

fn assert_code(res: &TestResponse, status: u16, code: &str) {
    assert_eq!(res.status, status, "{}", res.json);
    assert_eq!(res.json["error"]["code"], code, "{}", res.json);
}

/// Audit rows of `action`: (actor, detail).
async fn audits(app: &TestApp, action: &str) -> Vec<(Option<Uuid>, Value)> {
    sqlx::query_as("SELECT actor_id, detail FROM audit_log WHERE action = $1 ORDER BY at, id")
        .bind(action)
        .fetch_all(&app.pool)
        .await
        .unwrap()
}

/// Every audit detail as one string (for "never contains" checks).
async fn all_audit_text(app: &TestApp) -> String {
    let rows: Vec<Value> = sqlx::query_scalar("SELECT detail FROM audit_log")
        .fetch_all(&app.pool)
        .await
        .unwrap();
    rows.iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n")
}

/// A delivered order released by the buyer, its payout accepted (pending).
async fn pending_payout(app: &TestApp) -> (Fixture, Uuid, String) {
    let f = delivered_order(app).await;
    server::escrow::release_order(
        &app.state,
        f.order_id,
        server::escrow::ReleaseReason::BuyerConfirmed,
    )
    .await
    .unwrap();
    let (id, reference, status): (Uuid, String, String) =
        sqlx::query_as("SELECT id, reference, status::text FROM payouts")
            .fetch_one(&app.pool)
            .await
            .unwrap();
    assert_eq!(status, "pending");
    (f, id, reference)
}

/// A paid-out payout whose success was then reversed by the bank.
async fn reversed_payout(app: &TestApp) -> (Fixture, Uuid, String) {
    let (f, id, reference) = pending_payout(app).await;
    app.fake
        .set_payout_state(&reference, PayoutState::Successful { fee_kobo: 3_000, amount_kobo: None });
    settle_payout(&app.state, &reference).await.unwrap();
    app.fake.set_payout_state(
        &reference,
        PayoutState::Reversed {
            reason: "account closed".into(),
        },
    );
    assert_eq!(
        settle_payout(&app.state, &reference).await.unwrap(),
        SettlePayoutOutcome::Settled {
            status: "failed".into()
        }
    );
    (f, id, reference)
}

// ------------------------------------------------------------ backstop/release

#[sqlx::test]
async fn owner_releases_backstop_order(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let f = delivered_order(&app).await;
    let owner = app.owner_token().await;
    // Unreleased 30 days after payment: the backstop job flags it.
    exec(
        &app,
        "UPDATE orders SET backstop_at = now() - interval '1 minute'",
    )
    .await;
    server::escrow::jobs::run_due_jobs(&app.state).await;

    let res = get(&app, &owner, "/admin/orders/backstop").await;
    assert_eq!(res.status, 200, "{}", res.json);
    let items = res.json["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["order_id"], f.order_id.to_string());
    assert_eq!(items[0]["status"], "delivered");
    // No buyer data in the admin list.
    assert!(items[0].get("buyer_id").is_none(), "{}", items[0]);

    let path = format!("/admin/orders/{}/release", f.order_id);
    let res = post(
        &app,
        &owner,
        &path,
        json!({ "note": "seller showed proof" }),
    )
    .await;
    assert_eq!(res.status, 200, "{}", res.json);
    assert_eq!(res.json["status"], "completed");
    assert_eq!(order_status(&app, f.order_id).await, "completed");
    assert_eq!(
        text(&app, "SELECT release_reason FROM payouts").await,
        "owner_decision"
    );
    let rows = audits(&app, "admin.order_released").await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, Some(owner_id(&app).await));
    assert_eq!(rows[0].1["note"], "seller showed proof");
    assert_eq!(rows[0].1["prior_status"], "delivered");

    // A retry acts once: 409 with the current state.
    let res = post(&app, &owner, &path, json!({ "note": "again" })).await;
    assert_code(&res, 409, "invalid_order_state");
    assert!(
        res.json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("completed")
    );
    assert_eq!(count(&app, "SELECT count(*) FROM payouts").await, 1);
    // Released orders leave the backstop list.
    let res = get(&app, &owner, "/admin/orders/backstop").await;
    assert_eq!(res.json["items"].as_array().unwrap().len(), 0);
    assert_orders_balance(&app).await;
}

#[sqlx::test]
async fn owner_release_clears_the_hold_in_the_same_step(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let f = delivered_order(&app).await;
    let owner = app.owner_token().await;
    let res = post(
        &app,
        &owner,
        &format!("/admin/orders/{}/hold", f.order_id),
        json!({ "reason": "buyer complaint" }),
    )
    .await;
    assert_eq!(res.status, 200, "{}", res.json);

    let res = post(
        &app,
        &owner,
        &format!("/admin/orders/{}/release", f.order_id),
        json!({ "note": "complaint resolved for seller" }),
    )
    .await;
    assert_eq!(res.status, 200, "{}", res.json);
    let (held, reason): (bool, Option<String>) =
        sqlx::query_as("SELECT held_at IS NOT NULL, hold_reason FROM orders")
            .fetch_one(&app.pool)
            .await
            .unwrap();
    assert!(!held);
    assert_eq!(reason, None);
    // Not gated by the hold: the payout went out inline.
    assert_eq!(
        text(&app, "SELECT status::text FROM payouts").await,
        "pending"
    );
    assert_eq!(app.fake.calls("create_payout"), 1);
    assert_eq!(
        audits(&app, "admin.order_released").await[0].1["was_held"],
        true
    );
}

#[sqlx::test]
async fn release_rejects_orders_not_shipped(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let f = paid_order(&app, false).await;
    let owner = app.owner_token().await;
    let res = post(
        &app,
        &owner,
        &format!("/admin/orders/{}/release", f.order_id),
        json!({ "note": "n" }),
    )
    .await;
    assert_code(&res, 409, "invalid_order_state");
    let res = post(
        &app,
        &owner,
        &format!("/admin/orders/{}/release", Uuid::new_v4()),
        json!({ "note": "n" }),
    )
    .await;
    assert_code(&res, 404, "not_found");
    let res = post(
        &app,
        &owner,
        "/admin/orders/nope/release",
        json!({ "note": "n" }),
    )
    .await;
    assert_code(&res, 404, "not_found");
    // A note is required.
    let res = post(
        &app,
        &owner,
        &format!("/admin/orders/{}/release", f.order_id),
        json!({ "note": "  " }),
    )
    .await;
    assert_code(&res, 422, "validation_failed");
}

// ------------------------------------------------------------------ roles

#[sqlx::test]
async fn support_can_view_but_not_release(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let f = delivered_order(&app).await;
    let support = app.make_support().await;
    assert_eq!(
        get(&app, &support, "/admin/orders/backstop").await.status,
        200
    );
    assert_eq!(
        get(&app, &support, "/admin/reconciliation").await.status,
        200
    );
    let res = post(
        &app,
        &support,
        &format!("/admin/orders/{}/release", f.order_id),
        json!({ "note": "n" }),
    )
    .await;
    assert_code(&res, 403, "forbidden");
    assert_eq!(order_status(&app, f.order_id).await, "delivered");
}

#[sqlx::test]
async fn support_gets_403_on_every_mutation_route(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let support = app.make_support().await;
    let (_, buyer) = app.sign_up_verified(BUYER).await;
    let id = Uuid::new_v4();
    let note = json!({ "note": "n", "provider_reference": "REF1", "reason": "r", "name": "Day" });
    let routes: Vec<(Method, String)> = vec![
        (Method::POST, format!("/admin/orders/{id}/release")),
        (Method::POST, format!("/admin/orders/{id}/refund")),
        (Method::POST, format!("/admin/orders/{id}/hold")),
        (Method::POST, format!("/admin/orders/{id}/unhold")),
        (Method::POST, format!("/admin/payouts/{id}/mark-paid")),
        (Method::POST, format!("/admin/payouts/{id}/requeue")),
        (Method::POST, format!("/admin/refunds/{id}/mark-refunded")),
        (Method::POST, format!("/admin/checkouts/{id}/clear-flag")),
        (Method::POST, format!("/admin/checkouts/{id}/resolve")),
        (Method::PUT, "/admin/bank-holidays/2026-12-25".to_string()),
        (
            Method::DELETE,
            "/admin/bank-holidays/2026-12-25".to_string(),
        ),
        (Method::PATCH, "/admin/settings".to_string()),
    ];
    for (method, path) in &routes {
        for token in [&support, &buyer] {
            let res = app
                .request(method.clone(), path, Some(note.clone()), Some(token))
                .await;
            assert_code(&res, 403, "forbidden");
        }
        let res = app
            .request(method.clone(), path, Some(note.clone()), None)
            .await;
        assert_eq!(res.status, 401, "{method} {path}");
    }
    // Owner-only reads.
    for path in [
        "/admin/payouts?status=manual",
        "/admin/checkouts?status=flagged",
    ] {
        assert_code(&get(&app, &support, path).await, 403, "forbidden");
    }
    // Nothing reached the audit log as an admin action.
    assert_eq!(
        count(
            &app,
            "SELECT count(*) FROM audit_log WHERE action LIKE 'admin.%'"
        )
        .await,
        0
    );
}

// ----------------------------------------------------------------- refunds

#[sqlx::test]
async fn owner_refunds_a_held_delivered_order_without_restocking(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let f = delivered_order(&app).await;
    let owner = app.owner_token().await;
    let path = format!("/admin/orders/{}/refund", f.order_id);
    assert_eq!(app.stock(f.variant_id).await, 4);

    // Not held: the complaint process comes first.
    let res = post(&app, &owner, &path, json!({ "note": "goods faulty" })).await;
    assert_code(&res, 409, "order_not_held");

    post(
        &app,
        &owner,
        &format!("/admin/orders/{}/hold", f.order_id),
        json!({ "reason": "buyer complaint" }),
    )
    .await;
    let res = post(&app, &owner, &path, json!({ "note": "goods faulty" })).await;
    assert_eq!(res.status, 200, "{}", res.json);
    // The fake cannot refund: manual, the order stays refund_pending.
    assert_eq!(res.json["refund_status"], "manual");
    assert_eq!(order_status(&app, f.order_id).await, "refund_pending");
    // The goods are with the buyer: no stock comes back.
    assert_eq!(app.stock(f.variant_id).await, 4);
    // Items + delivery from escrow, no cut.
    assert_eq!(balance(&app, "escrow").await, 0);
    assert_eq!(balance(&app, "refunds").await, PRICE);
    // I3: the provider kept its 1,500 collection fee; the platform absorbs it,
    // so the order's books match the cash: in PRICE, out PRICE, fee 1,500.
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT account::text, sum(amount_kobo)::bigint FROM ledger_entries \
         WHERE order_id = $1 GROUP BY account ORDER BY account::text",
    )
    .bind(f.order_id)
    .fetch_all(&app.pool)
    .await
    .unwrap();
    let rows: Vec<(&str, i64)> = rows.iter().map(|(a, n)| (a.as_str(), *n)).collect();
    assert_eq!(
        rows,
        [
            ("buyer_funds", -PRICE),
            ("escrow", 0),
            ("platform_revenue", -1_500),
            ("provider_fees", 1_500),
            ("refunds", PRICE),
        ]
    );
    let rows = audits(&app, "admin.order_refunded").await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, Some(owner_id(&app).await));
    assert_eq!(rows[0].1["prior_status"], "delivered");
    assert_eq!(rows[0].1["restore_stock"], false);

    // A retry: 409 with the current state, nothing sent twice.
    let res = post(&app, &owner, &path, json!({ "note": "again" })).await;
    assert_code(&res, 409, "invalid_order_state");
    assert_eq!(app.fake.calls("refund"), 1);
    assert_eq!(count(&app, "SELECT count(*) FROM refunds").await, 1);
    assert_orders_balance(&app).await;
}

#[sqlx::test]
async fn owner_refund_of_a_paid_order_restocks(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let f = paid_order(&app, false).await;
    let owner = app.owner_token().await;
    app.fake.set_refund_supported(true);
    let res = post(
        &app,
        &owner,
        &format!("/admin/orders/{}/refund", f.order_id),
        json!({ "note": "seller out of stock" }),
    )
    .await;
    assert_eq!(res.status, 200, "{}", res.json);
    assert_eq!(res.json["refund_status"], "processing");
    assert_eq!(order_status(&app, f.order_id).await, "refunded");
    assert_eq!(app.stock(f.variant_id).await, 5);
}

#[sqlx::test]
async fn refund_after_release_is_rejected(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let (f, _, _) = pending_payout(&app).await;
    let owner = app.owner_token().await;
    let res = post(
        &app,
        &owner,
        &format!("/admin/orders/{}/refund", f.order_id),
        json!({ "note": "late complaint" }),
    )
    .await;
    assert_code(&res, 409, "already_released");
}

#[sqlx::test]
async fn mark_refunded_completes_a_manual_refund(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let f = paid_order(&app, false).await;
    let owner = app.owner_token().await;
    // Default fake: NotSupported -> manual.
    post(
        &app,
        &owner,
        &format!("/admin/orders/{}/refund", f.order_id),
        json!({ "note": "cancel" }),
    )
    .await;
    let refund_id: Uuid = sqlx::query_scalar("SELECT id FROM refunds")
        .fetch_one(&app.pool)
        .await
        .unwrap();
    let path = format!("/admin/refunds/{refund_id}/mark-refunded");

    // Both fields are required.
    let res = post(
        &app,
        &owner,
        &path,
        json!({ "note": "paid back by transfer" }),
    )
    .await;
    assert_code(&res, 422, "validation_failed");
    let res = post(
        &app,
        &owner,
        &path,
        json!({ "note": "paid back by transfer", "provider_reference": "" }),
    )
    .await;
    assert_code(&res, 422, "validation_failed");

    let body = json!({ "note": "paid back by transfer", "provider_reference": "NIP-000123" });
    let res = post(&app, &owner, &path, body.clone()).await;
    assert_eq!(res.status, 200, "{}", res.json);
    assert_eq!(res.json["status"], "completed");
    assert_eq!(order_status(&app, f.order_id).await, "refunded");
    let (status, settled_by, manual_ref): (String, Option<String>, Option<String>) =
        sqlx::query_as("SELECT status::text, settled_by, manual_reference FROM refunds")
            .fetch_one(&app.pool)
            .await
            .unwrap();
    assert_eq!(status, "completed");
    assert_eq!(settled_by.as_deref(), Some("owner"));
    assert_eq!(manual_ref.as_deref(), Some("NIP-000123"));
    let rows = audits(&app, "admin.refund_marked_refunded").await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, Some(owner_id(&app).await));
    assert_eq!(rows[0].1["prior_status"], "manual");
    assert_eq!(rows[0].1["provider_reference"], "NIP-000123");

    // Booked once at request; marking it adds no ledger entries.
    assert_eq!(balance(&app, "refunds").await, PRICE);
    let res = post(&app, &owner, &path, body).await;
    assert_code(&res, 409, "invalid_refund_state");
    assert!(
        res.json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("completed")
    );
}

#[sqlx::test]
async fn mark_refunded_refuses_a_refund_still_being_tried(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let f = paid_order(&app, false).await;
    let owner = app.owner_token().await;
    app.fake.set_refund_supported(true);
    app.fake
        .fail_refund_with(ProviderError::Unavailable(anyhow::anyhow!("down")));
    server::escrow::refund::refund_order(&app.state, f.order_id, true)
        .await
        .unwrap();
    let refund_id: Uuid = sqlx::query_scalar("SELECT id FROM refunds")
        .fetch_one(&app.pool)
        .await
        .unwrap();
    let res = post(
        &app,
        &owner,
        &format!("/admin/refunds/{refund_id}/mark-refunded"),
        json!({ "note": "n", "provider_reference": "R1" }),
    )
    .await;
    assert_code(&res, 409, "invalid_refund_state");
    assert_eq!(order_status(&app, f.order_id).await, "refund_pending");
}

// ----------------------------------------------------------------- payouts

#[sqlx::test]
async fn manual_payout_mark_paid_audited(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let owner = app.owner_token().await;
    app.fake.fail_payout_with(ProviderError::NotSupported);
    let f = delivered_order(&app).await;
    server::escrow::release_order(
        &app.state,
        f.order_id,
        server::escrow::ReleaseReason::BuyerConfirmed,
    )
    .await
    .unwrap();
    let payout_id: Uuid = sqlx::query_scalar("SELECT id FROM payouts WHERE status = 'manual'")
        .fetch_one(&app.pool)
        .await
        .unwrap();

    let res = get(&app, &owner, "/admin/payouts?status=manual").await;
    assert_eq!(res.status, 200, "{}", res.json);
    let items = res.json["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["id"], payout_id.to_string());
    assert_eq!(items[0]["account_last4"], "6785");
    assert!(!res.json.to_string().contains(ACCOUNT_NUMBER));
    assert_eq!(
        get(&app, &owner, "/admin/payouts?status=failed").await.json["items"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
    assert_code(
        &get(&app, &owner, "/admin/payouts?status=bogus").await,
        422,
        "validation_failed",
    );

    let path = format!("/admin/payouts/{payout_id}/mark-paid");
    // A note that carries an account number is refused (no PII in the audit log).
    let res = post(
        &app,
        &owner,
        &path,
        json!({ "note": format!("paid to {ACCOUNT_NUMBER}"), "provider_reference": "TRF1" }),
    )
    .await;
    assert_code(&res, 422, "validation_failed");
    let res = post(&app, &owner, &path, json!({ "provider_reference": "TRF1" })).await;
    assert_code(&res, 422, "validation_failed");

    let body = json!({ "note": "paid by bank transfer", "provider_reference": "TRF-778899" });
    let res = post(&app, &owner, &path, body.clone()).await;
    assert_eq!(res.status, 200, "{}", res.json);
    assert_eq!(res.json["status"], "successful");
    assert_eq!(res.json["settled_by"], "owner");
    assert_eq!(balance(&app, "seller_payable").await, 0);
    assert_eq!(balance(&app, "seller_paid").await, PAYOUT);
    assert_orders_balance(&app).await;

    let rows = audits(&app, "admin.payout_marked_paid").await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, Some(owner_id(&app).await));
    assert_eq!(rows[0].1["prior_status"], "manual");
    assert_eq!(rows[0].1["note"], "paid by bank transfer");
    assert_eq!(rows[0].1["provider_reference"], "TRF-778899");
    assert!(!all_audit_text(&app).await.contains(ACCOUNT_NUMBER));

    // Repeat: 409 with the current status, booked once.
    let res = post(&app, &owner, &path, body).await;
    assert_code(&res, 409, "invalid_payout_state");
    assert!(
        res.json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("successful")
    );
    assert_eq!(balance(&app, "seller_paid").await, PAYOUT);
    // Provider answers never change an owner-settled payout.
    let reference = text(&app, "SELECT reference FROM payouts").await;
    app.fake
        .set_payout_state(&reference, PayoutState::Failed { reason: "x".into() });
    settle_payout(&app.state, &reference).await.unwrap();
    assert_eq!(
        text(&app, "SELECT status::text FROM payouts").await,
        "successful"
    );
    assert_eq!(balance(&app, "seller_paid").await, PAYOUT);
}

#[sqlx::test]
async fn mark_paid_refuses_a_payout_in_flight(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let (_, payout_id, _) = pending_payout(&app).await;
    let owner = app.owner_token().await;
    let path = format!("/admin/payouts/{payout_id}/mark-paid");
    let body = json!({ "note": "n", "provider_reference": "R1" });
    let res = post(&app, &owner, &path, body.clone()).await;
    assert_code(&res, 409, "invalid_payout_state");
    exec(
        &app,
        "UPDATE payouts SET status = 'sending', sending_until = now() + interval '1 minute'",
    )
    .await;
    assert_code(
        &post(&app, &owner, &path, body).await,
        409,
        "invalid_payout_state",
    );
    assert_eq!(balance(&app, "seller_paid").await, 0);
}

#[sqlx::test]
async fn reversal_requires_reverification_and_requeue_mints_a_new_reference(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let (f, payout_id, old_ref) = reversed_payout(&app).await;
    let owner = app.owner_token().await;
    assert_eq!(balance(&app, "seller_payable").await, PAYOUT);

    // The reversal sends the seller's account back for verification.
    let (status, reason): (String, Option<String>) =
        sqlx::query_as("SELECT status::text, pending_reason FROM payout_accounts")
            .fetch_one(&app.pool)
            .await
            .unwrap();
    assert_eq!(
        (status.as_str(), reason.as_deref()),
        ("pending", Some("payout_reversed"))
    );
    assert_eq!(
        audits(&app, "payout_account.reverify_required").await.len(),
        1
    );
    // The closed attempt is kept.
    let (attempt_ref, outcome): (String, String) =
        sqlx::query_as("SELECT reference, outcome FROM payout_attempts")
            .fetch_one(&app.pool)
            .await
            .unwrap();
    assert_eq!(
        (attempt_ref.as_str(), outcome.as_str()),
        (old_ref.as_str(), "reversed")
    );

    let path = format!("/admin/payouts/{payout_id}/requeue");
    let body = json!({ "note": "seller fixed the account" });
    let res = post(&app, &owner, &path, body.clone()).await;
    assert_code(&res, 409, "payout_account_unverified");
    let calls = app.fake.calls("payout_status");

    // The owner re-verifies the account (echoing the changed_at they reviewed).
    let view = get(
        &app,
        &owner,
        &format!("/admin/stores/{}/payout-account", f.store_id),
    )
    .await;
    let res = post(
        &app,
        &owner,
        &format!("/admin/stores/{}/payout-account/verify", f.store_id),
        json!({ "account_name": "ADA OBI", "changed_at": view.json["changed_at"] }),
    )
    .await;
    assert_eq!(res.status, 200, "{}", res.json);

    // The provider must confirm the old reference was not paid.
    app.fake
        .set_payout_state(&old_ref, PayoutState::Successful { fee_kobo: 0, amount_kobo: None });
    let res = post(&app, &owner, &path, body.clone()).await;
    assert_code(&res, 409, "payout_not_confirmed_reversed");
    assert_eq!(app.fake.calls("payout_status"), calls + 1);
    assert_eq!(app.fake.calls("create_payout"), 1);

    app.fake.set_payout_state(
        &old_ref,
        PayoutState::Reversed {
            reason: "account closed".into(),
        },
    );
    let res = post(&app, &owner, &path, body.clone()).await;
    assert_eq!(res.status, 200, "{}", res.json);
    let new_ref = res.json["reference"].as_str().unwrap().to_string();
    assert_ne!(new_ref, old_ref);
    assert!(new_ref.starts_with("PO"));
    // Sent inline with the new reference to the re-verified account.
    assert_eq!(res.json["status"], "pending");
    let sent = app.fake.payouts();
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[1].reference, new_ref);
    assert_eq!(sent[1].account_number, ACCOUNT_NUMBER);
    let rows = audits(&app, "admin.payout_requeued").await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].1["previous_reference"], old_ref);
    assert_eq!(rows[0].1["note"], "seller fixed the account");

    // Not twice: the payout is no longer a reversed one.
    assert_code(
        &post(&app, &owner, &path, body).await,
        409,
        "invalid_payout_state",
    );

    // The second success books once more under its own round.
    app.fake
        .set_payout_state(&new_ref, PayoutState::Successful { fee_kobo: 2_000, amount_kobo: None });
    settle_payout(&app.state, &new_ref).await.unwrap();
    assert_eq!(
        count(
            &app,
            "SELECT count(*) FROM ledger_postings WHERE kind = 'payout_success'"
        )
        .await,
        2
    );
    assert_eq!(balance(&app, "seller_payable").await, 0);
    assert_eq!(balance(&app, "seller_paid").await, PAYOUT);
    // A late webhook for the old reference is an unknown reference now.
    assert_eq!(
        settle_payout(&app.state, &old_ref).await.unwrap(),
        SettlePayoutOutcome::UnknownReference
    );
    assert_orders_balance(&app).await;
}

#[sqlx::test]
async fn requeue_inconclusive_lookup_needs_the_owners_attestation(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let (f, payout_id, old_ref) = reversed_payout(&app).await;
    let owner = app.owner_token().await;
    let view = get(
        &app,
        &owner,
        &format!("/admin/stores/{}/payout-account", f.store_id),
    )
    .await;
    let res = post(
        &app,
        &owner,
        &format!("/admin/stores/{}/payout-account/verify", f.store_id),
        json!({ "account_name": "ADA OBI", "changed_at": view.json["changed_at"] }),
    )
    .await;
    assert_eq!(res.status, 200, "{}", res.json);
    app.fake
        .fail_payout_status_with(ProviderError::Inconclusive);
    let path = format!("/admin/payouts/{payout_id}/requeue");

    // Without attestation: 409, same reference, nothing sent.
    let res = post(&app, &owner, &path, json!({ "note": "seller fixed it" })).await;
    assert_code(&res, 409, "provider_lookup_inconclusive");
    assert_eq!(text(&app, "SELECT reference FROM payouts").await, old_ref);
    assert_eq!(app.fake.calls("create_payout"), 1);
    assert_eq!(audits(&app, "admin.payout_requeued").await.len(), 0);

    // With attestation: a new reference, sent, and the attestation audited.
    let res = post(
        &app,
        &owner,
        &path,
        json!({ "note": "checked Mono dashboard: not paid", "attest_provider_checked": true }),
    )
    .await;
    assert_eq!(res.status, 200, "{}", res.json);
    let new_ref = res.json["reference"].as_str().unwrap().to_string();
    assert_ne!(new_ref, old_ref);
    assert_eq!(app.fake.calls("create_payout"), 2);
    let rows = audits(&app, "admin.payout_requeued").await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].1["attested"], true);
    assert_eq!(rows[0].1["lookup"], "inconclusive");
    assert_eq!(rows[0].1["previous_reference"], old_ref);
}

#[sqlx::test]
async fn requeue_needs_a_reversal(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let owner = app.owner_token().await;
    app.fake.fail_payout_with(ProviderError::Rejected {
        code: "bad".into(),
        message: "bad".into(),
    });
    let f = delivered_order(&app).await;
    server::escrow::release_order(
        &app.state,
        f.order_id,
        server::escrow::ReleaseReason::BuyerConfirmed,
    )
    .await
    .unwrap();
    let (payout_id, status): (Uuid, String) =
        sqlx::query_as("SELECT id, status::text FROM payouts")
            .fetch_one(&app.pool)
            .await
            .unwrap();
    assert_eq!(status, "failed");
    let res = post(
        &app,
        &owner,
        &format!("/admin/payouts/{payout_id}/requeue"),
        json!({ "note": "retry" }),
    )
    .await;
    assert_code(&res, 409, "not_reversed");
    assert_eq!(app.fake.calls("payout_status"), 0);
}

#[sqlx::test]
async fn mark_paid_after_a_reversal_books_success_again(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let (_, payout_id, old_ref) = reversed_payout(&app).await;
    let owner = app.owner_token().await;
    let res = post(
        &app,
        &owner,
        &format!("/admin/payouts/{payout_id}/mark-paid"),
        json!({ "note": "paid to the new account by hand", "provider_reference": "TRF-1" }),
    )
    .await;
    assert_eq!(res.status, 200, "{}", res.json);
    assert_eq!(balance(&app, "seller_payable").await, 0);
    assert_eq!(balance(&app, "seller_paid").await, PAYOUT);
    // A late reversal event for the old attempt changes nothing.
    settle_payout(&app.state, &old_ref).await.unwrap();
    assert_eq!(
        text(&app, "SELECT status::text FROM payouts").await,
        "successful"
    );
    assert_eq!(balance(&app, "seller_paid").await, PAYOUT);
    assert_orders_balance(&app).await;
}

#[sqlx::test]
async fn owner_verify_refuses_an_account_already_verified(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let f = paid_order(&app, false).await;
    let owner = app.owner_token().await;
    let view = get(
        &app,
        &owner,
        &format!("/admin/stores/{}/payout-account", f.store_id),
    )
    .await;
    assert_eq!(view.json["status"], "verified");
    let res = post(
        &app,
        &owner,
        &format!("/admin/stores/{}/payout-account/verify", f.store_id),
        json!({ "account_name": "SOMEONE ELSE", "changed_at": view.json["changed_at"] }),
    )
    .await;
    assert_code(&res, 409, "already_verified");
}

// ------------------------------------------------------------- hold/unhold

#[sqlx::test]
async fn hold_and_unhold_are_state_conditional_and_audited(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let f = paid_order(&app, false).await;
    let owner = app.owner_token().await;
    let hold = format!("/admin/orders/{}/hold", f.order_id);
    let unhold = format!("/admin/orders/{}/unhold", f.order_id);

    assert_code(
        &post(&app, &owner, &hold, json!({})).await,
        422,
        "validation_failed",
    );
    let res = post(&app, &owner, &hold, json!({ "reason": "buyer complaint" })).await;
    assert_eq!(res.status, 200, "{}", res.json);
    assert_eq!(res.json["held"], true);
    assert_eq!(res.json["hold_reason"], "buyer complaint");
    assert_code(
        &post(&app, &owner, &hold, json!({ "reason": "again" })).await,
        409,
        "already_held",
    );
    // The ship-deadline job (switched on: it ships off) skips a held order.
    exec(
        &app,
        "UPDATE platform_settings SET jobs_ship_deadline_enabled = true",
    )
    .await;
    exec(
        &app,
        "UPDATE orders SET ship_by = now() - interval '1 hour'",
    )
    .await;
    server::escrow::jobs::run_due_jobs(&app.state).await;
    assert_eq!(order_status(&app, f.order_id).await, "paid");

    let res = post(&app, &owner, &unhold, json!({ "note": "resolved" })).await;
    assert_eq!(res.status, 200, "{}", res.json);
    assert_eq!(res.json["held"], false);
    assert_code(
        &post(&app, &owner, &unhold, json!({ "note": "again" })).await,
        409,
        "not_held",
    );
    let held = audits(&app, "admin.order_held").await;
    assert_eq!(held.len(), 1);
    assert_eq!(held[0].0, Some(owner_id(&app).await));
    assert_eq!(held[0].1["reason"], "buyer complaint");
    assert_eq!(audits(&app, "admin.order_unheld").await.len(), 1);
}

// --------------------------------------------------- checkouts/reconciliation

/// A checkout for one unit whose payment came back for the wrong amount.
async fn mismatched_checkout(app: &TestApp, buyer_phone: &str, variant_id: Uuid) -> Uuid {
    let (_, buyer) = app.sign_up_verified(buyer_phone).await;
    assert_eq!(app.set_cart(&buyer, variant_id, 1).await.status, 200);
    let res = app.checkout(&buyer, METHOD).await;
    assert_eq!(res.status, 201, "{}", res.json);
    let reference = res.json["reference"].as_str().unwrap().to_string();
    app.fake.set_payment(
        &reference,
        PaymentStatus::Successful {
            amount_kobo: 1,
            fee_kobo: 0,
            currency: "NGN".into(),
            held: false,
            reference: reference.clone(),
        },
    );
    assert_eq!(
        settle_payment(&app.state, &reference).await.unwrap(),
        SettleOutcome::AmountMismatch
    );
    res.json["checkout_id"].as_str().unwrap().parse().unwrap()
}

#[sqlx::test]
async fn mismatch_list_shows_flagged(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let flagged = paid_order(&app, true).await;
    let mismatched = mismatched_checkout(&app, BUYER_2, flagged.variant_id).await;
    let owner = app.owner_token().await;

    let res = get(&app, &owner, "/admin/checkouts?status=amount_mismatch").await;
    assert_eq!(res.status, 200, "{}", res.json);
    let items = res.json["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["id"], mismatched.to_string());
    assert_eq!(items[0]["status"], "amount_mismatch");
    assert!(items[0].get("buyer_id").is_none());

    let res = get(&app, &owner, "/admin/checkouts?status=flagged").await;
    let items = res.json["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["id"], flagged.checkout_id.to_string());
    assert_eq!(items[0]["provider_flagged"], true);
    assert_code(
        &get(&app, &owner, "/admin/checkouts?status=paid").await,
        422,
        "validation_failed",
    );

    let res = get(&app, &owner, "/admin/reconciliation").await;
    assert_eq!(res.status, 200, "{}", res.json);
    assert_eq!(res.json["flagged_checkouts"].as_array().unwrap().len(), 1);
    assert_eq!(
        res.json["mismatched_checkouts"].as_array().unwrap().len(),
        1
    );
}

#[sqlx::test]
async fn clear_flag_lets_the_payout_go(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let f = paid_order(&app, true).await;
    let owner = app.owner_token().await;
    let path = format!("/admin/checkouts/{}/clear-flag", f.checkout_id);
    let res = post(
        &app,
        &owner,
        &path,
        json!({ "note": "checked with Mono: settled" }),
    )
    .await;
    assert_eq!(res.status, 200, "{}", res.json);
    assert_eq!(res.json["provider_flagged"], false);
    let flagged: bool = sqlx::query_scalar("SELECT provider_flagged FROM checkouts")
        .fetch_one(&app.pool)
        .await
        .unwrap();
    assert!(!flagged);
    let rows = audits(&app, "admin.checkout_flag_cleared").await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, Some(owner_id(&app).await));
    assert_code(
        &post(&app, &owner, &path, json!({ "note": "again" })).await,
        409,
        "not_flagged",
    );
}

#[sqlx::test]
async fn reconciliation_masks_accounts_and_is_audited(pool: PgPool) {
    let app = TestApp::new(pool).await;
    app.fake.fail_payout_with(ProviderError::NotSupported);
    let f = delivered_order(&app).await;
    server::escrow::release_order(
        &app.state,
        f.order_id,
        server::escrow::ReleaseReason::BuyerConfirmed,
    )
    .await
    .unwrap();
    let owner = app.owner_token().await;
    let support = app.make_support().await;

    let res = get(&app, &owner, "/admin/reconciliation").await;
    assert_eq!(res.status, 200, "{}", res.json);
    let payouts = res.json["payouts"].as_array().unwrap();
    assert_eq!(payouts.len(), 1);
    assert_eq!(payouts[0]["status"], "manual");
    assert_eq!(payouts[0]["account_last4"], "6785");
    assert_eq!(payouts[0]["bank_code"], "058");

    let res = get(&app, &support, "/admin/reconciliation").await;
    assert_eq!(res.status, 200, "{}", res.json);
    let payouts = res.json["payouts"].as_array().unwrap();
    assert_eq!(payouts[0]["account_last4"], "6785");
    // Support's view is narrower: no bank code.
    assert!(payouts[0].get("bank_code").is_none(), "{}", payouts[0]);
    let body = res.json.to_string();
    assert!(!body.contains(ACCOUNT_NUMBER));
    // No buyer id or contact data (the buyer's phone in any format).
    assert!(
        !body.contains("buyer_id") && !body.contains("2222221"),
        "{body}"
    );

    let viewed = audits(&app, "admin.viewed").await;
    assert_eq!(viewed.len(), 2);
    assert_eq!(viewed[0].1["view"], "reconciliation");
    assert!(viewed.iter().all(|(actor, _)| actor.is_some()));
}

// ------------------------------------------------------------ bank holidays

#[sqlx::test]
async fn bank_holidays_are_edited_by_the_owner_and_audited(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let owner = app.make_owner().await;
    let support = app.make_support().await;
    let path = "/admin/bank-holidays/2026-12-25";
    let res = app
        .request(
            Method::PUT,
            path,
            Some(json!({ "name": "Christmas Day" })),
            Some(&owner),
        )
        .await;
    assert_eq!(res.status, 200, "{}", res.json);
    assert_eq!(res.json["day"], "2026-12-25");
    let res = app
        .request(
            Method::PUT,
            path,
            Some(json!({ "name": "Christmas" })),
            Some(&owner),
        )
        .await;
    assert_eq!(res.status, 200, "{}", res.json);

    let res = get(&app, &support, "/admin/bank-holidays").await;
    assert_eq!(res.status, 200, "{}", res.json);
    assert_eq!(
        res.json["items"],
        json!([{ "day": "2026-12-25", "name": "Christmas" }])
    );

    let res = app
        .request(
            Method::PUT,
            "/admin/bank-holidays/2026-13-01",
            Some(json!({ "name": "x" })),
            Some(&owner),
        )
        .await;
    assert_code(&res, 422, "validation_failed");
    let res = app
        .request(
            Method::PUT,
            path,
            Some(json!({ "name": " " })),
            Some(&owner),
        )
        .await;
    assert_code(&res, 422, "validation_failed");

    let res = app.request(Method::DELETE, path, None, Some(&owner)).await;
    assert_eq!(res.status, 204, "{}", res.json);
    let res = app.request(Method::DELETE, path, None, Some(&owner)).await;
    assert_code(&res, 404, "not_found");
    let set = audits(&app, "admin.bank_holiday_set").await;
    assert_eq!(set.len(), 2);
    assert_eq!(set[1].1["old_name"], "Christmas Day");
    assert_eq!(audits(&app, "admin.bank_holiday_removed").await.len(), 1);
    // The list read is audited like the other admin reads.
    let viewed = audits(&app, "admin.viewed").await;
    assert_eq!(viewed.len(), 1);
    assert_eq!(viewed[0].1["view"], "bank_holidays");
}

// --------------------------------------------------------- fee snapshots

#[sqlx::test]
async fn fee_change_does_not_alter_a_paid_orders_payout(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let f = delivered_order(&app).await;
    let owner = app.owner_token().await;
    let current = get(&app, &owner, "/admin/settings").await;
    let res = app
        .request(
            Method::PATCH,
            "/admin/settings",
            Some(json!({
                "fee_small_kobo": 20_000,
                "stamp_duty_kobo": 0,
                "expected_updated_at": current.json["updated_at"],
            })),
            Some(&owner),
        )
        .await;
    assert_eq!(res.status, 200, "{}", res.json);
    server::escrow::release_order(
        &app.state,
        f.order_id,
        server::escrow::ReleaseReason::BuyerConfirmed,
    )
    .await
    .unwrap();
    let (amount, fee): (i64, i64) = sqlx::query_as("SELECT amount_kobo, fee_kobo FROM payouts")
        .fetch_one(&app.pool)
        .await
        .unwrap();
    assert_eq!((amount, fee), (PAYOUT, 3_500));
}

// ------------------------------------------- fix round 1: mark-paid checks

/// A released payout whose create timed out, so its outcome is `unknown`.
/// `found`: the provider did take it (find_payout finds it).
async fn unknown_payout(app: &TestApp, found: bool) -> (Uuid, String) {
    if found {
        app.fake.timeout_after_success_payout();
    } else {
        app.fake.fail_payout_with(ProviderError::Timeout);
    }
    let f = delivered_order(app).await;
    server::escrow::release_order(
        &app.state,
        f.order_id,
        server::escrow::ReleaseReason::BuyerConfirmed,
    )
    .await
    .unwrap();
    app.fake.clear_payout_failure();
    let (id, reference, status): (Uuid, String, String) =
        sqlx::query_as("SELECT id, reference, status::text FROM payouts")
            .fetch_one(&app.pool)
            .await
            .unwrap();
    assert_eq!(status, "unknown");
    (id, reference)
}

fn mark_paid_body() -> Value {
    json!({ "note": "paid by bank transfer", "provider_reference": "TRF-1" })
}

#[sqlx::test]
async fn mark_paid_on_a_payout_the_provider_paid_settles_it_normally(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let (id, reference) = unknown_payout(&app, true).await;
    let owner = app.owner_token().await;
    app.fake
        .set_payout_state(&reference, PayoutState::Successful { fee_kobo: 3_000, amount_kobo: None });
    let finds = app.fake.calls("find_payout");
    let statuses = app.fake.calls("payout_status");
    let fees = balance(&app, "provider_fees").await;

    let path = format!("/admin/payouts/{id}/mark-paid");
    let res = post(&app, &owner, &path, mark_paid_body()).await;
    assert_eq!(res.status, 200, "{}", res.json);
    assert_eq!(res.json["status"], "successful");
    assert_eq!(res.json["settled_by"], "provider");
    // Our lookup, then settle_payout's status call and its id lookup.
    assert_eq!(app.fake.calls("find_payout"), finds + 2);
    assert_eq!(app.fake.calls("payout_status"), statuses + 1);
    assert_eq!(app.fake.calls("create_payout"), 1);
    // Booked as the provider's success: the real fee, not owner-settled.
    assert_eq!(balance(&app, "seller_paid").await, PAYOUT);
    assert_eq!(balance(&app, "provider_fees").await, fees + 3_000);
    let manual: Option<String> = sqlx::query_scalar("SELECT manual_reference FROM payouts")
        .fetch_one(&app.pool)
        .await
        .unwrap();
    assert_eq!(manual, None);
    assert_eq!(audits(&app, "admin.payout_marked_paid").await.len(), 0);
    let checked = audits(&app, "admin.payout_provider_checked").await;
    assert_eq!(checked.len(), 1);
    assert_eq!(checked[0].0, Some(owner_id(&app).await));
    assert_eq!(checked[0].1["outcome"], "successful");
    assert_orders_balance(&app).await;
    // Settled once: a retry is 409.
    assert_code(
        &post(&app, &owner, &path, mark_paid_body()).await,
        409,
        "invalid_payout_state",
    );
    assert_eq!(balance(&app, "seller_paid").await, PAYOUT);
}

#[sqlx::test]
async fn mark_paid_proceeds_when_the_provider_never_got_the_payout(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let (id, _) = unknown_payout(&app, false).await;
    let owner = app.owner_token().await;
    let finds = app.fake.calls("find_payout");
    let statuses = app.fake.calls("payout_status");
    let res = post(
        &app,
        &owner,
        &format!("/admin/payouts/{id}/mark-paid"),
        mark_paid_body(),
    )
    .await;
    assert_eq!(res.status, 200, "{}", res.json);
    assert_eq!(res.json["settled_by"], "owner");
    assert_eq!(app.fake.calls("find_payout"), finds + 1);
    assert_eq!(app.fake.calls("payout_status"), statuses);
    assert_eq!(balance(&app, "seller_paid").await, PAYOUT);
    let rows = audits(&app, "admin.payout_marked_paid").await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].1["prior_status"], "unknown");
}

#[sqlx::test]
async fn mark_paid_is_503_when_the_provider_cannot_answer(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let (id, _) = unknown_payout(&app, false).await;
    let owner = app.owner_token().await;
    app.fake
        .fail_find_payout_with(ProviderError::Unavailable(anyhow::anyhow!("down")));
    let entries = count(&app, "SELECT count(*) FROM ledger_entries").await;
    let statuses = app.fake.calls("payout_status");
    let res = post(
        &app,
        &owner,
        &format!("/admin/payouts/{id}/mark-paid"),
        mark_paid_body(),
    )
    .await;
    assert_code(&res, 503, "provider_unavailable");
    assert_eq!(app.fake.calls("payout_status"), statuses);
    assert_eq!(
        text(&app, "SELECT status::text FROM payouts").await,
        "unknown"
    );
    assert_eq!(
        count(&app, "SELECT count(*) FROM ledger_entries").await,
        entries
    );
    assert_eq!(audits(&app, "admin.payout_marked_paid").await.len(), 0);
}

#[sqlx::test]
async fn mark_paid_inconclusive_lookup_needs_the_owners_attestation(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let (id, _) = unknown_payout(&app, false).await;
    let owner = app.owner_token().await;
    app.fake.fail_find_payout_with(ProviderError::Inconclusive);
    let entries = count(&app, "SELECT count(*) FROM ledger_entries").await;
    let path = format!("/admin/payouts/{id}/mark-paid");

    // Without attestation: 409 and nothing changes.
    let res = post(&app, &owner, &path, mark_paid_body()).await;
    assert_code(&res, 409, "provider_lookup_inconclusive");
    let mut declined = mark_paid_body();
    declined["attest_provider_checked"] = json!(false);
    assert_code(
        &post(&app, &owner, &path, declined).await,
        409,
        "provider_lookup_inconclusive",
    );
    assert_eq!(
        text(&app, "SELECT status::text FROM payouts").await,
        "unknown"
    );
    assert_eq!(
        count(&app, "SELECT count(*) FROM ledger_entries").await,
        entries
    );
    assert_eq!(audits(&app, "admin.payout_marked_paid").await.len(), 0);
    assert_eq!(app.fake.calls("create_payout"), 1);

    // With attestation: the owner's payment is recorded and audited as such.
    let mut body = mark_paid_body();
    body["attest_provider_checked"] = json!(true);
    let res = post(&app, &owner, &path, body).await;
    assert_eq!(res.status, 200, "{}", res.json);
    assert_eq!(res.json["status"], "successful");
    assert_eq!(res.json["settled_by"], "owner");
    assert_eq!(balance(&app, "seller_paid").await, PAYOUT);
    let rows = audits(&app, "admin.payout_marked_paid").await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, Some(owner_id(&app).await));
    assert_eq!(rows[0].1["attested"], true);
    assert_eq!(rows[0].1["lookup"], "inconclusive");
    assert_eq!(app.fake.calls("create_payout"), 1, "never re-created");
    assert_orders_balance(&app).await;
}

#[sqlx::test]
async fn mark_paid_audit_records_no_attestation_when_lookup_was_conclusive(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let (id, _) = unknown_payout(&app, false).await;
    let owner = app.owner_token().await;
    let mut body = mark_paid_body();
    body["attest_provider_checked"] = json!(true);
    let res = post(
        &app,
        &owner,
        &format!("/admin/payouts/{id}/mark-paid"),
        body,
    )
    .await;
    assert_eq!(res.status, 200, "{}", res.json);
    let rows = audits(&app, "admin.payout_marked_paid").await;
    assert_eq!(rows[0].1["attested"], false);
    assert_eq!(rows[0].1["lookup"], "absent");
}

#[sqlx::test]
async fn mark_paid_on_a_payout_the_provider_failed_is_409_then_allowed(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let (id, reference) = unknown_payout(&app, true).await;
    let owner = app.owner_token().await;
    app.fake
        .set_payout_state(&reference, PayoutState::Failed { reason: "x".into() });
    let path = format!("/admin/payouts/{id}/mark-paid");
    let res = post(&app, &owner, &path, mark_paid_body()).await;
    assert_code(&res, 409, "invalid_payout_state");
    assert!(
        res.json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("failed")
    );
    assert_eq!(balance(&app, "seller_paid").await, 0);
    // Confirmed never paid: the owner may now record a hand payment.
    let res = post(&app, &owner, &path, mark_paid_body()).await;
    assert_eq!(res.status, 200, "{}", res.json);
    assert_eq!(res.json["settled_by"], "owner");
    assert_eq!(balance(&app, "seller_paid").await, PAYOUT);
    assert_orders_balance(&app).await;
}

#[sqlx::test]
async fn mark_paid_refuses_a_payout_still_pending_at_the_provider(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let (id, _) = unknown_payout(&app, true).await;
    let owner = app.owner_token().await;
    // The fake answers Pending by default.
    let res = post(
        &app,
        &owner,
        &format!("/admin/payouts/{id}/mark-paid"),
        mark_paid_body(),
    )
    .await;
    assert_code(&res, 409, "payout_pending_at_provider");
    assert_eq!(balance(&app, "seller_paid").await, 0);
}

/// M8: mark-paid racing a send that claims the same unknown payout.
#[sqlx::test]
async fn mark_paid_racing_a_send_acts_once(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let (id, _) = unknown_payout(&app, false).await;
    let owner = app.owner_token().await;
    exec(&app, "UPDATE payouts SET next_attempt_at = NULL").await;
    let creates = app.fake.calls("create_payout");
    app.fake
        .delay_next_payout(std::time::Duration::from_millis(300));
    let path = format!("/admin/payouts/{id}/mark-paid");
    let (sent, res) = tokio::join!(
        server::escrow::send_payout(&app.state, id),
        post(&app, &owner, &path, mark_paid_body()),
    );
    let sent = sent.unwrap();
    let created = app.fake.calls("create_payout") - creates;
    let owner_won = res.status == 200;
    assert!(
        owner_won ^ (created == 1),
        "exactly one wins: mark-paid {} {}, creates {created}, send {}",
        res.status,
        res.json,
        sent.status
    );
    if owner_won {
        assert_eq!(sent.status, "successful");
    } else {
        assert_eq!(res.status, 409, "{}", res.json);
        assert_eq!(sent.status, "pending");
    }
    assert!(
        count(
            &app,
            "SELECT count(*) FROM ledger_postings WHERE kind = 'payout_success'"
        )
        .await
            <= 1
    );
    assert!(balance(&app, "seller_paid").await <= PAYOUT);
    assert_orders_balance(&app).await;
}

// -------------------------------------------- fix round 1: checkout resolve

#[sqlx::test]
async fn resolving_a_checkout_takes_it_off_the_lists(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let flagged = paid_order(&app, true).await;
    let mismatched = mismatched_checkout(&app, BUYER_2, flagged.variant_id).await;
    let owner = app.owner_token().await;
    let entries = count(&app, "SELECT count(*) FROM ledger_entries").await;
    let body = json!({ "note": "refunded by hand", "provider_reference": "NIP-42" });

    let path = format!("/admin/checkouts/{mismatched}/resolve");
    let res = post(&app, &owner, &path, body.clone()).await;
    assert_eq!(res.status, 200, "{}", res.json);
    assert_eq!(res.json["resolved"], true);
    // A retry acts once.
    assert_code(
        &post(&app, &owner, &path, body.clone()).await,
        409,
        "already_resolved",
    );
    // (2): a flagged checkout is not resolved (that would hide its gated
    // payout); clearing the flag takes it off the list.
    let path = format!("/admin/checkouts/{}/resolve", flagged.checkout_id);
    assert_code(
        &post(&app, &owner, &path, body.clone()).await,
        409,
        "clear_flag_first",
    );
    let path = format!("/admin/checkouts/{}/clear-flag", flagged.checkout_id);
    let res = post(&app, &owner, &path, json!({ "note": "checked" })).await;
    assert_eq!(res.status, 200, "{}", res.json);
    for which in ["amount_mismatch", "flagged"] {
        let res = get(&app, &owner, &format!("/admin/checkouts?status={which}")).await;
        assert_eq!(res.status, 200, "{}", res.json);
        assert_eq!(res.json["items"].as_array().unwrap().len(), 0, "{which}");
    }
    let res = get(&app, &owner, "/admin/reconciliation").await;
    assert_eq!(res.json["flagged_checkouts"].as_array().unwrap().len(), 0);
    assert_eq!(
        res.json["mismatched_checkouts"].as_array().unwrap().len(),
        0
    );
    // Only the owner's handling is recorded: no money moves.
    assert_eq!(
        count(&app, "SELECT count(*) FROM ledger_entries").await,
        entries
    );
    let rows = audits(&app, "admin.checkout_resolved").await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, Some(owner_id(&app).await));
    assert_eq!(rows[0].1["prior_status"], "amount_mismatch");
    assert_eq!(rows[0].1["provider_reference"], "NIP-42");
}

#[sqlx::test]
async fn resolve_refuses_a_checkout_needing_nothing(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let f = paid_order(&app, false).await;
    let owner = app.owner_token().await;
    let res = post(
        &app,
        &owner,
        &format!("/admin/checkouts/{}/resolve", f.checkout_id),
        json!({ "note": "n", "provider_reference": "R1" }),
    )
    .await;
    assert_code(&res, 409, "nothing_to_resolve");
    assert_eq!(audits(&app, "admin.checkout_resolved").await.len(), 0);
}

// ------------------------------------------------ fix round 1: support views

#[sqlx::test]
async fn support_does_not_see_hold_reasons(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let f = delivered_order(&app).await;
    let owner = app.owner_token().await;
    let support = app.make_support().await;
    exec(
        &app,
        "UPDATE orders SET backstop_at = now() - interval '1 minute'",
    )
    .await;
    server::escrow::jobs::run_due_jobs(&app.state).await;
    let res = post(
        &app,
        &owner,
        &format!("/admin/orders/{}/hold", f.order_id),
        json!({ "reason": "buyer says the item is fake" }),
    )
    .await;
    assert_eq!(res.status, 200, "{}", res.json);

    let res = get(&app, &owner, "/admin/orders/backstop").await;
    assert_eq!(
        res.json["items"][0]["hold_reason"],
        "buyer says the item is fake"
    );
    let res = get(&app, &support, "/admin/orders/backstop").await;
    assert_eq!(res.status, 200, "{}", res.json);
    let item = &res.json["items"][0];
    assert_eq!(item["held"], true);
    assert!(item.get("hold_reason").is_none(), "{item}");
    assert!(!res.json.to_string().contains("fake"));
    let res = get(&app, &support, "/admin/reconciliation").await;
    assert!(!res.json.to_string().contains("fake"));
}

// ------------------------------------------ fix round 1: refunds and notes

#[sqlx::test]
async fn mark_refunded_flags_an_unknown_provider_outcome(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let f = paid_order(&app, false).await;
    let owner = app.owner_token().await;
    app.fake.set_refund_supported(true);
    app.fake.fail_refund_with(ProviderError::Timeout);
    server::escrow::refund::refund_order(&app.state, f.order_id, true)
        .await
        .unwrap();
    // Out of sends: the job escalates it to manual.
    exec(&app, "UPDATE refunds SET attempts = 8").await;
    server::escrow::jobs::run_due_jobs(&app.state).await;
    assert_eq!(
        text(&app, "SELECT status::text FROM refunds").await,
        "manual"
    );
    let id: Uuid = sqlx::query_scalar("SELECT id FROM refunds")
        .fetch_one(&app.pool)
        .await
        .unwrap();
    let res = post(
        &app,
        &owner,
        &format!("/admin/refunds/{id}/mark-refunded"),
        json!({ "note": "paid back by hand", "provider_reference": "NIP-9" }),
    )
    .await;
    assert_eq!(res.status, 200, "{}", res.json);
    assert_eq!(res.json["status"], "completed");
    assert_eq!(res.json["provider_attempts"], 8);
    assert_eq!(res.json["provider_outcome_unknown"], true);
    let rows = audits(&app, "admin.refund_marked_refunded").await;
    assert_eq!(rows[0].1["provider_attempts"], 8);
    assert_eq!(rows[0].1["provider_outcome_unknown"], true);
}

#[sqlx::test]
async fn mark_refunded_refuses_when_the_order_moved(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let f = paid_order(&app, false).await;
    let owner = app.owner_token().await;
    // Default fake: NotSupported -> manual.
    server::escrow::refund::refund_order(&app.state, f.order_id, true)
        .await
        .unwrap();
    assert_eq!(
        text(&app, "SELECT status::text FROM refunds").await,
        "manual"
    );
    exec(&app, "UPDATE orders SET status = 'refunded'").await;
    let id: Uuid = sqlx::query_scalar("SELECT id FROM refunds")
        .fetch_one(&app.pool)
        .await
        .unwrap();
    let res = post(
        &app,
        &owner,
        &format!("/admin/refunds/{id}/mark-refunded"),
        json!({ "note": "n", "provider_reference": "R1" }),
    )
    .await;
    assert_code(&res, 409, "order_state_changed");
    // Rolled back.
    assert_eq!(
        text(&app, "SELECT status::text FROM refunds").await,
        "manual"
    );
    assert_eq!(audits(&app, "admin.refund_marked_refunded").await.len(), 0);
}

#[sqlx::test]
async fn overrides_refuse_account_numbers_in_any_spelling(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let owner = app.owner_token().await;
    let path = format!("/admin/payouts/{}/mark-paid", Uuid::new_v4());
    for body in [
        json!({ "note": "paid to 0123 456 785", "provider_reference": "R1" }),
        json!({ "note": "paid to 012-345-6785", "provider_reference": "R1" }),
        json!({ "note": "paid", "provider_reference": ACCOUNT_NUMBER }),
    ] {
        assert_code(
            &post(&app, &owner, &path, body).await,
            422,
            "validation_failed",
        );
    }
}

// ------------------------------------------------- late provider outcomes

/// A payout webhook for `reference` through the route (fake provider).
async fn payout_webhook(app: &TestApp, event_id: &str, reference: &str) -> TestResponse {
    let body = json!({ "event_id": event_id, "kind": "payout", "reference": reference });
    app.send_raw(
        Method::POST,
        "/webhooks/fake",
        &[(FAKE_SIGNATURE_HEADER, FAKE_WEBHOOK_SECRET)],
        body.to_string().into_bytes(),
    )
    .await
}

fn assert_received(res: &TestResponse) {
    assert_eq!(res.status, 200, "{}", res.json);
    assert_eq!(res.json["status"], "received", "{}", res.json);
}

/// A manual payout (the provider cannot pay out) the owner then paid by hand.
async fn owner_settled_payout(app: &TestApp) -> (Uuid, String) {
    app.fake.fail_payout_with(ProviderError::NotSupported);
    let f = delivered_order(app).await;
    server::escrow::release_order(
        &app.state,
        f.order_id,
        server::escrow::ReleaseReason::BuyerConfirmed,
    )
    .await
    .unwrap();
    let (id, reference): (Uuid, String) = sqlx::query_as("SELECT id, reference FROM payouts")
        .fetch_one(&app.pool)
        .await
        .unwrap();
    let owner = app.owner_token().await;
    let res = post(
        app,
        &owner,
        &format!("/admin/payouts/{id}/mark-paid"),
        json!({ "note": "paid by bank transfer", "provider_reference": "TRF-1" }),
    )
    .await;
    assert_eq!(res.json["settled_by"], "owner", "{}", res.json);
    (id, reference)
}

/// (1)+(8): a provider success on a payout the owner already paid by hand is
/// a double payment. It changes nothing, the webhook still answers 200, and
/// it raises one alert (audit row and attention list) with no account data.
#[sqlx::test]
async fn late_provider_success_on_an_owner_settled_payout_alerts(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let (payout_id, reference) = owner_settled_payout(&app).await;
    let alerts = || audits(&app, "payout.provider_outcome_after_close");

    // A provider failure is consistent with the owner paying: no alert.
    app.fake.set_payout_state(
        &reference,
        PayoutState::Failed {
            reason: "x".into(),
        },
    );
    assert_received(&payout_webhook(&app, "po-1", &reference).await);
    assert!(alerts().await.is_empty());

    // The provider cannot be asked: 503 so it redelivers; nothing recorded.
    app.fake
        .fail_payout_status_with(ProviderError::Unavailable(anyhow::anyhow!("down")));
    let res = payout_webhook(&app, "po-2", &reference).await;
    assert_code(&res, 503, "payout_status_unavailable");
    assert!(alerts().await.is_empty());
    app.fake.clear_payout_status_failure();

    // It paid the seller as well.
    app.fake.set_payout_state(
        &reference,
        PayoutState::Successful {
            fee_kobo: 3_000,
            amount_kobo: Some(PAYOUT),
        },
    );
    assert_received(&payout_webhook(&app, "po-3", &reference).await);
    let rows = alerts().await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, None);
    assert_eq!(
        rows[0].1,
        json!({
            "reference_kind": "owner_settled",
            "outcome": "successful",
            "payout_id": payout_id,
        })
    );
    // Nothing else changed: still the owner's, booked once.
    assert_eq!(text(&app, "SELECT settled_by FROM payouts").await, "owner");
    assert_eq!(balance(&app, "seller_paid").await, PAYOUT);
    assert_eq!(
        count(
            &app,
            "SELECT count(*) FROM ledger_postings WHERE kind = 'payout_success'"
        )
        .await,
        1
    );
    assert!(!all_audit_text(&app).await.contains(ACCOUNT_NUMBER));

    // On the attention list, for the owner and for Support.
    let owner = app.owner_token().await;
    let support = app.make_support().await;
    for token in [&owner, &support] {
        let res = get(&app, token, "/admin/reconciliation").await;
        assert_eq!(res.status, 200, "{}", res.json);
        let items = res.json["late_provider_outcomes"].as_array().unwrap();
        assert_eq!(items.len(), 1, "{}", res.json);
        assert_eq!(items[0]["payout_id"], payout_id.to_string());
        assert_eq!(items[0]["reference_kind"], "owner_settled");
        assert_eq!(items[0]["outcome"], "successful");
        assert!(!res.json.to_string().contains(ACCOUNT_NUMBER));
    }
}

/// (1)+(8): a provider success on a retired reference (a reversed attempt the
/// owner re-queued under a new reference) is a double payment too. Its
/// reversal repeats what `payout_attempts` already records: no alert.
#[sqlx::test]
async fn late_provider_success_on_a_retired_reference_alerts(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let (f, payout_id, old_ref) = reversed_payout(&app).await;
    let owner = app.owner_token().await;
    let alerts = || audits(&app, "payout.provider_outcome_after_close");
    let view = get(
        &app,
        &owner,
        &format!("/admin/stores/{}/payout-account", f.store_id),
    )
    .await;
    let res = post(
        &app,
        &owner,
        &format!("/admin/stores/{}/payout-account/verify", f.store_id),
        json!({ "account_name": "ADA OBI", "changed_at": view.json["changed_at"] }),
    )
    .await;
    assert_eq!(res.status, 200, "{}", res.json);
    let res = post(
        &app,
        &owner,
        &format!("/admin/payouts/{payout_id}/requeue"),
        json!({ "note": "seller fixed the account" }),
    )
    .await;
    assert_eq!(res.status, 200, "{}", res.json);
    let new_ref = res.json["reference"].as_str().unwrap().to_string();

    // The old reference's reversal again: already recorded, no alert.
    assert_received(&payout_webhook(&app, "po-old-1", &old_ref).await);
    assert!(alerts().await.is_empty());

    // Then the bank reports the old reference paid after all.
    app.fake.set_payout_state(
        &old_ref,
        PayoutState::Successful {
            fee_kobo: 0,
            amount_kobo: None,
        },
    );
    assert_received(&payout_webhook(&app, "po-old-2", &old_ref).await);
    let rows = alerts().await;
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].1,
        json!({
            "reference_kind": "retired_attempt",
            "outcome": "successful",
            "payout_id": payout_id,
        })
    );
    // The event is processed (a redelivery is a duplicate); the live payout
    // under its new reference is untouched.
    assert_eq!(
        payout_webhook(&app, "po-old-2", &old_ref).await.json["status"],
        "duplicate"
    );
    assert_eq!(text(&app, "SELECT reference FROM payouts").await, new_ref);
    assert_eq!(alerts().await.len(), 1);
    let res = get(&app, &owner, "/admin/reconciliation").await;
    let items = res.json["late_provider_outcomes"].as_array().unwrap();
    assert_eq!(items.len(), 1, "{}", res.json);
    assert_eq!(items[0]["reference_kind"], "retired_attempt");
}

/// (10): the provider reports a different amount than the payout booked. The
/// money has moved, so it still settles (booked at the payout's own amount),
/// with an alert carrying both numbers.
#[sqlx::test]
async fn payout_amount_mismatch_settles_and_alerts(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let (_, payout_id, reference) = pending_payout(&app).await;
    app.fake.set_payout_state(
        &reference,
        PayoutState::Successful {
            fee_kobo: 3_000,
            amount_kobo: Some(PAYOUT - 100),
        },
    );
    assert_eq!(
        settle_payout(&app.state, &reference).await.unwrap(),
        SettlePayoutOutcome::Settled {
            status: "successful".into()
        }
    );
    assert_eq!(balance(&app, "seller_paid").await, PAYOUT);
    assert_orders_balance(&app).await;
    let rows = audits(&app, "payout.amount_mismatch").await;
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].1,
        json!({ "payout_id": payout_id, "expected_kobo": PAYOUT, "reported_kobo": PAYOUT - 100 })
    );
    let owner = app.owner_token().await;
    let res = get(&app, &owner, "/admin/reconciliation").await;
    let items = res.json["payout_amount_mismatches"].as_array().unwrap();
    assert_eq!(items.len(), 1, "{}", res.json);
    assert_eq!(items[0]["payout_id"], payout_id.to_string());
    assert_eq!(items[0]["expected_kobo"], PAYOUT);
    assert_eq!(items[0]["reported_kobo"], PAYOUT - 100);
}

#[sqlx::test]
async fn payout_matching_amount_raises_no_alert(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let (_, _, reference) = pending_payout(&app).await;
    app.fake.set_payout_state(
        &reference,
        PayoutState::Successful {
            fee_kobo: 3_000,
            amount_kobo: Some(PAYOUT),
        },
    );
    settle_payout(&app.state, &reference).await.unwrap();
    assert_eq!(balance(&app, "seller_paid").await, PAYOUT);
    assert!(audits(&app, "payout.amount_mismatch").await.is_empty());
}

// --------------------------------------------- final wave: override checks

/// (3): a `defer` bumping `updated_at` (held order, unverified account) while
/// mark-paid asks the provider does not turn into a spurious 409.
#[sqlx::test]
async fn mark_paid_ignores_an_updated_at_bump_during_the_lookup(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let (id, _) = unknown_payout(&app, false).await;
    let owner = app.owner_token().await;
    app.fake
        .delay_next_find_payout(std::time::Duration::from_millis(400));
    let path = format!("/admin/payouts/{id}/mark-paid");
    let bump = async {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        exec(
            &app,
            "UPDATE payouts SET updated_at = now() + interval '1 second'",
        )
        .await;
    };
    let (res, ()) = tokio::join!(post(&app, &owner, &path, mark_paid_body()), bump);
    assert_eq!(res.status, 200, "{}", res.json);
    assert_eq!(res.json["settled_by"], "owner");
    assert_eq!(balance(&app, "seller_paid").await, PAYOUT);
}

/// (9): once the provider is known to have the payout, a status it cannot
/// tell is 409 `payout_pending_at_provider`, even with the owner's attestation.
#[sqlx::test]
async fn mark_paid_found_then_inconclusive_is_never_attestable(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let (id, _) = unknown_payout(&app, true).await;
    let owner = app.owner_token().await;
    app.fake.fail_payout_status_with(ProviderError::Inconclusive);
    let path = format!("/admin/payouts/{id}/mark-paid");
    let mut body = mark_paid_body();
    body["attest_provider_checked"] = json!(true);
    for b in [mark_paid_body(), body] {
        let res = post(&app, &owner, &path, b).await;
        assert_code(&res, 409, "payout_pending_at_provider");
    }
    assert_eq!(
        text(&app, "SELECT status::text FROM payouts").await,
        "unknown"
    );
    assert_eq!(balance(&app, "seller_paid").await, 0);
    assert_eq!(audits(&app, "admin.payout_marked_paid").await.len(), 0);
}

/// M2 (documented choice): a `failed` payout holds the provider's own final
/// answer, so mark-paid does not ask again, even with a provider id.
#[sqlx::test]
async fn mark_paid_on_a_failed_payout_does_not_ask_the_provider_again(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let (_, id, reference) = pending_payout(&app).await;
    app.fake
        .set_payout_state(&reference, PayoutState::Failed { reason: "x".into() });
    settle_payout(&app.state, &reference).await.unwrap();
    exec(&app, "UPDATE payouts SET provider_payout_id = 'PROV-1'").await;
    assert_eq!(text(&app, "SELECT status::text FROM payouts").await, "failed");
    let finds = app.fake.calls("find_payout");
    let statuses = app.fake.calls("payout_status");
    let owner = app.owner_token().await;
    let res = post(
        &app,
        &owner,
        &format!("/admin/payouts/{id}/mark-paid"),
        mark_paid_body(),
    )
    .await;
    assert_eq!(res.status, 200, "{}", res.json);
    assert_eq!(res.json["settled_by"], "owner");
    assert_eq!(app.fake.calls("find_payout"), finds);
    assert_eq!(app.fake.calls("payout_status"), statuses);
    assert_eq!(audits(&app, "admin.payout_marked_paid").await[0].1["lookup"], "not_needed");
}

/// M7: re-queue answers 503 `provider_unavailable` like mark-paid.
#[sqlx::test]
async fn requeue_is_503_when_the_provider_cannot_answer(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let (f, payout_id, _) = reversed_payout(&app).await;
    let owner = app.owner_token().await;
    let view = get(
        &app,
        &owner,
        &format!("/admin/stores/{}/payout-account", f.store_id),
    )
    .await;
    let res = post(
        &app,
        &owner,
        &format!("/admin/stores/{}/payout-account/verify", f.store_id),
        json!({ "account_name": "ADA OBI", "changed_at": view.json["changed_at"] }),
    )
    .await;
    assert_eq!(res.status, 200, "{}", res.json);
    app.fake
        .fail_payout_status_with(ProviderError::Unavailable(anyhow::anyhow!("down")));
    let res = post(
        &app,
        &owner,
        &format!("/admin/payouts/{payout_id}/requeue"),
        json!({ "note": "seller fixed the account" }),
    )
    .await;
    assert_code(&res, 503, "provider_unavailable");
    assert_eq!(audits(&app, "admin.payout_requeued").await.len(), 0);
}

/// M8: the payout-account admin routes check the role before the body.
#[sqlx::test]
async fn payout_account_admin_routes_check_the_role_before_the_body(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let f = delivered_order(&app).await;
    let support = app.make_support().await;
    let (_, seller) = app.sign_up_verified("08033333331").await;
    let path = format!("/admin/stores/{}/payout-account/verify", f.store_id);
    for token in [&support, &seller] {
        let res = post(&app, token, &path, json!(["not", "a", "body"])).await;
        assert_eq!(res.status, 403, "{}", res.json);
        let res = get(
            &app,
            token,
            &format!("/admin/stores/{}/payout-account", f.store_id),
        )
        .await;
        assert_eq!(res.status, 403, "{}", res.json);
    }
    // The owner with the same body gets the body error.
    let owner = app.owner_token().await;
    let res = post(&app, &owner, &path, json!(["not", "a", "body"])).await;
    assert!(res.status == 400 || res.status == 422, "{}", res.json);
}

async fn blocked(app: &TestApp, token: String) -> Vec<Value> {
    let res = get(app, &token, "/admin/reconciliation").await;
    assert_eq!(res.status, 200, "{}", res.json);
    res.json["blocked_payouts"].as_array().unwrap().clone()
}

/// M9: a scheduled payout waiting over 7 days on a person (held order,
/// flagged payment, unverified account) is on the attention list.
#[sqlx::test]
async fn reconciliation_lists_payouts_blocked_for_a_week(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let (f, id, _) = pending_payout(&app).await;
    let owner = app.owner_token().await;
    exec(
        &app,
        "UPDATE payouts SET status = 'scheduled', send_after = now() - interval '6 days'",
    )
    .await;
    exec(&app, "UPDATE orders SET held_at = now(), hold_reason = 'x'").await;
    assert!(blocked(&app, owner.clone()).await.is_empty(), "under a week");

    exec(&app, "UPDATE payouts SET send_after = now() - interval '8 days'").await;
    let items = blocked(&app, owner.clone()).await;
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["id"], id.to_string());
    // Support sees it too, without the bank code.
    let items = blocked(&app, app.make_support().await).await;
    assert_eq!(items.len(), 1);
    assert!(items[0]["bank_code"].is_null());

    // Each blocker on its own lists it; nothing blocking, it is not listed.
    exec(&app, "UPDATE orders SET held_at = NULL, hold_reason = NULL").await;
    assert!(blocked(&app, owner.clone()).await.is_empty(), "not blocked");
    exec(&app, "UPDATE checkouts SET provider_flagged = true").await;
    assert_eq!(blocked(&app, owner.clone()).await.len(), 1, "flagged");
    exec(&app, "UPDATE checkouts SET provider_flagged = false").await;
    exec(
        &app,
        &format!(
            "UPDATE payout_accounts SET status = 'pending', verified_by = NULL, \
             verified_at = NULL WHERE store_id = '{}'",
            f.store_id
        ),
    )
    .await;
    assert_eq!(blocked(&app, owner.clone()).await.len(), 1, "unverified");
}

/// M5: the database refuses a negative payout and a successful payout that
/// does not say who settled it.
#[sqlx::test]
async fn payouts_refuse_negative_amounts_and_unattributed_success(pool: PgPool) {
    let app = TestApp::new(pool).await;
    pending_payout(&app).await;
    for sql in [
        "UPDATE payouts SET amount_kobo = -1",
        "UPDATE payouts SET status = 'successful', settled_by = NULL",
    ] {
        let err = sqlx::query(sql).execute(&app.pool).await.unwrap_err();
        assert!(err.to_string().contains("check constraint"), "{sql}: {err}");
    }
    exec(&app, "UPDATE payouts SET status = 'successful', settled_by = 'provider'").await;
}
