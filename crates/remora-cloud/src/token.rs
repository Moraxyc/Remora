//! Tokens the cloud issues: HS256 JWTs, because clients read claims out of them.
//!
//! - User tokens carry the `csrf` claim clients send back, and no `exp`: like UU's, they are
//!   used until rejected. Each names its session (`sid`), which can be revoked.
//! - Room tokens authenticate one signaling connection, as a device's host or as a viewer
//!   joining it, and expire.
//!
//! Each token names its kind, so one kind is never accepted as the other.

use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, KeyInit, Mac};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::Sha256;

/// `{"alg":"HS256","typ":"JWT"}`, base64url-encoded.
const HEADER: &str = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9";

/// How long a room token can be used to connect to signaling.
const ROOM_TOKEN_LIFETIME: Duration = Duration::from_secs(60 * 60);

/// Secret that signs every token.
pub struct TokenKey([u8; 32]);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Kind {
    User,
    Room,
}

#[derive(Serialize, Deserialize)]
struct UserClaims {
    kind: Kind,
    sub: String,
    sid: String,
    csrf: String,
    iat: u64,
}

/// What a user token says: who, and in which session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserToken {
    pub user_id: String,
    pub session_id: String,
}

/// What a room token grants: signaling for the room of `device_id`, on behalf of user `sub`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Room {
    pub sub: String,
    pub device_id: String,
    #[serde(flatten)]
    pub role: Role,
}

/// Which side of the room a connection is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum Role {
    /// The device's own host.
    Host,
    /// A viewer that joined the room to control the host.
    Viewer {
        /// The device the viewer runs on, when it said so.
        from_device: Option<String>,
        /// Joined with `force_join`: its desktop session ends other devices' ones.
        take_over: bool,
    },
}

#[derive(Serialize, Deserialize)]
struct RoomClaims {
    kind: Kind,
    #[serde(flatten)]
    room: Room,
    exp: u64,
}

impl TokenKey {
    pub fn new(key: [u8; 32]) -> Self {
        Self(key)
    }

    pub fn issue_user(&self, token: &UserToken) -> String {
        self.sign(&UserClaims {
            kind: Kind::User,
            sub: token.user_id.clone(),
            sid: token.session_id.clone(),
            csrf: hex::encode(random::<16>()),
            iat: now(),
        })
    }

    /// What a user token this key issued says. Whether its session is still open is for the
    /// caller to check.
    pub fn verify_user(&self, token: &str) -> Option<UserToken> {
        let claims: UserClaims = self.verify(token)?;
        (claims.kind == Kind::User).then_some(UserToken {
            user_id: claims.sub,
            session_id: claims.sid,
        })
    }

    pub fn issue_room(&self, room: Room) -> String {
        self.sign(&RoomClaims {
            kind: Kind::Room,
            room,
            exp: now() + ROOM_TOKEN_LIFETIME.as_secs(),
        })
    }

    /// What an unexpired room token this key issued grants.
    pub fn verify_room(&self, token: &str) -> Option<Room> {
        let claims: RoomClaims = self.verify(token)?;
        (claims.kind == Kind::Room && claims.exp > now()).then_some(claims.room)
    }

    fn sign(&self, claims: &impl Serialize) -> String {
        let payload = serde_json::to_vec(claims).expect("claims serialize");
        let unsigned = format!("{HEADER}.{}", URL_SAFE_NO_PAD.encode(payload));
        let signature = self.mac(&unsigned).finalize().into_bytes();
        format!("{unsigned}.{}", URL_SAFE_NO_PAD.encode(signature))
    }

    fn verify<C: DeserializeOwned>(&self, token: &str) -> Option<C> {
        let (unsigned, signature) = token.rsplit_once('.')?;
        let (header, payload) = unsigned.split_once('.')?;
        if header != HEADER {
            return None;
        }
        let signature = URL_SAFE_NO_PAD.decode(signature).ok()?;
        self.mac(unsigned).verify_slice(&signature).ok()?;
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).ok()?).ok()
    }

    fn mac(&self, unsigned: &str) -> Hmac<Sha256> {
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.0).expect("HMAC accepts any key length");
        mac.update(unsigned.as_bytes());
        mac
    }
}

