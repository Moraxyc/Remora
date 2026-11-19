//! The user, and how requests prove they act for it.

use std::sync::Arc;

use axum::extract::{FromRef, FromRequestParts, State};
use axum::http::header::AUTHORIZATION;
use axum::http::request::Parts;
use rusqlite::OptionalExtension;
use serde::Serialize;

use crate::Database;
use crate::api::{self, ApiError, Empty};
use crate::token::{TokenKey, UserToken, random};

/// The only user.
pub const OWNER_ID: &str = "1";
pub const OWNER_NICKNAME: &str = "Remora";

/// Logins that have not been revoked. Each user token belongs to one.
#[derive(Debug, Clone)]
pub struct Sessions(Database);

impl From<Database> for Sessions {
    fn from(database: Database) -> Self {
        Self(database)
    }
}

impl Sessions {
    /// Opens a session for `user_id`, and says what its token carries.
    pub fn start(&self, user_id: &str) -> rusqlite::Result<UserToken> {
        let session_id = hex::encode(random::<16>());
        self.0.connection().execute(
            "INSERT INTO sessions (id, user_id) VALUES (?1, ?2)",
            [&session_id, user_id],
        )?;
        Ok(UserToken {
            user_id: user_id.to_owned(),
            session_id,
        })
    }

    fn end(&self, session_id: &str) -> rusqlite::Result<()> {
        self.0
            .connection()
            .execute("DELETE FROM sessions WHERE id = ?1", [session_id])?;
        Ok(())
    }

    fn is_open(&self, token: &UserToken) -> rusqlite::Result<bool> {
        let found = self
            .0
            .connection()
            .query_row(
                "SELECT 1 FROM sessions WHERE id = ?1 AND user_id = ?2",
                [&token.session_id, &token.user_id],
                |_| Ok(()),
            )
            .optional()?;
        Ok(found.is_some())
    }
}

/// A request carrying a valid user token of an open session as `Authorization: Bearer
/// <token>`. Clients also send the token and its csrf as cookies and `X-CSRF-TOKEN`; those are
/// browser protections and are not needed when the token comes from a header.
pub struct User {
    pub id: String,
    pub session_id: String,
}

impl<S> FromRequestParts<S> for User
where
    Arc<TokenKey>: FromRef<S>,
    Sessions: FromRef<S>,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, ApiError> {
        let token = parts
            .headers
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .and_then(|token| Arc::<TokenKey>::from_ref(state).verify_user(token))
            .ok_or(ApiError::TOKEN_INVALID)?;
        if !Sessions::from_ref(state).is_open(&token)? {
            return Err(ApiError::TOKEN_INVALID);
        }
        Ok(Self {
            id: token.user_id,
            session_id: token.session_id,
        })
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

/// `POST /api/v1/user/logout`, with an empty body: ends the session of the token it carries.
pub async fn logout(
    user: User,
    State(sessions): State<Sessions>,
) -> Result<api::Ok<Empty>, ApiError> {
    sessions.end(&user.session_id)?;
    tracing::info!(user_id = %user.id, "logged out");
    Ok(api::Ok(Empty {}))
}
