//! Background jobs (Task 9). Every due comparison uses the database clock, so
//! the tests back-date the stored deadlines (`expires_at`, `ship_by`,
//! `release_at`, `backstop_at`, ...) with SQL and then run one tick. Each
//! time-based test also asserts that the due case changed something.

mod common;

use std::time::Duration;

use axum::http::Method;
use common::TestApp;
use serde_json::json;
use server::{
    checkout::{SettleOutcome, settle_payment},
    escrow::jobs::{
        JobReport, MAX_REFUND_ATTEMPTS, SHIP_DEADLINE_PER_TICK, refund_missed_ship_deadline,
        run_due_jobs, run_loop_every,
    },
    orders::service::{mark_delivered, mark_picked_up},
    payments::{PaymentStatus, PayoutState, ProviderError},
};
use sqlx::PgPool;
use time::OffsetDateTime;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const METHOD: &str = "mono_directpay";
const SELLER: &str = "08011111111";
const BUYER: &str = "08022222221";
const PRICE: i64 = 100_000;

struct Fixture {
    order_id: Uuid,
    store_id: Uuid,
    variant_id: Uuid,
    buyer: String,
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

async fn checkout_status(app: &TestApp, id: Uuid) -> String {
    sqlx::query_scalar("SELECT status::text FROM checkouts WHERE id = $1")
        .bind(id)
        .fetch_one(&app.pool)
        .await
        .unwrap()
}

async fn payout_status(app: &TestApp) -> String {
    text(app, "SELECT status::text FROM payouts").await
}

async fn tick(app: &TestApp) -> JobReport {
    run_due_jobs(&app.state).await
}

/// The ship-deadline job ships switched off (no pickup route before plan 3);
/// tests of the job itself switch it on.
async fn ship_deadline_on(app: &TestApp) {
    exec(
        app,
        "UPDATE platform_settings SET jobs_ship_deadline_enabled = true",
    )
    .await;
}

/// A pending checkout for one unit; returns (checkout id, reference, buyer token, listing variant).
async fn pending_checkout(app: &TestApp) -> (Uuid, String, String, Uuid, Uuid) {
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
    (
        checkout_id,
        reference,
        buyer,
        listing.variant_id,
        listing.store_id,
    )
}

fn paid(app: &TestApp, reference: &str, total: i64) {
    app.fake.set_payment(
        reference,
        PaymentStatus::Successful {
            amount_kobo: total,
            fee_kobo: 100,
            currency: "NGN".into(),
            held: false,
            reference: reference.to_string(),
        },
    );
}

/// One paid order. The payment is back-dated 10 days, so a payout released
/// from it is past the provider's settlement wait and sends at once.
async fn paid_order(app: &TestApp) -> Fixture {
    let (checkout_id, reference, buyer, variant_id, store_id) = pending_checkout(app).await;
    paid(app, &reference, PRICE);
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
        store_id,
        variant_id,
        buyer,
    }
}

async fn delivered_order(app: &TestApp) -> Fixture {
    let f = paid_order(app).await;
    let now = OffsetDateTime::now_utc();
    mark_picked_up(&app.pool, f.order_id, now).await.unwrap();
    mark_delivered(&app.pool, f.order_id, now).await.unwrap();
    f
}

/// A delivered order whose payout is released (due at once) by the job.
async fn released_by_job(app: &TestApp) -> Fixture {
    let f = delivered_order(app).await;
    exec(
        app,
        "UPDATE orders SET release_at = now() - interval '1 minute'",
    )
    .await;
    let r = tick(app).await;
    assert_eq!(r.released, 1);
    f
}

// ---------------------------------------------------------------- expiry

#[sqlx::test]
async fn checkout_expires_after_30_min_restores_stock(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let (checkout_id, _, _, variant_id, _) = pending_checkout(&app).await;
    assert_eq!(app.reserved(variant_id).await, 1);

    // 29 minutes in: the stored deadline is a minute away.
    exec(
        &app,
        "UPDATE checkouts SET expires_at = now() + interval '1 minute'",
    )
    .await;
    let r = tick(&app).await;
    assert_eq!(r.expired, 0);
    assert_eq!(checkout_status(&app, checkout_id).await, "pending");
    assert_eq!(app.reserved(variant_id).await, 1);

    // 31 minutes in.
    exec(
        &app,
        "UPDATE checkouts SET expires_at = now() - interval '1 minute'",
    )
    .await;
    let r = tick(&app).await;
    assert_eq!(r.expired, 1);
    assert_eq!(checkout_status(&app, checkout_id).await, "expired");
    assert_eq!(app.reserved(variant_id).await, 0);
    assert_eq!(app.stock(variant_id).await, 5);
    assert_eq!(
        text(&app, "SELECT status::text FROM orders").await,
        "cancelled"
    );
    assert_eq!(
        count(
            &app,
            "SELECT count(*) FROM audit_log WHERE action = 'checkout.expired'"
        )
        .await,
        1
    );
}

#[sqlx::test]
async fn lost_webhook_reverify_settles_recently_expired_checkout(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let (checkout_id, reference, _, _, _) = pending_checkout(&app).await;
    exec(
        &app,
        "UPDATE checkouts SET expires_at = now() - interval '1 minute'",
    )
    .await;
    // The buyer paid but the webhook was lost: the job's verify finds it.
    paid(&app, &reference, PRICE);
    let r = tick(&app).await;
    assert_eq!(r.expired, 1);
    assert_eq!(r.reverified, 1);
    assert_eq!(checkout_status(&app, checkout_id).await, "paid");
    assert_eq!(app.fake.calls("verify_payment"), 1);
}

#[sqlx::test]
async fn lost_webhook_reverify_backs_off_and_stops_after_48h(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let (checkout_id, _, _, _, _) = pending_checkout(&app).await;
    exec(
        &app,
        "UPDATE checkouts SET expires_at = now() - interval '1 minute'",
    )
    .await;
    // Still unpaid at the provider: verified once, then not again next tick.
    tick(&app).await;
    assert_eq!(checkout_status(&app, checkout_id).await, "expired");
    assert_eq!(app.fake.calls("verify_payment"), 1);
    app.state.verify_gate.clear();
    tick(&app).await;
    assert_eq!(app.fake.calls("verify_payment"), 1, "backed off");

    // Due again: verified again.
    exec(
        &app,
        "UPDATE checkouts SET next_reverify_at = now() - interval '1 second'",
    )
    .await;
    app.state.verify_gate.clear();
    tick(&app).await;
    assert_eq!(app.fake.calls("verify_payment"), 2);

    // Expired more than 48h ago: never verified again.
    exec(
        &app,
        "UPDATE checkouts SET next_reverify_at = NULL, expires_at = now() - interval '49 hours'",
    )
    .await;
    app.state.verify_gate.clear();
    tick(&app).await;
    assert_eq!(app.fake.calls("verify_payment"), 2);
}

