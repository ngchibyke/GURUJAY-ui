mod common;

use axum::http::Method;
use common::{TEST_PASSWORD, TestApp};
use serde_json::json;
use sqlx::PgPool;
use tracing_test::traced_test;

const PHONE: &str = "08031234567";

fn assert_err(res: &common::TestResponse, status: u16, code: &str) {
    assert_eq!(res.status, status, "body: {}", res.json);
    assert_eq!(res.json["error"]["code"], code, "body: {}", res.json);
}

fn wrong_code(real: &str) -> &'static str {
    if real == "000000" { "111111" } else { "000000" }
}

#[sqlx::test]
async fn sign_up_then_verify_returns_token(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let signup = app.sign_up(PHONE, TEST_PASSWORD).await;
    assert_eq!(signup.status, 201);
    let user_id = signup.json["user_id"].as_str().unwrap().to_string();

    let verify = app
        .request(
            Method::POST,
            "/auth/verify-phone",
            Some(json!({ "user_id": user_id, "code": app.last_code(PHONE) })),
            None,
        )
        .await;
    assert_eq!(verify.status, 200);
    let token = verify.json["token"].as_str().unwrap();
    assert!(!token.is_empty());

    let me = app.request(Method::GET, "/me", None, Some(token)).await;
    assert_eq!(me.status, 200);
    assert_eq!(me.json["phone"], "+2348031234567");
    assert_eq!(me.json["id"], user_id);
    assert_eq!(me.json["display_name"], "Ada");
    assert_eq!(me.json["role"], "user");
    assert_eq!(me.json["is_seller"], false);
}

#[sqlx::test]
async fn sign_up_stores_phone_encrypted(pool: PgPool) {
    let app = TestApp::new(pool.clone()).await;
    assert_eq!(app.sign_up(PHONE, TEST_PASSWORD).await.status, 201);
    let enc: Vec<u8> = sqlx::query_scalar("SELECT phone_enc FROM users").fetch_one(&pool).await.unwrap();
    assert!(!enc.is_empty());
    let needle = b"8031234567";
    assert!(!enc.windows(needle.len()).any(|w| w == needle));
}

#[sqlx::test]
async fn duplicate_phone_any_format_conflicts(pool: PgPool) {
    let app = TestApp::new(pool).await;
    app.sign_up_verified("08031234567").await;
    let dup = app.sign_up("+234 803 123 4567", TEST_PASSWORD).await;
    assert_err(&dup, 409, "phone_taken");
}

#[sqlx::test]
async fn short_password_rejected(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let res = app.sign_up(PHONE, "short1").await;
    assert_err(&res, 422, "validation_failed");
}

#[sqlx::test]
async fn sign_in_wrong_password_is_401(pool: PgPool) {
    let app = TestApp::new(pool).await;
    app.sign_up_verified(PHONE).await;
    let wrong = app
        .request(Method::POST, "/auth/sign-in", Some(json!({ "phone": PHONE, "password": "not-the-password" })), None)
        .await;
    assert_err(&wrong, 401, "invalid_credentials");
    let unknown = app
        .request(
            Method::POST,
            "/auth/sign-in",
            Some(json!({ "phone": "08077777777", "password": "not-the-password" })),
            None,
        )
        .await;
    assert_err(&unknown, 401, "invalid_credentials");
    assert_eq!(wrong.json, unknown.json);

    let ok = app
        .request(Method::POST, "/auth/sign-in", Some(json!({ "phone": PHONE, "password": TEST_PASSWORD })), None)
        .await;
    assert_eq!(ok.status, 200);
    assert!(!ok.json["token"].as_str().unwrap().is_empty());
}

#[sqlx::test]
async fn sign_in_unverified_is_403(pool: PgPool) {
    let app = TestApp::new(pool.clone()).await;
    assert_eq!(app.sign_up(PHONE, TEST_PASSWORD).await.status, 201);
    let res = app
        .request(Method::POST, "/auth/sign-in", Some(json!({ "phone": PHONE, "password": TEST_PASSWORD })), None)
        .await;
    assert_err(&res, 403, "phone_unverified");
    assert!(res.json["token"].is_null());
    // Sign-up OTP was superseded by a fresh one: exactly one live code remains.
    let live: i64 = sqlx::query_scalar("SELECT count(*) FROM otp_codes WHERE consumed_at IS NULL")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(live, 1);
}

#[sqlx::test]
async fn otp_wrong_five_times_locks(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let signup = app.sign_up(PHONE, TEST_PASSWORD).await;
    let user_id = signup.json["user_id"].as_str().unwrap().to_string();
    let real = app.last_code(PHONE);
    let bad = wrong_code(&real);
    for _ in 0..5 {
        let res = app
            .request(Method::POST, "/auth/verify-phone", Some(json!({ "user_id": user_id, "code": bad })), None)
            .await;
        assert_err(&res, 422, "otp_invalid");
    }
    let locked = app
        .request(Method::POST, "/auth/verify-phone", Some(json!({ "user_id": user_id, "code": real })), None)
        .await;
    assert_err(&locked, 422, "otp_locked");
}

#[sqlx::test]
async fn otp_expired_rejected(pool: PgPool) {
    let app = TestApp::new(pool.clone()).await;
    let signup = app.sign_up(PHONE, TEST_PASSWORD).await;
    let user_id = signup.json["user_id"].as_str().unwrap().to_string();
    sqlx::query("UPDATE otp_codes SET expires_at = now() - interval '1 second'")
        .execute(&pool)
        .await
        .unwrap();
    let res = app
        .request(
            Method::POST,
            "/auth/verify-phone",
            Some(json!({ "user_id": user_id, "code": app.last_code(PHONE) })),
            None,
        )
        .await;
    assert_err(&res, 422, "otp_expired");
}

