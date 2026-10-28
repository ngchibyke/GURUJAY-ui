mod common;

use axum::http::Method;
use common::TestApp;
use serde_json::json;
use server::{
    checkout::{SettleOutcome, settle_payment},
    error::AppError,
    escrow::{
        PayoutOutcome, ReleaseReason, SettlePayoutOutcome, refund::refund_order, release_order,
        send_payout, settle_payout,
    },
    ledger::{self, Account, Entry},
    orders::service::{mark_delivered, mark_picked_up},
    payments::{
        PaymentStatus, PayoutState, ProviderError,
        fake::{FAKE_SIGNATURE_HEADER, FAKE_WEBHOOK_SECRET},
    },
};
use sqlx::PgPool;
use time::OffsetDateTime;
use tracing_test::traced_test;
use uuid::Uuid;

const METHOD: &str = "mono_directpay";
const SELLER: &str = "08011111111";
const BUYER: &str = "08022222221";
/// The account number `make_active_store_with_product` sets.
const ACCOUNT_NUMBER: &str = "0123456785";

struct Fixture {
    order_id: Uuid,
    checkout_id: Uuid,
    store_id: Uuid,
    variant_id: Uuid,
}

#[derive(Debug, sqlx::FromRow)]
struct PayoutRow {
    id: Uuid,
    reference: String,
    status: String,
    amount_kobo: i64,
    fee_kobo: i64,
    attempts: i32,
    last_error: Option<String>,
    provider_payout_id: Option<String>,
    bank_code: Option<String>,
    account_last4: Option<String>,
    release_reason: Option<String>,
    provider_fee_kobo: Option<i64>,
}

/// One paid order (one unit at `price`, provider fee `fee`), picked up.
/// `old_payment`: back-date the payment 10 days, so the payout's settlement
/// wait is over and release sends it inline.
async fn picked_up_order(app: &TestApp, price: i64, fee: i64, old_payment: bool) -> Fixture {
    picked_up_order_with_delivery(app, price, fee, 0, old_payment).await
}

/// [`picked_up_order`] with a delivery fee of `delivery` on the order (and in
/// the checkout total the buyer pays).
async fn picked_up_order_with_delivery(
    app: &TestApp,
    price: i64,
    fee: i64,
    delivery: i64,
    old_payment: bool,
) -> Fixture {
    let listing = app
        .make_active_store_with_product(SELLER, "Mama Put", price, 5)
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
    let total = res.json["total_kobo"].as_i64().unwrap() + delivery;
    if delivery > 0 {
        exec(
            app,
            &format!(
                "UPDATE orders SET delivery_fee_kobo = {delivery} WHERE checkout_id = '{checkout_id}'"
            ),
        )
        .await;
        exec(
            app,
            &format!("UPDATE checkouts SET total_kobo = {total} WHERE id = '{checkout_id}'"),
        )
        .await;
    }
    app.fake.set_payment(
        &reference,
        PaymentStatus::Successful {
            amount_kobo: total,
            fee_kobo: fee,
            currency: "NGN".into(),
            held: false,
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
    if old_payment {
        exec(
            app,
            &format!("UPDATE checkouts SET paid_at = now() - interval '10 days' WHERE id = '{checkout_id}'"),
        )
        .await;
    }
    mark_picked_up(&app.pool, order_id, OffsetDateTime::now_utc())
        .await
        .unwrap();
    Fixture {
        order_id,
        checkout_id,
        store_id: listing.store_id,
        variant_id: listing.variant_id,
    }
}

async fn exec(app: &TestApp, sql: &str) {
    sqlx::query(sql).execute(&app.pool).await.unwrap();
}

async fn scalar_bool(app: &TestApp, sql: &str) -> bool {
    sqlx::query_scalar(sql).fetch_one(&app.pool).await.unwrap()
}

async fn count(app: &TestApp, sql: &str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(&app.pool).await.unwrap()
}

async fn payout(app: &TestApp) -> PayoutRow {
    sqlx::query_as(
        "SELECT id, reference, status::text AS status, amount_kobo, fee_kobo, attempts, \
                last_error, provider_payout_id, bank_code, account_last4, release_reason, \
                provider_fee_kobo \
         FROM payouts",
    )
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

/// Makes a scheduled/unknown payout due again (skips its backoff).
async fn make_due(app: &TestApp) {
    exec(app, "UPDATE payouts SET next_attempt_at = NULL").await;
}

async fn release(app: &TestApp, f: &Fixture) {
    release_order(&app.state, f.order_id, ReleaseReason::BuyerConfirmed)
        .await
        .unwrap();
}

async fn send(app: &TestApp, id: Uuid) -> PayoutOutcome {
    send_payout(&app.state, id).await.unwrap()
}

// ---------------------------------------------------------------- release

#[sqlx::test]
async fn release_amount_matches_formula(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let f = picked_up_order(&app, 1_000_000, 1_500, true).await;
    release(&app, &f).await;

    let p = payout(&app).await;
    // 1,000,000 - 50,000 cut - 1,500 fee share = 948,500 -> fee 3,500 -> 945,000.
    assert_eq!(p.amount_kobo, 945_000);
    assert_eq!(p.fee_kobo, 3_500);
    assert_eq!(p.status, "pending");
    assert_eq!(p.release_reason.as_deref(), Some("buyer_confirmed"));
    assert!(p.reference.starts_with("PO") && p.reference.len() <= 24);
    assert_eq!(
        p.provider_payout_id,
        Some(format!("fakepo_{}", p.reference))
    );
    assert_eq!(p.bank_code.as_deref(), Some("058"));
    assert_eq!(p.account_last4.as_deref(), Some("6785"));
    assert_eq!(order_status(&app, f.order_id).await, "completed");
    assert!(
        scalar_bool(&app, "SELECT completed_at IS NOT NULL FROM orders").await,
        "completed_at set"
    );

    let sent = app.fake.payouts();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].reference, p.reference);
    assert_eq!(sent[0].amount_kobo, 945_000);
    assert_eq!(sent[0].bank_code, "058");
    assert_eq!(sent[0].account_number, ACCOUNT_NUMBER);

    let rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT account::text, amount_kobo FROM ledger_entries WHERE kind = 'release' ORDER BY id",
    )
    .fetch_all(&app.pool)
    .await
    .unwrap();
    assert_eq!(
        rows,
        [
            ("escrow".to_string(), -1_000_000),
            ("platform_revenue".to_string(), 50_000),
            ("provider_fees".to_string(), 1_500),
            ("platform_revenue".to_string(), 3_500),
            ("seller_payable".to_string(), 945_000),
        ]
    );
    assert_eq!(balance(&app, "escrow").await, 0);
    assert_orders_balance(&app).await;
}

#[sqlx::test]
async fn release_from_delivered_and_wrong_states_are_noops(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let f = picked_up_order(&app, 10_000, 10, true).await;
    mark_delivered(&app.pool, f.order_id, OffsetDateTime::now_utc())
        .await
        .unwrap();
    exec(
        &app,
        "UPDATE orders SET release_at = now() - interval '1 minute'",
    )
    .await;
    release_order(&app.state, f.order_id, ReleaseReason::AutoAfterDelivery)
        .await
        .unwrap();
    assert_eq!(
        payout(&app).await.release_reason.as_deref(),
        Some("auto_after_delivery")
    );
    assert_eq!(order_status(&app, f.order_id).await, "completed");

    // Completed (has a payout): no-op.
    release(&app, &f).await;
    assert_eq!(count(&app, "SELECT count(*) FROM payouts").await, 1);
    assert_eq!(app.fake.calls("create_payout"), 1);

    // A paid (not yet picked up) order: no-op, nothing booked.
    exec(
        &app,
        &format!(
            "UPDATE orders SET status = 'paid' WHERE id = '{}'",
            f.order_id
        ),
    )
    .await;
    exec(&app, "UPDATE payouts SET status = 'manual'").await;
    let entries = count(&app, "SELECT count(*) FROM ledger_entries").await;
    release(&app, &f).await;
    assert_eq!(
        count(&app, "SELECT count(*) FROM ledger_entries").await,
        entries
    );

    assert!(matches!(
        release_order(&app.state, Uuid::new_v4(), ReleaseReason::OwnerDecision).await,
        Err(AppError::NotFound)
    ));
}