// ---------------------------------------------------------------- ship deadline

/// I1: with no pickup route yet, the job would refund every paid order and
/// strike its seller, so a fresh database ships with it off.
#[sqlx::test]
async fn ship_deadline_is_off_on_a_fresh_database(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let f = paid_order(&app).await;
    exec(
        &app,
        "UPDATE orders SET ship_by = now() - interval '1 hour'",
    )
    .await;
    let r = tick(&app).await;
    assert_eq!(r.auto_cancelled, 0);
    assert_eq!(order_status(&app, f.order_id).await, "paid");
    assert_eq!(count(&app, "SELECT count(*) FROM seller_strikes").await, 0);
    assert_eq!(count(&app, "SELECT count(*) FROM refunds").await, 0);
    let default: String = text(
        &app,
        "SELECT column_default FROM information_schema.columns \
         WHERE table_name = 'platform_settings' AND column_name = 'jobs_ship_deadline_enabled'",
    )
    .await;
    assert_eq!(default, "false");
}

#[sqlx::test]
async fn ship_deadline_48h_refunds_and_strikes(pool: PgPool) {
    let app = TestApp::new(pool).await;
    ship_deadline_on(&app).await;
    let f = paid_order(&app).await;
    app.fake.set_refund_supported(true);
    assert_eq!(app.stock(f.variant_id).await, 4);

    // 47h after payment.
    exec(
        &app,
        "UPDATE orders SET ship_by = now() + interval '1 hour'",
    )
    .await;
    let r = tick(&app).await;
    assert_eq!(r.auto_cancelled, 0);
    assert_eq!(order_status(&app, f.order_id).await, "paid");

    // 49h after payment.
    exec(
        &app,
        "UPDATE orders SET ship_by = now() - interval '1 hour'",
    )
    .await;
    let r = tick(&app).await;
    assert_eq!(r.auto_cancelled, 1);
    assert_eq!(order_status(&app, f.order_id).await, "refunded");
    assert_eq!(
        text(&app, "SELECT reason FROM refunds").await,
        "ship_deadline"
    );
    assert_eq!(
        text(&app, "SELECT status::text FROM refunds").await,
        "processing"
    );
    assert_eq!(app.fake.calls("refund"), 1);
    assert_eq!(app.stock(f.variant_id).await, 5, "stock back on hand");
    let strikes: Vec<(Uuid, Uuid, String)> =
        sqlx::query_as("SELECT store_id, order_id, reason FROM seller_strikes")
            .fetch_all(&app.pool)
            .await
            .unwrap();
    assert_eq!(
        strikes,
        [(f.store_id, f.order_id, "ship_deadline".to_string())]
    );
    assert_eq!(
        count(
            &app,
            "SELECT count(*) FROM audit_log WHERE action = 'order.ship_deadline_missed'"
        )
        .await,
        1
    );

    // Again: nothing more.
    let r = tick(&app).await;
    assert_eq!(r.auto_cancelled, 0);
    assert_eq!(count(&app, "SELECT count(*) FROM seller_strikes").await, 1);
    assert_eq!(app.fake.calls("refund"), 1);
}

#[sqlx::test]
async fn ship_deadline_kill_switch_and_held_orders_skip(pool: PgPool) {
    let app = TestApp::new(pool).await;
    ship_deadline_on(&app).await;
    let f = paid_order(&app).await;
    exec(
        &app,
        "UPDATE orders SET ship_by = now() - interval '1 hour'",
    )
    .await;

    // The owner switches the job off through the settings route (audited).
    let owner = app.owner_token().await;
    let switch = |on: bool| {
        let (app, owner) = (&app, &owner);
        async move {
            let version = app
                .request(Method::GET, "/admin/settings", None, Some(owner))
                .await
                .json["updated_at"]
                .clone();
            app.request(
                Method::PATCH,
                "/admin/settings",
                Some(json!({ "jobs_ship_deadline_enabled": on, "expected_updated_at": version })),
                Some(owner),
            )
            .await
        }
    };
    let res = switch(false).await;
    assert_eq!(res.status, 200, "{}", res.json);
    assert_eq!(res.json["jobs_ship_deadline_enabled"], false);
    assert_eq!(
        count(
            &app,
            "SELECT count(*) FROM audit_log WHERE action = 'settings.updated' \
             AND detail ? 'jobs_ship_deadline_enabled'"
        )
        .await,
        1
    );
    let r = tick(&app).await;
    assert_eq!(r.auto_cancelled, 0);
    assert_eq!(order_status(&app, f.order_id).await, "paid");

    assert_eq!(switch(true).await.status, 200);
    exec(
        &app,
        "UPDATE orders SET held_at = now(), hold_reason = 'complaint'",
    )
    .await;
    let r = tick(&app).await;
    assert_eq!(r.auto_cancelled, 0);
    assert_eq!(order_status(&app, f.order_id).await, "paid");
    assert_eq!(count(&app, "SELECT count(*) FROM seller_strikes").await, 0);

    exec(&app, "UPDATE orders SET held_at = NULL, hold_reason = NULL").await;
    let r = tick(&app).await;
    assert_eq!(r.auto_cancelled, 1);
    assert_eq!(count(&app, "SELECT count(*) FROM seller_strikes").await, 1);
}

