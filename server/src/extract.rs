use axum::{
    Json,
    extract::{FromRequest, Request},
};
use serde::de::DeserializeOwned;

use crate::error::AppError;

/// JSON body extractor whose rejections use the standard JSON error envelope.
/// The message is generic: serde's text is never echoed back.
pub struct AppJson<T>(pub T);

impl<S, T> FromRequest<S> for AppJson<T>
where
    S: Send + Sync,
    T: DeserializeOwned,
{
    type Rejection = AppError;

    async fn from_request(req: Request, state: &S) -> Result<Self, AppError> {
        match Json::<T>::from_request(req, state).await {
            Ok(Json(value)) => Ok(AppJson(value)),
            Err(_) => Err(AppError::Validation(
                "Request body is missing or invalid".into(),
            )),
        }
    }
}

/// Query-string extractor whose rejections use the standard JSON error envelope
/// (422 `validation_failed`). The message is generic: serde's text is never echoed back.
pub struct AppQuery<T>(pub T);

impl<S, T> axum::extract::FromRequestParts<S> for AppQuery<T>
where
    S: Send + Sync,
    T: DeserializeOwned,
{
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &S,
    ) -> Result<Self, AppError> {
        match axum::extract::Query::<T>::from_request_parts(parts, state).await {
            Ok(axum::extract::Query(value)) => Ok(AppQuery(value)),
            Err(_) => Err(AppError::Validation("Query string is invalid".into())),
        }
    }
}