#[sqlx::test]
async fn concurrent_release_one_payout(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let f = picked_up_order(&app, 100_000, 100, true).await;
    let (a, b) = tokio::join!(
        release_order(&app.state, f.order_id, ReleaseReason::BuyerConfirmed),
        release_order(&app.state, f.order_id, ReleaseReason::BuyerConfirmed),
    );
    a.unwrap();
    b.unwrap();
    assert_eq!(count(&app, "SELECT count(*) FROM payouts").await, 1);
    assert_eq!(app.fake.calls("create_payout"), 1);
    assert_eq!(
        count(
            &app,
            "SELECT count(*) FROM ledger_postings WHERE kind = 'release'"
        )
        .await,
        1
    );
    assert_orders_balance(&app).await;
}

#[sqlx::test]
async fn release_posts_once(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let f = picked_up_order(&app, 100_000, 100, true).await;
    release(&app, &f).await;
    release(&app, &f).await;
    assert_eq!(
        count(
            &app,
            "SELECT count(*) FROM ledger_entries WHERE kind = 'release'"
        )
        .await,
        5
    );

    // The key guard on its own: a second post with the same key inserts nothing.
    let key = f.order_id.to_string();
    let entry = |account, amount_kobo| Entry {
        order_id: Some(f.order_id),
        checkout_id: Some(f.checkout_id),
        account,
        amount_kobo,
        kind: "release",
        reference: None,
    };
    let mut tx = app.pool.begin().await.unwrap();
    let posted = ledger::post(
        &mut tx,
        ("release", &key),
        &[
            entry(Account::Escrow, -1),
            entry(Account::PlatformRevenue, 1),
        ],
    )
    .await
    .unwrap();
    assert!(!posted);
    tx.commit().await.unwrap();
    assert_eq!(
        count(
            &app,
            "SELECT count(*) FROM ledger_entries WHERE kind = 'release'"
        )
        .await,
        5
    );
    // Payment and release keys were both recorded.
    assert_eq!(
        count(
            &app,
            "SELECT count(*) FROM ledger_postings WHERE kind IN ('payment', 'release')"
        )
        .await,
        2
    );
    // Append-only.
    assert!(
        sqlx::query("DELETE FROM ledger_postings")
            .execute(&app.pool)
            .await
            .is_err()
    );
}

#[sqlx::test]
async fn non_positive_payout_goes_manual(pool: PgPool) {
    let app = TestApp::new(pool).await;
    // 3,000 - 150 cut - 10 share = 2,840; fee 3,500 -> -660.
    let f = picked_up_order(&app, 3_000, 10, true).await;
    release(&app, &f).await;
    let p = payout(&app).await;
    assert_eq!(p.status, "manual");
    assert_eq!(p.amount_kobo, 0);
    assert_eq!(p.last_error.as_deref(), Some("non_positive_payout"));
    assert_eq!(app.fake.calls("create_payout"), 0);
    assert_eq!(order_status(&app, f.order_id).await, "completed");
    // The seller never owes the platform: no negative (or any) payable.
    assert_eq!(balance(&app, "seller_payable").await, 0);
    assert_eq!(
        count(
            &app,
            "SELECT count(*) FROM ledger_entries WHERE account = 'seller_payable'"
        )
        .await,
        0
    );
    // The platform absorbs the 660 shortfall out of cut + delivery (150 + 0).
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT account::text, amount_kobo FROM ledger_entries WHERE kind = 'release' ORDER BY id",
    )
    .fetch_all(&app.pool)
    .await
    .unwrap();
    assert_eq!(
        rows,
        [
            ("escrow".to_string(), -3_000),
            ("platform_revenue".to_string(), 150 - 660),
            ("provider_fees".to_string(), 10),
            ("platform_revenue".to_string(), 3_500),
        ]
    );
    assert_eq!(balance(&app, "escrow").await, 0);
    assert_orders_balance(&app).await;
    // send_payout never sends a manual payout.
    assert_eq!(send(&app, p.id).await.status, "manual");
    assert_eq!(app.fake.calls("create_payout"), 0);
}

#[sqlx::test]
async fn refund_after_release_rejected(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let f = picked_up_order(&app, 10_000, 10, true).await;
    release(&app, &f).await;
    let err = refund_order(&app.state, f.order_id, true)
        .await
        .unwrap_err();
    assert!(
        matches!(err, AppError::Conflict("already_released")),
        "{err:?}"
    );
    assert_eq!(count(&app, "SELECT count(*) FROM refunds").await, 0);
    assert_eq!(app.fake.calls("refund"), 0);
}

// ---------------------------------------------------------------- send gates

#[sqlx::test]
async fn payout_waits_for_settlement_time(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let f = picked_up_order(&app, 10_000, 10, false).await;
    release(&app, &f).await;
    let p = payout(&app).await;
    assert_eq!(p.status, "scheduled");
    assert_eq!(order_status(&app, f.order_id).await, "completed");
    assert_eq!(app.fake.calls("create_payout"), 0);
    // Settlement is the next working day 19:00 WAT plus the 2h buffer: always > 20h away.
    assert!(
        scalar_bool(
            &app,
            "SELECT send_after > now() + interval '20 hours' \
                AND send_after < now() + interval '5 days' FROM payouts"
        )
        .await
    );

    // Called early (e.g. by the job), it stays scheduled until send_after.
    assert_eq!(send(&app, p.id).await.status, "scheduled");
    assert_eq!(app.fake.calls("create_payout"), 0);
    assert!(scalar_bool(&app, "SELECT next_attempt_at = send_after FROM payouts").await);

    exec(
        &app,
        "UPDATE payouts SET send_after = now() - interval '1 minute'",
    )
    .await;
    make_due(&app).await;
    assert_eq!(send(&app, p.id).await.status, "pending");
    assert_eq!(app.fake.calls("create_payout"), 1);
}

#[sqlx::test]
async fn unverified_account_not_paid_out(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let f = picked_up_order(&app, 10_000, 10, true).await;
    exec(
        &app,
        &format!(
            "UPDATE payout_accounts SET status = 'pending', pending_reason = 'name_mismatch', \
             verified_by = NULL, verified_at = NULL WHERE store_id = '{}'",
            f.store_id
        ),
    )
    .await;
    release(&app, &f).await;
    let p = payout(&app).await;
    assert_eq!(p.status, "scheduled");
    assert_eq!(p.last_error.as_deref(), Some("account_not_verified"));
    assert_eq!(app.fake.calls("create_payout"), 0);
    assert!(scalar_bool(&app, "SELECT next_attempt_at > now() FROM payouts").await);
    // The money is booked to the seller regardless.
    assert_eq!(balance(&app, "seller_payable").await, p.amount_kobo);

    // No account at all: same.
    exec(&app, "DELETE FROM payout_accounts").await;
    make_due(&app).await;
    assert_eq!(send(&app, p.id).await.status, "scheduled");
    assert_eq!(app.fake.calls("create_payout"), 0);
}

#[sqlx::test]
async fn payout_held_48h_after_account_change(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let f = picked_up_order(&app, 10_000, 10, true).await;
    exec(
        &app,
        "UPDATE payout_accounts SET changed_at = now() - interval '1 hour', \
                previous_account_existed = true",
    )
    .await;
    release(&app, &f).await;
    let p = payout(&app).await;
    assert_eq!(p.status, "scheduled");
    assert_eq!(p.last_error.as_deref(), Some("account_change_hold"));
    assert_eq!(app.fake.calls("create_payout"), 0);
    assert!(
        scalar_bool(
            &app,
            "SELECT p.next_attempt_at = a.changed_at + interval '48 hours' \
             FROM payouts p, payout_accounts a"
        )
        .await
    );

    // The first account ever set is not held.
    exec(
        &app,
        "UPDATE payout_accounts SET previous_account_existed = false",
    )
    .await;
    make_due(&app).await;
    assert_eq!(send(&app, p.id).await.status, "pending");
    assert_eq!(app.fake.calls("create_payout"), 1);
}

