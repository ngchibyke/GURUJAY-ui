mod common;

use axum::http::Method;
use sqlx::PgPool;

#[sqlx::test]
async fn unknown_route_is_json_404(pool: PgPool) {
    let app = common::TestApp::new(pool).await;
    let res = app.request(Method::GET, "/no-such-route/08031234567", None, None).await;
    assert_eq!(res.status, 404);
    assert_eq!(res.json["error"]["code"], "not_found");
    assert!(!res.json.to_string().contains("08031234567"), "path must not be echoed");
}

#[sqlx::test]
async fn wrong_method_is_json_405(pool: PgPool) {
    let app = common::TestApp::new(pool).await;
    for (method, path) in [(Method::DELETE, "/health"), (Method::GET, "/auth/sign-in"), (Method::PUT, "/stores/mine")] {
        let res = app.request(method.clone(), path, None, None).await;
        assert_eq!(res.status, 405, "{method} {path}");
        assert_eq!(res.json["error"]["code"], "method_not_allowed", "{method} {path}");
    }
}
