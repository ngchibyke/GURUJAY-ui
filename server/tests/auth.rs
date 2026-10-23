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
    let vid = signup.json["verification_id"].as_str().unwrap();
    assert_eq!(vid.len(), 43, "32 bytes, base64url without padding");

    let verify = app.verify(&signup.json["verification_id"], &app.last_code(PHONE)).await;
    assert_eq!(verify.status, 200, "body: {}", verify.json);
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
async fn verification_id_is_stored_only_as_hash(pool: PgPool) {
    let app = TestApp::new(pool.clone()).await;
    let signup = app.sign_up(PHONE, TEST_PASSWORD).await;
    let vid = signup.json["verification_id"].as_str().unwrap();
    let stored: Vec<u8> = sqlx::query_scalar("SELECT verification_hash FROM otp_codes")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(stored.len(), 32);
    assert!(!stored.windows(vid.len()).any(|w| w == vid.as_bytes()));
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
    let wrong = app.sign_in(PHONE, "not-the-password").await;
    assert_err(&wrong, 401, "invalid_credentials");
    let unknown = app.sign_in("08077777777", "not-the-password").await;
    assert_err(&unknown, 401, "invalid_credentials");
    assert_eq!(wrong.json, unknown.json);

    let ok = app.sign_in(PHONE, TEST_PASSWORD).await;
    assert_eq!(ok.status, 200);
    assert!(!ok.json["token"].as_str().unwrap().is_empty());
}

#[sqlx::test]
async fn sign_in_unverified_is_403(pool: PgPool) {
    let app = TestApp::new(pool.clone()).await;
    let signup = app.sign_up(PHONE, TEST_PASSWORD).await;
    assert_eq!(signup.status, 201);
    let res = app.sign_in(PHONE, TEST_PASSWORD).await;
    assert_err(&res, 403, "phone_unverified");
    assert!(res.json["token"].is_null());
    let vid = &res.json["verification_id"];
    assert_eq!(vid.as_str().unwrap().len(), 43);
    // Throttled (within 60s): no new SMS; the live code is re-bound to a fresh id.
    assert_eq!(app.otp.sent_count(), 1);
    let live: i64 = sqlx::query_scalar("SELECT count(*) FROM otp_codes WHERE consumed_at IS NULL")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(live, 1);

    // The sign-up id no longer works; the id from sign-in does, with the code already sent.
    let stale = app.verify(&signup.json["verification_id"], &app.last_code(PHONE)).await;
    assert_err(&stale, 422, "otp_invalid");
    let ok = app.verify(vid, &app.last_code(PHONE)).await;
    assert_eq!(ok.status, 200, "body: {}", ok.json);
}

#[sqlx::test]
async fn sign_in_unverified_wrong_password_is_401(pool: PgPool) {
    let app = TestApp::new(pool).await;
    assert_eq!(app.sign_up(PHONE, TEST_PASSWORD).await.status, 201);
    let res = app.sign_in(PHONE, "not-the-password").await;
    assert_err(&res, 401, "invalid_credentials");
    assert!(res.json["verification_id"].is_null());
}

#[sqlx::test]
async fn otp_wrong_five_times_locks(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let signup = app.sign_up(PHONE, TEST_PASSWORD).await;
    let vid = &signup.json["verification_id"];
    let real = app.last_code(PHONE);
    let bad = wrong_code(&real);
    for _ in 0..5 {
        assert_err(&app.verify(vid, bad).await, 422, "otp_invalid");
    }
    assert_err(&app.verify(vid, &real).await, 422, "otp_locked");
}

#[sqlx::test]
async fn otp_expired_rejected(pool: PgPool) {
    let app = TestApp::new(pool.clone()).await;
    let signup = app.sign_up(PHONE, TEST_PASSWORD).await;
    sqlx::query("UPDATE otp_codes SET expires_at = now() - interval '1 second'")
        .execute(&pool)
        .await
        .unwrap();
    let res = app.verify(&signup.json["verification_id"], &app.last_code(PHONE)).await;
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
    let vid = res.json["verification_id"].as_str().unwrap().to_string();
    let code = app.last_code("08031234567");
    app.verify(&res.json["verification_id"], &code).await;
    app.sign_in("08031234567", password).await;

    assert!(logs_contain("response"), "expected request logs to be captured");
    assert!(!logs_contain(password));
    assert!(!logs_contain("8031234567"));
    assert!(!logs_contain(&code));
    assert!(!logs_contain(&vid));
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

    let verify = app.verify(&second.json["verification_id"], &app.last_code(PHONE)).await;
    assert_eq!(verify.status, 200);
    assert_eq!(app.sign_in(PHONE, "second-password-2").await.status, 200);
    assert_err(&app.sign_in(PHONE, "first-password-1").await, 401, "invalid_credentials");
}

#[sqlx::test]
async fn reclaim_invalidates_previous_verification(pool: PgPool) {
    let app = TestApp::new(pool).await;
    // A signs up and receives code1 for vid1.
    let a = app.sign_up(PHONE, "victim-password-1").await;
    assert_eq!(a.status, 201);
    let code1 = app.last_code(PHONE);
    // B reclaims the unverified phone with another password (outside the send throttle).
    app.age_otps(120).await;
    let b = app.sign_up(PHONE, "attacker-password-2").await;
    assert_eq!(b.status, 201);
    assert_eq!(app.otp.sent_count(), 2);
    let code2 = app.last_code(PHONE);

    assert_err(&app.verify(&a.json["verification_id"], &code1).await, 422, "otp_invalid");
    // The old id cannot be paired with the newest code either.
    assert_err(&app.verify(&a.json["verification_id"], &code2).await, 422, "otp_invalid");

    let ok = app.verify(&b.json["verification_id"], &code2).await;
    assert_eq!(ok.status, 200, "body: {}", ok.json);
    assert_eq!(app.sign_in(PHONE, "attacker-password-2").await.status, 200);
    assert_err(&app.sign_in(PHONE, "victim-password-1").await, 401, "invalid_credentials");
}

