mod common;

use axum::http::Method;
use common::TestApp;
use serde_json::{Value, json};
use sqlx::PgPool;

async fn get(app: &TestApp, token: &str) -> common::TestResponse {
    app.request(Method::GET, "/admin/settings", None, Some(token))
        .await
}

/// The settings' current `updated_at`, as the API returns it.
async fn current_version(app: &TestApp) -> String {
    let at: time::OffsetDateTime = sqlx::query_scalar("SELECT updated_at FROM platform_settings")
        .fetch_one(&app.pool)
        .await
        .unwrap();
    at.format(&time::format_description::well_known::Rfc3339)
        .unwrap()
}

/// PATCH with the current `expected_updated_at` added (unless the body has one).
async fn patch(app: &TestApp, token: &str, mut body: Value) -> common::TestResponse {
    if let Some(obj) = body.as_object_mut()
        && !obj.contains_key("expected_updated_at")
    {
        obj.insert(
            "expected_updated_at".into(),
            json!(current_version(app).await),
        );
    }
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

async fn audit_count(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM audit_log WHERE action = 'settings.updated'")
        .fetch_one(pool)
        .await
        .unwrap()
}

#[sqlx::test]
async fn backstop_must_strictly_outlast_release(pool: PgPool) {
    let app = TestApp::new(pool.clone()).await;
    let owner = app.make_owner().await;
    // Stored release is 72h: a 1-day backstop alone is invalid.
    let res = patch(&app, &owner, json!({ "backstop_days": 1 })).await;
    assert_eq!(res.status, 422, "{}", res.json);
    assert_eq!(res.json["error"]["code"], "invalid_settings");
    // Rejected patches leave no audit row.
    assert_eq!(audit_count(&pool).await, 0);

    // Backstop is at its default of 30 days.
    // 720h == 30 days: equal is not strictly greater.
    let res = patch(&app, &owner, json!({ "release_after_hours": 720 })).await;
    assert_eq!(res.status, 422, "{}", res.json);
    assert_eq!(res.json["error"]["code"], "invalid_settings");
    assert_eq!(get(&app, &owner).await.json["release_after_hours"], 72);
    assert_eq!(audit_count(&pool).await, 0);
}

#[sqlx::test]
async fn database_checks_reject_bad_rows(pool: PgPool) {
    for sql in [
        "UPDATE platform_settings SET platform_cut_bps = 3001",
        "UPDATE platform_settings SET checkout_ttl_minutes = 4",
        "UPDATE platform_settings SET backstop_days = 1",
        "UPDATE platform_settings SET fee_small_kobo = -1",
        "UPDATE platform_settings SET enabled_methods = '{}'",
    ] {
        assert!(sqlx::query(sql).execute(&pool).await.is_err(), "{sql}");
    }
}

#[test]
fn migration_fee_cap_matches_limits_module() {
    let sql = include_str!("../migrations/0008_platform_settings.sql");
    let cap = server::limits::MAX_PRICE_KOBO.to_string();
    for col in ["fee_small_kobo", "fee_large_kobo", "stamp_duty_kobo"] {
        assert!(sql.contains(&format!("{col} BETWEEN 0 AND {cap}")), "{col}");
    }
}

#[sqlx::test]
async fn rejected_patch_writes_no_audit_row(pool: PgPool) {
    let app = TestApp::new(pool.clone()).await;
    let owner = app.make_owner().await;
    assert_eq!(
        patch(&app, &owner, json!({ "platform_cut_bps": 3001 }))
            .await
            .status,
        422
    );
    assert_eq!(patch(&app, &owner, json!({})).await.status, 422);
    assert_eq!(audit_count(&pool).await, 0);
    // A no-op patch (same value) changes nothing, so audits nothing.
    assert_eq!(
        patch(&app, &owner, json!({ "platform_cut_bps": 500 }))
            .await
            .status,
        200
    );
    assert_eq!(audit_count(&pool).await, 0);
}

#[sqlx::test]
async fn patch_requires_expected_updated_at(pool: PgPool) {
    let app = TestApp::new(pool.clone()).await;
    let owner = app.make_owner().await;
    let res = app
        .request(
            Method::PATCH,
            "/admin/settings",
            Some(json!({ "platform_cut_bps": 450 })),
            Some(&owner),
        )
        .await;
    assert_eq!(res.status, 422, "{}", res.json);
    assert_eq!(res.json["error"]["code"], "validation_failed");
    assert_eq!(get(&app, &owner).await.json["platform_cut_bps"], 500);
}

#[sqlx::test]
async fn stale_patch_is_rejected(pool: PgPool) {
    let app = TestApp::new(pool.clone()).await;
    let owner = app.make_owner().await;
    let seen = get(&app, &owner).await.json["updated_at"].clone();
    // Another edit lands after this owner loaded the settings.
    assert_eq!(
        patch(&app, &owner, json!({ "platform_cut_bps": 450 }))
            .await
            .status,
        200
    );
    let res = patch(
        &app,
        &owner,
        json!({ "platform_cut_bps": 400, "expected_updated_at": seen }),
    )
    .await;
    assert_eq!(res.status, 409, "{}", res.json);
    assert_eq!(res.json["error"]["code"], "stale_settings");
    assert_eq!(get(&app, &owner).await.json["platform_cut_bps"], 450);
    assert_eq!(audit_count(&pool).await, 1);
    // With the version it now holds, the edit goes through.
    let now = get(&app, &owner).await.json["updated_at"].clone();
    let res = patch(
        &app,
        &owner,
        json!({ "platform_cut_bps": 400, "expected_updated_at": now }),
    )
    .await;
    assert_eq!(res.status, 200, "{}", res.json);
}
