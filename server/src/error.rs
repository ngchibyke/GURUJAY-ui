use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::json;

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
    #[error("not found")]
    NotFound,
    #[error("conflict: {0}")]
    Conflict(&'static str),
    #[error("unprocessable: {0}")]
    Unprocessable(&'static str, String),
    #[error("too many requests: {0}")]
    TooManyRequests(&'static str),
    #[error("internal error")]
    Internal(#[from] anyhow::Error),
}

fn body(code: &str, message: &str) -> serde_json::Value {
    json!({ "error": { "code": code, "message": message } })
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, payload) = match self {
            AppError::Validation(msg) => (StatusCode::UNPROCESSABLE_ENTITY, body("validation_failed", &msg)),
            AppError::Unauthorized => (StatusCode::UNAUTHORIZED, body("unauthorized", "Authentication required")),
            AppError::InvalidCredentials => {
                (StatusCode::UNAUTHORIZED, body("invalid_credentials", "Phone number or password is incorrect"))
            }
            AppError::Forbidden(code) => (StatusCode::FORBIDDEN, body(code, "You are not allowed to do this")),
            AppError::NotFound => (StatusCode::NOT_FOUND, body("not_found", "Not found")),
            AppError::Conflict(code) => (StatusCode::CONFLICT, body(code, "Conflict with existing data")),
            AppError::Unprocessable(code, msg) => (StatusCode::UNPROCESSABLE_ENTITY, body(code, &msg)),
            AppError::TooManyRequests(code) => {
                (StatusCode::TOO_MANY_REQUESTS, body(code, "Too many attempts, try again later"))
            }
            AppError::Internal(err) => {
                tracing::error!(error = ?err, "internal error");
                (StatusCode::INTERNAL_SERVER_ERROR, body("internal", "Something went wrong"))
            }
        };
        (status, Json(payload)).into_response()
    }
}