#[sqlx::test]
async fn reclaim_during_throttle_invalidates_old_code(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let a = app.sign_up(PHONE, "victim-password-1").await;
    let code1 = app.last_code(PHONE);
    // Within 60s: the reclaim is throttled, so nothing new is sent...
    let b = app.sign_up(PHONE, "attacker-password-2").await;
    assert_eq!(b.status, 201);
    assert_eq!(app.otp.sent_count(), 1);
    // ...but the victim's code and id are dead, and code1 does not unlock B's id either.
    assert_err(&app.verify(&a.json["verification_id"], &code1).await, 422, "otp_invalid");
    assert_err(&app.verify(&b.json["verification_id"], &code1).await, 422, "otp_invalid");

    // Once the throttle allows, signing in with B's password sends a fresh code.
    app.age_otps(120).await;
    let resend = app.sign_in(PHONE, "attacker-password-2").await;
    assert_err(&resend, 403, "phone_unverified");
    assert_eq!(app.otp.sent_count(), 2);
    let ok = app.verify(&resend.json["verification_id"], &app.last_code(PHONE)).await;
    assert_eq!(ok.status, 200, "body: {}", ok.json);
}

#[sqlx::test]
async fn cross_user_code_is_invalid(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let x = app.sign_up("08011111111", TEST_PASSWORD).await;
    let y = app.sign_up("08022222222", TEST_PASSWORD).await;
    let x_code = app.last_code("08011111111");
    let y_code = app.last_code("08022222222");
    // Codes are random; a 1-in-a-million collision would make the pairing legitimately valid.
    assert_ne!(x_code, y_code, "random codes collided; re-run");
    assert_err(&app.verify(&y.json["verification_id"], &x_code).await, 422, "otp_invalid");
    assert_err(&app.verify(&x.json["verification_id"], &y_code).await, 422, "otp_invalid");
    // Each id still verifies with its own code.
    assert_eq!(app.verify(&x.json["verification_id"], &x_code).await.status, 200);
    assert_eq!(app.verify(&y.json["verification_id"], &y_code).await.status, 200);
}

#[sqlx::test]
async fn otp_issue_is_throttled(pool: PgPool) {
    let app = TestApp::new(pool).await;
    assert_eq!(app.sign_up(PHONE, TEST_PASSWORD).await.status, 201);
    assert_eq!(app.otp.sent_count(), 1);
    for _ in 0..3 {
        assert_err(&app.sign_in(PHONE, TEST_PASSWORD).await, 403, "phone_unverified");
    }
    assert_eq!(app.otp.sent_count(), 1, "sign-ins within 60s must not send");

    // Past the 60s window a new code goes out (2nd of the hour).
    app.age_otps(120).await;
    app.sign_in(PHONE, TEST_PASSWORD).await;
    assert_eq!(app.otp.sent_count(), 2);

    // Reach 5 in the hour, then the 6th is skipped even after the 60s window.
    for expected in 3..=5 {
        app.age_otps(120).await;
        app.sign_in(PHONE, TEST_PASSWORD).await;
        assert_eq!(app.otp.sent_count(), expected);
    }
    app.age_otps(120).await;
    let res = app.sign_in(PHONE, TEST_PASSWORD).await;
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
    let no_vid = app
        .request(Method::POST, "/auth/verify-phone", Some(json!({ "code": "123456" })), None)
        .await;
    assert_err(&no_vid, 422, "validation_failed");
    // The old body shape is refused: user_id no longer identifies the attempt.
    let old_shape = app
        .request(
            Method::POST,
            "/auth/verify-phone",
            Some(json!({ "user_id": "00000000-0000-0000-0000-000000000000", "code": "123456" })),
            None,
        )
        .await;
    assert_err(&old_shape, 422, "validation_failed");
}

#[sqlx::test]
async fn sign_in_locked_after_10_failures(pool: PgPool) {
    let app = TestApp::new(pool.clone()).await;
    app.sign_up_verified(PHONE).await;
    app.sign_up_verified("08022222222").await;
    for _ in 0..10 {
        assert_err(&app.sign_in(PHONE, "not-the-password").await, 401, "invalid_credentials");
    }
    // The 11th attempt is refused before the password is checked, even when it is right.
    assert_err(&app.sign_in(PHONE, TEST_PASSWORD).await, 429, "too_many_attempts");
    // Any accepted format of the same phone shares the counter; other phones do not.
    assert_err(&app.sign_in("+234 803 123 4567", TEST_PASSWORD).await, 429, "too_many_attempts");
    assert_eq!(app.sign_in("08022222222", TEST_PASSWORD).await.status, 200);

    // The window is a rolling hour.
    sqlx::query("UPDATE sign_in_failures SET at = at - interval '61 minutes'")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(app.sign_in(PHONE, TEST_PASSWORD).await.status, 200);
}

#[sqlx::test]
async fn sign_in_unknown_phone_is_capped_too(pool: PgPool) {
    let app = TestApp::new(pool).await;
    for _ in 0..10 {
        assert_err(&app.sign_in("08077777777", "not-the-password").await, 401, "invalid_credentials");
    }
    assert_err(&app.sign_in("08077777777", "not-the-password").await, 429, "too_many_attempts");
}
