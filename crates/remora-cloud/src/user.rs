//! Users, and how clients' requests prove they act for one.

use std::sync::Arc;

use axum::extract::{FromRef, FromRequestParts, State};
use axum::http::header::AUTHORIZATION;
use axum::http::request::Parts;
use rusqlite::{OptionalExtension, params};
use serde::Serialize;

use crate::Database;
use crate::api::{self, ApiError, Empty};
use crate::device::Device;
use crate::oidc::Identity;
use crate::token::{TokenKey, UserToken, now, random};

/// The user who signs in with the password.
pub const OWNER_ID: &str = "1";

/// Users, by numeric id. Each is the owner or an OpenID Connect identity.
#[derive(Debug, Clone)]
pub struct Users(Database);

impl From<Database> for Users {
    fn from(database: Database) -> Self {
        Self(database)
    }
}

/// A user as shown to them and to their clients.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Profile {
    pub id: String,
    pub name: String,
}

impl Users {
    /// The user of an OpenID Connect identity, made the first time it signs in. Their name
    /// follows the provider's.
    pub fn of_identity(&self, identity: &Identity) -> rusqlite::Result<Profile> {
        let mut connection = self.0.connection();
        let transaction = connection.transaction()?;
        let profile = |row: &rusqlite::Row| {
            Ok(Profile {
                id: row.get(0)?,
                name: row.get(1)?,
            })
        };
        let known = transaction
            .query_row(
                "UPDATE users SET name = ?3 WHERE issuer = ?1 AND subject = ?2
                 RETURNING id, name",
                [&identity.issuer, &identity.subject, &identity.name],
                profile,
            )
            .optional()?;
        let user = match known {
            Some(user) => user,
            None => transaction.query_row(
                "INSERT INTO users (id, issuer, subject, name)
                 SELECT CAST(MAX(CAST(id AS INTEGER)) + 1 AS TEXT), ?1, ?2, ?3 FROM users
                 RETURNING id, name",
                [&identity.issuer, &identity.subject, &identity.name],
                profile,
            )?,
        };
        transaction.commit()?;
        Ok(user)
    }

    pub fn get(&self, id: &str) -> rusqlite::Result<Option<Profile>> {
        self.0
            .connection()
            .query_row("SELECT id, name FROM users WHERE id = ?1", [id], |row| {
                Ok(Profile {
                    id: row.get(0)?,
                    name: row.get(1)?,
                })
            })
            .optional()
    }
}

/// Logins that have not been revoked. Each user token belongs to one.
#[derive(Debug, Clone)]
pub struct Sessions(Database);

impl From<Database> for Sessions {
    fn from(database: Database) -> Self {
        Self(database)
    }
}

/// One of a user's logged-in clients, as shown in the web app.
pub struct ClientSession {
    pub id: String,
    /// Seconds since the Unix epoch, or `None` for logins created before timestamps were kept.
    pub created: Option<u64>,
    pub device: Option<Device>,
}

impl Sessions {
    /// Opens a session for `user_id`, and says what its token carries.
    pub fn start(&self, user_id: &str) -> rusqlite::Result<UserToken> {
        let session_id = hex::encode(random::<16>());
        let created: i64 = now().try_into().unwrap_or(i64::MAX);
        self.0.connection().execute(
            "INSERT INTO sessions (id, user_id, created) VALUES (?1, ?2, ?3)",
            params![&session_id, user_id, created],
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

    /// Client logins of `user_id`, newest first.
    pub fn owned_by(&self, user_id: &str) -> rusqlite::Result<Vec<ClientSession>> {
        let connection = self.0.connection();
        let mut statement = connection.prepare_cached(
            "SELECT sessions.id, sessions.created, devices.id, devices.name, devices.platform
             FROM sessions
             LEFT JOIN devices ON devices.id = sessions.device_id
             WHERE sessions.user_id = ?1
             ORDER BY sessions.created DESC, sessions.id",
        )?;
        statement
            .query_map([user_id], |row| {
                let created: i64 = row.get(1)?;
                let device = row
                    .get::<_, Option<String>>(2)?
                    .map(|id| -> rusqlite::Result<Device> {
                        Ok(Device {
                            id,
                            name: row.get(3)?,
                            platform: row.get(4)?,
                        })
                    })
                    .transpose()?;
                Ok(ClientSession {
                    id: row.get(0)?,
                    created: u64::try_from(created).ok().filter(|&at| at > 0),
                    device,
                })
            })?
            .collect()
    }

    /// Revokes one of `user_id`'s client logins. Returns `None` if there is no such login for
    /// that user, or `Some(device_id)` with the device the login was bound to, if any.
    pub fn revoke(
        &self,
        session_id: &str,
        user_id: &str,
    ) -> rusqlite::Result<Option<Option<String>>> {
        self.0
            .connection()
            .query_row(
                "DELETE FROM sessions WHERE id = ?1 AND user_id = ?2 RETURNING device_id",
                [session_id, user_id],
                |row| row.get(0),
            )
            .optional()
    }

    pub(crate) fn is_open(&self, token: &UserToken) -> rusqlite::Result<bool> {
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
    nickname: String,
}

/// `GET /api/v1/user/info`.
pub async fn info(
    user: User,
    State(users): State<Users>,
) -> Result<api::Ok<InfoResponse>, ApiError> {
    let profile = users.get(&user.id)?.ok_or(ApiError::TOKEN_INVALID)?;
    Ok(api::Ok(InfoResponse {
        user_id: profile.id,
        nickname: profile.name,
    }))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(issuer: &str, subject: &str, name: &str) -> Identity {
        Identity {
            issuer: issuer.into(),
            subject: subject.into(),
            name: name.into(),
        }
    }

    #[test]
    fn an_identity_is_one_user_named_as_its_provider_says() {
        let users = Users::from(Database::in_memory());
        let alice = users
            .of_identity(&identity("https://a", "alice", "Alice"))
            .unwrap();
        assert_eq!(alice.id, "2", "after the owner");
        let renamed = users
            .of_identity(&identity("https://a", "alice", "Al"))
            .unwrap();
        assert_eq!(
            renamed,
            Profile {
                id: "2".into(),
                name: "Al".into()
            }
        );
        assert_eq!(users.get("2").unwrap(), Some(renamed));

        let elsewhere = users
            .of_identity(&identity("https://b", "alice", "Alice"))
            .unwrap();
        assert_eq!(elsewhere.id, "3", "the subject is only unique per issuer");
        assert_eq!(users.get("1").unwrap().unwrap().name, "Owner");
    }
}
