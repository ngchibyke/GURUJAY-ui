use axum::{
    extract::FromRequestParts,
    http::{HeaderMap, header, request::Parts},
};
use serde::Serialize;
use uuid::Uuid;

use super::sessions::hash_token;
use crate::{AppState, error::AppError};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, sqlx::Type)]
#[sqlx(type_name = "user_role", rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Support,
    Owner,
}

#[derive(Debug, Clone)]
pub struct AuthUser {
    pub id: Uuid,
    pub role: Role,
    pub is_seller: bool,
    pub phone_verified: bool,
}

impl AuthUser {
    /// Owner satisfies every role check.
    pub fn require_role(&self, role: Role) -> Result<(), AppError> {
        if self.role == Role::Owner || self.role == role {
            Ok(())
        } else {
            Err(AppError::Forbidden("forbidden"))
        }
    }
}

pub fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let token = value.strip_prefix("Bearer ")?.trim();
    (!token.is_empty()).then_some(token)
}

impl FromRequestParts<AppState> for AuthUser {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, AppError> {
        let token = bearer_token(&parts.headers).ok_or(AppError::Unauthorized)?;
        let row: Option<(Uuid, Role, bool, bool)> = sqlx::query_as(
            "SELECT u.id, u.role, u.is_seller, u.phone_verified_at IS NOT NULL \
             FROM sessions s JOIN users u ON u.id = s.user_id \
             WHERE s.token_hash = $1 AND s.revoked_at IS NULL AND s.expires_at > now() \
               AND u.deleted_at IS NULL",
        )
        .bind(hash_token(token))
        .fetch_optional(&state.db)
        .await
        .map_err(anyhow::Error::from)?;
        let (id, role, is_seller, phone_verified) = row.ok_or(AppError::Unauthorized)?;
        Ok(AuthUser {
            id,
            role,
            is_seller,
            phone_verified,
        })
    }
}
