mod common;

use axum::http::Method;
use common::TestApp;
use serde_json::json;
use server::payments::ProviderError;
use sqlx::PgPool;
use uuid::Uuid;

const NUBAN: &str = "0123456785";

/// Seller with a pending store; returns (token, store id, slug).
async fn seller_store(app: &TestApp) -> (String, Uuid, String) {
    let (_, token) = app.make_seller("08011111111").await;
    let res = app
        .request(
            Method::POST,
            "/stores",
            Some(json!({
                "name": "Mama Put",
                "pickup_address": "12 Unique Street, Wuse 2",
                "pickup_state": "FCT",
                "delivery_courier": true,
                "delivery_self": false
            })),
            Some(&token),
        )
        .await;
    assert_eq!(res.status, 201, "{}", res.json);
    (
        token,
        res.json["id"].as_str().unwrap().parse().unwrap(),
        res.json["slug"].as_str().unwrap().to_string(),
    )
}

async fn use_fake_for_payouts(app: &TestApp) {
    sqlx::query("UPDATE platform_settings SET payout_provider = 'fake'")
        .execute(&app.pool)
        .await
        .unwrap();
}

async fn put(app: &TestApp, token: &str, id: Uuid, number: &str) -> common::TestResponse {
    app.request(
        Method::PUT,
        &format!("/stores/{id}/payout-account"),
        Some(json!({ "bank_code": "058", "account_number": number, "password": common::TEST_PASSWORD })),
        Some(token),
    )
    .await
}

async fn store_status(app: &TestApp, id: Uuid) -> String {
    sqlx::query_scalar("SELECT status::text FROM stores WHERE id = $1")
        .bind(id)
        .fetch_one(&app.pool)
        .await
        .unwrap()
}

async fn public_status(app: &TestApp, slug: &str) -> u16 {
    app.request(Method::GET, &format!("/stores/{slug}"), None, None)
        .await
        .status
}

#[sqlx::test]
async fn provider_verified_account_activates_store(pool: PgPool) {
    let app = TestApp::new(pool).await;
    use_fake_for_payouts(&app).await;
    app.fake.set_lookup(Ok("ADA OBI".into()));
    let (token, id, slug) = seller_store(&app).await;
    assert_eq!(public_status(&app, &slug).await, 404);
    let res = put(&app, &token, id, NUBAN).await;
    assert_eq!(res.status, 200, "{}", res.json);
    assert_eq!(res.json["status"], "verified");
    assert_eq!(res.json["account_name"], "ADA OBI");
    assert_eq!(store_status(&app, id).await, "active");
    assert_eq!(public_status(&app, &slug).await, 200);
    let by: String =
        sqlx::query_scalar("SELECT verified_by FROM payout_accounts WHERE store_id = $1")
            .bind(id)
            .fetch_one(&app.pool)
            .await
            .unwrap();
    assert_eq!(by, "provider");
}

#[sqlx::test]
async fn unsupported_lookup_leaves_pending(pool: PgPool) {
    let app = TestApp::new(pool).await;
    use_fake_for_payouts(&app).await;
    app.fake.set_lookup(Err(ProviderError::NotSupported));
    let (token, id, _) = seller_store(&app).await;
    let res = put(&app, &token, id, NUBAN).await;
    assert_eq!(res.status, 200, "{}", res.json);
    assert_eq!(res.json["status"], "pending");
    assert!(res.json["account_name"].is_null());
    assert_eq!(store_status(&app, id).await, "pending");
}

#[sqlx::test]
async fn transient_provider_failure_leaves_pending(pool: PgPool) {
    let app = TestApp::new(pool).await;
    use_fake_for_payouts(&app).await;
    app.fake.set_lookup(Err(ProviderError::Timeout));
    let (token, id, _) = seller_store(&app).await;
    let res = put(&app, &token, id, NUBAN).await;
    assert_eq!(res.status, 200, "{}", res.json);
    assert_eq!(res.json["status"], "pending");
    assert_eq!(store_status(&app, id).await, "pending");
}

#[sqlx::test]
async fn unregistered_payout_provider_leaves_pending(pool: PgPool) {
    // Default setting is "mono", which TestApp does not register.
    let app = TestApp::new(pool).await;
    let (token, id, _) = seller_store(&app).await;
    let res = put(&app, &token, id, NUBAN).await;
    assert_eq!(res.status, 200, "{}", res.json);
    assert_eq!(res.json["status"], "pending");
    assert_eq!(app.fake.calls("resolve_account"), 0);
}

