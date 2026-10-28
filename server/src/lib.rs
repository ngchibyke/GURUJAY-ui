pub mod access_codes;
pub mod admin;
pub mod audit;
pub mod auth;
pub mod cart;
pub mod checkout;
pub mod config;
pub mod crypto;
pub mod error;
pub mod escrow;
pub mod extract;
pub mod ledger;
pub mod limits;
pub mod money;
pub mod orders;
pub mod payments;
pub mod payout_accounts;
pub mod phone;
pub mod products;
pub mod settings;
pub mod stores;

use axum::{Json, Router, extract::MatchedPath, http::Request, routing::get};
use std::sync::Arc;

use serde_json::{Value, json};
use sqlx::PgPool;
use tower_http::trace::TraceLayer;

use crate::{
    auth::otp::OtpSender, crypto::FieldCipher, error::AppError, payments::ProviderRegistry,
};

#[derive(Clone)]
pub struct AppState {
    pub db: PgPool,
    pub cipher: Arc<FieldCipher>,
    pub index_key: Arc<[u8; 32]>,
    pub otp: Arc<dyn OtpSender>,
    /// Configured payment providers. Production never registers `FakeProvider`.
    pub providers: Arc<ProviderRegistry>,
    /// Fixed return URL sent to the payment provider (config `PUBLIC_REDIRECT_URL`).
    pub payment_redirect_url: String,
    /// Cooldown and concurrency cap on outbound payment verify calls.
    pub verify_gate: Arc<payments::VerifyGate>,
}

async fn health() -> Json<Value> {
    Json(json!({ "status": "ok" }))
}

/// Unknown paths get the JSON envelope; the path is never echoed back.
async fn not_found() -> AppError {
    AppError::NotFound
}

async fn method_not_allowed() -> AppError {
    AppError::MethodNotAllowed
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
            |res: &axum::response::Response,
             latency: std::time::Duration,
             _span: &tracing::Span| {
                tracing::info!(
                    status = res.status().as_u16(),
                    latency_ms = latency.as_millis() as u64,
                    "response"
                );
            },
        );

    Router::new()
        .route("/health", get(health))
        .merge(auth::routes::router())
        .merge(access_codes::routes::router())
        .merge(stores::routes::router())
        .merge(products::routes::router())
        .merge(settings::routes::router())
        .merge(payout_accounts::routes::router())
        .merge(cart::routes::router())
        .merge(checkout::routes::router())
        .merge(orders::routes::router())
        .merge(payments::webhooks::router())
        .merge(admin::routes::router())
        // Must follow every route: it applies to the method routers registered so far.
        .method_not_allowed_fallback(method_not_allowed)
        .fallback(not_found)
        .layer(trace)
        .with_state(state)
}

/// Options for the server's database pool. Every connection gets
/// `idle_in_transaction_session_timeout = 60s`: a transaction left open (a
/// stuck job claim, a hung request) is ended by Postgres instead of holding
/// its row locks and blocking VACUUM on the hot tables.
pub fn pool_options(max_connections: u32) -> sqlx::postgres::PgPoolOptions {
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(max_connections)
        .after_connect(|conn, _meta| {
            Box::pin(async move {
                sqlx::Executor::execute(conn, "SET idle_in_transaction_session_timeout = '60s'")
                    .await?;
                Ok(())
            })
        })
}