#[sqlx::test]
async fn payout_sent_after_48h_hold_ends(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let f = picked_up_order(&app, 10_000, 10, true).await;
    exec(
        &app,
        "UPDATE payout_accounts SET changed_at = now() - interval '49 hours', \
                previous_account_existed = true",
    )
    .await;
    release(&app, &f).await;
    assert_eq!(payout(&app).await.status, "pending");
}

#[sqlx::test]
async fn held_order_not_paid_out(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let f = picked_up_order(&app, 10_000, 10, true).await;
    exec(
        &app,
        "UPDATE orders SET held_at = now(), hold_reason = 'complaint'",
    )
    .await;
    let err = release_order(&app.state, f.order_id, ReleaseReason::BuyerConfirmed)
        .await
        .unwrap_err();
    assert!(matches!(err, AppError::Conflict("order_held")), "{err:?}");
    // Not completed, nothing booked or sent.
    assert_eq!(order_status(&app, f.order_id).await, "picked_up");
    assert_eq!(count(&app, "SELECT count(*) FROM payouts").await, 0);
    assert_eq!(
        count(
            &app,
            "SELECT count(*) FROM ledger_entries WHERE kind = 'release'"
        )
        .await,
        0
    );
    assert_eq!(app.fake.calls("create_payout"), 0);
    assert_eq!(balance(&app, "escrow").await, 10_000);
    // Never `already_released`: the money is still in escrow for the owner's
    // decision. (refund_order today accepts only `paid` orders, so a
    // picked-up order answers `not_refundable`; dispute refunds are plan 5.)
    let err = refund_order(&app.state, f.order_id, false)
        .await
        .unwrap_err();
    assert!(
        matches!(err, AppError::Conflict("not_refundable")),
        "{err:?}"
    );
}

#[sqlx::test]
async fn held_order_cannot_be_released_except_by_owner(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let f = picked_up_order(&app, 10_000, 10, true).await;
    mark_delivered(&app.pool, f.order_id, OffsetDateTime::now_utc())
        .await
        .unwrap();
    exec(
        &app,
        "UPDATE orders SET held_at = now(), hold_reason = 'complaint', release_at = now() - interval '1 minute'",
    )
    .await;
    for reason in [
        ReleaseReason::BuyerConfirmed,
        ReleaseReason::AutoAfterDelivery,
    ] {
        let err = release_order(&app.state, f.order_id, reason)
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Conflict("order_held")), "{err:?}");
    }
    assert_eq!(order_status(&app, f.order_id).await, "delivered");
    assert_eq!(count(&app, "SELECT count(*) FROM payouts").await, 0);

    // The owner resolves the complaint in the seller's favour.
    release_order(&app.state, f.order_id, ReleaseReason::OwnerDecision)
        .await
        .unwrap();
    assert_eq!(order_status(&app, f.order_id).await, "completed");
    let p = payout(&app).await;
    assert_eq!(p.release_reason.as_deref(), Some("owner_decision"));
    // The send gate still holds it until the hold itself is lifted.
    assert_eq!(p.status, "scheduled");
    assert_eq!(p.last_error.as_deref(), Some("order_held"));
    assert_eq!(app.fake.calls("create_payout"), 0);

    exec(&app, "UPDATE orders SET held_at = NULL, hold_reason = NULL").await;
    make_due(&app).await;
    assert_eq!(send(&app, p.id).await.status, "pending");
    assert_eq!(app.fake.calls("create_payout"), 1);
}

#[sqlx::test]
async fn auto_release_requires_delivered_and_due(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let f = picked_up_order(&app, 10_000, 10, true).await;
    let auto = || release_order(&app.state, f.order_id, ReleaseReason::AutoAfterDelivery);

    // Picked up, never delivered (even with a release_at somehow set): skipped.
    exec(
        &app,
        "UPDATE orders SET release_at = now() - interval '1 day'",
    )
    .await;
    auto().await.unwrap();
    assert_eq!(order_status(&app, f.order_id).await, "picked_up");

    // Delivered, deadline not reached: skipped.
    mark_delivered(&app.pool, f.order_id, OffsetDateTime::now_utc())
        .await
        .unwrap();
    auto().await.unwrap();
    assert_eq!(order_status(&app, f.order_id).await, "delivered");
    assert_eq!(count(&app, "SELECT count(*) FROM payouts").await, 0);
    assert_eq!(count(&app, "SELECT count(*) FROM ledger_entries").await, 2);

    // Due: released.
    exec(
        &app,
        "UPDATE orders SET release_at = now() - interval '1 second'",
    )
    .await;
    auto().await.unwrap();
    assert_eq!(order_status(&app, f.order_id).await, "completed");
    assert_eq!(
        payout(&app).await.release_reason.as_deref(),
        Some("auto_after_delivery")
    );
}

#[sqlx::test]
async fn flagged_checkout_not_paid_out(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let f = picked_up_order(&app, 10_000, 10, true).await;
    exec(&app, "UPDATE checkouts SET provider_flagged = true").await;
    release(&app, &f).await;
    let p = payout(&app).await;
    assert_eq!(p.status, "scheduled");
    assert_eq!(p.last_error.as_deref(), Some("provider_flagged"));
    assert_eq!(app.fake.calls("create_payout"), 0);
}

// ---------------------------------------------------------------- send outcomes

#[sqlx::test]
async fn provider_unavailable_reschedules_with_backoff(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let f = picked_up_order(&app, 10_000, 10, true).await;
    app.fake
        .fail_payout_with(ProviderError::Unavailable(anyhow::anyhow!("down")));
    release(&app, &f).await;
    let p = payout(&app).await;
    assert_eq!(p.status, "scheduled");
    assert_eq!(p.last_error.as_deref(), Some("unavailable"));
    assert_eq!(p.attempts, 1);
    assert!(
        scalar_bool(
            &app,
            "SELECT next_attempt_at >= now() + interval '4 minutes' \
                AND next_attempt_at <= now() + interval '7 minutes' FROM payouts"
        )
        .await
    );
    // Not due yet: a second call does nothing.
    assert_eq!(send(&app, p.id).await.status, "scheduled");
    assert_eq!(app.fake.calls("create_payout"), 1);

    app.fake.clear_payout_failure();
    make_due(&app).await;
    assert_eq!(send(&app, p.id).await.status, "pending");
    let sent = app.fake.payouts();
    assert_eq!(sent.len(), 2);
    assert!(sent.iter().all(|r| r.reference == p.reference));
    assert_eq!(payout(&app).await.last_error, None);
}

#[sqlx::test]
async fn unavailable_after_max_attempts_goes_manual(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let f = picked_up_order(&app, 10_000, 10, true).await;
    exec(&app, "UPDATE checkouts SET provider_flagged = true").await;
    release(&app, &f).await;
    exec(&app, "UPDATE checkouts SET provider_flagged = false").await;
    exec(&app, "UPDATE payouts SET attempts = 7").await;
    make_due(&app).await;
    app.fake
        .fail_payout_with(ProviderError::Unavailable(anyhow::anyhow!("down")));
    let p = payout(&app).await;
    assert_eq!(send(&app, p.id).await.status, "manual");
    assert_eq!(payout(&app).await.attempts, 8);
}

#[sqlx::test]
async fn rejected_payout_goes_failed(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let f = picked_up_order(&app, 10_000, 10, true).await;
    app.fake.fail_payout_with(ProviderError::Rejected {
        code: "invalid_account".into(),
        message: "bad account".into(),
    });
    release(&app, &f).await;
    let p = payout(&app).await;
    assert_eq!(p.status, "failed");
    assert_eq!(p.last_error.as_deref(), Some("rejected"));
    // Failed is terminal for the sender: never re-created automatically.
    app.fake.clear_payout_failure();
    make_due(&app).await;
    assert_eq!(send(&app, p.id).await.status, "failed");
    assert_eq!(app.fake.calls("create_payout"), 1);
    // The seller is still owed the money.
    assert_eq!(balance(&app, "seller_payable").await, p.amount_kobo);
}

