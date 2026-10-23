use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::json;

/// A value that must never appear in logs or `Debug` output (e.g. a verification id).
pub struct Redacted(pub String);

impl std::fmt::Debug for Redacted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[redacted]")
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("validation failed: {0}")]
    Validation(String),
    #[error("unauthorized")]
    Unauthorized,
    #[error("invalid credentials")]
    InvalidCredentials,
    #[error("forbidden: {0}")]
    Forbidden(&'static str),
    /// 403 `phone_unverified`, carrying the verification id to use with the code sent by SMS.
    #[error("phone unverified")]
    PhoneUnverified(Redacted),
    #[error("not found")]
    NotFound,
    #[error("method not allowed")]
    MethodNotAllowed,
    #[error("conflict: {0}")]
    Conflict(&'static str),
    #[error("unprocessable: {0}")]
    Unprocessable(&'static str, String),
    #[error("too many requests: {0}")]
    TooManyRequests(&'static str),
    /// 502 `payment_provider_unavailable`: the payment provider failed or is not configured.
    #[error("payment provider unavailable")]
    ProviderUnavailable,
    /// 501 `not_supported`: the provider does not offer this operation.
    #[error("not supported")]
    NotSupported,
    #[error("internal error")]
    Internal(#[from] anyhow::Error),
}

fn body(code: &str, message: &str) -> serde_json::Value {
    json!({ "error": { "code": code, "message": message } })
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, payload) = match self {
            AppError::Validation(msg) => (
                StatusCode::UNPROCESSABLE_ENTITY,
                body("validation_failed", &msg),
            ),
            AppError::Unauthorized => (
                StatusCode::UNAUTHORIZED,
                body("unauthorized", "Authentication required"),
            ),
            AppError::InvalidCredentials => (
                StatusCode::UNAUTHORIZED,
                body(
                    "invalid_credentials",
                    "Phone number or password is incorrect",
                ),
            ),
            AppError::Forbidden(code) => (
                StatusCode::FORBIDDEN,
                body(code, "You are not allowed to do this"),
            ),
            AppError::PhoneUnverified(Redacted(verification_id)) => {
                let mut payload = body("phone_unverified", "Verify your phone number to continue");
                payload["verification_id"] = json!(verification_id);
                (StatusCode::FORBIDDEN, payload)
            }
            AppError::NotFound => (StatusCode::NOT_FOUND, body("not_found", "Not found")),
            AppError::MethodNotAllowed => (
                StatusCode::METHOD_NOT_ALLOWED,
                body("method_not_allowed", "Method not allowed"),
            ),
            AppError::Conflict(code) => (
                StatusCode::CONFLICT,
                body(code, "Conflict with existing data"),
            ),
            AppError::Unprocessable(code, msg) => {
                (StatusCode::UNPROCESSABLE_ENTITY, body(code, &msg))
            }
            AppError::TooManyRequests(code) => (
                StatusCode::TOO_MANY_REQUESTS,
                body(code, "Too many attempts, try again later"),
            ),
            AppError::ProviderUnavailable => (
                StatusCode::BAD_GATEWAY,
                body(
                    "payment_provider_unavailable",
                    "The payment provider is unavailable, try again later",
                ),
            ),
            AppError::NotSupported => (
                StatusCode::NOT_IMPLEMENTED,
                body("not_supported", "This operation is not supported"),
            ),
            AppError::Internal(err) => {
                tracing::error!(error = ?err, "internal error");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    body("internal", "Something went wrong"),
                )
            }
        };
        (status, Json(payload)).into_response()
    }
}
