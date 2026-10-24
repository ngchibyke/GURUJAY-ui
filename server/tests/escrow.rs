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
    let total = res.json["total_kobo"].as_i64().unwrap();
    let checkout_id: Uuid = res.json["checkout_id"].as_str().unwrap().parse().unwrap();
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
        release_order(&app.state, f.order_id, ReleaseReason::AutoAfterDelivery),
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
    assert_eq!(p.amount_kobo, -660);
    assert_eq!(app.fake.calls("create_payout"), 0);
    assert_eq!(order_status(&app, f.order_id).await, "completed");
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
    release(&app, &f).await;
    let p = payout(&app).await;
    assert_eq!(p.status, "scheduled");
    assert_eq!(p.last_error.as_deref(), Some("order_held"));
    assert_eq!(app.fake.calls("create_payout"), 0);

    exec(&app, "UPDATE orders SET held_at = NULL, hold_reason = NULL").await;
    make_due(&app).await;
    assert_eq!(send(&app, p.id).await.status, "pending");
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
        .set_payout_state(&p.reference, PayoutState::Successful { fee_kobo: 3_000 });
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
        .set_payout_state(&p.reference, PayoutState::Successful { fee_kobo: 3_000 });
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
        .set_payout_state(&p.reference, PayoutState::Successful { fee_kobo: 0 });
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
        .set_payout_state(&p.reference, PayoutState::Successful { fee_kobo: 0 });
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
        .set_payout_state(&p.reference, PayoutState::Successful { fee_kobo: 3_000 });
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
