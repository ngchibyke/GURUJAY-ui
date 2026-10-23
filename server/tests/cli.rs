mod common;

use common::{TEST_PASSWORD, TestApp};
use server::{
    auth::{password::hash_password, routes::create_owner_account},
    error::AppError,
};
use sqlx::PgPool;

const PHONE: &str = "08031234567";
const E164: &str = "+2348031234567";

#[sqlx::test]
async fn create_owner_makes_verified_owner_and_audits(pool: PgPool) {
    let app = TestApp::new(pool.clone()).await;
    let hash = hash_password(TEST_PASSWORD).unwrap();
    let id = create_owner_account(&app.state, E164, "Owner", &hash)
        .await
        .expect("create owner");

    let (role, verified): (String, bool) =
        sqlx::query_as("SELECT role::text, phone_verified_at IS NOT NULL FROM users WHERE id = $1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(role, "owner");
    assert!(verified);

    let (actor, subject, detail): (Option<uuid::Uuid>, Option<uuid::Uuid>, String) = sqlx::query_as(
        "SELECT actor_id, subject_id, detail::text FROM audit_log WHERE action = 'user.owner_created'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(actor, None);
    assert_eq!(subject, Some(id));
    assert!(
        !detail.contains("8031234567"),
        "no phone in audit detail: {detail}"
    );
    assert!(
        !detail.contains("Owner"),
        "no name in audit detail: {detail}"
    );

    assert_eq!(app.sign_in(PHONE, TEST_PASSWORD).await.status, 200);
}

#[sqlx::test]
async fn create_owner_over_unverified_signup_retires_its_codes(pool: PgPool) {
    let app = TestApp::new(pool.clone()).await;
    let signup = app.sign_up(PHONE, "someone-elses-pass").await;
    assert_eq!(signup.status, 201);
    let code = app.last_code(PHONE);

    let hash = hash_password(TEST_PASSWORD).unwrap();
    create_owner_account(&app.state, E164, "Owner", &hash)
        .await
        .expect("create owner");
    let res = app.verify(&signup.json["verification_id"], &code).await;
    assert_eq!(res.status, 422);
    assert_eq!(res.json["error"]["code"], "otp_invalid");
    assert_eq!(app.sign_in(PHONE, TEST_PASSWORD).await.status, 200);
}

#[sqlx::test]
async fn create_owner_on_verified_phone_changes_nothing(pool: PgPool) {
    let app = TestApp::new(pool.clone()).await;
    let (id, _) = app.sign_up_verified(PHONE).await;
    let hash = hash_password("another-password-9").unwrap();
    let err = create_owner_account(&app.state, E164, "Owner", &hash)
        .await
        .unwrap_err();
    assert!(matches!(err, AppError::Conflict("phone_taken")), "{err:?}");

    let role: String = sqlx::query_scalar("SELECT role::text FROM users WHERE id = $1")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(role, "user");
    let audits: i64 =
        sqlx::query_scalar("SELECT count(*) FROM audit_log WHERE action = 'user.owner_created'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(audits, 0);
}