#[sqlx::test]
async fn provider_rejection_is_account_not_found(pool: PgPool) {
    let app = TestApp::new(pool).await;
    use_fake_for_payouts(&app).await;
    app.fake.set_lookup(Err(ProviderError::Rejected {
        code: "not_found".into(),
        message: "no such account".into(),
    }));
    let (token, id, _) = seller_store(&app).await;
    let res = put(&app, &token, id, NUBAN).await;
    assert_eq!(res.status, 422, "{}", res.json);
    assert_eq!(res.json["error"]["code"], "account_not_found");
    assert!(!res.json.to_string().contains(NUBAN));
    // Nothing was saved.
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM payout_accounts")
        .fetch_one(&app.pool)
        .await
        .unwrap();
    assert_eq!(n, 0);
    assert_eq!(store_status(&app, id).await, "pending");
}

#[sqlx::test]
async fn owner_manual_verify_activates(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let (token, id, slug) = seller_store(&app).await;
    put(&app, &token, id, NUBAN).await;
    let admin = app.owner_token().await;
    let path = format!("/admin/stores/{id}/payout-account/verify");
    let seen = seen_changed_at(&app, id, &admin).await;
    let res = app
        .request(
            Method::POST,
            &path,
            Some(json!({ "account_name": "ADA OBI", "changed_at": seen })),
            Some(&admin),
        )
        .await;
    assert_eq!(res.status, 200, "{}", res.json);
    assert_eq!(res.json["status"], "verified");
    assert_eq!(store_status(&app, id).await, "active");
    assert_eq!(public_status(&app, &slug).await, 200);
    let got = app
        .request(
            Method::GET,
            &format!("/stores/{id}/payout-account"),
            None,
            Some(&token),
        )
        .await;
    assert_eq!(got.json["account_name"], "ADA OBI");
    // Audit carries who verified, never the name or number.
    let rows: Vec<String> = sqlx::query_scalar(
        "SELECT detail::text FROM audit_log WHERE action IN ('payout_account.updated', 'payout_account.verified')",
    )
    .fetch_all(&app.pool)
    .await
    .unwrap();
    assert_eq!(rows.len(), 2);
    for r in &rows {
        assert!(!r.contains("ADA") && !r.contains(NUBAN), "{r}");
    }
    assert!(rows.iter().any(|r| r.contains("owner")));
    assert!(rows.iter().any(|r| r.contains("6785")));
}

#[sqlx::test]
async fn manual_verify_requires_owner_role_and_account(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let (token, id, _) = seller_store(&app).await;
    let path = format!("/admin/stores/{id}/payout-account/verify");
    let body = json!({ "account_name": "ADA OBI", "changed_at": "2026-01-01T00:00:00Z" });
    let admin = app.owner_token().await;
    // No account yet.
    let none = app
        .request(Method::POST, &path, Some(body.clone()), Some(&admin))
        .await;
    assert_eq!(none.status, 404);
    put(&app, &token, id, NUBAN).await;
    let seller = app
        .request(Method::POST, &path, Some(body.clone()), Some(&token))
        .await;
    assert_eq!(seller.status, 403);
    let support = app.make_support().await;
    let s = app
        .request(Method::POST, &path, Some(body.clone()), Some(&support))
        .await;
    assert_eq!(s.status, 403);
    let empty = app
        .request(
            Method::POST,
            &path,
            Some(json!({ "account_name": "  ", "changed_at": "2026-01-01T00:00:00Z" })),
            Some(&admin),
        )
        .await;
    assert_eq!(empty.status, 422);
}

#[sqlx::test]
async fn changing_account_deactivates_store(pool: PgPool) {
    let app = TestApp::new(pool).await;
    use_fake_for_payouts(&app).await;
    app.fake.set_lookup(Ok("ADA OBI".into()));
    let (token, id, slug) = seller_store(&app).await;
    put(&app, &token, id, NUBAN).await;
    assert_eq!(store_status(&app, id).await, "active");
    app.fake.set_lookup(Err(ProviderError::NotSupported));
    let res = put(&app, &token, id, "9876543216").await;
    assert_eq!(res.json["status"], "pending");
    assert!(res.json["account_name"].is_null());
    assert_eq!(store_status(&app, id).await, "pending");
    assert_eq!(public_status(&app, &slug).await, 404);
}

