pub mod access_codes;
pub mod audit;
pub mod auth;
pub mod config;
pub mod crypto;
pub mod error;
pub mod extract;
pub mod phone;

use axum::{
    Json, Router,
    extract::MatchedPath,
    http::Request,
    routing::get,
};
use std::sync::Arc;

use serde_json::{Value, json};
use sqlx::PgPool;
use tower_http::trace::TraceLayer;

use crate::{auth::otp::OtpSender, crypto::FieldCipher};

#[derive(Clone)]
pub struct AppState {
    pub db: PgPool,
    pub cipher: Arc<FieldCipher>,
    pub index_key: Arc<[u8; 32]>,
    pub otp: Arc<dyn OtpSender>,
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
        .merge(auth::routes::router())
        .merge(access_codes::routes::router())
        .layer(trace)
        .with_state(state)
}