impl fmt::Debug for TokenKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TokenKey(..)")
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

/// `N` bytes from the OS random source.
pub(crate) fn random<const N: usize>() -> [u8; N] {
    let mut bytes = [0; N];
    getrandom::fill(&mut bytes).expect("OS random source available");
    bytes
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    impl TokenKey {
        fn generate() -> Self {
            Self(random())
        }
    }

    fn payload(token: &str) -> Value {
        let payload = token.split('.').nth(1).unwrap();
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).unwrap()).unwrap()
    }

    fn user() -> UserToken {
        UserToken {
            user_id: "1".into(),
            session_id: "s".into(),
        }
    }

    fn room() -> Room {
        Room {
            sub: "1".into(),
            device_id: "d".into(),
            role: Role::Host,
        }
    }

    #[test]
    fn user_tokens_are_jwts_with_a_csrf_claim() {
        let token = TokenKey::generate().issue_user(&user());
        let parts: Vec<&str> = token.split('.').collect();
        assert_eq!(parts.len(), 3);
        assert_eq!(
            URL_SAFE_NO_PAD.decode(parts[0]).unwrap(),
            br#"{"alg":"HS256","typ":"JWT"}"#
        );

        let claims = payload(&token);
        assert_eq!(claims["sub"], "1");
        assert_eq!(claims["sid"], "s");
        assert_eq!(claims["csrf"].as_str().unwrap().len(), 32);
        assert!(claims.get("exp").is_none());
    }

    #[test]
    fn every_user_token_gets_its_own_csrf() {
        let key = TokenKey::generate();
        let csrf = || payload(&key.issue_user(&user()))["csrf"].clone();
        assert_ne!(csrf(), csrf());
    }

    #[test]
    fn room_tokens_expire() {
        let key = TokenKey::generate();
        let token = key.issue_room(room());
        let exp = payload(&token)["exp"].as_u64().unwrap();
        assert!(exp > now() && exp <= now() + ROOM_TOKEN_LIFETIME.as_secs());
        assert_eq!(key.verify_room(&token), Some(room()));

        let expired = key.sign(&RoomClaims {
            kind: Kind::Room,
            room: room(),
            exp: now() - 1,
        });
        assert_eq!(key.verify_room(&expired), None);
    }

    #[test]
    fn kinds_are_not_interchangeable() {
        let key = TokenKey::generate();
        assert_eq!(key.verify_room(&key.issue_user(&user())), None);
        assert_eq!(key.verify_user(&key.issue_room(room())), None);
    }

    #[test]
    fn rejects_foreign_or_tampered_tokens() {
        let key = TokenKey::generate();
        let token = key.issue_user(&user());
        assert_eq!(key.verify_user(&token), Some(user()));
        assert_eq!(TokenKey::generate().verify_user(&token), None);

        let parts: Vec<&str> = token.split('.').collect();
        let forged = json!({ "kind": "user", "sub": "2", "sid": "s", "csrf": "x", "iat": 0 });
        let forged = URL_SAFE_NO_PAD.encode(forged.to_string());
        assert_eq!(
            key.verify_user(&format!("{}.{forged}.{}", parts[0], parts[2])),
            None
        );

        let none_alg = URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#);
        assert_eq!(key.verify_user(&format!("{none_alg}.{}.", parts[1])), None);
        assert_eq!(key.verify_user(&format!("{token}.x")), None);
        assert_eq!(key.verify_user(""), None);
        assert_eq!(key.verify_user("a.b"), None);
    }

    #[test]
    fn debug_hides_key() {
        assert_eq!(format!("{:?}", TokenKey([7; 32])), "TokenKey(..)");
    }
}
