//! Self-hostable stand-in for the UU Remote cloud.

mod api;
mod app;
pub mod bus;
mod gateway;
mod relay;
mod room;
mod settings;
mod store;

use std::fmt;
use std::str::FromStr;
use std::sync::Arc;

use axum::Router;
use remora_proto::control::SignKey;
use serde::Deserialize;

pub use api::address::{PeerAddress, TrustedProxies, TrustedProxy};
pub use api::auth::oidc::{InvalidOidc, OidcProvider, OidcRegistration};
pub use api::auth::password::{InvalidPasswordHash, OwnerPassword};
pub use bus::{Bus, BusMessage, LocalBus};
pub use gateway::Ping;
pub use relay::ice::{Ice, IceError, IceUrl, TurnSecret};
pub use room::ReconnectGrace;
pub use settings::{CloudRole, IceSettings, Listen, LoginSettings, Roles, Settings, SettingsError};
pub use store::database::{Database, DatabaseError};

pub struct Config {
    /// Which service roles run in this instance.
    pub roles: Roles,
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
    /// Confirm every QR login as soon as it is generated, instead of asking the owner.
    pub auto_confirm: bool,
    /// What the owner signs in to the web app with. Without it, nobody can sign in as the
    /// owner.
    pub owner_password: Option<OwnerPassword>,
    /// The OpenID Connect provider users sign in with.
    pub oidc: Option<OidcProvider>,
    /// Reverse proxies whose `X-Forwarded-For` names the client.
    pub trusted_proxies: TrustedProxies,
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
    /// Whether clients reach this server over TLS.
    pub fn is_secure(&self) -> bool {
        self.secure
    }

    /// The signaling URL handed to clients: the same origin as `ws://` or `wss://`.
    fn signaling(&self) -> String {
        let scheme = if self.secure { "wss" } else { "ws" };
        format!("{scheme}://{}", self.authority)
    }
}

impl fmt::Display for PublicUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let scheme = if self.secure { "https" } else { "http" };
        write!(f, "{scheme}://{}", self.authority)
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

/// Assembles the cloud with an isolated in-process [`LocalBus`]. Serve it with connection info
/// (`into_make_service_with_connect_info::<PeerAddress>`), so that limits tell clients apart
/// and login pages can show where a login came from.
pub fn router(config: Config) -> Router {
    router_with_bus(config, Arc::new(LocalBus::new()))
}

/// Assembles the cloud over an explicit [`Bus`], allowing multiple role-split instances to
/// communicate over a shared bus.
pub fn router_with_bus(config: Config, bus: Arc<dyn Bus>) -> Router {
    app::build(config, bus)
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
            parse("http://127.0.0.1:8680/").unwrap(),
            "ws://127.0.0.1:8680"
        );
        assert_eq!(parse("http://[::1]:8680").unwrap(), "ws://[::1]:8680");
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