#[sqlx::test]
async fn timeout_then_lookup_adopts_existing_payout(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let f = picked_up_order(&app, 10_000, 10, true).await;
    app.fake.timeout_after_success_payout();
    release(&app, &f).await;
    let p = payout(&app).await;
    assert_eq!(p.status, "unknown");
    assert_eq!(p.provider_payout_id, None);
    assert_eq!(app.fake.calls("create_payout"), 1);

    make_due(&app).await;
    assert_eq!(send(&app, p.id).await.status, "pending");
    assert_eq!(app.fake.calls("find_payout"), 1);
    assert_eq!(app.fake.calls("create_payout"), 1, "never re-created");
    assert_eq!(
        payout(&app).await.provider_payout_id,
        Some(format!("fakepo_{}", p.reference))
    );
}

#[sqlx::test]
async fn timeout_then_lookup_not_found_recreates_with_same_reference(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let f = picked_up_order(&app, 10_000, 10, true).await;
    app.fake.fail_payout_with(ProviderError::Timeout);
    release(&app, &f).await;
    let p = payout(&app).await;
    assert_eq!(p.status, "unknown");

    app.fake.clear_payout_failure();
    make_due(&app).await;
    assert_eq!(send(&app, p.id).await.status, "pending");
    assert_eq!(app.fake.calls("find_payout"), 1);
    let sent = app.fake.payouts();
    assert_eq!(sent.len(), 2);
    assert!(sent.iter().all(|r| r.reference == p.reference));
}

const MONO_WEBHOOK_SECRET: &str = "whsec_engine";

/// An `unknown` payout (its create timed out at the Fake) handed to Mono, and
/// a state whose only provider is the real Mono adapter on a mock server that
/// lists no disbursements (inconclusive lookup) and refuses to be asked to
/// create one (`expect(0)`, checked when the server drops).
async fn unknown_mono_payout(app: &TestApp) -> (server::AppState, wiremock::MockServer, PayoutRow) {
    use server::payments::{
        ProviderRegistry,
        mono::{AmountUnit, MonoConfig, MonoProvider, Secret},
    };
    use std::{sync::Arc, time::Duration};
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };

    let f = picked_up_order(app, 10_000, 10, true).await;
    app.fake.fail_payout_with(ProviderError::Timeout);
    release(app, &f).await;
    let p = payout(app).await;
    assert_eq!(p.status, "unknown");
    exec(app, "UPDATE payouts SET provider = 'mono'").await;
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v3/payments/disburse/disbursements"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "status": "successful",
            "data": []
        })))
        .expect(1..)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v3/payments/disburse/disbursements"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {"id": "dup"}})))
        .expect(0)
        .mount(&server)
        .await;
    let mono = MonoProvider::new(MonoConfig {
        secret_key: Secret::new("test_sk_engine"),
        webhook_secret: Secret::new(MONO_WEBHOOK_SECRET),
        base_url: server.uri(),
        live: false,
        directpay_unit: AmountUnit::Kobo,
        disburse_unit: Some(AmountUnit::Kobo),
        disburse_source_account: Some("688a0b672f88771f77d05cf5".into()),
        payout_recipient_email: Some("payouts@platform.test".into()),
        connect_timeout: Duration::from_secs(5),
        timeout: Duration::from_secs(15),
        insecure_http_for_tests: true,
    })
    .unwrap();
    let mut providers = ProviderRegistry::new();
    providers.register(Arc::new(mono));
    let mut state = app.state.clone();
    state.providers = Arc::new(providers);
    (state, server, p)
}

/// Through the real Mono adapter (mock server): a payout whose create timed
/// out is looked up; an empty list (unconfirmed filter, REVISIT(Q5,Q6)) keeps
/// it `unknown` and never re-creates it.
#[sqlx::test]
async fn mono_empty_lookup_never_recreates_the_payout(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let (state, _server, p) = unknown_mono_payout(&app).await;
    for _ in 0..3 {
        make_due(&app).await;
        let out = send_payout(&state, p.id).await.unwrap();
        assert_ne!(out.status, "pending", "never created on an empty lookup");
    }
    let after = payout(&app).await;
    assert!(
        matches!(after.status.as_str(), "unknown" | "manual"),
        "{}",
        after.status
    );
    assert_eq!(after.provider_payout_id, None);
    // `expect(0)` on the POST is verified when the server drops.
}

/// A Mono payout webhook whose status cannot be told (no provider id,
/// inconclusive lookup): 200 so Mono stops retrying, event left unprocessed,
/// payout untouched.
#[sqlx::test]
async fn mono_webhook_for_an_inconclusive_payout_is_200_and_unprocessed(pool: PgPool) {
    use axum::{body::Body, http::Request};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    let app = TestApp::new(pool).await;
    let (state, _server, p) = unknown_mono_payout(&app).await;
    let body = json!({
        "event": "event.disbursement.transaction.successful",
        "event_id": "evt_inconclusive_1",
        "data": {
            "disbursement_reference": p.reference,
            "reference": p.reference,
            "amount": p.amount_kobo,
            "status": "successful"
        }
    });
    for _ in 0..2 {
        let res = server::build_app(state.clone())
            .oneshot(
                Request::post("/webhooks/mono")
                    .header("content-type", "application/json")
                    .header("mono-webhook-secret", MONO_WEBHOOK_SECRET)
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        // Never "duplicate": the event stays unprocessed for a later delivery.
        assert_eq!(json["status"], "received");
    }
    let processed: Option<OffsetDateTime> = sqlx::query_scalar(
        "SELECT processed_at FROM provider_events WHERE provider = 'mono' AND event_id = $1",
    )
    .bind("evt_inconclusive_1")
    .fetch_one(&app.pool)
    .await
    .unwrap();
    assert_eq!(processed, None);
    let after = payout(&app).await;
    assert_eq!(after.status, "unknown");
    assert_eq!(after.provider_payout_id, None);
}

/// Owner mark-paid against the real Mono adapter: inconclusive lookup is 409
/// without attestation (nothing changes), owner-settled and audited with it.
#[sqlx::test]
async fn mono_mark_paid_inconclusive_needs_attestation(pool: PgPool) {
    use server::admin::money::{MarkedPaid, mark_paid};

    let app = TestApp::new(pool).await;
    app.owner_token().await;
    let owner: Uuid = sqlx::query_scalar("SELECT id FROM users WHERE role = 'owner'")
        .fetch_one(&app.pool)
        .await
        .unwrap();
    let (state, _server, p) = unknown_mono_payout(&app).await;
    let paid_before = balance(&app, "seller_paid").await;

    let err = mark_paid(&state, owner, p.id, "paid by transfer", "TRF-9", false)
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            AppError::ConflictDetail("provider_lookup_inconclusive", _)
        ),
        "{err:?}"
    );
    assert_eq!(payout(&app).await.status, "unknown");
    assert_eq!(balance(&app, "seller_paid").await, paid_before);

    let done = mark_paid(&state, owner, p.id, "paid by transfer", "TRF-9", true)
        .await
        .unwrap();
    assert_eq!(done, MarkedPaid::ByOwner);
    assert_eq!(payout(&app).await.status, "successful");
    assert_eq!(
        balance(&app, "seller_paid").await,
        paid_before + p.amount_kobo
    );
    let detail: serde_json::Value = sqlx::query_scalar(
        "SELECT detail FROM audit_log WHERE action = 'admin.payout_marked_paid'",
    )
    .fetch_one(&app.pool)
    .await
    .unwrap();
    assert_eq!(detail["attested"], true);
    assert_eq!(detail["lookup"], "inconclusive");
}

