mod common;

use axum::http::Method;
use common::TestApp;
use serde_json::json;
use sqlx::PgPool;

fn is_code_shape(code: &str) -> bool {
    let b = code.as_bytes();
    b.len() == 11
        && b[5] == b'-'
        && b.iter()
            .enumerate()
            .all(|(i, c)| i == 5 || (c.is_ascii_uppercase() || (b'2'..=b'9').contains(c)))
}

#[sqlx::test]
async fn owner_creates_code_and_user_redeems(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let owner = app.make_owner().await;
    let (_, code) = app.create_access_code(&owner, json!({ "label": "launch" })).await;
    assert!(is_code_shape(&code), "bad shape: {code}");

    let (_, token) = app.sign_up_verified("08011111111").await;
    let res = app.redeem(&token, &code).await;
    assert_eq!(res.status, 200, "{}", res.json);
    assert_eq!(res.json["is_seller"], true);

    let me = app.request(Method::GET, "/me", None, Some(&token)).await;
    assert_eq!(me.json["is_seller"], true);
}

#[sqlx::test]
async fn code_redeem_is_case_and_dash_insensitive(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let owner = app.make_owner().await;
    let (_, code) = app.create_access_code(&owner, json!({})).await;
    let (_, token) = app.sign_up_verified("08011111111").await;
    let messy = code.replace('-', "").to_lowercase();
    assert_eq!(app.redeem(&token, &messy).await.status, 200);
}

#[sqlx::test]
async fn non_owner_cannot_create_codes(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let _owner = app.make_owner().await;
    let support = app.make_support().await;
    let (_, user) = app.sign_up_verified("08011111111").await;
    for token in [&user, &support] {
        let res = app
            .request(Method::POST, "/admin/access-codes", Some(json!({})), Some(token))
            .await;
        assert_eq!(res.status, 403);
        assert_eq!(res.json["error"]["code"], "forbidden");
    }
}

#[sqlx::test]
async fn list_never_returns_plain_code(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let owner = app.make_owner().await;
    let (_, code) = app.create_access_code(&owner, json!({ "label": "x" })).await;
    let res = app.request(Method::GET, "/admin/access-codes", None, Some(&owner)).await;
    assert_eq!(res.status, 200);
    let item = &res.json.as_array().unwrap()[0];
    assert!(item.get("code").is_none());
    assert!(item.get("code_hash").is_none());
    let plain = code.replace('-', "");
    assert_eq!(item["last4"], &plain[plain.len() - 4..]);
    assert!(!res.json.to_string().contains(&plain));
}

#[sqlx::test]
async fn expired_code_rejected(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let owner = app.make_owner().await;
    let (id, code) = app
        .create_access_code(&owner, json!({ "expires_at": "2999-01-01T00:00:00Z" }))
        .await;
    sqlx::query("UPDATE access_codes SET expires_at = now() - interval '1 minute' WHERE id = $1")
        .bind(id)
        .execute(&app.pool)
        .await
        .unwrap();
    let (_, token) = app.sign_up_verified("08011111111").await;
    let res = app.redeem(&token, &code).await;
    assert_eq!(res.status, 422);
    assert_eq!(res.json["error"]["code"], "code_expired");

    let past = app
        .request(
            Method::POST,
            "/admin/access-codes",
            Some(json!({ "expires_at": "2000-01-01T00:00:00Z" })),
            Some(&owner),
        )
        .await;
    assert_eq!(past.status, 422);
    assert_eq!(past.json["error"]["code"], "validation_failed");
}

#[sqlx::test]
async fn revoked_code_rejected(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let owner = app.make_owner().await;
    let (id, code) = app.create_access_code(&owner, json!({})).await;
    let revoke = app
        .request(Method::POST, &format!("/admin/access-codes/{id}/revoke"), None, Some(&owner))
        .await;
    assert_eq!(revoke.status, 204);
    let (_, token) = app.sign_up_verified("08011111111").await;
    let res = app.redeem(&token, &code).await;
    assert_eq!(res.status, 422);
    assert_eq!(res.json["error"]["code"], "code_revoked");
}

#[sqlx::test]
async fn single_use_code_exhausted(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let owner = app.make_owner().await;
    let (_, code) = app.create_access_code(&owner, json!({ "max_uses": 1 })).await;
    let (_, a) = app.sign_up_verified("08011111111").await;
    let (_, b) = app.sign_up_verified("08022222222").await;
    assert_eq!(app.redeem(&a, &code).await.status, 200);
    let res = app.redeem(&b, &code).await;
    assert_eq!(res.status, 422);
    assert_eq!(res.json["error"]["code"], "code_exhausted");
}

#[sqlx::test]
async fn unknown_code_rejected(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let (_, token) = app.sign_up_verified("08011111111").await;
    let res = app.redeem(&token, "AAAAA-AAAAA").await;
    assert_eq!(res.status, 422);
    assert_eq!(res.json["error"]["code"], "code_invalid");
}

