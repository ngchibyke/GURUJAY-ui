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
    payments::{PaymentStatus, PayoutState, ProviderError},
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
        .set_payout_state(&reference, PayoutState::Successful { fee_kobo: 3_000 });
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
        .set_payout_state(&old_ref, PayoutState::Successful { fee_kobo: 0 });
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
        .set_payout_state(&new_ref, PayoutState::Successful { fee_kobo: 2_000 });
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
    // The ship-deadline job skips a held order.
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
