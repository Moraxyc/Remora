//! The user, and how requests prove they act for it.

use std::sync::Arc;

use axum::extract::{FromRef, FromRequestParts};
use axum::http::header::AUTHORIZATION;
use axum::http::request::Parts;
use serde::Serialize;

use crate::api::{self, ApiError};
use crate::token::TokenKey;

/// The only user.
pub const OWNER_ID: &str = "1";
pub const OWNER_NICKNAME: &str = "Remora";

/// A request carrying a valid user token as `Authorization: Bearer <token>`. Clients also send
/// the token and its csrf as cookies and `X-CSRF-TOKEN`; those are browser protections and are
/// not needed when the token comes from a header.
pub struct User {
    pub id: String,
}

impl<S> FromRequestParts<S> for User
where
    Arc<TokenKey>: FromRef<S>,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, ApiError> {
        let tokens = Arc::<TokenKey>::from_ref(state);
        parts
            .headers
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .and_then(|token| tokens.verify_user(token))
            .map(|id| Self { id })
            .ok_or(ApiError::TOKEN_INVALID)
    }
}

#[derive(Serialize)]
pub struct InfoResponse {
    user_id: String,
    nickname: &'static str,
}

/// `GET /api/v1/user/info`.
pub async fn info(user: User) -> api::Ok<InfoResponse> {
    api::Ok(InfoResponse {
        user_id: user.id,
        nickname: OWNER_NICKNAME,
    })
}