#[sqlx::test]
async fn suspended_store_is_never_unsuspended(pool: PgPool) {
    let app = TestApp::new(pool).await;
    use_fake_for_payouts(&app).await;
    app.fake.set_lookup(Ok("ADA OBI".into()));
    let (token, id, _) = seller_store(&app).await;
    sqlx::query("UPDATE stores SET status = 'suspended' WHERE id = $1")
        .bind(id)
        .execute(&app.pool)
        .await
        .unwrap();
    let res = put(&app, &token, id, NUBAN).await;
    assert_eq!(res.json["status"], "verified");
    assert_eq!(store_status(&app, id).await, "suspended");
    let admin = app.owner_token().await;
    let seen = seen_changed_at(&app, id, &admin).await;
    app.request(
        Method::POST,
        &format!("/admin/stores/{id}/payout-account/verify"),
        Some(json!({ "account_name": "X", "changed_at": seen })),
        Some(&admin),
    )
    .await;
    assert_eq!(store_status(&app, id).await, "suspended");
}

#[sqlx::test]
async fn account_number_encrypted_and_masked(pool: PgPool) {
    let app = TestApp::new(pool).await;
    use_fake_for_payouts(&app).await;
    app.fake.set_lookup(Ok("ADA OBI".into()));
    let (token, id, _) = seller_store(&app).await;
    let put_res = put(&app, &token, id, NUBAN).await;
    assert!(!put_res.json.to_string().contains(NUBAN));
    let (enc, name_enc, last4): (Vec<u8>, Option<Vec<u8>>, String) = sqlx::query_as(
        "SELECT account_number_enc, account_name_enc, account_last4 FROM payout_accounts \
         WHERE store_id = $1",
    )
    .bind(id)
    .fetch_one(&app.pool)
    .await
    .unwrap();
    assert!(!String::from_utf8_lossy(&enc).contains(NUBAN));
    assert!(!String::from_utf8_lossy(&name_enc.unwrap()).contains("ADA OBI"));
    assert_eq!(last4, "6785");
    let got = app
        .request(
            Method::GET,
            &format!("/stores/{id}/payout-account"),
            None,
            Some(&token),
        )
        .await;
    assert_eq!(got.status, 200);
    assert_eq!(got.json["last4"], "6785");
    assert_eq!(got.json["bank_code"], "058");
    assert!(!got.json.to_string().contains(NUBAN));
    // Only the owner reads the holder's name; support and ordinary users may not.
    let support = app.make_support().await;
    let by_support = app
        .request(
            Method::GET,
            &format!("/admin/stores/{id}/payout-account"),
            None,
            Some(&support),
        )
        .await;
    assert_eq!(by_support.status, 403);
    let admin = app.owner_token().await;
    let admin_view = app
        .request(
            Method::GET,
            &format!("/admin/stores/{id}/payout-account"),
            None,
            Some(&admin),
        )
        .await;
    assert_eq!(admin_view.status, 200);
    assert_eq!(admin_view.json["account_name"], "ADA OBI");
    let (_, other) = app.sign_up_verified("08077777777").await;
    let denied = app
        .request(
            Method::GET,
            &format!("/admin/stores/{id}/payout-account"),
            None,
            Some(&other),
        )
        .await;
    assert_eq!(denied.status, 403);
}

#[sqlx::test]
async fn invalid_nuban_rejected(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let (token, id, _) = seller_store(&app).await;
    for number in [
        "012345678",
        "01234567890",
        "01234abcde",
        "\u{0660}\u{0661}\u{0662}\u{0663}\u{0664}\u{0665}\u{0666}\u{0667}\u{0668}\u{0669}",
        "",
    ] {
        let res = put(&app, &token, id, number).await;
        assert_eq!(res.status, 422, "{number}: {}", res.json);
        assert_eq!(res.json["error"]["code"], "validation_failed");
    }
    for code in ["12", "1234567", "05a"] {
        let res = app
            .request(
                Method::PUT,
                &format!("/stores/{id}/payout-account"),
                Some(json!({ "bank_code": code, "account_number": NUBAN })),
                Some(&token),
            )
            .await;
        assert_eq!(res.status, 422, "{code}");
    }
    let ok = app
        .request(
            Method::PUT,
            &format!("/stores/{id}/payout-account"),
            Some(json!({ "bank_code": "090267", "account_number": NUBAN })),
            Some(&token),
        )
        .await;
    assert_eq!(ok.status, 200);
}

