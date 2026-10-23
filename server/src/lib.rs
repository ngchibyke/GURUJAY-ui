pub mod config;
pub mod error;

use axum::{
    Json, Router,
    extract::MatchedPath,
    http::Request,
    routing::get,
};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower_http::trace::TraceLayer;

#[derive(Clone)]
pub struct AppState {
    pub db: PgPool,
}

async fn health() -> Json<Value> {
    Json(json!({ "status": "ok" }))
}

/// Builds the application router. Request tracing records only the method,
/// the matched route template and the response status: never the URI query,
/// headers or bodies (no PII in logs).
pub fn build_app(state: AppState) -> Router {
    let trace = TraceLayer::new_for_http()
        .make_span_with(|req: &Request<_>| {
            let route = req
                .extensions()
                .get::<MatchedPath>()
                .map(MatchedPath::as_str)
                .unwrap_or("unmatched");
            tracing::info_span!("request", method = %req.method(), route = %route)
        })
        .on_request(())
        .on_response(
            |res: &axum::response::Response, latency: std::time::Duration, _span: &tracing::Span| {
                tracing::info!(status = res.status().as_u16(), latency_ms = latency.as_millis() as u64, "response");
            },
        );

    Router::new()
        .route("/health", get(health))
        .layer(trace)
        .with_state(state)
}