#[sqlx::test]
async fn expired_sending_lease_is_treated_as_unknown(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let f = picked_up_order(&app, 10_000, 10, true).await;
    exec(&app, "UPDATE checkouts SET provider_flagged = true").await;
    release(&app, &f).await;
    exec(&app, "UPDATE checkouts SET provider_flagged = false").await;
    make_due(&app).await;
    let p = payout(&app).await;

    // A live lease: someone else is sending; nothing happens.
    exec(
        &app,
        "UPDATE payouts SET status = 'sending', sending_until = now() + interval '2 minutes'",
    )
    .await;
    assert_eq!(send(&app, p.id).await.status, "sending");
    assert_eq!(
        app.fake.calls("create_payout") + app.fake.calls("find_payout"),
        0
    );

    // The sender died: the lease expired. Look it up first, adopt what exists.
    exec(
        &app,
        "UPDATE payouts SET sending_until = now() - interval '1 minute'",
    )
    .await;
    app.fake.set_found_payout(&p.reference, Some("x9"));
    assert_eq!(send(&app, p.id).await.status, "pending");
    assert_eq!(app.fake.calls("find_payout"), 1);
    assert_eq!(app.fake.calls("create_payout"), 0);
    assert_eq!(payout(&app).await.provider_payout_id.as_deref(), Some("x9"));
}

#[sqlx::test]
async fn inflight_payout_keeps_its_account_snapshot(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let f = picked_up_order(&app, 10_000, 10, true).await;
    app.fake.fail_payout_with(ProviderError::Timeout);
    release(&app, &f).await;
    assert_eq!(payout(&app).await.status, "unknown");

    // The seller changes the account (long enough ago to pass the hold).
    let enc = app.state.cipher.encrypt("1111111111");
    sqlx::query(
        "UPDATE payout_accounts SET bank_code = '011', account_number_enc = $1, \
                account_last4 = '1111', previous_account_existed = true, \
                changed_at = now() - interval '49 hours'",
    )
    .bind(enc)
    .execute(&app.pool)
    .await
    .unwrap();

    app.fake.clear_payout_failure();
    make_due(&app).await;
    let p = payout(&app).await;
    assert_eq!(send(&app, p.id).await.status, "pending");
    let sent = app.fake.payouts();
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[1].bank_code, "058");
    assert_eq!(sent[1].account_number, ACCOUNT_NUMBER);
    let p = payout(&app).await;
    assert_eq!(p.bank_code.as_deref(), Some("058"));
    assert_eq!(p.account_last4.as_deref(), Some("6785"));
}

#[traced_test]
#[sqlx::test]
async fn payout_never_logs_or_audits_the_account_number(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let f = picked_up_order(&app, 10_000, 10, true).await;
    app.fake.fail_payout_with(ProviderError::Timeout);
    release(&app, &f).await;
    app.fake.clear_payout_failure();
    make_due(&app).await;
    send(&app, payout(&app).await.id).await;
    assert!(!logs_contain(ACCOUNT_NUMBER));
    assert_eq!(
        count(
            &app,
            &format!("SELECT count(*) FROM audit_log WHERE detail::text LIKE '%{ACCOUNT_NUMBER}%'")
        )
        .await,
        0
    );
}

// ---------------------------------------------------------------- settle

async fn pending_payout(app: &TestApp) -> PayoutRow {
    let f = picked_up_order(app, 1_000_000, 1_500, true).await;
    release(app, &f).await;
    let p = payout(app).await;
    assert_eq!(p.status, "pending");
    p
}

#[sqlx::test]
async fn payout_success_moves_payable_to_paid(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let p = pending_payout(&app).await;
    app.fake
        .set_payout_state(&p.reference, PayoutState::Successful { fee_kobo: 3_000, amount_kobo: None });
    assert_eq!(
        settle_payout(&app.state, &p.reference).await.unwrap(),
        SettlePayoutOutcome::Settled {
            status: "successful".into()
        }
    );
    let after = payout(&app).await;
    assert_eq!(after.status, "successful");
    assert_eq!(after.provider_fee_kobo, Some(3_000));
    assert_eq!(balance(&app, "seller_payable").await, 0);
    assert_eq!(balance(&app, "seller_paid").await, 945_000);
    assert_eq!(balance(&app, "provider_fees").await, 1_500 + 3_000);
    assert_eq!(
        balance(&app, "platform_revenue").await,
        50_000 + 3_500 - 3_000
    );
    assert_orders_balance(&app).await;

    // Idempotent.
    let entries = count(&app, "SELECT count(*) FROM ledger_entries").await;
    settle_payout(&app.state, &p.reference).await.unwrap();
    assert_eq!(
        count(&app, "SELECT count(*) FROM ledger_entries").await,
        entries
    );
    assert_eq!(balance(&app, "seller_paid").await, 945_000);
}

#[sqlx::test]
async fn reversal_posts_once(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let p = pending_payout(&app).await;
    app.fake
        .set_payout_state(&p.reference, PayoutState::Successful { fee_kobo: 3_000, amount_kobo: None });
    settle_payout(&app.state, &p.reference).await.unwrap();
    app.fake.set_payout_state(
        &p.reference,
        PayoutState::Reversed {
            reason: "account closed".into(),
        },
    );
    for _ in 0..2 {
        assert_eq!(
            settle_payout(&app.state, &p.reference).await.unwrap(),
            SettlePayoutOutcome::Settled {
                status: "failed".into()
            }
        );
    }
    assert_eq!(
        count(
            &app,
            "SELECT count(*) FROM ledger_postings WHERE kind = 'payout_reversal'"
        )
        .await,
        1
    );
    // Everything the success booked is undone: the seller is owed again.
    assert_eq!(balance(&app, "seller_payable").await, 945_000);
    assert_eq!(balance(&app, "seller_paid").await, 0);
    assert_eq!(balance(&app, "provider_fees").await, 1_500);
    assert_eq!(balance(&app, "platform_revenue").await, 50_000 + 3_500);
    assert_orders_balance(&app).await;
    // A failed payout is never re-sent automatically.
    make_due(&app).await;
    assert_eq!(send(&app, p.id).await.status, "failed");
    assert_eq!(app.fake.calls("create_payout"), 1);
}

#[sqlx::test]
async fn failed_after_success_reported_as_failed_also_reverses(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let p = pending_payout(&app).await;
    app.fake
        .set_payout_state(&p.reference, PayoutState::Successful { fee_kobo: 0, amount_kobo: None });
    settle_payout(&app.state, &p.reference).await.unwrap();
    app.fake.set_payout_state(
        &p.reference,
        PayoutState::Failed {
            reason: "returned".into(),
        },
    );
    settle_payout(&app.state, &p.reference).await.unwrap();
    assert_eq!(payout(&app).await.status, "failed");
    assert_eq!(balance(&app, "seller_payable").await, 945_000);
    assert_eq!(balance(&app, "seller_paid").await, 0);
}

#[sqlx::test]
async fn failed_while_pending_books_nothing(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let p = pending_payout(&app).await;
    app.fake.set_payout_state(
        &p.reference,
        PayoutState::Failed {
            reason: "insufficient balance".into(),
        },
    );
    let entries = count(&app, "SELECT count(*) FROM ledger_entries").await;
    settle_payout(&app.state, &p.reference).await.unwrap();
    let after = payout(&app).await;
    assert_eq!(after.status, "failed");
    assert_eq!(after.last_error.as_deref(), Some("provider_failed"));
    assert_eq!(
        count(&app, "SELECT count(*) FROM ledger_entries").await,
        entries
    );
    assert_eq!(balance(&app, "seller_payable").await, 945_000);
}

#[sqlx::test]
async fn pending_over_24h_goes_manual_not_failed(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let p = pending_payout(&app).await;
    // Still pending, under 24h: unchanged.
    assert_eq!(
        settle_payout(&app.state, &p.reference).await.unwrap(),
        SettlePayoutOutcome::Settled {
            status: "pending".into()
        }
    );
    assert!(scalar_bool(&app, "SELECT last_polled_at IS NOT NULL FROM payouts").await);

    exec(
        &app,
        "UPDATE payouts SET pending_since = now() - interval '25 hours'",
    )
    .await;
    settle_payout(&app.state, &p.reference).await.unwrap();
    let after = payout(&app).await;
    assert_eq!(after.status, "manual");
    assert_eq!(after.last_error.as_deref(), Some("pending_over_24h"));
    assert_eq!(app.fake.calls("create_payout"), 1);

    // A late success still settles a manual payout the provider accepted.
    app.fake
        .set_payout_state(&p.reference, PayoutState::Successful { fee_kobo: 0, amount_kobo: None });
    settle_payout(&app.state, &p.reference).await.unwrap();
    assert_eq!(payout(&app).await.status, "successful");
    assert_eq!(balance(&app, "seller_paid").await, 945_000);
}

