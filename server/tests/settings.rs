mod common;

use axum::http::Method;
use common::TestApp;
use serde_json::{Value, json};
use sqlx::PgPool;

async fn get(app: &TestApp, token: &str) -> common::TestResponse {
    app.request(Method::GET, "/admin/settings", None, Some(token))
        .await
}

async fn patch(app: &TestApp, token: &str, body: Value) -> common::TestResponse {
    app.request(Method::PATCH, "/admin/settings", Some(body), Some(token))
        .await
}

#[sqlx::test]
async fn defaults_are_spec_values(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let owner = app.make_owner().await;
    let res = get(&app, &owner).await;
    assert_eq!(res.status, 200, "{}", res.json);
    let j = &res.json;
    assert_eq!(j["platform_cut_bps"], 500);
    assert_eq!(j["fee_small_kobo"], 3500);
    assert_eq!(j["fee_large_kobo"], 5000);
    assert_eq!(j["fee_threshold_kobo"], 5_000_000);
    assert_eq!(j["stamp_duty_kobo"], 5000);
    assert_eq!(j["stamp_duty_threshold_kobo"], 1_000_000);
    assert_eq!(j["enabled_methods"], json!(["mono_directpay"]));
    assert_eq!(j["payout_provider"], "mono");
    assert_eq!(j["checkout_ttl_minutes"], 30);
    assert_eq!(j["ship_deadline_hours"], 48);
    assert_eq!(j["release_after_hours"], 72);
    assert_eq!(j["backstop_days"], 30);
    assert_eq!(j["payout_settlement_buffer_hours"], 2);
}

#[sqlx::test]
async fn support_can_read_user_cannot(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let _owner = app.make_owner().await;
    let support = app.make_support().await;
    let (_, user) = app.sign_up_verified("08011111111").await;
    assert_eq!(get(&app, &support).await.status, 200);
    assert_eq!(get(&app, &user).await.status, 403);
    let none = app
        .request(Method::GET, "/admin/settings", None, None)
        .await;
    assert_eq!(none.status, 401);
}

#[sqlx::test]
async fn owner_updates_cut(pool: PgPool) {
    let app = TestApp::new(pool.clone()).await;
    let owner = app.make_owner().await;
    let res = patch(&app, &owner, json!({ "platform_cut_bps": 450 })).await;
    assert_eq!(res.status, 200, "{}", res.json);
    assert_eq!(res.json["platform_cut_bps"], 450);
    assert_eq!(get(&app, &owner).await.json["platform_cut_bps"], 450);
    // Untouched fields keep their values.
    assert_eq!(get(&app, &owner).await.json["fee_small_kobo"], 3500);

    let detail: String =
        sqlx::query_scalar("SELECT detail::text FROM audit_log WHERE action = 'settings.updated'")
            .fetch_one(&pool)
            .await
            .expect("audit row present");
    let v: Value = serde_json::from_str(&detail).unwrap();
    assert_eq!(v, json!({ "platform_cut_bps": { "old": 500, "new": 450 } }));
}

#[sqlx::test]
async fn support_cannot_patch(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let _owner = app.make_owner().await;
    let support = app.make_support().await;
    let res = patch(&app, &support, json!({ "platform_cut_bps": 450 })).await;
    assert_eq!(res.status, 403);
    assert_eq!(get(&app, &support).await.json["platform_cut_bps"], 500);
}

#[sqlx::test]
async fn cut_out_of_range_rejected(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let owner = app.make_owner().await;
    for bad in [json!(3001), json!(-1)] {
        let res = patch(&app, &owner, json!({ "platform_cut_bps": bad })).await;
        assert_eq!(res.status, 422, "{}", res.json);
    }
    assert_eq!(
        patch(&app, &owner, json!({ "platform_cut_bps": 3000 }))
            .await
            .status,
        200
    );
    assert_eq!(
        patch(&app, &owner, json!({ "platform_cut_bps": 0 }))
            .await
            .status,
        200
    );
}

