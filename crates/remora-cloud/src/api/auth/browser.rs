//! Browsers signed in to the web app. A browser's cookie is a random secret; the database
//! keeps only its hash, with the CSRF token that the browser's changes must carry.

use std::time::Duration;

use axum::extract::{FromRef, FromRequestParts};
use axum::http::request::Parts;
use axum::http::{HeaderMap, Method, header};
use rusqlite::{OptionalExtension, params};
use sha2::{Digest, Sha256};

use crate::Database;
use crate::api::user::Profile;
use crate::api::web::WebError;
use crate::contracts::{now, random};

pub const COOKIE: &str = "remora_session";

/// How long a browser stays signed in.
pub const LIFETIME: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// Carries the CSRF token on every request that changes something.
const CSRF_HEADER: &str = "x-csrf-token";

#[derive(Debug, Clone)]
pub struct Browsers(Database);

impl From<Database> for Browsers {
    fn from(database: Database) -> Self {
        Self(database)
    }
}

/// A browser that has just signed in.
pub struct Started {
    /// The cookie's value, which only the browser keeps.
    pub cookie: String,
}

/// A signed-in browser of a user, as shown in the web app.
pub struct BrowserSession {
    pub id: String,
    pub created: u64,
    pub expires: u64,
}

impl Browsers {
    /// Signs a browser in as `user_id`.
    pub fn start(&self, user_id: &str) -> rusqlite::Result<Started> {
        let cookie = hex::encode(random::<32>());
        let now = seconds();
        let connection = self.0.connection();
        connection.execute("DELETE FROM browsers WHERE expires <= ?1", [now])?;
        connection.execute(
            "INSERT INTO browsers (id, user_id, csrf, created, expires)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                hash(&cookie),
                user_id,
                hex::encode(random::<16>()),
                now,
                now.saturating_add(LIFETIME.as_secs().try_into().unwrap_or(i64::MAX))
            ],
        )?;
        Ok(Started { cookie })
    }

    /// The browser that sent `cookie`, unless it has signed out or expired.
    fn find(&self, cookie: &str) -> rusqlite::Result<Option<SignedIn>> {
        self.0
            .connection()
            .query_row(
                "SELECT browsers.id, browsers.csrf, users.id, users.name
                 FROM browsers JOIN users ON users.id = browsers.user_id
                 WHERE browsers.id = ?1 AND browsers.expires > ?2",
                params![hash(cookie), seconds()],
                |row| {
                    Ok(SignedIn {
                        browser_id: row.get(0)?,
                        csrf: row.get(1)?,
                        user: Profile {
                            id: row.get(2)?,
                            name: row.get(3)?,
                        },
                    })
                },
            )
            .optional()
    }

    /// Signs browser `browser_id` out.
    pub fn end(&self, browser_id: &str) -> rusqlite::Result<()> {
        self.0
            .connection()
            .execute("DELETE FROM browsers WHERE id = ?1", [browser_id])?;
        Ok(())
    }

    /// Active browsers of `user_id`, newest first.
    pub fn owned_by(&self, user_id: &str) -> rusqlite::Result<Vec<BrowserSession>> {
        let connection = self.0.connection();
        let mut statement = connection.prepare_cached(
            "SELECT id, created, expires FROM browsers
             WHERE user_id = ?1 AND expires > ?2
             ORDER BY created DESC, id",
        )?;
        statement
            .query_map(params![user_id, seconds()], |row| {
                Ok(BrowserSession {
                    id: row.get(0)?,
                    created: row.get::<_, i64>(1)?.try_into().unwrap_or(0),
                    expires: row.get::<_, i64>(2)?.try_into().unwrap_or(0),
                })
            })?
            .collect()
    }

    /// Signs one of `user_id`'s browsers out; `false` if there is no such browser for that user.
    pub fn revoke(&self, browser_id: &str, user_id: &str) -> rusqlite::Result<bool> {
        let changed = self.0.connection().execute(
            "DELETE FROM browsers WHERE id = ?1 AND user_id = ?2",
            [browser_id, user_id],
        )?;
        Ok(changed > 0)
    }
}

/// Now, as SQLite stores it.
fn seconds() -> i64 {
    now().try_into().unwrap_or(i64::MAX)
}

fn hash(cookie: &str) -> String {
    hex::encode(Sha256::digest(cookie.as_bytes()))
}

/// A request from a signed-in browser. A request that may change something must also carry
/// the browser's CSRF token in `X-CSRF-Token`.
#[derive(Debug)]
pub struct SignedIn {
    pub user: Profile,
    pub browser_id: String,
    pub csrf: String,
}

impl SignedIn {
    /// The browser that sent `headers`, if it is signed in.
    pub fn from_headers(
        browsers: &Browsers,
        headers: &HeaderMap,
    ) -> rusqlite::Result<Option<Self>> {
        match read_cookie(headers, COOKIE) {
            Some(cookie) => browsers.find(cookie),
            None => Ok(None),
        }
    }
}

impl<S> FromRequestParts<S> for SignedIn
where
    Browsers: FromRef<S>,
    S: Send + Sync,
{
    type Rejection = WebError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, WebError> {
        let browsers = Browsers::from_ref(state);
        let signed_in =
            Self::from_headers(&browsers, &parts.headers)?.ok_or(WebError::SignedOut)?;
        let safe = matches!(parts.method, Method::GET | Method::HEAD);
        if !safe {
            let sent = parts
                .headers
                .get(CSRF_HEADER)
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default();
            if !same_secret(sent, &signed_in.csrf) {
                return Err(WebError::Csrf);
            }
        }
        Ok(signed_in)
    }
}

/// The value of cookie `name` in a request.
pub fn read_cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|cookies| cookies.split(';'))
        .filter_map(|cookie| cookie.trim().split_once('='))
        .find(|(found, _)| *found == name)
        .map(|(_, value)| value)
}

/// Compares secrets without stopping at the first difference.
fn same_secret(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0, |diff, (x, y)| diff | (x ^ y))
            == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_browser_stays_signed_in_until_it_signs_out() {
        let browsers = Browsers::from(Database::in_memory());
        let started = browsers.start("1").unwrap();
        let signed_in = browsers.find(&started.cookie).unwrap().unwrap();
        assert_eq!(signed_in.user.id, "1");
        assert_eq!(signed_in.csrf.len(), 32);
        assert!(browsers.find("other").unwrap().is_none());

        browsers.end(&signed_in.browser_id).unwrap();
        assert!(browsers.find(&started.cookie).unwrap().is_none());
    }

    #[test]
    fn the_database_keeps_no_cookie() {
        let database = Database::in_memory();
        let started = Browsers::from(database.clone()).start("1").unwrap();
        let ids: String = database
            .connection()
            .query_row("SELECT group_concat(id || csrf) FROM browsers", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert!(!ids.contains(&started.cookie));
    }

    #[test]
    fn expired_browsers_are_signed_out() {
        let database = Database::in_memory();
        let browsers = Browsers::from(database.clone());
        let started = browsers.start("1").unwrap();
        database
            .connection()
            .execute("UPDATE browsers SET expires = ?1", [seconds()])
            .unwrap();
        assert!(browsers.find(&started.cookie).unwrap().is_none());
    }

    #[test]
    fn compares_secrets() {
        assert!(same_secret("abc", "abc"));
        assert!(!same_secret("abc", "abd"));
        assert!(!same_secret("abc", "ab"));
    }
}
