//! `POST /webhooks/{provider}`: unauthenticated provider callbacks.
//!
//! - The body is taken as raw bytes, capped at 64 KiB on this route only
//!   (413 `payload_too_large`, nothing stored).
//! - The provider's secret/signature is checked before the body is parsed:
//!   unknown provider -> 404, mismatch -> 401 `invalid_signature`.
//! - A malformed body after a valid signature, and events we do not act on,
//!   -> 200 `{"status":"ignored"}` (nothing stored).
//! - Payment events are deduplicated on `(provider, event_id)`: a row with
//!   `processed_at` set -> 200 `{"status":"duplicate"}`. Otherwise the payment
//!   is settled ([`settle`](crate::checkout::settle::settle)), which marks the
//!   event processed with the state change. When the payment cannot be verified
//!   right now the answer is 503 `verify_unavailable`, so the provider redelivers.
//!
//! Logging: the provider name and event kind only, never the body or headers
//! (bodies can carry the payer's bank account).

use anyhow::Context;
use axum::{
    Json, Router,
    body::Bytes,
    extract::{Path, State, rejection::BytesRejection},
    http::{HeaderMap, StatusCode},
    middleware::map_response,
    response::{IntoResponse, Response},
    routing::post,
};
use serde_json::json;
use tower_http::limit::RequestBodyLimitLayer;

use super::{ProviderError, WebhookEvent};
use crate::{
    AppState,
    checkout::settle::{SettleOutcome, Source, settle},
    error::AppError,
};

/// Largest webhook body accepted.
pub const MAX_WEBHOOK_BODY: usize = 64 * 1024;
const MAX_EVENT_ID_LEN: usize = 200;

pub fn router() -> Router<AppState> {
    // Router::layer here wraps only this router's route (it is merged afterwards).
    Router::new()
        .route("/webhooks/{provider}", post(receive))
        .layer(RequestBodyLimitLayer::new(MAX_WEBHOOK_BODY))
        // Outermost, so it also sees the limit layer's own 413.
        .layer(map_response(envelope_413))
}

/// The limit layer answers an over-long declared length itself, in plain text;
/// give it the JSON error envelope.
async fn envelope_413(res: Response) -> Response {
    if res.status() == StatusCode::PAYLOAD_TOO_LARGE {
        AppError::PayloadTooLarge.into_response()
    } else {
        res
    }
}

fn status(s: &'static str) -> Response {
    (StatusCode::OK, Json(json!({ "status": s }))).into_response()
}

async fn receive(
    State(state): State<AppState>,
    Path(provider): Path<String>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Result<Response, AppError> {
    let provider = state
        .providers
        .by_name(&provider)
        .ok_or(AppError::NotFound)?;
    let body = body.map_err(|e| {
        if e.status() == StatusCode::PAYLOAD_TOO_LARGE {
            AppError::PayloadTooLarge
        } else {
            AppError::Validation("Request body could not be read".into())
        }
    })?;
    let name = provider.name();
    let event = match provider.verify_webhook(&headers, &body) {
        Ok(event) => event,
        Err(ProviderError::InvalidSignature) => {
            tracing::warn!(provider = name, "webhook with invalid signature");
            return Err(AppError::InvalidSignature);
        }
        Err(_) => {
            tracing::info!(provider = name, kind = "malformed", "webhook ignored");
            return Ok(status("ignored"));
        }
    };
    let (event_id, reference) = match event {
        WebhookEvent::Payment {
            event_id,
            reference,
        } => (event_id, reference),
        // Payout events are settled by escrow::settle_payout (Task 8).
        WebhookEvent::Payout { .. } => {
            tracing::info!(provider = name, kind = "payout", "webhook ignored");
            return Ok(status("ignored"));
        }
        _ => {
            tracing::info!(provider = name, kind = "other", "webhook ignored");
            return Ok(status("ignored"));
        }
    };
    if event_id.is_empty() || event_id.len() > MAX_EVENT_ID_LEN {
        tracing::info!(provider = name, kind = "bad_event_id", "webhook ignored");
        return Ok(status("ignored"));
    }
    tracing::info!(provider = name, kind = "payment", "webhook received");

    sqlx::query(
        "INSERT INTO provider_events (provider, event_id, kind) VALUES ($1, $2, 'payment') \
         ON CONFLICT (provider, event_id) DO NOTHING",
    )
    .bind(name)
    .bind(&event_id)
    .execute(&state.db)
    .await
    .context("record provider event")?;
    let processed: bool = sqlx::query_scalar(
        "SELECT processed_at IS NOT NULL FROM provider_events WHERE provider = $1 AND event_id = $2",
    )
    .bind(name)
    .bind(&event_id)
    .fetch_one(&state.db)
    .await
    .context("load provider event")?;
    if processed {
        return Ok(status("duplicate"));
    }

    let source = Source::Webhook {
        provider: name,
        event_id: &event_id,
    };
    match settle(&state, &reference, source).await? {
        SettleOutcome::RetryLater => Err(AppError::Unavailable("verify_unavailable")),
        _ => Ok(status("received")),
    }
}
