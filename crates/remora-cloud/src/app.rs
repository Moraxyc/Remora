//! Assembles routers and background role tasks according to the configured [`CloudRole`]s.

use std::sync::Arc;

use axum::Router;
use axum::extract::{DefaultBodyLimit, FromRef};
use axum::middleware::{from_fn, from_fn_with_state, map_response};
use axum::routing::{delete, get, post};

use crate::bus::Bus;
use crate::gateway::presence::Connections;
use crate::room::RoomClient;
use crate::{CloudRole, Config, Ping, PublicUrl, TrustedProxies, api, gateway, relay, room};

/// Everything the HTTP/WebSocket handlers share.
#[derive(Clone, FromRef)]
struct Cloud {
    devices: api::device::Devices,
    lan_reports: api::wake::LanReports,
    files: api::files::Files,
    sessions: api::user::Sessions,
    users: api::user::Users,
    browsers: api::auth::browser::Browsers,
    logins: api::auth::login::Logins,
    tokens: Arc<api::auth::token::TokenKey>,
    public_url: PublicUrl,
    rooms: RoomClient,
    bus: Arc<dyn Bus>,
    connections: Connections,
    ping: Ping,
    owner_password: Option<Arc<api::auth::password::OwnerPassword>>,
    oidc: Option<Arc<api::auth::oidc::OidcProvider>>,
    trusted_proxies: TrustedProxies,
    limits: Arc<api::middleware::limit::Limits>,
}

impl FromRef<Cloud> for room::Tickets {
    fn from_ref(cloud: &Cloud) -> Self {
        Self {
            tokens: cloud.tokens.clone(),
            public_url: cloud.public_url.clone(),
        }
    }
}

impl FromRef<Cloud> for api::files::FileService {
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

impl FromRef<Cloud> for api::web::Web {
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

impl FromRef<Cloud> for gateway::Switchboard {
    fn from_ref(cloud: &Cloud) -> Self {
        Self {
            bus: cloud.bus.clone(),
            connections: cloud.connections.clone(),
            ping: cloud.ping,
        }
    }
}

impl FromRef<Cloud> for gateway::Admission {
    fn from_ref(cloud: &Cloud) -> Self {
        Self {
            tokens: cloud.tokens.clone(),
            devices: cloud.devices.clone(),
        }
    }
}

/// Starts the enabled background role tasks and builds the [`Router`] for enabled HTTP/WS roles.
pub fn build(config: Config, bus: Arc<dyn Bus>) -> Router {
    let roles = config.roles.clone();
    let ice = Arc::new(config.ice);
    let connections = Connections::new();

    if roles.contains(CloudRole::Room) {
        room::registry::spawn(bus.clone(), config.reconnect_grace);
    }
    if roles.contains(CloudRole::Relay) {
        relay::ice::spawn(bus.clone(), ice);
    }
    if roles.contains(CloudRole::Gateway) {
        gateway::presence::spawn_outbound(bus.clone(), connections.clone());
    }

    let cloud = Cloud {
        devices: config.database.clone().into(),
        lan_reports: config.database.clone().into(),
        files: config.database.clone().into(),
        sessions: config.database.clone().into(),
        users: config.database.clone().into(),
        browsers: config.database.clone().into(),
        logins: api::auth::login::Logins::new(config.public_url.clone(), config.auto_confirm),
        tokens: config.database.tokens(),
        public_url: config.public_url,
        rooms: RoomClient::new(bus.clone()),
        bus,
        connections,
        ping: config.ping,
        owner_password: config.owner_password.map(Arc::new),
        oidc: config.oidc.map(Arc::new),
        trusted_proxies: config.trusted_proxies,
        limits: Default::default(),
    };

    let mut router = Router::new();

    if roles.contains(CloudRole::Api) {
        let api_routes = Router::new()
            .route(
                "/api/v1/device/windows/init",
                post(api::device::windows_init),
            )
            .route("/api/v1/device/list", get(api::device::list))
            .route("/api/v1/device/wake_on_lan/info", post(api::wake::report))
            .route("/api/v1/device/wallpaper", post(api::files::set_wallpaper))
            .route("/api/v1/tool/fp/token", get(api::files::token))
            .route(
                "/api/v1/device/{device_id}/unbind",
                post(api::device::unbind),
            )
            .route("/api/v1/qrcode/gen/login", post(api::auth::login::generate))
            .route(
                "/api/v1/qrcode/login/status",
                post(api::auth::login::status),
            )
            .route("/api/v1/login/by_qrcode", post(api::auth::login::by_qrcode))
            .route("/api/v1/user/info", get(api::user::info))
            .route("/api/v1/user/logout", post(api::user::logout))
            .route("/api/v1/room/create", post(room::create))
            .route(
                "/api/v1/room/join/by_device/{device_id}",
                post(room::join_by_device),
            )
            .layer(from_fn(api::middleware::profile::detect_profile))
            .with_state(cloud.clone());
        let api_routes = match config.sign_key {
            Some(key) => api_routes.layer(from_fn_with_state(
                Arc::new(key),
                api::middleware::signing::require_signature,
            )),
            None => api_routes,
        };
        let web_api = Router::new()
            .route(
                "/api/web/session",
                get(api::web::session)
                    .post(api::web::sign_in_with_password)
                    .delete(api::web::sign_out),
            )
            .route("/api/web/oidc/start", get(api::web::start_oidc))
            .route(api::auth::oidc::CALLBACK_PATH, get(api::web::finish_oidc))
            .route(
                "/api/web/logins/{id}",
                get(api::web::login).post(api::web::decide_login),
            )
            .route("/api/web/devices", get(api::web::devices))
            .route("/api/web/devices/{id}", delete(api::web::unbind_device))
            .route("/api/web/devices/{id}/wake", post(api::web::wake_device))
            .route("/api/web/client-sessions", get(api::web::client_sessions))
            .route(
                "/api/web/client-sessions/{id}",
                delete(api::web::revoke_client_session),
            )
            .route("/api/web/browsers", get(api::web::browsers))
            .route("/api/web/browsers/{id}", delete(api::web::revoke_browser))
            .layer(map_response(api::web::no_store))
            .with_state(cloud.clone());
        // Uploads carry an upload token instead of a signature, and files are fetched without
        // credentials. A form around the file may add a little to its size.
        let files = Router::new()
            .route(
                api::files::UPLOAD_PATH,
                post(api::files::upload)
                    .layer(DefaultBodyLimit::max(api::files::MAX_FILE_SIZE + 64 * 1024)),
            )
            .route("/api/files/{id}", get(api::files::serve))
            .with_state(cloud.clone());

        router = router.merge(api_routes).merge(web_api).merge(files);
    }

    if roles.contains(CloudRole::Gateway) {
        // Signaling requests carry the room token instead of a signature, and browsers sign
        // nothing.
        let signaling = Router::new()
            .route("/socket.io/", get(gateway::connect))
            .with_state(cloud);
        router = router.merge(signaling);
    }

    if roles.contains(CloudRole::Api) {
        router = router.fallback(api::web::serve_app);
    }

    router
}