#[sqlx::test]
async fn non_owner_cannot_set(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let (_, id, _) = seller_store(&app).await;
    let (_, other) = app.make_seller("08022222222").await;
    assert_eq!(put(&app, &other, id, NUBAN).await.status, 404);
    let got = app
        .request(
            Method::GET,
            &format!("/stores/{id}/payout-account"),
            None,
            Some(&other),
        )
        .await;
    assert_eq!(got.status, 404);
    assert_eq!(app.fake.calls("resolve_account"), 0);
}

#[sqlx::test]
async fn name_match_auto_verifies(pool: PgPool) {
    let app = TestApp::new(pool).await;
    use_fake_for_payouts(&app).await;
    // Registered display name is "Ada"; token order and extra names do not matter.
    app.fake.set_lookup(Ok("OBI, ADA CHINWE".into()));
    let (token, id, _) = seller_store(&app).await;
    assert_eq!(
        put(&app, &token, id, NUBAN).await.json["status"],
        "verified"
    );
}

#[sqlx::test]
async fn name_mismatch_stays_pending(pool: PgPool) {
    let app = TestApp::new(pool).await;
    use_fake_for_payouts(&app).await;
    app.fake.set_lookup(Ok("CHIDI OKAFOR".into()));
    let (token, id, _) = seller_store(&app).await;
    let res = put(&app, &token, id, NUBAN).await;
    assert_eq!(res.status, 200, "{}", res.json);
    assert_eq!(res.json["status"], "pending");
    assert_eq!(res.json["pending_reason"], "name_mismatch");
    // The seller is not shown the resolved name (no lookup oracle).
    assert!(res.json["account_name"].is_null());
    assert_eq!(store_status(&app, id).await, "pending");
    // The owner sees it for review.
    let admin = app.owner_token().await;
    let view = app
        .request(
            Method::GET,
            &format!("/admin/stores/{id}/payout-account"),
            None,
            Some(&admin),
        )
        .await;
    assert_eq!(view.json["account_name"], "CHIDI OKAFOR");
}

#[sqlx::test]
async fn checksum_failure_is_soft(pool: PgPool) {
    let app = TestApp::new(pool).await;
    use_fake_for_payouts(&app).await;
    app.fake.set_lookup(Ok("ADA OBI".into()));
    let (token, id, _) = seller_store(&app).await;
    let res = put(&app, &token, id, "0123456780").await;
    assert_eq!(res.status, 200, "{}", res.json);
    assert_eq!(res.json["status"], "pending");
    assert_eq!(res.json["pending_reason"], "checksum_failed");
    // A 6-digit bank code skips the check.
    let ok = app
        .request(
            Method::PUT,
            &format!("/stores/{id}/payout-account"),
            Some(json!({
                "bank_code": "090267",
                "account_number": "0123456780",
                "password": common::TEST_PASSWORD
            })),
            Some(&token),
        )
        .await;
    assert_eq!(ok.json["status"], "verified");
}

#[sqlx::test]
async fn change_requires_password(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let (token, id, _) = seller_store(&app).await;
    let path = format!("/stores/{id}/payout-account");
    // First set needs no password.
    let first = app
        .request(
            Method::PUT,
            &path,
            Some(json!({ "bank_code": "058", "account_number": NUBAN })),
            Some(&token),
        )
        .await;
    assert_eq!(first.status, 200, "{}", first.json);
    assert!(!previous_existed(&app).await);
    for body in [
        json!({ "bank_code": "058", "account_number": "9876543216" }),
        json!({ "bank_code": "058", "account_number": "9876543216", "password": "wrong-password-1" }),
    ] {
        let res = app
            .request(Method::PUT, &path, Some(body), Some(&token))
            .await;
        assert_eq!(res.status, 401, "{}", res.json);
        assert_eq!(res.json["error"]["code"], "invalid_credentials");
    }
    let still = app.request(Method::GET, &path, None, Some(&token)).await;
    assert_eq!(still.json["last4"], "6785");
    let ok = put(&app, &token, id, "9876543216").await;
    assert_eq!(ok.status, 200, "{}", ok.json);
    assert_eq!(ok.json["last4"], "3216");
    assert!(previous_existed(&app).await);
}

#[sqlx::test]
async fn sixth_put_in_a_day_is_rate_limited(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let (token, id, _) = seller_store(&app).await;
    for i in 0..5 {
        let res = put(&app, &token, id, NUBAN).await;
        assert_eq!(res.status, 200, "attempt {i}: {}", res.json);
    }
    let res = put(&app, &token, id, NUBAN).await;
    assert_eq!(res.status, 429, "{}", res.json);
    assert_eq!(res.json["error"]["code"], "too_many_attempts");
}