#[sqlx::test]
async fn settle_unknown_reference(pool: PgPool) {
    let app = TestApp::new(pool).await;
    assert_eq!(
        settle_payout(&app.state, "PONOSUCHREFERENCE")
            .await
            .unwrap(),
        SettlePayoutOutcome::UnknownReference
    );
    assert_eq!(app.fake.calls("payout_status"), 0);
}

fn payout_hook(event_id: &str, reference: &str) -> Vec<u8> {
    json!({ "event_id": event_id, "kind": "payout", "reference": reference })
        .to_string()
        .into_bytes()
}

#[sqlx::test]
async fn payout_webhook_settles_status(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let p = pending_payout(&app).await;
    app.fake
        .set_payout_state(&p.reference, PayoutState::Successful { fee_kobo: 3_000, amount_kobo: None });
    let headers = [(FAKE_SIGNATURE_HEADER, FAKE_WEBHOOK_SECRET)];
    let res = app
        .send_raw(
            Method::POST,
            "/webhooks/fake",
            &headers,
            payout_hook("po-1", &p.reference),
        )
        .await;
    assert_eq!(res.status, 200, "{}", res.json);
    assert_eq!(res.json["status"], "received");
    assert_eq!(payout(&app).await.status, "successful");
    assert!(
        scalar_bool(
            &app,
            "SELECT processed_at IS NOT NULL FROM provider_events WHERE event_id = 'po-1'"
        )
        .await
    );

    let res = app
        .send_raw(
            Method::POST,
            "/webhooks/fake",
            &headers,
            payout_hook("po-1", &p.reference),
        )
        .await;
    assert_eq!(res.json["status"], "duplicate");
    assert_eq!(app.fake.calls("payout_status"), 1);
    assert_eq!(balance(&app, "seller_paid").await, 945_000);

    // Unknown reference: 200, processed, no provider call.
    let res = app
        .send_raw(
            Method::POST,
            "/webhooks/fake",
            &headers,
            payout_hook("po-2", "PONOPE"),
        )
        .await;
    assert_eq!(res.status, 200);
    assert_eq!(app.fake.calls("payout_status"), 1);
}

// ---------------------------------------------------------------- orders

#[sqlx::test]
async fn mark_delivered_sets_release_at_from_settings_at_that_time(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let f = picked_up_order(&app, 10_000, 10, true).await;
    // Picking up again is a no-op; delivering before pickup is not allowed (checked below).
    let at = OffsetDateTime::now_utc();
    mark_picked_up(&app.pool, f.order_id, at).await.unwrap();
    exec(
        &app,
        "UPDATE platform_settings SET release_after_hours = 24",
    )
    .await;
    mark_delivered(&app.pool, f.order_id, at).await.unwrap();
    let (status, release_ok): (String, bool) = sqlx::query_as(
        "SELECT status::text, release_at = delivered_at + interval '24 hours' FROM orders",
    )
    .fetch_one(&app.pool)
    .await
    .unwrap();
    assert_eq!(status, "delivered");
    assert!(release_ok);
    // A later settings change does not move the stored deadline.
    exec(
        &app,
        "UPDATE platform_settings SET release_after_hours = 72",
    )
    .await;
    mark_delivered(&app.pool, f.order_id, at).await.unwrap();
    assert!(
        scalar_bool(
            &app,
            "SELECT release_at = delivered_at + interval '24 hours' FROM orders"
        )
        .await
    );
    // Back to picked up is not a valid transition.
    assert!(matches!(
        mark_picked_up(&app.pool, f.order_id, at).await,
        Err(AppError::Conflict("invalid_order_state"))
    ));
    assert!(matches!(
        mark_delivered(&app.pool, Uuid::new_v4(), at).await,
        Err(AppError::NotFound)
    ));
    let _ = f.variant_id;
}

#[sqlx::test]
async fn deliver_requires_pickup(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let f = picked_up_order(&app, 10_000, 10, true).await;
    exec(
        &app,
        &format!(
            "UPDATE orders SET status = 'paid' WHERE id = '{}'",
            f.order_id
        ),
    )
    .await;
    assert!(matches!(
        mark_delivered(&app.pool, f.order_id, OffsetDateTime::now_utc()).await,
        Err(AppError::Conflict("invalid_order_state"))
    ));
}

// ---------------------------------------------------------------- fix round 1

#[sqlx::test]
async fn release_ledger_routes_delivery_to_platform_revenue(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let f = picked_up_order_with_delivery(&app, 1_000_000, 1_500, 200_000, true).await;
    release(&app, &f).await;
    let p = payout(&app).await;
    // Delivery never reaches the seller: the payout is the same as without it.
    assert_eq!(p.amount_kobo, 945_000);
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT account::text, amount_kobo FROM ledger_entries WHERE kind = 'release' ORDER BY id",
    )
    .fetch_all(&app.pool)
    .await
    .unwrap();
    assert_eq!(
        rows,
        [
            ("escrow".to_string(), -1_200_000),
            ("platform_revenue".to_string(), 50_000 + 200_000),
            ("provider_fees".to_string(), 1_500),
            ("platform_revenue".to_string(), 3_500),
            ("seller_payable".to_string(), 945_000),
        ]
    );
    assert_eq!(balance(&app, "escrow").await, 0);
    assert_orders_balance(&app).await;
}

#[sqlx::test]
async fn concurrent_send_payout_creates_once(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let f = picked_up_order(&app, 10_000, 10, true).await;
    exec(&app, "UPDATE checkouts SET provider_flagged = true").await;
    release(&app, &f).await;
    exec(&app, "UPDATE checkouts SET provider_flagged = false").await;
    make_due(&app).await;
    let id = payout(&app).await.id;
    // The first sender holds its lease while the provider is slow.
    app.fake
        .delay_next_payout(std::time::Duration::from_millis(500));
    let (a, b) = tokio::join!(send_payout(&app.state, id), send_payout(&app.state, id));
    let mut statuses = vec![a.unwrap().status, b.unwrap().status];
    statuses.sort();
    assert_eq!(statuses, ["pending", "sending"]);
    assert_eq!(app.fake.calls("create_payout"), 1);
    assert_eq!(app.fake.calls("find_payout"), 0);
    let p = payout(&app).await;
    assert_eq!(p.status, "pending");
    assert_eq!(p.attempts, 1);
}

#[sqlx::test]
async fn refund_and_payment_posting_keys_are_recorded(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let f = picked_up_order(&app, 10_000, 10, true).await;
    exec(&app, "UPDATE orders SET status = 'paid'").await;
    let out = refund_order(&app.state, f.order_id, false).await.unwrap();
    let keys: Vec<(String, String)> =
        sqlx::query_as("SELECT kind, ref FROM ledger_postings ORDER BY posted_at, kind")
            .fetch_all(&app.pool)
            .await
            .unwrap();
    assert!(keys.contains(&("payment".to_string(), f.checkout_id.to_string())));
    assert!(keys.contains(&("refund".to_string(), out.refund_id.to_string())));
    // I3: the absorbed collection fee is its own posting.
    assert!(keys.contains(&("refund_fee".to_string(), out.refund_id.to_string())));
    // Each key points at the txn that holds its entries.
    assert_eq!(
        count(
            &app,
            "SELECT count(*) FROM ledger_postings lp WHERE NOT EXISTS (SELECT 1 FROM ledger_entries e WHERE e.txn_id = lp.txn_id)"
        )
        .await,
        0
    );
}

