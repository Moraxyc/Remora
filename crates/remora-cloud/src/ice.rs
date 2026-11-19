//! ICE servers handed to both sides of a session.
//!
//! TURN credentials follow the TURN REST API convention that coturn implements as
//! `use-auth-secret`: the username is `<expiry>:<user id>`, and the password is the base64
//! HMAC-SHA1 of the username under a secret shared with the TURN server. Nothing has to be
//! stored, and every credential expires by itself.

use std::fmt;
use std::str::FromStr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use hmac::{Hmac, KeyInit, Mac};
use serde_json::{Value, json};
use sha1::Sha1;

/// How long issued TURN credentials stay valid. The TURN server checks them again whenever an
/// allocation is refreshed, so this bounds how long one session can stay relayed.
const CREDENTIAL_LIFETIME: Duration = Duration::from_secs(24 * 60 * 60);

/// The STUN and TURN servers clients are told about. Without any, clients use host candidates
/// only, which is enough on one network.
#[derive(Debug, Default)]
pub struct Ice {
    stun: Vec<String>,
    turn: Option<Turn>,
}

#[derive(Debug)]
struct Turn {
    urls: Vec<String>,
    secret: TurnSecret,
}

/// The secret shared with the TURN server (coturn's `static-auth-secret`).
pub struct TurnSecret(Vec<u8>);

impl TurnSecret {
    pub fn new(secret: impl Into<Vec<u8>>) -> Self {
        Self(secret.into())
    }
}

impl fmt::Debug for TurnSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TurnSecret(..)")
    }
}

/// A `stun:`, `stuns:`, `turn:` or `turns:` URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IceUrl(String);

impl FromStr for IceUrl {
    type Err = &'static str;

    fn from_str(url: &str) -> Result<Self, Self::Err> {
        let (scheme, rest) = url.split_once(':').ok_or("expected a stun: or turn: URL")?;
        if !matches!(scheme, "stun" | "stuns" | "turn" | "turns") || rest.is_empty() {
            return Err("expected a stun:, stuns:, turn: or turns: URL");
        }
        Ok(Self(url.to_owned()))
    }
}

impl IceUrl {
    fn is_turn(&self) -> bool {
        self.0.starts_with("turn")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IceError {
    TurnWithoutSecret,
    SecretWithoutTurn,
}

impl fmt::Display for IceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::TurnWithoutSecret => "TURN servers need the secret shared with them",
            Self::SecretWithoutTurn => "a TURN secret was given but no TURN server",
        })
    }
}

impl std::error::Error for IceError {}

impl Ice {
    pub fn new(urls: Vec<IceUrl>, secret: Option<TurnSecret>) -> Result<Self, IceError> {
        let (turn, stun): (Vec<_>, Vec<_>) = urls.into_iter().partition(IceUrl::is_turn);
        let turn = match (turn.is_empty(), secret) {
            (true, None) => None,
            (true, Some(_)) => return Err(IceError::SecretWithoutTurn),
            (false, None) => return Err(IceError::TurnWithoutSecret),
            (false, Some(secret)) => Some(Turn {
                urls: turn.into_iter().map(|url| url.0).collect(),
                secret,
            }),
        };
        Ok(Self {
            stun: stun.into_iter().map(|url| url.0).collect(),
            turn,
        })
    }

    /// `iceServers` for a session of `user_id`, with fresh TURN credentials.
    pub fn servers(&self, user_id: &str) -> Value {
        self.servers_at(user_id, SystemTime::now())
    }

    fn servers_at(&self, user_id: &str, now: SystemTime) -> Value {
        let mut servers = Vec::new();
        if !self.stun.is_empty() {
            servers.push(json!({ "urls": self.stun }));
        }
        if let Some(turn) = &self.turn {
            let expiry = (now + CREDENTIAL_LIFETIME)
                .duration_since(UNIX_EPOCH)
                .map_or(0, |since| since.as_secs());
            let username = format!("{expiry}:{user_id}");
            let mut mac =
                Hmac::<Sha1>::new_from_slice(&turn.secret.0).expect("HMAC accepts any key length");
            mac.update(username.as_bytes());
            let credential = STANDARD.encode(mac.finalize().into_bytes());
            servers
                .push(json!({ "urls": turn.urls, "username": username, "credential": credential }));
        }
        Value::Array(servers)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn urls(urls: &[&str]) -> Vec<IceUrl> {
        urls.iter().map(|url| url.parse().unwrap()).collect()
    }

    #[test]
    fn parses_ice_urls() {
        for url in [
            "stun:a:3478",
            "stuns:a",
            "turn:a:3478?transport=tcp",
            "turns:a:5349",
        ] {
            assert!(url.parse::<IceUrl>().is_ok(), "{url}");
        }
        for url in ["", "stun:", "https://a", "turnx:a", "a:3478"] {
            assert!(url.parse::<IceUrl>().is_err(), "{url}");
        }
    }

    #[test]
    fn without_servers_there_are_none() {
        assert_eq!(Ice::default().servers("1"), json!([]));
    }

    // Expected credential computed independently:
    // printf '%s' 1700086400:1 | openssl dgst -sha1 -hmac test-turn-secret -binary | base64
    #[test]
    fn turn_credentials_follow_the_rest_api_convention() {
        let ice = Ice::new(
            urls(&[
                "stun:turn.test:3478",
                "turn:turn.test:3478",
                "turns:turn.test:5349",
            ]),
            Some(TurnSecret::new("test-turn-secret")),
        )
        .unwrap();
        let now = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        assert_eq!(
            ice.servers_at("1", now),
            json!([
                { "urls": ["stun:turn.test:3478"] },
                {
                    "urls": ["turn:turn.test:3478", "turns:turn.test:5349"],
                    "username": "1700086400:1",
                    "credential": "HGpR8GDLvUwN0fq4+uvRsDA6eIg=",
                },
            ])
        );
    }

    #[test]
    fn turn_and_secret_come_together() {
        let secret = || Some(TurnSecret::new("s"));
        assert_eq!(
            Ice::new(urls(&["turn:a"]), None).unwrap_err(),
            IceError::TurnWithoutSecret
        );
        assert_eq!(
            Ice::new(urls(&["stun:a"]), secret()).unwrap_err(),
            IceError::SecretWithoutTurn
        );
        assert!(Ice::new(urls(&["stun:a"]), None).is_ok());
    }

    #[test]
    fn debug_hides_the_secret() {
        let ice = Ice::new(urls(&["turn:a"]), Some(TurnSecret::new("hidden"))).unwrap();
        assert!(!format!("{ice:?}").contains("hidden"));
    }
}