/// An order cannot be `paid` with a refund row: `begin_refund` moves it to
/// `refund_pending` in the same transaction that inserts the row (and
/// `refunds.order_id` is unique). The guard covers the race instead: the job
/// claimed the order while `paid`, the buyer cancelled before the job's lock,
/// and `begin_refund` then hands back the buyer's existing refund. Calling the
/// per-order action on such an order reproduces that state deterministically.
#[sqlx::test]
async fn ship_deadline_after_a_racing_buyer_cancel_gives_no_strike(pool: PgPool) {
    let app = TestApp::new(pool).await;
    ship_deadline_on(&app).await;
    let f = paid_order(&app).await;
    exec(
        &app,
        "UPDATE orders SET ship_by = now() - interval '1 hour'",
    )
    .await;
    let res = app
        .request(
            Method::POST,
            &format!("/orders/{}/cancel", f.order_id),
            None,
            Some(&f.buyer),
        )
        .await;
    assert_eq!(res.status, 200, "{}", res.json);
    let refunds_before = app.fake.calls("refund");

    // The buyer's refund (reason "refund"): the reason check refuses it.
    assert!(
        !refund_missed_ship_deadline(&app.state, f.order_id)
            .await
            .unwrap()
    );
    // Even a refund carrying the deadline reason from an earlier transaction:
    // the created_at check refuses it.
    exec(&app, "UPDATE refunds SET reason = 'ship_deadline'").await;
    assert!(
        !refund_missed_ship_deadline(&app.state, f.order_id)
            .await
            .unwrap()
    );

    assert_eq!(count(&app, "SELECT count(*) FROM seller_strikes").await, 0);
    assert_eq!(
        count(
            &app,
            "SELECT count(*) FROM audit_log WHERE action = 'order.ship_deadline_missed'"
        )
        .await,
        0
    );
    assert_eq!(app.fake.calls("refund"), refunds_before);
    assert_eq!(tick(&app).await.auto_cancelled, 0);
}

#[sqlx::test]
async fn ship_deadline_skips_flagged_payments(pool: PgPool) {
    let app = TestApp::new(pool).await;
    ship_deadline_on(&app).await;
    let f = paid_order(&app).await;
    app.fake.set_refund_supported(true);
    exec(
        &app,
        "UPDATE orders SET ship_by = now() - interval '1 hour'",
    )
    .await;
    exec(&app, "UPDATE checkouts SET provider_flagged = true").await;
    let r = tick(&app).await;
    assert_eq!((r.auto_cancelled, r.failed), (0, 0));
    // The in-transaction re-check refuses it too.
    assert!(
        !refund_missed_ship_deadline(&app.state, f.order_id)
            .await
            .unwrap()
    );
    assert_eq!(order_status(&app, f.order_id).await, "paid");
    assert_eq!(count(&app, "SELECT count(*) FROM refunds").await, 0);
    assert_eq!(count(&app, "SELECT count(*) FROM seller_strikes").await, 0);
    assert_eq!(app.fake.calls("refund"), 0);

    exec(&app, "UPDATE checkouts SET provider_flagged = false").await;
    assert_eq!(tick(&app).await.auto_cancelled, 1);
    assert_eq!(order_status(&app, f.order_id).await, "refunded");
    assert_eq!(count(&app, "SELECT count(*) FROM seller_strikes").await, 1);
    assert_eq!(app.fake.calls("refund"), 1);
}

#[sqlx::test]
async fn ship_deadline_caps_refunds_per_tick(pool: PgPool) {
    let app = TestApp::new(pool).await;
    ship_deadline_on(&app).await;
    let listing = app
        .make_active_store_with_product(SELLER, "Mama Put", PRICE, 100)
        .await;
    let (_, buyer) = app.sign_up_verified(BUYER).await;
    let n = SHIP_DEADLINE_PER_TICK + 1;
    for _ in 0..n {
        assert_eq!(
            app.set_cart(&buyer, listing.variant_id, 1).await.status,
            200
        );
        let res = app.checkout(&buyer, METHOD).await;
        assert_eq!(res.status, 201, "{}", res.json);
        let reference = res.json["reference"].as_str().unwrap().to_string();
        paid(&app, &reference, PRICE);
        assert_eq!(
            settle_payment(&app.state, &reference).await.unwrap(),
            SettleOutcome::Paid
        );
    }
    exec(
        &app,
        "UPDATE orders SET ship_by = now() - interval '1 hour'",
    )
    .await;

    let r = tick(&app).await;
    assert_eq!(r.auto_cancelled, 20);
    assert_eq!(
        count(&app, "SELECT count(*) FROM orders WHERE status = 'paid'").await,
        1
    );
    let r = tick(&app).await;
    assert_eq!(r.auto_cancelled, 1);
    assert_eq!(
        count(&app, "SELECT count(*) FROM orders WHERE status = 'paid'").await,
        0
    );
    assert_eq!(count(&app, "SELECT count(*) FROM seller_strikes").await, n);
}

/// M1: a row whose engine call fails is pushed back, not reclaimed at the
/// head of the batch every tick.
#[sqlx::test]
async fn failed_row_is_pushed_back(pool: PgPool) {
    let app = TestApp::new(pool).await;
    ship_deadline_on(&app).await;
    let f = paid_order(&app).await;
    exec(
        &app,
        "UPDATE orders SET ship_by = now() - interval '1 hour'",
    )
    .await;
    // Another session holds the variant lock: begin_refund times out (5s).
    let mut blocker = app.pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM product_variants WHERE id = $1 FOR UPDATE")
        .bind(f.variant_id)
        .execute(&mut *blocker)
        .await
        .unwrap();
    let r = tick(&app).await;
    assert_eq!((r.auto_cancelled, r.failed), (0, 1));
    blocker.rollback().await.unwrap();

    // Pushed back: not claimed on the next tick even though it is now free.
    let r = tick(&app).await;
    assert_eq!((r.auto_cancelled, r.failed), (0, 0));
    assert_eq!(order_status(&app, f.order_id).await, "paid");
    assert_eq!(
        count(
            &app,
            "SELECT count(*) FROM job_row_failures WHERE job = 'ship_deadline' \
             AND failures = 1 AND next_try_at > now() + interval '4 minutes'"
        )
        .await,
        1
    );

    // Once its push-back passes, it is processed.
    exec(
        &app,
        "UPDATE job_row_failures SET next_try_at = now() - interval '1 second'",
    )
    .await;
    assert_eq!(tick(&app).await.auto_cancelled, 1);
    // A success clears the row's push-back.
    assert_eq!(
        count(&app, "SELECT count(*) FROM job_row_failures").await,
        0
    );
}

// ---------------------------------------------------------------- auto-release

#[sqlx::test]
async fn release_72h_after_delivered(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let f = delivered_order(&app).await;

    // 71h after delivery.
    exec(
        &app,
        "UPDATE orders SET release_at = now() + interval '1 hour'",
    )
    .await;
    let r = tick(&app).await;
    assert_eq!(r.released, 0);
    assert_eq!(order_status(&app, f.order_id).await, "delivered");
    assert_eq!(count(&app, "SELECT count(*) FROM payouts").await, 0);

    // 73h after delivery.
    exec(
        &app,
        "UPDATE orders SET release_at = now() - interval '1 hour'",
    )
    .await;
    let r = tick(&app).await;
    assert_eq!(r.released, 1);
    assert_eq!(order_status(&app, f.order_id).await, "completed");
    assert_eq!(
        text(&app, "SELECT release_reason FROM payouts").await,
        "auto_after_delivery"
    );
    assert_eq!(payout_status(&app).await, "pending");
    assert_eq!(app.fake.calls("create_payout"), 1);
}