#[sqlx::test]
async fn unknown_escalates_to_manual_after_max_attempts_and_still_settles(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let f = picked_up_order(&app, 10_000, 10, true).await;
    app.fake.fail_payout_with(ProviderError::Timeout);
    release(&app, &f).await;
    assert_eq!(payout(&app).await.status, "unknown");
    exec(&app, "UPDATE payouts SET attempts = 7").await;
    make_due(&app).await;
    let p = payout(&app).await;
    assert_eq!(send(&app, p.id).await.status, "manual");
    let after = payout(&app).await;
    assert_eq!(after.attempts, 8);
    assert_eq!(
        after.last_error.as_deref(),
        Some("unknown_after_max_attempts")
    );
    assert_eq!(
        count(
            &app,
            "SELECT count(*) FROM audit_log WHERE action = 'payout.escalated_manual'"
        )
        .await,
        1
    );
    // Manual now: never sent again automatically.
    app.fake.clear_payout_failure();
    make_due(&app).await;
    assert_eq!(send(&app, p.id).await.status, "manual");
    assert_eq!(app.fake.calls("create_payout"), 2);

    // It did go out after all: the success settles it and records the id.
    app.fake.set_found_payout(&p.reference, Some("x7"));
    app.fake
        .set_payout_state(&p.reference, PayoutState::Successful { fee_kobo: 0, amount_kobo: None });
    settle_payout(&app.state, &p.reference).await.unwrap();
    let settled = payout(&app).await;
    assert_eq!(settled.status, "successful");
    assert_eq!(settled.provider_payout_id.as_deref(), Some("x7"));
    assert_eq!(balance(&app, "seller_paid").await, settled.amount_kobo);
}

#[sqlx::test]
async fn success_without_payout_id_stores_the_found_id(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let f = picked_up_order(&app, 10_000, 10, true).await;
    app.fake.fail_payout_with(ProviderError::Timeout);
    release(&app, &f).await;
    let p = payout(&app).await;
    assert_eq!(p.status, "unknown");
    assert_eq!(p.provider_payout_id, None);
    app.fake.set_found_payout(&p.reference, Some("x8"));
    app.fake
        .set_payout_state(&p.reference, PayoutState::Successful { fee_kobo: 0, amount_kobo: None });
    settle_payout(&app.state, &p.reference).await.unwrap();
    let after = payout(&app).await;
    assert_eq!(after.status, "successful");
    assert_eq!(after.provider_payout_id.as_deref(), Some("x8"));
}

// ---------------------------------------------------------------- end to end

