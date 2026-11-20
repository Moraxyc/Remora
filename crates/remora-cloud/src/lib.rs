//! Self-hostable stand-in for the UU Remote cloud.

mod address;
mod api;
mod app;
mod browser;
mod database;
mod device;
mod files;
mod ice;
mod limit;
mod login;
mod oidc;
mod password;
mod room;
mod rooms;
mod settings;
mod signaling;
mod signing;
mod token;
mod user;
mod wake;
mod web;

use std::fmt;
use std::str::FromStr;
use std::sync::Arc;

use axum::Router;
use axum::extract::{DefaultBodyLimit, FromRef};
use axum::middleware::{from_fn_with_state, map_response};
use axum::routing::{delete, get, post};
use remora_proto::cloud::SignKey;
use serde::Deserialize;

pub use address::{PeerAddress, TrustedProxies, TrustedProxy};
pub use database::{Database, DatabaseError};
pub use ice::{Ice, IceError, IceUrl, TurnSecret};
pub use oidc::{InvalidOidc, OidcProvider, OidcRegistration};
pub use password::{InvalidPasswordHash, OwnerPassword};
pub use settings::{IceSettings, Listen, LoginSettings, Settings, SettingsError};
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

/// Everything the endpoints share.
#[derive(Clone, FromRef)]
struct Cloud {
    devices: device::Devices,
    lan_reports: wake::LanReports,
    files: files::Files,
    sessions: user::Sessions,
    users: user::Users,
    browsers: browser::Browsers,
    logins: login::Logins,
    tokens: Arc<token::TokenKey>,
    public_url: PublicUrl,
    rooms: rooms::Rooms,
    ping: Ping,
    reconnect_grace: ReconnectGrace,
    ice: Arc<Ice>,
    owner_password: Option<Arc<OwnerPassword>>,
    oidc: Option<Arc<OidcProvider>>,
    trusted_proxies: TrustedProxies,
    limits: Arc<limit::Limits>,
}

impl FromRef<Cloud> for room::Tickets {
    fn from_ref(cloud: &Cloud) -> Self {
        Self {
            tokens: cloud.tokens.clone(),
            public_url: cloud.public_url.clone(),
        }
    }
}

impl FromRef<Cloud> for files::FileService {
    fn from_ref(cloud: &Cloud) -> Self {
        Self {
            files: cloud.files.clone(),
            devices: cloud.devices.clone(),
            sessions: cloud.sessions.clone(),
            tokens: cloud.tokens.clone(),
            public_url: cloud.public_url.clone(),
        }
    }
}

impl FromRef<Cloud> for web::Web {
    fn from_ref(cloud: &Cloud) -> Self {
        Self {
            logins: cloud.logins.clone(),
            browsers: cloud.browsers.clone(),
            users: cloud.users.clone(),
            devices: cloud.devices.clone(),
            lan_reports: cloud.lan_reports.clone(),
            files: cloud.files.clone(),
            sessions: cloud.sessions.clone(),
            rooms: cloud.rooms.clone(),
            password: cloud.owner_password.clone(),
            oidc: cloud.oidc.clone(),
            public_url: cloud.public_url.clone(),
            limits: cloud.limits.clone(),
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

/// The REST API, signaling, and the web app with its API. Serve it with connection info
/// (`into_make_service_with_connect_info::<PeerAddress>`), so that limits tell clients apart
/// and login pages can show where a login came from.
pub fn router(config: Config) -> Router {
    let cloud = Cloud {
        devices: config.database.clone().into(),
        lan_reports: config.database.clone().into(),
        files: config.database.clone().into(),
        sessions: config.database.clone().into(),
        users: config.database.clone().into(),
        browsers: config.database.clone().into(),
        logins: login::Logins::new(config.public_url.clone(), config.auto_confirm),
        tokens: config.database.tokens(),
        public_url: config.public_url,
        rooms: Default::default(),
        ping: config.ping,
        reconnect_grace: config.reconnect_grace,
        ice: Arc::new(config.ice),
        owner_password: config.owner_password.map(Arc::new),
        oidc: config.oidc.map(Arc::new),
        trusted_proxies: config.trusted_proxies,
        limits: Default::default(),
    };
    let api = Router::new()
        .route("/api/v1/device/windows/init", post(device::windows_init))
        .route("/api/v1/device/list", get(device::list))
        .route("/api/v1/device/wake_on_lan/info", post(wake::report))
        .route("/api/v1/device/wallpaper", post(files::set_wallpaper))
        .route("/api/v1/tool/fp/token", get(files::token))
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
    let web_api = Router::new()
        .route(
            "/api/web/session",
            get(web::session)
                .post(web::sign_in_with_password)
                .delete(web::sign_out),
        )
        .route("/api/web/oidc/start", get(web::start_oidc))
        .route(oidc::CALLBACK_PATH, get(web::finish_oidc))
        .route(
            "/api/web/logins/{id}",
            get(web::login).post(web::decide_login),
        )
        .route("/api/web/devices", get(web::devices))
        .route("/api/web/devices/{id}", delete(web::unbind_device))
        .route("/api/web/devices/{id}/wake", post(web::wake_device))
        .route("/api/web/client-sessions", get(web::client_sessions))
        .route(
            "/api/web/client-sessions/{id}",
            delete(web::revoke_client_session),
        )
        .route("/api/web/browsers", get(web::browsers))
        .route("/api/web/browsers/{id}", delete(web::revoke_browser))
        .layer(map_response(web::no_store))
        .with_state(cloud.clone());
    // Uploads carry an upload token instead of a signature, and files are fetched without
    // credentials. A form around the file may add a little to its size.
    let files = Router::new()
        .route(
            files::UPLOAD_PATH,
            post(files::upload).layer(DefaultBodyLimit::max(files::MAX_FILE_SIZE + 64 * 1024)),
        )
        .route("/api/files/{id}", get(files::serve))
        .with_state(cloud.clone());
    // Signaling requests carry the room token instead of a signature, and browsers sign
    // nothing.
    let signaling = Router::new()
        .route("/socket.io/", get(signaling::connect))
        .with_state(cloud);
    api.merge(web_api)
        .merge(files)
        .merge(signaling)
        .fallback(app::serve)
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
