mod common;

use axum::http::Method;
use sqlx::PgPool;

#[sqlx::test]
async fn health_returns_ok(pool: PgPool) {
    let app = common::TestApp::new(pool).await;
    let res = app.request(Method::GET, "/health", None, None).await;
    assert_eq!(res.status, 200);
    assert_eq!(res.json["status"], "ok");
}