#[sqlx::test]
async fn no_release_without_delivered_at(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let f = paid_order(&app).await;
    mark_picked_up(
        &app.pool,
        f.order_id,
        OffsetDateTime::now_utc() - time::Duration::days(10),
    )
    .await
    .unwrap();
    exec(
        &app,
        "UPDATE orders SET paid_at = now() - interval '10 days', ship_by = now() - interval '8 days'",
    )
    .await;
    let r = tick(&app).await;
    assert_eq!(r.released, 0);
    assert_eq!(order_status(&app, f.order_id).await, "picked_up");
    assert_eq!(count(&app, "SELECT count(*) FROM payouts").await, 0);

    // Positive control: once delivered and due, it releases.
    mark_delivered(&app.pool, f.order_id, OffsetDateTime::now_utc())
        .await
        .unwrap();
    exec(
        &app,
        "UPDATE orders SET release_at = now() - interval '1 minute'",
    )
    .await;
    assert_eq!(tick(&app).await.released, 1);
}

#[sqlx::test]
async fn auto_release_skips_held_and_flagged(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let f = delivered_order(&app).await;
    exec(
        &app,
        "UPDATE orders SET release_at = now() - interval '1 hour'",
    )
    .await;

    exec(
        &app,
        "UPDATE orders SET held_at = now(), hold_reason = 'complaint'",
    )
    .await;
    assert_eq!(tick(&app).await.released, 0);
    assert_eq!(tick(&app).await.failed, 0, "a held order is not even tried");
    assert_eq!(order_status(&app, f.order_id).await, "delivered");

    exec(&app, "UPDATE orders SET held_at = NULL, hold_reason = NULL").await;
    exec(&app, "UPDATE checkouts SET provider_flagged = true").await;
    let r = tick(&app).await;
    assert_eq!((r.released, r.failed), (0, 0));
    assert_eq!(order_status(&app, f.order_id).await, "delivered");

    exec(&app, "UPDATE checkouts SET provider_flagged = false").await;
    assert_eq!(tick(&app).await.released, 1);
    assert_eq!(order_status(&app, f.order_id).await, "completed");
}

// ---------------------------------------------------------------- backstop

#[sqlx::test]
async fn backstop_flags_once_at_30_days(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let f = paid_order(&app).await;
    mark_picked_up(&app.pool, f.order_id, OffsetDateTime::now_utc())
        .await
        .unwrap();

    exec(
        &app,
        "UPDATE orders SET backstop_at = now() + interval '1 hour'",
    )
    .await;
    assert_eq!(tick(&app).await.flagged, 0);

    exec(
        &app,
        "UPDATE orders SET backstop_at = now() - interval '1 hour'",
    )
    .await;
    assert_eq!(tick(&app).await.flagged, 1);
    let first: OffsetDateTime =
        sqlx::query_scalar("SELECT backstop_flagged_at FROM orders WHERE id = $1")
            .bind(f.order_id)
            .fetch_one(&app.pool)
            .await
            .unwrap();
    assert_eq!(tick(&app).await.flagged, 0);
    let again: OffsetDateTime =
        sqlx::query_scalar("SELECT backstop_flagged_at FROM orders WHERE id = $1")
            .bind(f.order_id)
            .fetch_one(&app.pool)
            .await
            .unwrap();
    assert_eq!(first, again);
    assert_eq!(
        count(
            &app,
            "SELECT count(*) FROM audit_log WHERE action = 'order.backstop_flagged'"
        )
        .await,
        1
    );
    // Flagging changes nothing else.
    assert_eq!(order_status(&app, f.order_id).await, "picked_up");
}

// ---------------------------------------------------------------- payouts

#[sqlx::test]
async fn payout_retry_then_manual_after_8(pool: PgPool) {
    let app = TestApp::new(pool).await;
    app.fake
        .fail_payout_with(ProviderError::Unavailable(anyhow::anyhow!("down")));
    released_by_job(&app).await;
    // The inline send after release was attempt 1.
    assert_eq!(payout_status(&app).await, "scheduled");
    assert_eq!(app.fake.calls("create_payout"), 1);

    // In backoff: not retried.
    let r = tick(&app).await;
    assert_eq!(r.payouts_processed, 0);
    assert_eq!(app.fake.calls("create_payout"), 1);

    for attempt in 2..=8 {
        exec(
            &app,
            "UPDATE payouts SET next_attempt_at = now() - interval '1 second'",
        )
        .await;
        let r = tick(&app).await;
        assert_eq!(r.payouts_processed, 1, "attempt {attempt}");
        assert_eq!(app.fake.calls("create_payout"), attempt);
    }
    assert_eq!(payout_status(&app).await, "manual");
    assert_eq!(count(&app, "SELECT attempts::bigint FROM payouts").await, 8);
    // Manual is final for the job.
    exec(&app, "UPDATE payouts SET next_attempt_at = NULL").await;
    tick(&app).await;
    assert_eq!(app.fake.calls("create_payout"), 8);
}

#[sqlx::test]
async fn payout_send_after_not_reached_is_not_claimed(pool: PgPool) {
    let app = TestApp::new(pool).await;
    app.fake
        .fail_payout_with(ProviderError::Unavailable(anyhow::anyhow!("down")));
    released_by_job(&app).await;
    exec(
        &app,
        "UPDATE payouts SET next_attempt_at = NULL, send_after = now() + interval '1 hour'",
    )
    .await;
    assert_eq!(tick(&app).await.payouts_processed, 0);
    exec(
        &app,
        "UPDATE payouts SET send_after = now() - interval '1 second'",
    )
    .await;
    app.fake.clear_payout_failure();
    assert_eq!(tick(&app).await.payouts_processed, 1);
    assert_eq!(payout_status(&app).await, "pending");
}

