//! Self-hostable stand-in for the UU Remote cloud.

mod api;
mod device;
mod login;
mod room;
mod signaling;
mod signing;
mod token;
mod user;

use std::fmt;
use std::str::FromStr;
use std::sync::Arc;

use axum::Router;
use axum::extract::FromRef;
use axum::middleware::from_fn_with_state;
use axum::routing::{get, post};
use remora_proto::cloud::SignKey;

pub use signaling::Ping;

pub struct Config {
    /// Where clients reach this server.
    pub public_url: PublicUrl,
    /// With a key, every request must carry a valid `x-param-sign`; without one, signatures
    /// are not checked.
    pub sign_key: Option<SignKey>,
    /// Signaling heartbeat.
    pub ping: Ping,
}

/// The `http://` or `https://` origin clients reach this server at, such as
/// `https://remora.example`. Clients sign the path they send, so it cannot have a path prefix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicUrl {
    secure: bool,
    authority: String,
}

impl PublicUrl {
    /// The signaling URL handed to clients: the same origin as `ws://` or `wss://`.
    fn signaling(&self) -> String {
        let scheme = if self.secure { "wss" } else { "ws" };
        format!("{scheme}://{}", self.authority)
    }
}

impl FromStr for PublicUrl {
    type Err = InvalidPublicUrl;

    fn from_str(url: &str) -> Result<Self, InvalidPublicUrl> {
        let (secure, rest) = if let Some(rest) = url.strip_prefix("https://") {
            (true, rest)
        } else if let Some(rest) = url.strip_prefix("http://") {
            (false, rest)
        } else {
            return Err(InvalidPublicUrl);
        };
        let authority = rest.strip_suffix('/').unwrap_or(rest);
        if authority.is_empty() || authority.contains(['/', '?', '#', '@']) {
            return Err(InvalidPublicUrl);
        }
        Ok(Self {
            secure,
            authority: authority.to_owned(),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidPublicUrl;

impl fmt::Display for InvalidPublicUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("expected an http:// or https:// origin without a path")
    }
}

impl std::error::Error for InvalidPublicUrl {}

/// Everything the endpoints share. All of it lives in memory for now.
#[derive(Clone, FromRef)]
struct Cloud {
    devices: device::Devices,
    logins: login::Logins,
    tokens: Arc<token::TokenKey>,
    public_url: PublicUrl,
    hosts: signaling::Hosts,
    ping: Ping,
}

/// The REST API and signaling, with fresh state.
pub fn router(config: Config) -> Router {
    let cloud = Cloud {
        devices: Default::default(),
        logins: Default::default(),
        tokens: Arc::new(token::TokenKey::generate()),
        public_url: config.public_url,
        hosts: Default::default(),
        ping: config.ping,
    };
    let api = Router::new()
        .route("/api/v1/device/windows/init", post(device::windows_init))
        .route("/api/v1/qrcode/gen/login", post(login::generate))
        .route("/api/v1/qrcode/login/status", post(login::status))
        .route("/api/v1/login/by_qrcode", post(login::by_qrcode))
        .route("/api/v1/user/info", get(user::info))
        .route("/api/v1/room/create", post(room::create))
        .with_state(cloud.clone());
    let api = match config.sign_key {
        Some(key) => api.layer(from_fn_with_state(
            Arc::new(key),
            signing::require_signature,
        )),
        None => api,
    };
    // Signaling requests carry the room token instead of a signature.
    let signaling = Router::new()
        .route("/socket.io/", get(signaling::connect))
        .with_state(cloud);
    api.merge(signaling)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_url_maps_to_a_signaling_url() {
        let parse = |url: &str| url.parse::<PublicUrl>().map(|url| url.signaling());
        assert_eq!(
            parse("https://remora.example").unwrap(),
            "wss://remora.example"
        );
        assert_eq!(
            parse("http://127.0.0.1:8080/").unwrap(),
            "ws://127.0.0.1:8080"
        );
        assert_eq!(parse("http://[::1]:8080").unwrap(), "ws://[::1]:8080");
    }

    #[test]
    fn public_url_is_a_bare_origin() {
        for url in [
            "remora.example",
            "ftp://remora.example",
            "https://",
            "https://remora.example/prefix",
            "https://remora.example?x",
            "https://user@remora.example",
        ] {
            assert_eq!(url.parse::<PublicUrl>(), Err(InvalidPublicUrl), "{url}");
        }
    }
}
