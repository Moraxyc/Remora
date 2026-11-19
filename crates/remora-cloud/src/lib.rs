//! Self-hostable stand-in for the UU Remote cloud.

mod api;
mod database;
mod device;
mod ice;
mod login;
mod room;
mod rooms;
mod settings;
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
use serde::Deserialize;

pub use database::{Database, DatabaseError};
pub use ice::{Ice, IceError, IceUrl, TurnSecret};
pub use settings::{IceSettings, LoginSettings, Settings, SettingsError};
pub use signaling::{Ping, ReconnectGrace};

pub struct Config {
    /// Where clients reach this server.
    pub public_url: PublicUrl,
    /// With a key, every request must carry a valid `x-param-sign`; without one, signatures
    /// are not checked.
    pub sign_key: Option<SignKey>,
    /// Signaling heartbeat.
    pub ping: Ping,
    /// How long sessions wait for their host to reconnect to signaling.
    pub reconnect_grace: ReconnectGrace,
    /// STUN and TURN servers for sessions.
    pub ice: Ice,
    /// Where the token key, registered devices and user sessions are kept.
    pub database: Database,
}

/// The `http://` or `https://` origin clients reach this server at, such as
/// `https://remora.example`. Clients sign the path they send, so it cannot have a path prefix.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(try_from = "String")]
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

impl TryFrom<String> for PublicUrl {
    type Error = InvalidPublicUrl;

    fn try_from(url: String) -> Result<Self, InvalidPublicUrl> {
        url.parse()
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

/// Everything the endpoints share.
#[derive(Clone, FromRef)]
struct Cloud {
    devices: device::Devices,
    sessions: user::Sessions,
    logins: login::Logins,
    tokens: Arc<token::TokenKey>,
    public_url: PublicUrl,
    rooms: rooms::Rooms,
    ping: Ping,
    reconnect_grace: ReconnectGrace,
    ice: Arc<Ice>,
}

impl FromRef<Cloud> for room::Tickets {
    fn from_ref(cloud: &Cloud) -> Self {
        Self {
            tokens: cloud.tokens.clone(),
            public_url: cloud.public_url.clone(),
        }
    }
}

impl FromRef<Cloud> for signaling::Switchboard {
    fn from_ref(cloud: &Cloud) -> Self {
        Self {
            rooms: cloud.rooms.clone(),
            ping: cloud.ping,
            reconnect_grace: cloud.reconnect_grace,
            ice: cloud.ice.clone(),
        }
    }
}

impl FromRef<Cloud> for signaling::Admission {
    fn from_ref(cloud: &Cloud) -> Self {
        Self {
            tokens: cloud.tokens.clone(),
            devices: cloud.devices.clone(),
        }
    }
}

/// The REST API and signaling.
pub fn router(config: Config) -> Router {
    let cloud = Cloud {
        devices: config.database.clone().into(),
        sessions: config.database.clone().into(),
        logins: Default::default(),
        tokens: config.database.tokens(),
        public_url: config.public_url,
        rooms: Default::default(),
        ping: config.ping,
        reconnect_grace: config.reconnect_grace,
        ice: Arc::new(config.ice),
    };
    let api = Router::new()
        .route("/api/v1/device/windows/init", post(device::windows_init))
        .route("/api/v1/device/list", get(device::list))
        .route("/api/v1/device/{device_id}/unbind", post(device::unbind))
        .route("/api/v1/qrcode/gen/login", post(login::generate))
        .route("/api/v1/qrcode/login/status", post(login::status))
        .route("/api/v1/login/by_qrcode", post(login::by_qrcode))
        .route("/api/v1/user/info", get(user::info))
        .route("/api/v1/user/logout", post(user::logout))
        .route("/api/v1/room/create", post(room::create))
        .route(
            "/api/v1/room/join/by_device/{device_id}",
            post(room::join_by_device),
        )
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