#[sqlx::test]
async fn payout_rejected_goes_manual_with_audit(pool: PgPool) {
    let app = TestApp::new(pool).await;
    app.fake
        .fail_payout_with(ProviderError::Unavailable(anyhow::anyhow!("down")));
    released_by_job(&app).await;
    app.fake.fail_payout_with(ProviderError::Rejected {
        code: "invalid_account".into(),
        message: "rejected".into(),
    });
    exec(&app, "UPDATE payouts SET next_attempt_at = NULL").await;
    let r = tick(&app).await;
    assert_eq!(r.payouts_processed, 1);
    assert_eq!(payout_status(&app).await, "manual");
    assert_eq!(
        text(&app, "SELECT last_error FROM payouts").await,
        "rejected"
    );
    assert_eq!(
        count(
            &app,
            "SELECT count(*) FROM audit_log WHERE action = 'payout.escalated_manual' \
             AND detail->>'reason' = 'rejected'"
        )
        .await,
        1
    );
}

#[sqlx::test]
async fn pending_payout_polled_at_most_hourly(pool: PgPool) {
    let app = TestApp::new(pool).await;
    released_by_job(&app).await;
    assert_eq!(payout_status(&app).await, "pending");
    let reference = text(&app, "SELECT reference FROM payouts").await;

    // Accepted just now: not polled.
    assert_eq!(tick(&app).await.payouts_processed, 0);
    assert_eq!(app.fake.calls("payout_status"), 0);

    // Accepted over an hour ago: polled once, then not again within the hour.
    exec(
        &app,
        "UPDATE payouts SET pending_since = now() - interval '2 hours'",
    )
    .await;
    assert_eq!(tick(&app).await.payouts_processed, 1);
    assert_eq!(app.fake.calls("payout_status"), 1);
    assert_eq!(tick(&app).await.payouts_processed, 0);
    assert_eq!(app.fake.calls("payout_status"), 1);
    assert_eq!(payout_status(&app).await, "pending");

    // An hour after the last poll: polled again and settled.
    exec(
        &app,
        "UPDATE payouts SET last_polled_at = now() - interval '61 minutes'",
    )
    .await;
    app.fake.set_payout_state(
        &reference,
        PayoutState::Successful {
            fee_kobo: 1_000,
            amount_kobo: None,
        },
    );
    assert_eq!(tick(&app).await.payouts_processed, 1);
    assert_eq!(payout_status(&app).await, "successful");
}

#[sqlx::test]
async fn pending_poll_unavailable_still_waits_an_hour(pool: PgPool) {
    let app = TestApp::new(pool).await;
    released_by_job(&app).await;
    // The payout's provider cannot be asked (not configured): the poll is a
    // "retry later", which must still wait an hour, not repeat every tick.
    exec(
        &app,
        "UPDATE payouts SET pending_since = now() - interval '2 hours', provider = 'gone'",
    )
    .await;
    let r = tick(&app).await;
    assert_eq!((r.payouts_processed, r.failed), (1, 0));
    assert_eq!(tick(&app).await.payouts_processed, 0);
    assert_eq!(payout_status(&app).await, "pending");
    exec(
        &app,
        "UPDATE payouts SET last_polled_at = now() - interval '61 minutes'",
    )
    .await;
    assert_eq!(tick(&app).await.payouts_processed, 1);
}

#[sqlx::test]
async fn expired_sending_lease_looked_up_by_job(pool: PgPool) {
    let app = TestApp::new(pool).await;
    app.fake
        .fail_payout_with(ProviderError::Unavailable(anyhow::anyhow!("down")));
    released_by_job(&app).await;
    let reference = text(&app, "SELECT reference FROM payouts").await;
    app.fake.clear_payout_failure();
    // A sender died mid-call: its lease ran out.
    exec(
        &app,
        "UPDATE payouts SET status = 'sending', next_attempt_at = NULL, \
                sending_until = now() - interval '1 minute'",
    )
    .await;
    app.fake.set_found_payout(&reference, Some("PROV123"));
    assert_eq!(tick(&app).await.payouts_processed, 1);
    assert_eq!(app.fake.calls("find_payout"), 1);
    assert_eq!(
        app.fake.calls("create_payout"),
        1,
        "adopted, not re-created"
    );
    assert_eq!(payout_status(&app).await, "pending");
}

// ---------------------------------------------------------------- refunds

/// M2: a due send for a held order or a flagged payment is not even claimed
/// (no lock, no defer write); a pending payout is still polled.
#[sqlx::test]
async fn payout_claim_skips_held_and_flagged(pool: PgPool) {
    let app = TestApp::new(pool).await;
    app.fake
        .fail_payout_with(ProviderError::Unavailable(anyhow::anyhow!("down")));
    released_by_job(&app).await;
    app.fake.clear_payout_failure();
    exec(&app, "UPDATE payouts SET next_attempt_at = NULL").await;

    exec(
        &app,
        "UPDATE orders SET held_at = now(), hold_reason = 'complaint'",
    )
    .await;
    assert_eq!(tick(&app).await.payouts_processed, 0);
    exec(&app, "UPDATE orders SET held_at = NULL, hold_reason = NULL").await;
    exec(&app, "UPDATE checkouts SET provider_flagged = true").await;
    assert_eq!(tick(&app).await.payouts_processed, 0);
    assert_eq!(app.fake.calls("create_payout"), 1);
    assert_eq!(payout_status(&app).await, "scheduled");
    assert!(
        count(
            &app,
            "SELECT count(*) FROM payouts WHERE next_attempt_at IS NULL"
        )
        .await
            == 1,
        "untouched, not deferred"
    );

    // A payout already accepted is still polled while the order is flagged.
    exec(&app, "UPDATE checkouts SET provider_flagged = false").await;
    assert_eq!(tick(&app).await.payouts_processed, 1);
    assert_eq!(payout_status(&app).await, "pending");
    exec(
        &app,
        "UPDATE payouts SET pending_since = now() - interval '2 hours'",
    )
    .await;
    exec(&app, "UPDATE checkouts SET provider_flagged = true").await;
    assert_eq!(tick(&app).await.payouts_processed, 1);
    assert_eq!(app.fake.calls("payout_status"), 1);
}