#[sqlx::test]
async fn unknown_method_rejected(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let owner = app.make_owner().await;
    let res = patch(
        &app,
        &owner,
        json!({ "enabled_methods": ["paystack_card"] }),
    )
    .await;
    assert_eq!(res.status, 422);
    assert_eq!(res.json["error"]["code"], "unknown_payment_method");
}

#[sqlx::test]
async fn methods_must_be_non_empty_and_unique(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let owner = app.make_owner().await;
    assert_eq!(
        patch(&app, &owner, json!({ "enabled_methods": [] }))
            .await
            .status,
        422
    );
    assert_eq!(
        patch(
            &app,
            &owner,
            json!({ "enabled_methods": ["mono_directpay", "mono_directpay"] })
        )
        .await
        .status,
        422
    );
    assert_eq!(
        patch(
            &app,
            &owner,
            json!({ "enabled_methods": ["mono_directpay"] })
        )
        .await
        .status,
        200
    );
}

#[sqlx::test]
async fn empty_and_unknown_fields_rejected(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let owner = app.make_owner().await;
    assert_eq!(patch(&app, &owner, json!({})).await.status, 422);
    assert_eq!(
        patch(&app, &owner, json!({ "platform_cut_bps": 400, "bogus": 1 }))
            .await
            .status,
        422
    );
}

#[sqlx::test]
async fn numeric_bounds_enforced(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let owner = app.make_owner().await;
    let bad = [
        json!({ "fee_small_kobo": -1 }),
        json!({ "fee_large_kobo": 1_000_000_001i64 }),
        json!({ "fee_threshold_kobo": -5 }),
        json!({ "stamp_duty_kobo": -1 }),
        json!({ "stamp_duty_threshold_kobo": -1 }),
        json!({ "checkout_ttl_minutes": 4 }),
        json!({ "checkout_ttl_minutes": 121 }),
        json!({ "ship_deadline_hours": 0 }),
        json!({ "ship_deadline_hours": 169 }),
        json!({ "release_after_hours": 0 }),
        json!({ "release_after_hours": 721 }),
        json!({ "backstop_days": 0 }),
        json!({ "backstop_days": 91 }),
        json!({ "payout_settlement_buffer_hours": -1 }),
        json!({ "payout_settlement_buffer_hours": 73 }),
        json!({ "payout_provider": "paystack" }),
    ];
    for body in bad {
        let res = patch(&app, &owner, body.clone()).await;
        assert_eq!(res.status, 422, "{body} -> {}", res.json);
    }
    let ok = patch(
        &app,
        &owner,
        json!({
            "fee_small_kobo": 0, "fee_large_kobo": 1_000_000_000i64,
            "fee_threshold_kobo": 0, "stamp_duty_kobo": 0,
            "stamp_duty_threshold_kobo": 0, "checkout_ttl_minutes": 5,
            "ship_deadline_hours": 168, "release_after_hours": 720,
            "backstop_days": 90, "payout_settlement_buffer_hours": 0,
            "payout_provider": "mono"
        }),
    )
    .await;
    assert_eq!(ok.status, 200, "{}", ok.json);
}

#[sqlx::test]
async fn settings_table_is_single_row(pool: PgPool) {
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM platform_settings")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n, 1);
    let second = sqlx::query("INSERT INTO platform_settings (id) VALUES (false)")
        .execute(&pool)
        .await;
    assert!(second.is_err());
    let dup = sqlx::query("INSERT INTO platform_settings (id) VALUES (true)")
        .execute(&pool)
        .await;
    assert!(dup.is_err());
}

#[sqlx::test]
async fn load_builds_fee_schedule(pool: PgPool) {
    let s = server::settings::service::load(&pool).await.unwrap();
    assert_eq!(s.fee_schedule(), server::money::FeeSchedule::default());
    assert_eq!(s.platform_cut_bps, 500);
}