/// The whole happy path across two stores, through the HTTP routes wherever
/// one exists (checkout, payment and payout webhooks, order views) and the
/// engine otherwise (pickup/delivery transitions, the job tick). Every kobo
/// amount below is worked out by hand from the default fee schedule, not read
/// back from the code under test.
#[sqlx::test]
async fn happy_path_end_to_end(pool: PgPool) {
    use server::escrow::jobs::run_due_jobs;

    let app = TestApp::new(pool).await;
    let hook = |kind: &str, event_id: &str, reference: &str| {
        json!({ "event_id": event_id, "kind": kind, "reference": reference })
            .to_string()
            .into_bytes()
    };
    let headers = [(FAKE_SIGNATURE_HEADER, FAKE_WEBHOOK_SECRET)];

    // 1. Two sellers, each with an active store and a verified payout account.
    // Store A sells one unit at N15,000 (stamp duty applies); store B two
    // units at N3,000 (a N6,000 order, no stamp duty).
    let a = app
        .make_active_store_with_product(SELLER, "Mama Put", 1_500_000, 5)
        .await;
    let b = app
        .make_active_store_with_product("08011111112", "Iya Basira", 300_000, 10)
        .await;
    for store in [a.store_id, b.store_id] {
        let (status, account): (String, String) = sqlx::query_as(
            "SELECT s.status::text, pa.status::text FROM stores s \
             JOIN payout_accounts pa ON pa.store_id = s.id WHERE s.id = $1",
        )
        .bind(store)
        .fetch_one(&app.pool)
        .await
        .unwrap();
        assert_eq!((status.as_str(), account.as_str()), ("active", "verified"));
    }
    let (_, buyer) = app.sign_up_verified(BUYER).await;
    assert_eq!(app.set_cart(&buyer, a.variant_id, 1).await.status, 200);
    assert_eq!(app.set_cart(&buyer, b.variant_id, 2).await.status, 200);

    // 2. Checkout, then the payment webhook.
    let res = app.checkout(&buyer, METHOD).await;
    assert_eq!(res.status, 201, "{}", res.json);
    const TOTAL: i64 = 1_500_000 + 2 * 300_000;
    assert_eq!(res.json["total_kobo"], TOTAL);
    let checkout_id: Uuid = res.json["checkout_id"].as_str().unwrap().parse().unwrap();
    let reference = res.json["reference"].as_str().unwrap().to_string();
    assert_eq!(app.reserved(a.variant_id).await, 1);
    assert_eq!(app.reserved(b.variant_id).await, 2);

    // Provider collection fee N21, shared by order total: 1,500 / 600.
    const COLLECTION_FEE: i64 = 2_100;
    app.fake.set_payment(
        &reference,
        PaymentStatus::Successful {
            amount_kobo: TOTAL,
            fee_kobo: COLLECTION_FEE,
            currency: "NGN".into(),
            held: false,
            reference: reference.clone(),
        },
    );
    let res = app
        .send_raw(
            Method::POST,
            "/webhooks/fake",
            &headers,
            hook("payment", "pay-1", &reference),
        )
        .await;
    assert_eq!(res.status, 200, "{}", res.json);
    assert_eq!(res.json["status"], "received");

    let res = app
        .request(
            Method::GET,
            &format!("/checkout/{checkout_id}"),
            None,
            Some(&buyer),
        )
        .await;
    assert_eq!(res.status, 200, "{}", res.json);
    assert_eq!(res.json["status"], "paid");
    let orders = res.json["orders"].as_array().unwrap();
    assert_eq!(orders.len(), 2);
    let order_of = |store: Uuid| -> Uuid {
        let o = orders
            .iter()
            .find(|o| o["store_id"] == store.to_string())
            .expect("an order per store");
        assert_eq!(o["status"], "paid");
        assert_eq!(o["delivery_fee_kobo"], 0);
        o["order_id"].as_str().unwrap().parse().unwrap()
    };
    let (order_a, order_b) = (order_of(a.store_id), order_of(b.store_id));
    // Stock committed, reservation released.
    assert_eq!(app.stock(a.variant_id).await, 4);
    assert_eq!(app.reserved(a.variant_id).await, 0);
    assert_eq!(app.stock(b.variant_id).await, 8);
    assert_eq!(app.reserved(b.variant_id).await, 0);

    // Expected payouts, by hand (cut 5%; payout fee N35, plus N50 stamp duty
    // when the pre-fee payout is at least N10,000):
    //   A: 1,500,000 - 75,000 cut - 1,500 fee share = 1,423,500 -> fee 8,500 -> 1,415,000
    //   B:   600,000 - 30,000 cut -   600 fee share =   569,400 -> fee 3,500 ->   565,900
    const CUT_A: i64 = 75_000;
    const CUT_B: i64 = 30_000;
    const SHARE_A: i64 = 1_500;
    const SHARE_B: i64 = 600;
    const FEE_A: i64 = 3_500 + 5_000;
    const FEE_B: i64 = 3_500;
    const PAYOUT_A: i64 = 1_415_000;
    const PAYOUT_B: i64 = 565_900;
    assert_eq!(1_500_000 - CUT_A - SHARE_A - FEE_A, PAYOUT_A);
    assert_eq!(600_000 - CUT_B - SHARE_B - FEE_B, PAYOUT_B);

    // Each seller sees their own order and the payout they can expect.
    for (listing, order, expected) in [(&a, order_a, PAYOUT_A), (&b, order_b, PAYOUT_B)] {
        let res = app
            .request(
                Method::GET,
                &format!("/orders/{order}"),
                None,
                Some(&listing.seller_token),
            )
            .await;
        assert_eq!(res.status, 200, "{}", res.json);
        assert_eq!(res.json["status"], "paid");
        assert_eq!(res.json["expected_payout_kobo"], expected);
    }

    // 3. Each seller's order is picked up, then delivered (no routes yet).
    let now = OffsetDateTime::now_utc();
    for order in [order_a, order_b] {
        mark_picked_up(&app.pool, order, now).await.unwrap();
        mark_delivered(&app.pool, order, now).await.unwrap();
        assert_eq!(order_status(&app, order).await, "delivered");
    }

    // 4. Nothing releases before the 72h deadline; at 73h both do.
    let r = run_due_jobs(&app.state).await;
    assert_eq!(r.released, 0, "{r:?}");
    exec(
        &app,
        "UPDATE orders SET release_at = now() - interval '1 hour'",
    )
    .await;
    let r = run_due_jobs(&app.state).await;
    assert_eq!((r.released, r.failed), (2, 0), "{r:?}");
    for order in [order_a, order_b] {
        assert_eq!(order_status(&app, order).await, "completed");
    }
    let payouts: Vec<(Uuid, String, String, i64, i64)> = sqlx::query_as(
        "SELECT order_id, reference, status::text, amount_kobo, fee_kobo FROM payouts",
    )
    .fetch_all(&app.pool)
    .await
    .unwrap();
    assert_eq!(payouts.len(), 2);
    let payout_of = |order: Uuid| {
        payouts
            .iter()
            .find(|p| p.0 == order)
            .cloned()
            .expect("a payout per order")
    };
    let (pa, pb) = (payout_of(order_a), payout_of(order_b));
    assert_eq!((pa.3, pa.4), (PAYOUT_A, FEE_A));
    assert_eq!((pb.3, pb.4), (PAYOUT_B, FEE_B));
    // The buyer's payment has not settled at the provider yet: still waiting.
    assert_eq!((pa.2.as_str(), pb.2.as_str()), ("scheduled", "scheduled"));
    assert!(app.fake.payouts().is_empty());

    // Past the settlement wait, the next tick sends both.
    exec(
        &app,
        "UPDATE payouts SET send_after = now() - interval '1 minute'",
    )
    .await;
    let r = run_due_jobs(&app.state).await;
    assert_eq!((r.payouts_processed, r.failed), (2, 0), "{r:?}");
    let mut sent: Vec<(String, i64, String, String)> = app
        .fake
        .payouts()
        .into_iter()
        .map(|p| (p.reference, p.amount_kobo, p.bank_code, p.account_number))
        .collect();
    sent.sort_by_key(|p| p.1);
    let account = || ("058".to_string(), ACCOUNT_NUMBER.to_string());
    assert_eq!(
        sent,
        [
            (pb.1.clone(), PAYOUT_B, account().0, account().1),
            (pa.1.clone(), PAYOUT_A, account().0, account().1),
        ]
    );

    // 5. Payout webhooks: both successful (provider transfer fees N12 / N10).
    const PROVIDER_PAYOUT_FEE_A: i64 = 1_200;
    const PROVIDER_PAYOUT_FEE_B: i64 = 1_000;
    for (p, fee, event) in [
        (&pa, PROVIDER_PAYOUT_FEE_A, "po-a"),
        (&pb, PROVIDER_PAYOUT_FEE_B, "po-b"),
    ] {
        app.fake
            .set_payout_state(&p.1, PayoutState::Successful { fee_kobo: fee, amount_kobo: None });
        let res = app
            .send_raw(
                Method::POST,
                "/webhooks/fake",
                &headers,
                hook("payout", event, &p.1),
            )
            .await;
        assert_eq!(res.status, 200, "{}", res.json);
        assert_eq!(res.json["status"], "received");
    }
    for (listing, order) in [(&a, order_a), (&b, order_b)] {
        let res = app
            .request(
                Method::GET,
                &format!("/orders/{order}"),
                None,
                Some(&listing.seller_token),
            )
            .await;
        assert_eq!(res.status, 200, "{}", res.json);
        assert_eq!(res.json["status"], "completed");
        assert_eq!(res.json["payout"]["status"], "successful");
    }

    // 6. Ledger, per order and in total.
    for (order, items, payout, revenue, fees) in [
        (
            order_a,
            1_500_000,
            PAYOUT_A,
            CUT_A + FEE_A - PROVIDER_PAYOUT_FEE_A,
            SHARE_A + PROVIDER_PAYOUT_FEE_A,
        ),
        (
            order_b,
            600_000,
            PAYOUT_B,
            CUT_B + FEE_B - PROVIDER_PAYOUT_FEE_B,
            SHARE_B + PROVIDER_PAYOUT_FEE_B,
        ),
    ] {
        let rows: Vec<(String, i64)> = sqlx::query_as(
            "SELECT account::text, sum(amount_kobo)::bigint FROM ledger_entries \
             WHERE order_id = $1 GROUP BY account ORDER BY account::text",
        )
        .bind(order)
        .fetch_all(&app.pool)
        .await
        .unwrap();
        let rows: Vec<(&str, i64)> = rows.iter().map(|(a, n)| (a.as_str(), *n)).collect();
        assert_eq!(
            rows,
            [
                ("buyer_funds", -items),
                ("escrow", 0),
                ("platform_revenue", revenue),
                ("provider_fees", fees),
                ("seller_paid", payout),
                ("seller_payable", 0),
            ],
            "order {order}"
        );
        assert_eq!(rows.iter().map(|r| r.1).sum::<i64>(), 0, "order sums to 0");
    }
    assert_orders_balance(&app).await;
    assert_eq!(
        count(
            &app,
            "SELECT count(*) FROM ledger_entries WHERE order_id IS NULL"
        )
        .await,
        0
    );
    assert_eq!(balance(&app, "escrow").await, 0);
    assert_eq!(balance(&app, "seller_payable").await, 0);
    assert_eq!(balance(&app, "seller_paid").await, 1_415_000 + 565_900);
    // (75,000 + 8,500 - 1,200) + (30,000 + 3,500 - 1,000)
    assert_eq!(balance(&app, "platform_revenue").await, 114_800);
    // 2,100 collection + 1,200 + 1,000 transfer fees
    assert_eq!(balance(&app, "provider_fees").await, 4_300);
    assert_eq!(balance(&app, "buyer_funds").await, -TOTAL);

    // Audit trail of the money flow.
    let actions: Vec<(String, i64)> = sqlx::query_as(
        "SELECT action, count(*) FROM audit_log \
         WHERE action LIKE 'checkout.%' OR action LIKE 'order.%' OR action LIKE 'payout.%' \
         GROUP BY action ORDER BY action",
    )
    .fetch_all(&app.pool)
    .await
    .unwrap();
    let actions: Vec<(&str, i64)> = actions.iter().map(|(a, n)| (a.as_str(), *n)).collect();
    assert_eq!(
        actions,
        [
            ("checkout.created", 1),
            ("checkout.paid", 1),
            ("order.delivered", 2),
            ("order.picked_up", 2),
            ("order.released", 2),
            ("payout.send_attempt", 2),
            ("payout.settled", 2),
        ]
    );
    let released: Vec<(Uuid, serde_json::Value)> =
        sqlx::query_as("SELECT subject_id, detail FROM audit_log WHERE action = 'order.released'")
            .fetch_all(&app.pool)
            .await
            .unwrap();
    for (order, payout, fee) in [(order_a, PAYOUT_A, FEE_A), (order_b, PAYOUT_B, FEE_B)] {
        let (_, d) = released.iter().find(|(id, _)| *id == order).unwrap();
        assert_eq!(d["reason"], "auto_after_delivery");
        assert_eq!(d["payout_kobo"], payout);
        assert_eq!(d["payout_fee_kobo"], fee);
    }
    // The payout account number never reaches the audit log.
    assert_eq!(
        count(
            &app,
            &format!("SELECT count(*) FROM audit_log WHERE detail::text LIKE '%{ACCOUNT_NUMBER}%'")
        )
        .await,
        0
    );
}