#[sqlx::test]
async fn refund_sweeper_stops_after_8_attempts(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let f = paid_order(&app).await;
    app.fake.set_refund_supported(true);
    app.fake
        .fail_refund_with(ProviderError::Unavailable(anyhow::anyhow!("down")));
    // Attempt 1 is the inline send.
    let out = server::escrow::refund::refund_order(&app.state, f.order_id, true)
        .await
        .unwrap();
    assert_eq!(out.status, "requested");
    let due = "UPDATE refunds SET updated_at = now() - interval '2 minutes', \
                                  next_attempt_at = NULL";
    for attempt in 2..=MAX_REFUND_ATTEMPTS {
        exec(&app, due).await;
        assert_eq!(tick(&app).await.refunds_retried, 1, "attempt {attempt}");
        assert_eq!(app.fake.calls("refund"), attempt as usize);
    }
    assert_eq!(
        text(&app, "SELECT status::text FROM refunds").await,
        "requested"
    );
    exec(&app, due).await;
    let r = tick(&app).await;
    assert_eq!(r.refunds_retried, 0);
    assert_eq!(app.fake.calls("refund"), 8, "no 9th call");
    assert_eq!(
        text(&app, "SELECT status::text FROM refunds").await,
        "manual"
    );
    assert_eq!(order_status(&app, f.order_id).await, "refund_pending");
    assert_eq!(
        count(
            &app,
            "SELECT count(*) FROM audit_log WHERE action = 'refund.escalated_manual' \
             AND detail->>'from' = 'requested' AND detail->>'reason' = 'unavailable'"
        )
        .await,
        1
    );
    // Final for the job.
    exec(&app, due).await;
    tick(&app).await;
    assert_eq!(app.fake.calls("refund"), 8);
    assert_eq!(
        count(
            &app,
            "SELECT count(*) FROM audit_log WHERE action = 'refund.escalated_manual'"
        )
        .await,
        1
    );
}

#[sqlx::test]
async fn refund_sweeper_retries_with_same_reference(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let f = paid_order(&app).await;
    app.fake.set_refund_supported(true);
    app.fake
        .fail_refund_with(ProviderError::Unavailable(anyhow::anyhow!("down")));
    let out = server::escrow::refund::refund_order(&app.state, f.order_id, true)
        .await
        .unwrap();
    assert_eq!(out.status, "requested");
    assert_eq!(app.fake.calls("refund"), 1);

    // Just attempted: the sweeper leaves it a minute.
    assert_eq!(tick(&app).await.refunds_retried, 0);
    assert_eq!(app.fake.calls("refund"), 1);
    exec(
        &app,
        "UPDATE refunds SET updated_at = now() - interval '2 minutes'",
    )
    .await;

    // Still down: the sweeper tries once, then backs off.
    let r = tick(&app).await;
    assert_eq!(r.refunds_retried, 1);
    assert_eq!(app.fake.calls("refund"), 2);
    exec(
        &app,
        "UPDATE refunds SET updated_at = now() - interval '2 minutes'",
    )
    .await;
    tick(&app).await;
    assert_eq!(app.fake.calls("refund"), 2, "backed off");

    app.fake.clear_refund_failure();
    exec(
        &app,
        "UPDATE refunds SET next_attempt_at = now() - interval '1 second'",
    )
    .await;
    let r = tick(&app).await;
    assert_eq!(r.refunds_retried, 1);
    assert_eq!(
        text(&app, "SELECT status::text FROM refunds").await,
        "processing"
    );
    assert_eq!(order_status(&app, f.order_id).await, "refunded");
    let refs: Vec<String> = app
        .fake
        .refunds()
        .into_iter()
        .map(|r| r.reference)
        .collect();
    assert_eq!(refs.len(), 3);
    assert!(
        refs.iter().all(|r| *r == out.reference),
        "same RF reference"
    );
}

// ---------------------------------------------------------------- invariants

#[sqlx::test]
async fn settings_change_does_not_move_existing_deadlines(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let f = delivered_order(&app).await;
    // A second checkout, still pending.
    assert_eq!(app.set_cart(&f.buyer, f.variant_id, 1).await.status, 200);
    let res = app.checkout(&f.buyer, METHOD).await;
    assert_eq!(res.status, 201, "{}", res.json);
    let pending: Uuid = res.json["checkout_id"].as_str().unwrap().parse().unwrap();

    // The owner shortens every timer to its minimum...
    let owner = app.owner_token().await;
    let res = app
        .request(
            Method::PATCH,
            "/admin/settings",
            Some(json!({
                "checkout_ttl_minutes": 5, "ship_deadline_hours": 1,
                "release_after_hours": 1, "backstop_days": 1,
                "expected_updated_at": app
                    .request(Method::GET, "/admin/settings", None, Some(&owner))
                    .await
                    .json["updated_at"]
                    .clone(),
            })),
            Some(&owner),
        )
        .await;
    assert_eq!(res.status, 200, "{}", res.json);
    // ...and the events those timers count from are long past.
    exec(
        &app,
        "UPDATE checkouts SET created_at = now() - interval '1 hour' WHERE status = 'pending'",
    )
    .await;
    exec(
        &app,
        "UPDATE orders SET paid_at = now() - interval '2 days', \
                delivered_at = now() - interval '2 days' WHERE status = 'delivered'",
    )
    .await;

    let r = tick(&app).await;
    assert_eq!(
        (r.expired, r.released, r.flagged, r.auto_cancelled),
        (0, 0, 0, 0)
    );
    assert_eq!(checkout_status(&app, pending).await, "pending");
    assert_eq!(order_status(&app, f.order_id).await, "delivered");

    // Positive control: the stored deadlines alone decide.
    exec(
        &app,
        "UPDATE checkouts SET expires_at = now() - interval '1 second' WHERE status = 'pending'",
    )
    .await;
    exec(
        &app,
        "UPDATE orders SET release_at = now() - interval '1 second' WHERE status = 'delivered'",
    )
    .await;
    let r = tick(&app).await;
    assert_eq!((r.expired, r.released), (1, 1));
    assert_eq!(checkout_status(&app, pending).await, "expired");
    assert_eq!(order_status(&app, f.order_id).await, "completed");
}

#[derive(Debug, PartialEq, sqlx::FromRow)]
struct Snapshot {
    orders: String,
    checkouts: String,
    payouts: String,
    refunds: String,
    strikes: i64,
    ledger: i64,
    audits: i64,
}

async fn snapshot(app: &TestApp) -> Snapshot {
    sqlx::query_as(
        "SELECT \
           (SELECT string_agg(id::text || status::text || \
                   coalesce(backstop_flagged_at::text, '-'), ',' ORDER BY id) FROM orders) AS orders, \
           (SELECT string_agg(id::text || status::text, ',' ORDER BY id) FROM checkouts) AS checkouts, \
           coalesce((SELECT string_agg(id::text || status::text || attempts::text, ',' ORDER BY id) \
                     FROM payouts), '') AS payouts, \
           coalesce((SELECT string_agg(id::text || status::text || attempts::text, ',' ORDER BY id) \
                     FROM refunds), '') AS refunds, \
           (SELECT count(*) FROM seller_strikes) AS strikes, \
           (SELECT count(*) FROM ledger_entries) AS ledger, \
           (SELECT count(*) FROM audit_log) AS audits",
    )
    .fetch_one(&app.pool)
    .await
    .unwrap()
}