#[sqlx::test]
async fn sign_out_revokes_token(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let (_, token) = app.sign_up_verified(PHONE).await;
    assert_eq!(app.request(Method::GET, "/me", None, Some(&token)).await.status, 200);
    let out = app.request(Method::POST, "/auth/sign-out", None, Some(&token)).await;
    assert_eq!(out.status, 204);
    let me = app.request(Method::GET, "/me", None, Some(&token)).await;
    assert_err(&me, 401, "unauthorized");
}

#[sqlx::test]
async fn expired_session_is_401(pool: PgPool) {
    let app = TestApp::new(pool.clone()).await;
    let (_, token) = app.sign_up_verified(PHONE).await;
    sqlx::query("UPDATE sessions SET expires_at = now() - interval '1 second'")
        .execute(&pool)
        .await
        .unwrap();
    let me = app.request(Method::GET, "/me", None, Some(&token)).await;
    assert_err(&me, 401, "unauthorized");
}

#[traced_test]
#[sqlx::test]
async fn password_never_in_logs(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let password = "very-secret-passw0rd-xyz";
    let res = app.sign_up("08031234567", password).await;
    assert_eq!(res.status, 201);
    let user_id = res.json["user_id"].as_str().unwrap().to_string();
    let code = app.last_code("08031234567");
    app.request(Method::POST, "/auth/verify-phone", Some(json!({ "user_id": user_id, "code": code })), None)
        .await;
    app.request(Method::POST, "/auth/sign-in", Some(json!({ "phone": "08031234567", "password": password })), None)
        .await;

    assert!(logs_contain("response"), "expected request logs to be captured");
    assert!(!logs_contain(password));
    assert!(!logs_contain("8031234567"));
    assert!(!logs_contain(&code));
}

#[sqlx::test]
async fn unverified_signup_can_be_reclaimed(pool: PgPool) {
    let app = TestApp::new(pool.clone()).await;
    let first = app.sign_up("08031234567", "first-password-1").await;
    assert_eq!(first.status, 201);
    app.age_otps(120).await;
    let second = app.sign_up("+234 803 123 4567", "second-password-2").await;
    assert_eq!(second.status, 201);
    assert_eq!(first.json["user_id"], second.json["user_id"]);
    let users: i64 = sqlx::query_scalar("SELECT count(*) FROM users").fetch_one(&pool).await.unwrap();
    assert_eq!(users, 1);

    let verify = app
        .request(
            Method::POST,
            "/auth/verify-phone",
            Some(json!({ "user_id": second.json["user_id"], "code": app.last_code(PHONE) })),
            None,
        )
        .await;
    assert_eq!(verify.status, 200);
    let ok = app
        .request(Method::POST, "/auth/sign-in", Some(json!({ "phone": PHONE, "password": "second-password-2" })), None)
        .await;
    assert_eq!(ok.status, 200);
    let old = app
        .request(Method::POST, "/auth/sign-in", Some(json!({ "phone": PHONE, "password": "first-password-1" })), None)
        .await;
    assert_err(&old, 401, "invalid_credentials");
}

#[sqlx::test]
async fn otp_issue_is_throttled(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let body = json!({ "phone": PHONE, "password": TEST_PASSWORD });
    assert_eq!(app.sign_up(PHONE, TEST_PASSWORD).await.status, 201);
    assert_eq!(app.otp.sent_count(), 1);
    for _ in 0..3 {
        let res = app.request(Method::POST, "/auth/sign-in", Some(body.clone()), None).await;
        assert_err(&res, 403, "phone_unverified");
    }
    assert_eq!(app.otp.sent_count(), 1, "sign-ins within 60s must not send");

    // Past the 60s window a new code goes out (2nd of the hour).
    app.age_otps(120).await;
    app.request(Method::POST, "/auth/sign-in", Some(body.clone()), None).await;
    assert_eq!(app.otp.sent_count(), 2);

    // Reach 5 in the hour, then the 6th is skipped even after the 60s window.
    for expected in 3..=5 {
        app.age_otps(120).await;
        app.request(Method::POST, "/auth/sign-in", Some(body.clone()), None).await;
        assert_eq!(app.otp.sent_count(), expected);
    }
    app.age_otps(120).await;
    let res = app.request(Method::POST, "/auth/sign-in", Some(body), None).await;
    assert_err(&res, 403, "phone_unverified");
    assert_eq!(app.otp.sent_count(), 5, "6th OTP within an hour must be skipped");
}

#[sqlx::test]
async fn malformed_body_is_json_422(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let missing = app
        .request(Method::POST, "/auth/sign-up", Some(json!({ "phone": PHONE, "display_name": "Ada" })), None)
        .await;
    assert_err(&missing, 422, "validation_failed");
    let garbage = app.request_raw(Method::POST, "/auth/sign-up", "not json{").await;
    assert_err(&garbage, 422, "validation_failed");
    let bad_uuid = app
        .request(Method::POST, "/auth/verify-phone", Some(json!({ "user_id": "nope", "code": "123456" })), None)
        .await;
    assert_err(&bad_uuid, 422, "validation_failed");
}