#[sqlx::test]
async fn already_seller_redeem_is_idempotent(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let owner = app.make_owner().await;
    let (_, first) = app.create_access_code(&owner, json!({})).await;
    let (second_id, second) = app.create_access_code(&owner, json!({})).await;
    let (_, token) = app.sign_up_verified("08011111111").await;
    assert_eq!(app.redeem(&token, &first).await.status, 200);
    let res = app.redeem(&token, &second).await;
    assert_eq!(res.status, 200);
    assert_eq!(res.json["is_seller"], true);
    let uses: i32 = sqlx::query_scalar("SELECT uses FROM access_codes WHERE id = $1")
        .bind(second_id)
        .fetch_one(&app.pool)
        .await
        .unwrap();
    assert_eq!(uses, 0);
}

#[sqlx::test]
async fn concurrent_single_use_redeem_one_wins(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let owner = app.make_owner().await;
    let (id, code) = app.create_access_code(&owner, json!({ "max_uses": 1 })).await;
    let (_, a) = app.sign_up_verified("08011111111").await;
    let (_, b) = app.sign_up_verified("08022222222").await;
    let (ra, rb) = tokio::join!(app.redeem(&a, &code), app.redeem(&b, &code));
    let mut statuses = [ra.status, rb.status];
    statuses.sort();
    assert_eq!(statuses, [200, 422]);
    let loser = if ra.status == 422 { &ra } else { &rb };
    assert_eq!(loser.json["error"]["code"], "code_exhausted");
    let uses: i32 = sqlx::query_scalar("SELECT uses FROM access_codes WHERE id = $1")
        .bind(id)
        .fetch_one(&app.pool)
        .await
        .unwrap();
    assert_eq!(uses, 1);
}

#[sqlx::test]
async fn redeem_and_create_are_audited(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let owner = app.make_owner().await;
    let (id, code) = app.create_access_code(&owner, json!({})).await;
    let (_, token) = app.sign_up_verified("08011111111").await;
    assert_eq!(app.redeem(&token, &code).await.status, 200);
    app.request(Method::POST, &format!("/admin/access-codes/{id}/revoke"), None, Some(&owner))
        .await;
    for action in ["access_code.created", "access_code.redeemed", "access_code.revoked"] {
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_log WHERE action = $1")
            .bind(action)
            .fetch_one(&app.pool)
            .await
            .unwrap();
        assert_eq!(n, 1, "{action}");
    }
    let details: Vec<String> =
        sqlx::query_scalar("SELECT detail::text FROM audit_log WHERE action LIKE 'access_code.%'")
            .fetch_all(&app.pool)
            .await
            .unwrap();
    let plain = code.replace('-', "");
    assert!(details.iter().all(|d| !d.contains(&plain)));
}

#[sqlx::test]
async fn redeem_attempts_are_rate_limited(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let owner = app.make_owner().await;
    let (_, code) = app.create_access_code(&owner, json!({})).await;
    let (_, token) = app.sign_up_verified("08011111111").await;
    for _ in 0..10 {
        assert_eq!(app.redeem(&token, "AAAAA-AAAAA").await.status, 422);
    }
    let res = app.redeem(&token, &code).await;
    assert_eq!(res.status, 429);
    assert_eq!(res.json["error"]["code"], "too_many_attempts");
}

#[sqlx::test]
async fn revoke_non_uuid_id_is_json_404(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let owner = app.make_owner().await;
    let res = app
        .request(Method::POST, "/admin/access-codes/not-a-uuid/revoke", None, Some(&owner))
        .await;
    assert_eq!(res.status, 404);
    assert_eq!(res.json["error"]["code"], "not_found");
    assert!(!res.json.to_string().contains("not-a-uuid"), "path value must not be echoed");
}

#[sqlx::test]
async fn revoke_unknown_uuid_is_404(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let owner = app.make_owner().await;
    let res = app
        .request(
            Method::POST,
            &format!("/admin/access-codes/{}/revoke", uuid::Uuid::new_v4()),
            None,
            Some(&owner),
        )
        .await;
    assert_eq!(res.status, 404);
    assert_eq!(res.json["error"]["code"], "not_found");
}

#[sqlx::test]
async fn non_owner_cannot_list_or_revoke_codes(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let owner = app.make_owner().await;
    let (id, _) = app.create_access_code(&owner, json!({})).await;
    let support = app.make_support().await;
    let (_, user) = app.sign_up_verified("08011111111").await;
    for token in [&user, &support] {
        let list = app.request(Method::GET, "/admin/access-codes", None, Some(token)).await;
        assert_eq!(list.status, 403);
        assert_eq!(list.json["error"]["code"], "forbidden");
        let revoke = app
            .request(Method::POST, &format!("/admin/access-codes/{id}/revoke"), None, Some(token))
            .await;
        assert_eq!(revoke.status, 403);
        assert_eq!(revoke.json["error"]["code"], "forbidden");
    }
    let revoked: bool = sqlx::query_scalar("SELECT revoked_at IS NOT NULL FROM access_codes WHERE id = $1")
        .bind(id)
        .fetch_one(&app.pool)
        .await
        .unwrap();
    assert!(!revoked);
}