#[sqlx::test]
async fn job_rerun_is_idempotent(pool: PgPool) {
    let app = TestApp::new(pool).await;
    ship_deadline_on(&app).await;
    app.fake.set_refund_supported(true);
    // One order due on each of release, ship deadline and backstop, plus an
    // expired checkout.
    let f = delivered_order(&app).await;
    exec(
        &app,
        "UPDATE orders SET release_at = now() - interval '1 minute', \
                backstop_at = now() - interval '1 minute'",
    )
    .await;
    assert_eq!(app.set_cart(&f.buyer, f.variant_id, 1).await.status, 200);
    let res = app.checkout(&f.buyer, METHOD).await;
    let second: Uuid = res.json["checkout_id"].as_str().unwrap().parse().unwrap();
    let reference = res.json["reference"].as_str().unwrap().to_string();
    paid(&app, &reference, PRICE);
    assert_eq!(
        settle_payment(&app.state, &reference).await.unwrap(),
        SettleOutcome::Paid
    );
    exec(
        &app,
        &format!(
            "UPDATE orders SET ship_by = now() - interval '1 minute' WHERE checkout_id = '{second}'"
        ),
    )
    .await;
    // A picked-up order past its backstop (and not due for anything else).
    assert_eq!(app.set_cart(&f.buyer, f.variant_id, 1).await.status, 200);
    let res = app.checkout(&f.buyer, METHOD).await;
    let fourth: Uuid = res.json["checkout_id"].as_str().unwrap().parse().unwrap();
    let reference = res.json["reference"].as_str().unwrap().to_string();
    paid(&app, &reference, PRICE);
    assert_eq!(
        settle_payment(&app.state, &reference).await.unwrap(),
        SettleOutcome::Paid
    );
    let backstop_order: Uuid = sqlx::query_scalar("SELECT id FROM orders WHERE checkout_id = $1")
        .bind(fourth)
        .fetch_one(&app.pool)
        .await
        .unwrap();
    mark_picked_up(&app.pool, backstop_order, OffsetDateTime::now_utc())
        .await
        .unwrap();
    exec(
        &app,
        &format!(
            "UPDATE orders SET backstop_at = now() - interval '1 minute' \
             WHERE id = '{backstop_order}'"
        ),
    )
    .await;
    assert_eq!(app.set_cart(&f.buyer, f.variant_id, 1).await.status, 200);
    let third = app.checkout(&f.buyer, METHOD).await;
    assert_eq!(third.status, 201, "{}", third.json);
    exec(
        &app,
        "UPDATE checkouts SET expires_at = now() - interval '1 minute' WHERE status = 'pending'",
    )
    .await;

    let first = tick(&app).await;
    assert_eq!(first.released, 1);
    assert_eq!(first.auto_cancelled, 1);
    assert_eq!(first.expired, 1);
    // The delivered order was released earlier in the same tick, so only the
    // picked-up one is flagged.
    assert_eq!(first.flagged, 1);
    assert_eq!(first.failed, 0);
    let after_first = snapshot(&app).await;
    let calls = (
        app.fake.calls("create_payout"),
        app.fake.calls("refund"),
        app.fake.calls("verify_payment"),
    );

    app.state.verify_gate.clear();
    let again = tick(&app).await;
    assert_eq!(again, JobReport::default());
    assert_eq!(snapshot(&app).await, after_first);
    assert_eq!(
        (
            app.fake.calls("create_payout"),
            app.fake.calls("refund"),
            app.fake.calls("verify_payment"),
        ),
        calls
    );
}

// sqlx::test has no multi-thread flavor; the two paths still interleave on their
// own DB connections, so the race is between two real transactions.
#[sqlx::test]
async fn received_and_job_race_one_payout(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let f = delivered_order(&app).await;
    exec(
        &app,
        "UPDATE orders SET release_at = now() - interval '1 minute'",
    )
    .await;
    let path = format!("/orders/{}/received", f.order_id);
    let (res, report) = tokio::join!(
        app.request(
            Method::POST,
            &path,
            Some(json!({ "confirm": true })),
            Some(&f.buyer)
        ),
        run_due_jobs(&app.state),
    );
    assert_eq!(res.status, 200, "{}", res.json);
    assert_eq!(report.failed, 0);
    assert_eq!(count(&app, "SELECT count(*) FROM payouts").await, 1);
    assert_eq!(app.fake.calls("create_payout"), 1);
    assert_eq!(order_status(&app, f.order_id).await, "completed");
}

// ---------------------------------------------------------------- loop & ops

#[sqlx::test]
async fn job_runs_record_last_success(pool: PgPool) {
    let app = TestApp::new(pool).await;
    tick(&app).await;
    let jobs: Vec<(String, bool)> =
        sqlx::query_as("SELECT job, last_success_at IS NOT NULL FROM job_runs ORDER BY job")
            .fetch_all(&app.pool)
            .await
            .unwrap();
    let names: Vec<&str> = jobs.iter().map(|(j, _)| j.as_str()).collect();
    assert_eq!(
        names,
        [
            "auto_release",
            "backstop",
            "expire_checkouts",
            "payouts",
            "refunds",
            "reverify_checkouts",
            "ship_deadline"
        ]
    );
    assert!(jobs.iter().all(|(_, ok)| *ok));
}