async fn previous_existed(app: &TestApp) -> bool {
    sqlx::query_scalar("SELECT previous_account_existed FROM payout_accounts")
        .fetch_one(&app.pool)
        .await
        .unwrap()
}

/// The `changed_at` the owner sees when reviewing the account.
async fn seen_changed_at(app: &TestApp, id: Uuid, admin: &str) -> serde_json::Value {
    let view = app
        .request(
            Method::GET,
            &format!("/admin/stores/{id}/payout-account"),
            None,
            Some(admin),
        )
        .await;
    assert_eq!(view.status, 200, "{}", view.json);
    view.json["changed_at"].clone()
}

#[sqlx::test]
async fn concurrent_puts_cannot_skip_the_password(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let (token, id, _) = seller_store(&app).await;
    let path = format!("/stores/{id}/payout-account");
    let body = json!({ "bank_code": "058", "account_number": NUBAN });
    let (a, b) = tokio::join!(
        app.request(Method::PUT, &path, Some(body.clone()), Some(&token)),
        app.request(Method::PUT, &path, Some(body.clone()), Some(&token)),
    );
    let mut statuses = [a.status, b.status];
    statuses.sort_unstable();
    assert_eq!(statuses, [200, 401], "{} / {}", a.json, b.json);
}

#[sqlx::test]
async fn rejected_lookups_count_toward_the_limit(pool: PgPool) {
    let app = TestApp::new(pool).await;
    use_fake_for_payouts(&app).await;
    app.fake.set_lookup(Err(ProviderError::Rejected {
        code: "not_found".into(),
        message: "no such account".into(),
    }));
    let (token, id, _) = seller_store(&app).await;
    for i in 0..5 {
        let res = put(&app, &token, id, NUBAN).await;
        assert_eq!(res.status, 422, "attempt {i}: {}", res.json);
    }
    let calls = app.fake.calls("resolve_account");
    let res = put(&app, &token, id, NUBAN).await;
    assert_eq!(res.status, 429, "{}", res.json);
    assert_eq!(app.fake.calls("resolve_account"), calls);
    let detail: String = sqlx::query_scalar(
        "SELECT detail::text FROM audit_log WHERE action = 'payout_account.lookup_rejected' LIMIT 1",
    )
    .fetch_one(&app.pool)
    .await
    .unwrap();
    assert!(
        !detail.contains(NUBAN) && detail.contains(&id.to_string()),
        "{detail}"
    );
}

#[sqlx::test]
async fn failed_reauth_counts_toward_the_limit(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let (token, id, _) = seller_store(&app).await;
    assert_eq!(put(&app, &token, id, NUBAN).await.status, 200);
    let path = format!("/stores/{id}/payout-account");
    let wrong =
        json!({ "bank_code": "058", "account_number": NUBAN, "password": "wrong-password-1" });
    for i in 0..4 {
        let res = app
            .request(Method::PUT, &path, Some(wrong.clone()), Some(&token))
            .await;
        assert_eq!(res.status, 401, "attempt {i}");
    }
    // One accepted PUT plus four failures: the sixth attempt is limited, even with the right password.
    let res = put(&app, &token, id, NUBAN).await;
    assert_eq!(res.status, 429, "{}", res.json);
}

#[sqlx::test]
async fn stale_owner_review_is_refused(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let (token, id, _) = seller_store(&app).await;
    put(&app, &token, id, NUBAN).await;
    let admin = app.owner_token().await;
    let seen = seen_changed_at(&app, id, &admin).await;
    // The seller changes the account after the owner looked at it.
    let changed = put(&app, &token, id, "9876543216").await;
    assert_eq!(changed.status, 200, "{}", changed.json);
    let path = format!("/admin/stores/{id}/payout-account/verify");
    let stale = app
        .request(
            Method::POST,
            &path,
            Some(json!({ "account_name": "ADA OBI", "changed_at": seen })),
            Some(&admin),
        )
        .await;
    assert_eq!(stale.status, 409, "{}", stale.json);
    assert_eq!(stale.json["error"]["code"], "stale_review");
    assert_eq!(store_status(&app, id).await, "pending");
    let fresh = seen_changed_at(&app, id, &admin).await;
    let ok = app
        .request(
            Method::POST,
            &path,
            Some(json!({ "account_name": "ADA OBI", "changed_at": fresh })),
            Some(&admin),
        )
        .await;
    assert_eq!(ok.status, 200, "{}", ok.json);
    assert_eq!(store_status(&app, id).await, "active");
}