#[sqlx::test]
async fn loop_runs_ticks_and_stops_on_cancel(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let (checkout_id, _, _, _, _) = pending_checkout(&app).await;
    exec(
        &app,
        "UPDATE checkouts SET expires_at = now() - interval '1 minute'",
    )
    .await;
    let token = CancellationToken::new();
    let handle = tokio::spawn(run_loop_every(
        app.state.clone(),
        token.clone(),
        Duration::from_millis(50),
    ));
    let mut expired = false;
    for _ in 0..100 {
        if checkout_status(&app, checkout_id).await == "expired" {
            expired = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(expired, "the loop ran a tick");
    token.cancel();
    tokio::time::timeout(Duration::from_secs(10), handle)
        .await
        .expect("loop stops after cancel")
        .unwrap();
}

#[tokio::test]
async fn pool_sets_idle_in_transaction_timeout() {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
    let pool = server::pool_options(2).connect(&url).await.unwrap();
    let v: String = sqlx::query_scalar("SHOW idle_in_transaction_session_timeout")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(v, "1min");
    pool.close().await;
}

// ------------------------------------------------- (7) failure-path checks

/// A test-only trigger that makes `table` updates matching `when` fail.
async fn fail_updates(app: &TestApp, table: &str, when: &str) {
    exec(
        app,
        "CREATE FUNCTION test_fail() RETURNS trigger LANGUAGE plpgsql AS \
         $$ BEGIN RAISE EXCEPTION 'test failure'; END $$",
    )
    .await;
    exec(
        app,
        &format!(
            "CREATE TRIGGER test_fail BEFORE UPDATE ON {table} FOR EACH ROW \
             WHEN ({when}) EXECUTE FUNCTION test_fail()"
        ),
    )
    .await;
}

/// A second paid order from the same buyer and listing.
async fn another_paid_order(app: &TestApp, f: &Fixture) -> Uuid {
    assert_eq!(app.set_cart(&f.buyer, f.variant_id, 1).await.status, 200);
    let res = app.checkout(&f.buyer, METHOD).await;
    assert_eq!(res.status, 201, "{}", res.json);
    let reference = res.json["reference"].as_str().unwrap().to_string();
    paid(app, &reference, PRICE);
    assert_eq!(
        settle_payment(&app.state, &reference).await.unwrap(),
        SettleOutcome::Paid
    );
    let checkout_id: Uuid = res.json["checkout_id"].as_str().unwrap().parse().unwrap();
    sqlx::query_scalar("SELECT id FROM orders WHERE checkout_id = $1")
        .bind(checkout_id)
        .fetch_one(&app.pool)
        .await
        .unwrap()
}

/// (7): the exhausted-refund escalation failing does not stop the sweeper
/// sending the other due refunds; the escalation runs on a later tick.
#[sqlx::test]
async fn refund_escalation_failure_still_lets_the_sweeper_send(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let f = paid_order(&app).await;
    let second = another_paid_order(&app, &f).await;
    app.fake.set_refund_supported(true);
    app.fake
        .fail_refund_with(ProviderError::Unavailable(anyhow::anyhow!("down")));
    for order in [f.order_id, second] {
        let out = server::escrow::refund::refund_order(&app.state, order, true)
            .await
            .unwrap();
        assert_eq!(out.status, "requested");
    }
    app.fake.clear_refund_failure();
    exec(
        &app,
        "UPDATE refunds SET updated_at = now() - interval '2 minutes', next_attempt_at = NULL",
    )
    .await;
    exec(
        &app,
        &format!(
            "UPDATE refunds SET attempts = {MAX_REFUND_ATTEMPTS} WHERE order_id = '{}'",
            f.order_id
        ),
    )
    .await;
    fail_updates(&app, "refunds", "NEW.status = 'manual'").await;
    let calls = app.fake.calls("refund");

    let r = tokio::time::timeout(Duration::from_secs(30), tick(&app))
        .await
        .unwrap();
    assert!(r.failed >= 1, "{r:?}");
    assert_eq!(r.refunds_retried, 1);
    assert_eq!(app.fake.calls("refund"), calls + 1);
    assert_eq!(order_status(&app, second).await, "refunded");
    assert_eq!(
        count(
            &app,
            "SELECT count(*) FROM refunds WHERE status = 'requested'"
        )
        .await,
        1
    );

    exec(&app, "DROP TRIGGER test_fail ON refunds").await;
    tick(&app).await;
    assert_eq!(
        count(&app, "SELECT count(*) FROM refunds WHERE status = 'manual'").await,
        1
    );
}

/// (7): the refund and reverify jobs push failed rows back through their own
/// columns; an error writes no `job_row_failures` row for them.
#[sqlx::test]
async fn refund_and_reverify_errors_write_no_push_back_rows(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let f = paid_order(&app).await;
    app.fake.set_refund_supported(true);
    app.fake
        .fail_refund_with(ProviderError::Unavailable(anyhow::anyhow!("down")));
    server::escrow::refund::refund_order(&app.state, f.order_id, true)
        .await
        .unwrap();
    exec(
        &app,
        "UPDATE refunds SET updated_at = now() - interval '2 minutes', next_attempt_at = NULL",
    )
    .await;
    // A lost-webhook checkout the reverify will find paid.
    let listing = app
        .make_active_store_with_product("08011111112", "Buka", PRICE, 5)
        .await;
    let (_, buyer) = app.sign_up_verified("08022222229").await;
    assert_eq!(
        app.set_cart(&buyer, listing.variant_id, 1).await.status,
        200
    );
    let res = app.checkout(&buyer, METHOD).await;
    assert_eq!(res.status, 201, "{}", res.json);
    let reference = res.json["reference"].as_str().unwrap().to_string();
    let checkout_id: Uuid = res.json["checkout_id"].as_str().unwrap().parse().unwrap();
    exec(
        &app,
        &format!("UPDATE checkouts SET expires_at = now() - interval '1 minute' WHERE id = '{checkout_id}'"),
    )
    .await;
    paid(&app, &reference, PRICE);

    // Both engine calls fail inside the database.
    exec(
        &app,
        "CREATE FUNCTION test_fail() RETURNS trigger LANGUAGE plpgsql AS \
         $$ BEGIN RAISE EXCEPTION 'test failure'; END $$",
    )
    .await;
    exec(
        &app,
        "CREATE TRIGGER test_fail_refunds BEFORE UPDATE ON refunds FOR EACH ROW \
         WHEN (NEW.sending_until IS NOT NULL) EXECUTE FUNCTION test_fail()",
    )
    .await;
    exec(
        &app,
        "CREATE TRIGGER test_fail_checkouts BEFORE UPDATE ON checkouts FOR EACH ROW \
         WHEN (NEW.status = 'paid') EXECUTE FUNCTION test_fail()",
    )
    .await;
    let r = tick(&app).await;
    assert!(r.failed >= 2, "{r:?}");
    assert_eq!(r.refunds_retried, 0);
    assert_eq!(r.reverified, 0);
    assert_eq!(checkout_status(&app, checkout_id).await, "expired");
    assert_eq!(
        count(
            &app,
            "SELECT count(*) FROM job_row_failures \
             WHERE job IN ('refunds', 'reverify_checkouts')"
        )
        .await,
        0
    );
}
