//! Scriptable fake UU cloud server for testing endpoint implementations in isolation.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Bytes;
use axum::extract::DefaultBodyLimit;
use axum::extract::ws::{Message, WebSocket};
use axum::extract::{Path, Query, State, WebSocketUpgrade};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use futures_util::SinkExt;
use md5::{Digest as _, Md5};
use prost::Message as _;
use remora_proto::control::{
    AssistDeviceRecord, AssistFavoritesRequest, CancelRemoteAssistRequest, JoinShareByCodeRequest,
    JoinShareByConfirmationRequest, ShareConfirmationRequest, ShareControlMode,
    ShareControlModeRequest, SignKey, UploadControlModeRequest, UploadShareSignRequest,
    verify_share_code_digest,
};
use remora_proto::error::ErrorCode;
use remora_proto::server::{EmptyData, error_envelope, ok_envelope, verify_request};
use remora_proto::signal::{
    Decoder, Handshake, Packet, SignalEvent, encode_ack, encode_binary_event, encode_connected,
    encode_event, placeholder,
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::error::LabError;
use crate::scenario::model::FakeCloudSpec;

/// Summary of an HTTP request received by [`FakeCloudInstance`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedHttpCall {
    pub method: String,
    pub path: String,
    pub body: Vec<u8>,
}

#[derive(Clone)]
struct DeviceRecord {
    device_id: String,
    client_id: String,
    name: String,
    platform: u8,
    controllable: bool,
    version_name: String,
    owner: Option<String>,
}

#[derive(Clone)]
struct RoomTicket {
    device_id: String,
    role: FakeRole,
    force_join: bool,
    shared: bool,
}

#[derive(Clone, PartialEq, Eq)]
enum FakeRole {
    Host,
    Viewer,
}

#[derive(Clone)]
struct GuestRecord {
    guest_id: String,
    device_id: String,
    room_created: bool,
}

#[derive(Clone)]
struct ShareRecord {
    connect_id: String,
    active: bool,
}

#[derive(Clone)]
struct SavedShareRecord {
    publisher_device_id: String,
    connect_id: String,
    platform: u8,
    remark: String,
    is_favorite: bool,
    last_connected_at: u64,
    favorited_at: u64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FakeConfirmStatus {
    Pending,
    Approved,
    Denied,
}

struct FakeConfirmation {
    device_id: String,
    connect_id: String,
    status: FakeConfirmStatus,
}

#[derive(Clone)]
struct StoredFile {
    id: String,
    device_id: Option<String>,
    mime: String,
    content: Vec<u8>,
}

#[derive(Default)]
struct CloudState {
    next_id: u64,
    sign_key: Option<SignKey>,
    script: Option<FakeCloudSpec>,
    devices_by_client: HashMap<String, DeviceRecord>,
    devices_by_id: HashMap<String, DeviceRecord>,
    qr_tickets: HashMap<String, String>,
    user_tokens: HashSet<String>,
    device_user_tokens: HashMap<String, Vec<String>>,
    room_tickets: HashMap<String, RoomTicket>,
    hosts_ws: HashMap<String, mpsc::Sender<Vec<WsOut>>>,
    viewers_ws: HashMap<String, mpsc::Sender<Vec<WsOut>>>,
    sessions: HashMap<String, (String, String, bool)>, // ice_id -> (device_id, viewer_conn_id, shared)
    guests: HashMap<String, GuestRecord>,
    guest_tokens: HashMap<String, String>, // token -> guest_id
    shares_by_device: HashMap<String, ShareRecord>,
    connect_id_to_device: HashMap<String, String>,
    saved_shares: Vec<SavedShareRecord>,
    control_mode_waiters: HashMap<String, oneshot::Sender<UploadControlModeRequest>>,
    sign_waiters: HashMap<String, oneshot::Sender<UploadShareSignRequest>>,
    confirmations: HashMap<String, FakeConfirmation>,
    upload_tokens: HashMap<String, (String, Option<String>)>, // token -> (user_token, device_id)
    files: Vec<StoredFile>,
    wallpapers: HashMap<String, String>, // device_id -> file_id
    http_calls: Vec<RecordedHttpCall>,
}

#[derive(Clone)]
enum WsOut {
    Text(String),
    Binary(Vec<u8>),
    Close,
}

#[derive(Clone)]
struct AppState {
    base_url: String,
    ws_url: String,
    inner: Arc<Mutex<CloudState>>,
}

/// In-process fake UU cloud server driven by defaults and optional YAML overrides.
pub struct FakeCloudInstance {
    base_url: String,
    sign_key: Option<SignKey>,
    inner: Arc<Mutex<CloudState>>,
    server_task: JoinHandle<()>,
}

impl Drop for FakeCloudInstance {
    fn drop(&mut self) {
        self.server_task.abort();
    }
}

impl FakeCloudInstance {
    pub async fn spawn(
        sign_key: Option<&str>,
        script: Option<FakeCloudSpec>,
    ) -> Result<Self, LabError> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let base_url = format!("http://{addr}");
        let ws_url = format!("ws://{addr}");
        let parsed_key = sign_key.map(SignKey::new);
        let inner = Arc::new(Mutex::new(CloudState {
            next_id: 1,
            sign_key: parsed_key.clone(),
            script,
            ..Default::default()
        }));

        let state = AppState {
            base_url: base_url.clone(),
            ws_url,
            inner: Arc::clone(&inner),
        };

        let app = Router::new()
            .route("/api/web/session", get(web_session))
            .route("/api/v1/device/windows/init", post(device_init_win))
            .route("/api/v1/device/macos/init", post(device_init_mac))
            .route("/api/v1/device/mac_controllable", post(mac_controllable))
            .route("/api/v1/qrcode/gen/login", post(qr_gen))
            .route("/api/v1/qrcode/login/status", post(qr_status))
            .route("/api/v1/login/by_qrcode", post(login_by_qrcode))
            .route("/api/v1/user/info", get(user_info))
            .route("/api/v1/user/logout", post(user_logout))
            .route("/api/v1/device/list", get(device_list))
            .route("/api/v1/device/groups/of/my", get(device_groups))
            .route(
                "/api/v1/device/{device_id}/preferences",
                get(device_preferences),
            )
            .route("/api/v1/device/share/info", get(device_share_info))
            .route(
                "/api/v1/device/share/info/refresh_id",
                post(device_share_info_refresh),
            )
            .route(
                "/api/v1/device/share/info/refresh",
                post(device_share_info_refresh),
            )
            .route("/api/v1/device/share/cancel", post(device_share_cancel))
            .route(
                "/api/v1/device/share/{id}/unbind",
                post(device_share_unbind),
            )
            .route("/api/v1/device/share/saved", get(device_share_saved))
            .route(
                "/api/v1/device/share/saved/favorites",
                get(device_share_saved_favorites_get).post(device_share_saved_favorites_post),
            )
            .route(
                "/api/v1/device/share/saved/{kind}",
                get(device_share_saved_kind),
            )
            .route(
                "/api/v1/device/share/saved/{kind}/{publisher_device_id}",
                delete(device_share_saved_delete),
            )
            .route("/api/v1/guest/create", post(guest_create))
            .route("/api/v1/guest/room/create", post(guest_room_create))
            .route("/api/v1/guest/share/info", post(guest_share_info))
            .route("/api/v1/guest/share/cancel", post(guest_share_cancel))
            .route(
                "/api/v1/guest/room/share/upload_control_mode",
                post(guest_upload_control_mode),
            )
            .route(
                "/api/v1/guest/room/share/upload/sign",
                post(guest_upload_sign),
            )
            .route(
                "/api/v1/guest/room/share/confirmation",
                post(guest_confirmation),
            )
            .route(
                "/api/v2/room/share/control_mode",
                post(room_share_control_mode),
            )
            .route(
                "/api/v2/room/share/upload_control_mode",
                post(room_share_upload_control_mode),
            )
            .route(
                "/api/v2/room/share/upload_sign",
                post(room_share_upload_sign),
            )
            .route(
                "/api/v2/room/share/confirmation",
                post(room_share_confirmation),
            )
            .route(
                "/api/v1/room/join/share/by_code",
                post(room_join_share_by_code),
            )
            .route(
                "/api/v2/room/join/share/by_code",
                post(room_join_share_by_code),
            )
            .route(
                "/api/v2/room/join/share/by_confirmation",
                post(room_join_share_by_confirmation),
            )
            .route(
                "/api/v2/room/share/cancel_remote_assist",
                post(room_share_cancel_remote_assist),
            )
            .route("/api/v1/device/{device_id}/unbind", post(device_unbind))
            .route("/api/v1/member/entitlements", get(member_entitlements))
            .route("/api/v1/release/fetch/update", get(release_fetch_update))
            .route(
                "/api/v1/release/fetch/update/{device_id}",
                get(release_fetch_update_device),
            )
            .route("/api/v1/tool/fp/token", get(fp_token))
            .route(
                "/api/files",
                post(upload_file).layer(DefaultBodyLimit::max(FAKE_MAX_FILE_SIZE + 1024 * 1024)),
            )
            .route("/api/files/{id}", get(serve_file))
            .route("/api/v1/device/wallpaper", post(set_wallpaper))
            .route("/api/v1/room/create", post(room_create))
            .route("/api/v1/room/join/by_device/{device_id}", post(room_join))
            .route("/socket.io/", get(ws_connect))
            .fallback(fallback_http)
            .with_state(state);

        let server_task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        Ok(Self {
            base_url,
            sign_key: parsed_key,
            inner,
            server_task,
        })
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn sign_key(&self) -> Option<&SignKey> {
        self.sign_key.as_ref()
    }

    pub fn recorded_http_calls(&self) -> Vec<RecordedHttpCall> {
        self.inner
            .lock()
            .expect("fakecloud lock poisoned")
            .http_calls
            .clone()
    }

    pub async fn shutdown(self) {
        self.server_task.abort();
    }
}

fn json_envelope(status: StatusCode, body: Value) -> Response {
    (status, axum::Json(body)).into_response()
}

fn check_http_preconditions(
    state: &AppState,
    method: &Method,
    path_and_query: &str,
    headers: &HeaderMap,
    body: &[u8],
) -> Option<Response> {
    let mut guard = state.inner.lock().expect("fakecloud lock poisoned");
    guard.http_calls.push(RecordedHttpCall {
        method: method.as_str().to_owned(),
        path: path_and_query.to_owned(),
        body: body.to_vec(),
    });

    if let Some(script) = &guard.script {
        let path_only = path_and_query.split('?').next().unwrap_or(path_and_query);
        for rule in &script.http_overrides {
            if rule.method.eq_ignore_ascii_case(method.as_str())
                && (rule.path == path_and_query || rule.path == path_only)
            {
                let status =
                    StatusCode::from_u16(rule.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
                let payload = if rule.code == 0 {
                    json!({
                        "code": 0,
                        "msg": rule.msg.as_deref().unwrap_or("ok"),
                        "data": rule.data.clone().unwrap_or_else(|| json!({}))
                    })
                } else {
                    json!({
                        "code": rule.code,
                        "msg": rule.msg.as_deref().unwrap_or("scripted error"),
                        "data": rule.data.clone()
                    })
                };
                return Some(json_envelope(status, payload));
            }
        }
    }

    if let Some(key) = &guard.sign_key {
        let header_pairs = headers
            .iter()
            .filter_map(|(k, v)| v.to_str().ok().map(|s| (k.as_str(), s)));
        if verify_request(key, method.as_str(), path_and_query, header_pairs, body).is_err() {
            return Some(json_envelope(
                StatusCode::FORBIDDEN,
                serde_json::to_value(error_envelope(
                    ErrorCode::InvalidSignature,
                    "invalid signature",
                ))
                .expect("envelope serializes"),
            ));
        }
    }
    None
}

async fn web_session() -> impl IntoResponse {
    json_envelope(StatusCode::OK, json!({ "signed_in": null }))
}

async fn fallback_http(
    State(state): State<AppState>,
    method: Method,
    uri: axum::http::Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let pq = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    if let Some(resp) = check_http_preconditions(&state, &method, pq, &headers, &body) {
        return resp;
    }
    json_envelope(
        StatusCode::NOT_FOUND,
        serde_json::to_value(error_envelope(ErrorCode::InvalidRequest, "not found"))
            .expect("envelope serializes"),
    )
}

async fn device_init_win(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: axum::http::Uri,
    body: Bytes,
) -> Response {
    device_init_impl(state, headers, uri, body, 1).await
}

async fn device_init_mac(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: axum::http::Uri,
    body: Bytes,
) -> Response {
    device_init_impl(state, headers, uri, body, 4).await
}

async fn device_init_impl(
    state: AppState,
    headers: HeaderMap,
    uri: axum::http::Uri,
    body: Bytes,
    platform: u8,
) -> Response {
    let pq = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    if let Some(resp) = check_http_preconditions(&state, &Method::POST, pq, &headers, &body) {
        return resp;
    }
    let Ok(val) = serde_json::from_slice::<Value>(&body) else {
        return json_envelope(
            StatusCode::UNPROCESSABLE_ENTITY,
            serde_json::to_value(error_envelope(ErrorCode::InvalidRequest, "invalid request"))
                .unwrap(),
        );
    };
    let Some(client_id) = val.get("client_id").and_then(Value::as_str) else {
        return json_envelope(
            StatusCode::UNPROCESSABLE_ENTITY,
            serde_json::to_value(error_envelope(
                ErrorCode::InvalidRequest,
                "missing client_id",
            ))
            .unwrap(),
        );
    };
    if client_id.is_empty() {
        return json_envelope(
            StatusCode::UNPROCESSABLE_ENTITY,
            serde_json::to_value(error_envelope(ErrorCode::InvalidRequest, "empty client_id"))
                .unwrap(),
        );
    }
    let name = val
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let controllable = val.get("controllable").and_then(Value::as_bool);
    let version_name = headers
        .get("x-param-vn")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .unwrap_or_default()
        .to_owned();
    let mut guard = state.inner.lock().expect("fakecloud lock poisoned");
    let rec = if let Some(existing) = guard.devices_by_client.get_mut(client_id) {
        existing.name = name.clone();
        if let Some(c) = controllable {
            existing.controllable = c;
        }
        if !version_name.is_empty() {
            existing.version_name = version_name.clone();
        }
        let cloned = existing.clone();
        if let Some(by_id) = guard.devices_by_id.get_mut(&cloned.device_id) {
            *by_id = cloned.clone();
        }
        cloned
    } else {
        let seq = guard.next_id;
        guard.next_id += 1;
        let id = format!("fake-dev-{seq}");
        let rec = DeviceRecord {
            device_id: id.clone(),
            client_id: client_id.to_owned(),
            name,
            platform,
            controllable: controllable.unwrap_or(true),
            version_name,
            owner: None,
        };
        guard
            .devices_by_client
            .insert(client_id.to_owned(), rec.clone());
        guard.devices_by_id.insert(id, rec.clone());
        rec
    };
    let mut obj = json!({
        "device_id": rec.device_id,
        "platform": rec.platform,
        "status": "DISCONNECTED",
        "participants_info": [],
        "version_name": rec.version_name,
        "support_wol": false,
        "controllable": rec.controllable,
        "publisher_availability_status": "offline",
        "controlled_support": matches!(rec.platform, 1 | 4 | 5),
        "update_started_at": 0,
    });
    if !rec.name.is_empty() {
        obj["alias"] = json!(rec.name);
    }
    json_envelope(
        StatusCode::OK,
        serde_json::to_value(ok_envelope(obj)).unwrap(),
    )
}

async fn qr_gen(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: axum::http::Uri,
    body: Bytes,
) -> Response {
    let pq = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    if let Some(resp) = check_http_preconditions(&state, &Method::POST, pq, &headers, &body) {
        return resp;
    }
    let mut guard = state.inner.lock().expect("fakecloud lock poisoned");
    let seq = guard.next_id;
    guard.next_id += 1;
    let ticket = format!("fake-ticket-{seq}");
    let jump_url = format!("{}/login/fake-{seq}", state.base_url);
    guard.qr_tickets.insert(ticket.clone(), jump_url.clone());
    json_envelope(
        StatusCode::OK,
        serde_json::to_value(ok_envelope(json!({
            "qrcode_jump_url": jump_url,
            "status_query_ticket": ticket,
        })))
        .unwrap(),
    )
}

async fn qr_status(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: axum::http::Uri,
    body: Bytes,
) -> Response {
    let pq = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    if let Some(resp) = check_http_preconditions(&state, &Method::POST, pq, &headers, &body) {
        return resp;
    }
    let Ok(val) = serde_json::from_slice::<Value>(&body) else {
        return json_envelope(
            StatusCode::UNPROCESSABLE_ENTITY,
            serde_json::to_value(error_envelope(ErrorCode::InvalidRequest, "invalid request"))
                .unwrap(),
        );
    };
    let ticket = val.get("token").and_then(Value::as_str).unwrap_or("");
    let url = val
        .get("qrcode_jump_url")
        .and_then(Value::as_str)
        .unwrap_or("");
    let guard = state.inner.lock().expect("fakecloud lock poisoned");
    if guard.qr_tickets.get(ticket).map(String::as_str) != Some(url) {
        return json_envelope(
            StatusCode::BAD_REQUEST,
            serde_json::to_value(error_envelope(ErrorCode::QrCodeExpired, "QR code expired"))
                .unwrap(),
        );
    }
    json_envelope(
        StatusCode::OK,
        serde_json::to_value(ok_envelope(json!({ "status": 4 }))).unwrap(),
    )
}

async fn login_by_qrcode(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: axum::http::Uri,
    body: Bytes,
) -> Response {
    let pq = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    if let Some(resp) = check_http_preconditions(&state, &Method::POST, pq, &headers, &body) {
        return resp;
    }
    let Ok(val) = serde_json::from_slice::<Value>(&body) else {
        return json_envelope(
            StatusCode::UNPROCESSABLE_ENTITY,
            serde_json::to_value(error_envelope(ErrorCode::InvalidRequest, "invalid request"))
                .unwrap(),
        );
    };
    let ticket = val.get("token").and_then(Value::as_str).unwrap_or("");
    let url = val
        .get("qrcode_jump_url")
        .and_then(Value::as_str)
        .unwrap_or("");
    let mut guard = state.inner.lock().expect("fakecloud lock poisoned");
    if guard.qr_tickets.get(ticket).map(String::as_str) != Some(url) {
        return json_envelope(
            StatusCode::BAD_REQUEST,
            serde_json::to_value(error_envelope(ErrorCode::QrCodeExpired, "QR code expired"))
                .unwrap(),
        );
    }
    guard.qr_tickets.remove(ticket);
    let seq = guard.next_id;
    guard.next_id += 1;
    let token = format!("fake-user-jwt-{seq}");
    guard.user_tokens.insert(token.clone());
    json_envelope(
        StatusCode::OK,
        serde_json::to_value(ok_envelope(json!({
            "token": token,
            "user_id": "1",
            "nickname": "Owner"
        })))
        .unwrap(),
    )
}

fn bearer_valid(guard: &CloudState, headers: &HeaderMap) -> Option<String> {
    let token = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))?;
    guard.user_tokens.contains(token).then(|| token.to_owned())
}

fn token_invalid_resp() -> Response {
    json_envelope(
        StatusCode::UNPROCESSABLE_ENTITY,
        serde_json::to_value(error_envelope(ErrorCode::TokenExpired, "login expired")).unwrap(),
    )
}

async fn user_info(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: axum::http::Uri,
) -> Response {
    let pq = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    if let Some(resp) = check_http_preconditions(&state, &Method::GET, pq, &headers, &[]) {
        return resp;
    }
    let guard = state.inner.lock().expect("fakecloud lock poisoned");
    if bearer_valid(&guard, &headers).is_none() {
        return token_invalid_resp();
    }
    json_envelope(
        StatusCode::OK,
        serde_json::to_value(ok_envelope(json!({
            "user_id": "1",
            "nickname": "Owner"
        })))
        .unwrap(),
    )
}

async fn user_logout(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: axum::http::Uri,
    body: Bytes,
) -> Response {
    let pq = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    if let Some(resp) = check_http_preconditions(&state, &Method::POST, pq, &headers, &body) {
        return resp;
    }
    let mut guard = state.inner.lock().expect("fakecloud lock poisoned");
    let Some(token) = bearer_valid(&guard, &headers) else {
        return token_invalid_resp();
    };
    guard.user_tokens.remove(&token);
    json_envelope(
        StatusCode::OK,
        serde_json::to_value(ok_envelope(EmptyData {})).unwrap(),
    )
}

async fn device_list(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: axum::http::Uri,
) -> Response {
    let pq = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    if let Some(resp) = check_http_preconditions(&state, &Method::GET, pq, &headers, &[]) {
        return resp;
    }
    let guard = state.inner.lock().expect("fakecloud lock poisoned");
    if bearer_valid(&guard, &headers).is_none() {
        return token_invalid_resp();
    }
    let caller_id = headers
        .get("x-param-device-id")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty());
    let to_entry = |d: &DeviceRecord| {
        let online = guard.hosts_ws.contains_key(&d.device_id);
        let mut obj = json!({
            "device_id": d.device_id,
            "platform": d.platform,
            "status": if online { "CONNECTED" } else { "DISCONNECTED" },
            "participants_info": [],
            "version_name": d.version_name,
            "support_wol": false,
            "controllable": d.controllable,
            "publisher_availability_status": if matches!(d.platform, 1 | 4 | 5) {
                Value::String(
                    if !online {
                        "offline"
                    } else if !d.controllable {
                        "control_off"
                    } else {
                        "ready"
                    }
                    .to_owned(),
                )
            } else {
                Value::Null
            },
            "controlled_support": matches!(d.platform, 1 | 4 | 5),
            "update_started_at": 0,
        });
        if !d.name.is_empty() {
            obj["alias"] = json!(d.name);
        }
        if let Some(file_id) = guard.wallpapers.get(&d.device_id) {
            obj["wallpaper_url"] = json!(format!("{}/api/files/{file_id}", state.base_url));
        }
        obj
    };
    let current_device = caller_id
        .and_then(|id| guard.devices_by_id.get(id))
        .map(to_entry);
    let mut owned: Vec<_> = guard
        .devices_by_id
        .values()
        .filter(|d| d.owner.as_deref() == Some("1"))
        .map(to_entry)
        .collect();
    owned.sort_by(|a, b| a["device_id"].as_str().cmp(&b["device_id"].as_str()));
    let mut others_shared = Vec::new();
    let mut saved = guard.saved_shares.clone();
    saved.sort_by(|a, b| a.publisher_device_id.cmp(&b.publisher_device_id));
    for rec in saved {
        if let Some(dev) = guard.devices_by_id.get(&rec.publisher_device_id) {
            if dev.owner.as_deref() == Some("1") {
                continue;
            }
            if guard
                .shares_by_device
                .get(&rec.publisher_device_id)
                .is_some_and(|s| s.active)
            {
                others_shared.push(to_entry(dev));
            }
        }
    }
    json_envelope(
        StatusCode::OK,
        serde_json::to_value(ok_envelope(json!({
            "current_device": current_device,
            "my_binded_devices": owned,
            "others_shared_devices": others_shared
        })))
        .unwrap(),
    )
}

async fn device_groups(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: axum::http::Uri,
) -> Response {
    let pq = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    if let Some(resp) = check_http_preconditions(&state, &Method::GET, pq, &headers, &[]) {
        return resp;
    }
    let guard = state.inner.lock().expect("fakecloud lock poisoned");
    if bearer_valid(&guard, &headers).is_none() {
        return token_invalid_resp();
    }
    let caller_id = headers
        .get("x-param-device-id")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
        .filter(|id| guard.devices_by_id.contains_key(*id));
    let mut desktop = Vec::new();
    let mut mobile = Vec::new();
    let mut tv = Vec::new();
    let mut owned: Vec<_> = guard
        .devices_by_id
        .values()
        .filter(|d| d.owner.as_deref() == Some("1"))
        .collect();
    owned.sort_by(|a, b| a.device_id.cmp(&b.device_id));
    for d in owned {
        let online = guard.hosts_ws.contains_key(&d.device_id);
        let mut obj = json!({
            "device_id": d.device_id,
            "platform": d.platform,
            "status": if online { "CONNECTED" } else { "DISCONNECTED" },
            "participants_info": [],
            "version_name": d.version_name,
            "support_wol": false,
            "controllable": d.controllable,
            "publisher_availability_status": if matches!(d.platform, 1 | 4 | 5) {
                Value::String(
                    if !online {
                        "offline"
                    } else if !d.controllable {
                        "control_off"
                    } else {
                        "ready"
                    }
                    .to_owned(),
                )
            } else {
                Value::Null
            },
            "controlled_support": matches!(d.platform, 1 | 4 | 5),
            "update_started_at": 0,
        });
        if !d.name.is_empty() {
            obj["alias"] = json!(d.name);
        }
        if matches!(d.platform, 2 | 3) {
            mobile.push(obj);
        } else if matches!(d.platform, 1 | 4 | 5) {
            desktop.push(obj);
        } else {
            tv.push(obj);
        }
    }
    json_envelope(
        StatusCode::OK,
        serde_json::to_value(ok_envelope(json!({
            "current_device_id": caller_id,
            "mobile_devices": mobile,
            "desktop_devices": desktop,
            "tv_devices": tv,
        })))
        .unwrap(),
    )
}

async fn mac_controllable(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: axum::http::Uri,
    body: Bytes,
) -> Response {
    let pq = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    if let Some(resp) = check_http_preconditions(&state, &Method::POST, pq, &headers, &body) {
        return resp;
    }
    let Ok(val) = serde_json::from_slice::<Value>(&body) else {
        return json_envelope(
            StatusCode::UNPROCESSABLE_ENTITY,
            serde_json::to_value(error_envelope(ErrorCode::InvalidRequest, "invalid request"))
                .unwrap(),
        );
    };
    let Some(controllable) = val.get("controllable").and_then(Value::as_bool) else {
        return json_envelope(
            StatusCode::UNPROCESSABLE_ENTITY,
            serde_json::to_value(error_envelope(ErrorCode::InvalidRequest, "invalid request"))
                .unwrap(),
        );
    };
    let mut guard = state.inner.lock().expect("fakecloud lock poisoned");
    if bearer_valid(&guard, &headers).is_none() {
        return token_invalid_resp();
    }
    let Some(device_id) = headers
        .get("x-param-device-id")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
    else {
        return json_envelope(
            StatusCode::BAD_REQUEST,
            serde_json::to_value(error_envelope(ErrorCode::UnknownDevice, "no such device"))
                .unwrap(),
        );
    };
    let Some(dev) = guard.devices_by_id.get_mut(&device_id) else {
        return json_envelope(
            StatusCode::BAD_REQUEST,
            serde_json::to_value(error_envelope(ErrorCode::UnknownDevice, "no such device"))
                .unwrap(),
        );
    };
    dev.controllable = controllable;
    let cid = dev.client_id.clone();
    if let Some(by_client) = guard.devices_by_client.get_mut(&cid) {
        by_client.controllable = controllable;
    }
    json_envelope(
        StatusCode::OK,
        serde_json::to_value(ok_envelope(EmptyData {})).unwrap(),
    )
}

async fn device_preferences(
    State(state): State<AppState>,
    Path(device_id): Path<String>,
    headers: HeaderMap,
    uri: axum::http::Uri,
) -> Response {
    let pq = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    if let Some(resp) = check_http_preconditions(&state, &Method::GET, pq, &headers, &[]) {
        return resp;
    }
    let guard = state.inner.lock().expect("fakecloud lock poisoned");
    if bearer_valid(&guard, &headers).is_none() {
        return token_invalid_resp();
    }
    if guard
        .devices_by_id
        .get(&device_id)
        .is_none_or(|d| d.owner.as_deref() != Some("1"))
    {
        return json_envelope(
            StatusCode::BAD_REQUEST,
            serde_json::to_value(error_envelope(ErrorCode::UnknownDevice, "no such device"))
                .unwrap(),
        );
    }
    json_envelope(
        StatusCode::OK,
        serde_json::to_value(ok_envelope(json!({
            "device_preferences": {
                "device_id": device_id,
                "tools": {
                    "sequence": [
                        "file_transfer",
                        "view_mode",
                        "remote_connection_cli",
                        "port_mapping",
                        "wake_on_lan",
                        "device_reboot",
                        "device_shutdown",
                    ]
                }
            }
        })))
        .unwrap(),
    )
}

async fn member_entitlements(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: axum::http::Uri,
) -> Response {
    let pq = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    if let Some(resp) = check_http_preconditions(&state, &Method::GET, pq, &headers, &[]) {
        return resp;
    }
    let guard = state.inner.lock().expect("fakecloud lock poisoned");
    if bearer_valid(&guard, &headers).is_none() {
        return token_invalid_resp();
    }
    json_envelope(
        StatusCode::OK,
        serde_json::to_value(ok_envelope(json!({
            "version": 0,
            "user_id": "1",
            "features": {}
        })))
        .unwrap(),
    )
}

fn connect_id_for(device_id: &str) -> String {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in device_id.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{:09}", 100_000_000 + (hash % 900_000_000))
}

async fn device_share_info(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: axum::http::Uri,
) -> Response {
    let pq = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    if let Some(resp) = check_http_preconditions(&state, &Method::GET, pq, &headers, &[]) {
        return resp;
    }
    let mut guard = state.inner.lock().expect("fakecloud lock poisoned");
    if bearer_valid(&guard, &headers).is_none() {
        return token_invalid_resp();
    }
    let Some(device_id) = headers
        .get("x-param-device-id")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
    else {
        return json_envelope(
            StatusCode::BAD_REQUEST,
            serde_json::to_value(error_envelope(ErrorCode::UnknownDevice, "no such device"))
                .unwrap(),
        );
    };
    let Some(dev) = guard.devices_by_id.get(&device_id).cloned() else {
        return json_envelope(
            StatusCode::BAD_REQUEST,
            serde_json::to_value(error_envelope(ErrorCode::UnknownDevice, "no such device"))
                .unwrap(),
        );
    };
    let connect_id = ensure_fake_share(&mut guard, &dev.device_id, true);
    let alias = if dev.name.is_empty() {
        dev.device_id
    } else {
        dev.name
    };
    json_envelope(
        StatusCode::OK,
        serde_json::to_value(ok_envelope(json!({
            "connect_id": connect_id,
            "connect_code": "",
            "alias": alias,
        })))
        .unwrap(),
    )
}

fn ensure_fake_share(guard: &mut CloudState, device_id: &str, active: bool) -> String {
    if let Some(existing) = guard.shares_by_device.get_mut(device_id) {
        if active {
            existing.active = true;
        }
        return existing.connect_id.clone();
    }
    let cid = connect_id_for(device_id);
    guard.shares_by_device.insert(
        device_id.to_owned(),
        ShareRecord {
            connect_id: cid.clone(),
            active,
        },
    );
    guard
        .connect_id_to_device
        .insert(cid.clone(), device_id.to_owned());
    cid
}

fn guest_from_headers(guard: &CloudState, headers: &HeaderMap) -> Option<GuestRecord> {
    let token = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))?;
    let guest_id = guard.guest_tokens.get(token)?;
    if let Some(hdr_guest) = headers
        .get("x-param-guest-id")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
        && hdr_guest != guest_id
    {
        return None;
    }
    guard.guests.get(guest_id).cloned()
}

fn fake_error(status: StatusCode, code: ErrorCode, msg: &str) -> Response {
    json_envelope(
        status,
        serde_json::to_value(error_envelope(code, msg)).unwrap(),
    )
}

async fn guest_create(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: axum::http::Uri,
    body: Bytes,
) -> Response {
    let pq = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    if let Some(resp) = check_http_preconditions(&state, &Method::POST, pq, &headers, &body) {
        return resp;
    }
    let mut guard = state.inner.lock().expect("fakecloud lock poisoned");
    let Some(device_id) = headers
        .get("x-param-device-id")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
    else {
        return fake_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            ErrorCode::InvalidRequest,
            "invalid request",
        );
    };
    if !guard.devices_by_id.contains_key(&device_id) {
        return fake_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            ErrorCode::InvalidRequest,
            "invalid request",
        );
    }
    let seq = guard.next_id;
    guard.next_id += 1;
    let guest_id = format!("fake-guest-{seq}");
    let token = format!("fake-guest-jwt-{seq}");
    guard.guests.insert(
        guest_id.clone(),
        GuestRecord {
            guest_id: guest_id.clone(),
            device_id,
            room_created: false,
        },
    );
    guard.guest_tokens.insert(token.clone(), guest_id.clone());
    json_envelope(
        StatusCode::OK,
        serde_json::to_value(ok_envelope(json!({
            "guest_id": guest_id,
            "token": token,
        })))
        .unwrap(),
    )
}

async fn guest_room_create(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: axum::http::Uri,
    body: Bytes,
) -> Response {
    let pq = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    if let Some(resp) = check_http_preconditions(&state, &Method::POST, pq, &headers, &body) {
        return resp;
    }
    let mut guard = state.inner.lock().expect("fakecloud lock poisoned");
    let Some(guest) = guest_from_headers(&guard, &headers) else {
        return token_invalid_resp();
    };
    if let Some(rec) = guard.guests.get_mut(&guest.guest_id) {
        rec.room_created = true;
    }
    let seq = guard.next_id;
    guard.next_id += 1;
    let ticket = format!("fake-guest-room-{seq}");
    guard.room_tickets.insert(
        ticket.clone(),
        RoomTicket {
            device_id: guest.device_id,
            role: FakeRole::Host,
            force_join: false,
            shared: false,
        },
    );
    json_envelope(
        StatusCode::OK,
        serde_json::to_value(ok_envelope(json!({
            "signaling_server": state.ws_url,
            "signaling_list": [state.ws_url],
            "sni_required_signaling_list": [],
            "streamer_config": "{}",
            "token": ticket,
            "ws_connect_timeout_ms": 16_000,
            "max_reconnect_delta": 300,
            "streamer_retry_delta_ms": 1_000,
            "international_connect": false,
        })))
        .unwrap(),
    )
}

async fn guest_share_info(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: axum::http::Uri,
    body: Bytes,
) -> Response {
    let pq = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    if let Some(resp) = check_http_preconditions(&state, &Method::POST, pq, &headers, &body) {
        return resp;
    }
    let mut guard = state.inner.lock().expect("fakecloud lock poisoned");
    let Some(guest) = guest_from_headers(&guard, &headers) else {
        return token_invalid_resp();
    };
    if !guest.room_created {
        return fake_error(
            StatusCode::BAD_REQUEST,
            ErrorCode::GuestRoomRequired,
            "guest room required",
        );
    }
    let Some(dev) = guard.devices_by_id.get(&guest.device_id).cloned() else {
        return fake_error(
            StatusCode::BAD_REQUEST,
            ErrorCode::UnknownDevice,
            "no such device",
        );
    };
    let connect_id = ensure_fake_share(&mut guard, &dev.device_id, true);
    let alias = if dev.name.is_empty() {
        dev.device_id
    } else {
        dev.name
    };
    json_envelope(
        StatusCode::OK,
        serde_json::to_value(ok_envelope(json!({
            "connect_id": connect_id,
            "connect_code": "",
            "alias": alias,
        })))
        .unwrap(),
    )
}

async fn kick_shared_for_device(state: &AppState, device_id: &str) {
    struct KickTarget {
        ice_id: String,
        viewer_tx: mpsc::Sender<Vec<WsOut>>,
        host_tx: Option<mpsc::Sender<Vec<WsOut>>>,
    }
    let to_kick: Vec<KickTarget> = {
        let mut guard = state.inner.lock().expect("fakecloud lock poisoned");
        let host_tx = guard.hosts_ws.get(device_id).cloned();
        let mut removed = Vec::new();
        guard.sessions.retain(|ice_id, (dev, viewer_id, shared)| {
            if dev == device_id && *shared {
                removed.push((ice_id.clone(), viewer_id.clone()));
                false
            } else {
                true
            }
        });
        removed
            .into_iter()
            .filter_map(|(ice_id, viewer_id)| {
                guard
                    .viewers_ws
                    .get(&viewer_id)
                    .cloned()
                    .map(|viewer_tx| KickTarget {
                        ice_id,
                        viewer_tx,
                        host_tx: host_tx.clone(),
                    })
            })
            .collect()
    };
    for KickTarget {
        ice_id,
        viewer_tx,
        host_tx,
    } in to_kick
    {
        let left = encode_event(
            "left",
            &[json!({ "ice_id": ice_id, "client_id": "fake-host" })],
            None,
        );
        let _ = viewer_tx.send(vec![WsOut::Text(left)]).await;
        if let Some(h_tx) = host_tx {
            let released = encode_event(
                "released",
                &[json!({ "ice_id": ice_id, "client_id": "fake-viewer" })],
                None,
            );
            let _ = h_tx.send(vec![WsOut::Text(released)]).await;
        }
    }
}

async fn guest_share_cancel(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: axum::http::Uri,
    body: Bytes,
) -> Response {
    let pq = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    if let Some(resp) = check_http_preconditions(&state, &Method::POST, pq, &headers, &body) {
        return resp;
    }
    let device_id = {
        let mut guard = state.inner.lock().expect("fakecloud lock poisoned");
        let Some(guest) = guest_from_headers(&guard, &headers) else {
            return token_invalid_resp();
        };
        if let Some(share) = guard.shares_by_device.get_mut(&guest.device_id) {
            share.active = false;
        }
        guard
            .confirmations
            .retain(|_, c| c.device_id != guest.device_id);
        guest.device_id
    };
    kick_shared_for_device(&state, &device_id).await;
    json_envelope(
        StatusCode::OK,
        serde_json::to_value(ok_envelope(EmptyData {})).unwrap(),
    )
}

async fn guest_upload_control_mode(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: axum::http::Uri,
    body: Bytes,
) -> Response {
    let pq = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    if let Some(resp) = check_http_preconditions(&state, &Method::POST, pq, &headers, &body) {
        return resp;
    }
    let Ok(req) = serde_json::from_slice::<UploadControlModeRequest>(&body) else {
        return fake_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            ErrorCode::InvalidRequest,
            "invalid request",
        );
    };
    let mut guard = state.inner.lock().expect("fakecloud lock poisoned");
    if guest_from_headers(&guard, &headers).is_none() {
        return token_invalid_resp();
    }
    if let Some(tx) = guard.control_mode_waiters.remove(&req.control_id) {
        let _ = tx.send(req);
    }
    json_envelope(
        StatusCode::OK,
        serde_json::to_value(ok_envelope(EmptyData {})).unwrap(),
    )
}

async fn guest_upload_sign(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: axum::http::Uri,
    body: Bytes,
) -> Response {
    let pq = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    if let Some(resp) = check_http_preconditions(&state, &Method::POST, pq, &headers, &body) {
        return resp;
    }
    let Ok(req) = serde_json::from_slice::<UploadShareSignRequest>(&body) else {
        return fake_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            ErrorCode::InvalidRequest,
            "invalid request",
        );
    };
    let mut guard = state.inner.lock().expect("fakecloud lock poisoned");
    if guest_from_headers(&guard, &headers).is_none() {
        return token_invalid_resp();
    }
    if let Some(tx) = guard.sign_waiters.remove(&req.control_id) {
        let _ = tx.send(req);
    }
    json_envelope(
        StatusCode::OK,
        serde_json::to_value(ok_envelope(EmptyData {})).unwrap(),
    )
}

async fn guest_confirmation(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: axum::http::Uri,
    body: Bytes,
) -> Response {
    let pq = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    if let Some(resp) = check_http_preconditions(&state, &Method::POST, pq, &headers, &body) {
        return resp;
    }
    let Ok(req) = serde_json::from_slice::<ShareConfirmationRequest>(&body) else {
        return fake_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            ErrorCode::InvalidRequest,
            "invalid request",
        );
    };
    let mut guard = state.inner.lock().expect("fakecloud lock poisoned");
    let Some(guest) = guest_from_headers(&guard, &headers) else {
        return token_invalid_resp();
    };
    if let Some(conf) = guard.confirmations.get_mut(&req.control_id)
        && conf.device_id == guest.device_id
    {
        conf.status = if req.allow {
            FakeConfirmStatus::Approved
        } else {
            FakeConfirmStatus::Denied
        };
    }
    json_envelope(
        StatusCode::OK,
        serde_json::to_value(ok_envelope(EmptyData {})).unwrap(),
    )
}

async fn device_share_info_refresh(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: axum::http::Uri,
    body: Bytes,
) -> Response {
    let pq = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    if let Some(resp) = check_http_preconditions(&state, &Method::POST, pq, &headers, &body) {
        return resp;
    }
    let mut guard = state.inner.lock().expect("fakecloud lock poisoned");
    if bearer_valid(&guard, &headers).is_none() {
        return token_invalid_resp();
    }
    let Some(device_id) = headers
        .get("x-param-device-id")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
    else {
        return fake_error(
            StatusCode::BAD_REQUEST,
            ErrorCode::UnknownDevice,
            "no such device",
        );
    };
    let Some(dev) = guard.devices_by_id.get(&device_id).cloned() else {
        return fake_error(
            StatusCode::BAD_REQUEST,
            ErrorCode::UnknownDevice,
            "no such device",
        );
    };
    let seq = guard.next_id;
    guard.next_id += 1;
    let new_cid = format!("{:09}", 100_000_000 + ((seq * 7919) % 900_000_000));
    if let Some(old) = guard.shares_by_device.insert(
        device_id.clone(),
        ShareRecord {
            connect_id: new_cid.clone(),
            active: true,
        },
    ) {
        guard.connect_id_to_device.remove(&old.connect_id);
    }
    guard
        .connect_id_to_device
        .insert(new_cid.clone(), device_id);
    let alias = if dev.name.is_empty() {
        dev.device_id
    } else {
        dev.name
    };
    json_envelope(
        StatusCode::OK,
        serde_json::to_value(ok_envelope(json!({
            "connect_id": new_cid,
            "connect_code": "",
            "alias": alias,
        })))
        .unwrap(),
    )
}

async fn device_share_cancel(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: axum::http::Uri,
    body: Bytes,
) -> Response {
    let pq = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    if let Some(resp) = check_http_preconditions(&state, &Method::POST, pq, &headers, &body) {
        return resp;
    }
    let device_id = {
        let mut guard = state.inner.lock().expect("fakecloud lock poisoned");
        if bearer_valid(&guard, &headers).is_none() {
            return token_invalid_resp();
        }
        let Some(device_id) = headers
            .get("x-param-device-id")
            .and_then(|v| v.to_str().ok())
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
        else {
            return fake_error(
                StatusCode::BAD_REQUEST,
                ErrorCode::UnknownDevice,
                "no such device",
            );
        };
        if let Some(share) = guard.shares_by_device.get_mut(&device_id) {
            share.active = false;
        }
        guard.confirmations.retain(|_, c| c.device_id != device_id);
        device_id
    };
    kick_shared_for_device(&state, &device_id).await;
    json_envelope(
        StatusCode::OK,
        serde_json::to_value(ok_envelope(EmptyData {})).unwrap(),
    )
}

async fn device_share_unbind(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    uri: axum::http::Uri,
    body: Bytes,
) -> Response {
    let pq = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    if let Some(resp) = check_http_preconditions(&state, &Method::POST, pq, &headers, &body) {
        return resp;
    }
    let mut guard = state.inner.lock().expect("fakecloud lock poisoned");
    if bearer_valid(&guard, &headers).is_none() {
        return token_invalid_resp();
    }
    let before = guard.saved_shares.len();
    guard
        .saved_shares
        .retain(|r| r.publisher_device_id != id && r.connect_id != id);
    if guard.saved_shares.len() == before {
        return fake_error(
            StatusCode::BAD_REQUEST,
            ErrorCode::UnknownDevice,
            "no such device",
        );
    }
    json_envelope(
        StatusCode::OK,
        serde_json::to_value(ok_envelope(EmptyData {})).unwrap(),
    )
}

#[derive(Deserialize, Default)]
struct SavedQuery {
    #[serde(default)]
    cursor: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

fn saved_page_response(
    guard: &CloudState,
    filter_fav: Option<bool>,
    query: SavedQuery,
) -> Response {
    let offset = query
        .cursor
        .as_deref()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let limit = query.limit.unwrap_or(50).clamp(1, 200);
    let mut items: Vec<AssistDeviceRecord> = guard
        .saved_shares
        .iter()
        .filter(|r| match filter_fav {
            Some(true) => r.is_favorite,
            Some(false) => r.last_connected_at > 0,
            None => true,
        })
        .map(|r| AssistDeviceRecord {
            publisher_device_id: r.publisher_device_id.clone(),
            connect_id: r.connect_id.clone(),
            platform: r.platform,
            remark: r.remark.clone(),
            is_favorite: r.is_favorite,
            last_connected_at: r.last_connected_at,
            favorited_at: r.favorited_at,
            extra: Default::default(),
        })
        .collect();
    items.sort_by(|a, b| a.publisher_device_id.cmp(&b.publisher_device_id));
    let total = items.len();
    let end = (offset + limit).min(total);
    let slice = if offset < total {
        items[offset..end].to_vec()
    } else {
        Vec::new()
    };
    let next = (end < total).then(|| end.to_string());
    json_envelope(
        StatusCode::OK,
        serde_json::to_value(ok_envelope(json!({ "saved": slice, "next": next }))).unwrap(),
    )
}

async fn device_share_saved(
    State(state): State<AppState>,
    Query(query): Query<SavedQuery>,
    headers: HeaderMap,
    uri: axum::http::Uri,
) -> Response {
    let pq = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    if let Some(resp) = check_http_preconditions(&state, &Method::GET, pq, &headers, &[]) {
        return resp;
    }
    let guard = state.inner.lock().expect("fakecloud lock poisoned");
    if bearer_valid(&guard, &headers).is_none() {
        return token_invalid_resp();
    }
    saved_page_response(&guard, None, query)
}

async fn device_share_saved_favorites_get(
    State(state): State<AppState>,
    Query(query): Query<SavedQuery>,
    headers: HeaderMap,
    uri: axum::http::Uri,
) -> Response {
    let pq = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    if let Some(resp) = check_http_preconditions(&state, &Method::GET, pq, &headers, &[]) {
        return resp;
    }
    let guard = state.inner.lock().expect("fakecloud lock poisoned");
    if bearer_valid(&guard, &headers).is_none() {
        return token_invalid_resp();
    }
    saved_page_response(&guard, Some(true), query)
}

async fn device_share_saved_kind(
    State(state): State<AppState>,
    Path(kind): Path<String>,
    Query(query): Query<SavedQuery>,
    headers: HeaderMap,
    uri: axum::http::Uri,
) -> Response {
    let pq = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    if let Some(resp) = check_http_preconditions(&state, &Method::GET, pq, &headers, &[]) {
        return resp;
    }
    let guard = state.inner.lock().expect("fakecloud lock poisoned");
    if bearer_valid(&guard, &headers).is_none() {
        return token_invalid_resp();
    }
    let filter_fav = match kind.as_str() {
        "favorites" => Some(true),
        "recent" => Some(false),
        _ => {
            return fake_error(
                StatusCode::UNPROCESSABLE_ENTITY,
                ErrorCode::InvalidRequest,
                "invalid request",
            );
        }
    };
    saved_page_response(&guard, filter_fav, query)
}

async fn device_share_saved_favorites_post(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: axum::http::Uri,
    body: Bytes,
) -> Response {
    let pq = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    if let Some(resp) = check_http_preconditions(&state, &Method::POST, pq, &headers, &body) {
        return resp;
    }
    let Ok(req) = serde_json::from_slice::<AssistFavoritesRequest>(&body) else {
        return fake_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            ErrorCode::InvalidRequest,
            "invalid request",
        );
    };
    let mut guard = state.inner.lock().expect("fakecloud lock poisoned");
    if bearer_valid(&guard, &headers).is_none() {
        return token_invalid_resp();
    }
    for fav in req.favorites {
        let Some(dev_id) = guard.connect_id_to_device.get(&fav.connect_id).cloned() else {
            if !req.skip_invalid {
                return fake_error(
                    StatusCode::BAD_REQUEST,
                    ErrorCode::IncorrectShareCode,
                    "invalid code",
                );
            }
            continue;
        };
        let Some(dev) = guard.devices_by_id.get(&dev_id).cloned() else {
            continue;
        };
        if dev.owner.as_deref() == Some("1") {
            return fake_error(
                StatusCode::BAD_REQUEST,
                ErrorCode::SameAccountShareFavorite,
                "same account",
            );
        }
        if let Some(existing) = guard
            .saved_shares
            .iter_mut()
            .find(|r| r.publisher_device_id == dev_id)
        {
            existing.is_favorite = true;
            existing.remark = fav.remark;
            existing.favorited_at = fav.favorited_at.max(1);
        } else {
            guard.saved_shares.push(SavedShareRecord {
                publisher_device_id: dev_id,
                connect_id: fav.connect_id,
                platform: dev.platform,
                remark: fav.remark,
                is_favorite: true,
                last_connected_at: 0,
                favorited_at: fav.favorited_at.max(1),
            });
        }
    }
    json_envelope(
        StatusCode::OK,
        serde_json::to_value(ok_envelope(EmptyData {})).unwrap(),
    )
}

async fn device_share_saved_delete(
    State(state): State<AppState>,
    Path((kind, publisher_device_id)): Path<(String, String)>,
    headers: HeaderMap,
    uri: axum::http::Uri,
) -> Response {
    let pq = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    if let Some(resp) = check_http_preconditions(&state, &Method::DELETE, pq, &headers, &[]) {
        return resp;
    }
    let mut guard = state.inner.lock().expect("fakecloud lock poisoned");
    if bearer_valid(&guard, &headers).is_none() {
        return token_invalid_resp();
    }
    match kind.as_str() {
        "favorites" => {
            for r in &mut guard.saved_shares {
                if r.publisher_device_id == publisher_device_id {
                    r.is_favorite = false;
                }
            }
        }
        "recent" => {
            for r in &mut guard.saved_shares {
                if r.publisher_device_id == publisher_device_id {
                    r.last_connected_at = 0;
                }
            }
        }
        _ => {
            return fake_error(
                StatusCode::UNPROCESSABLE_ENTITY,
                ErrorCode::InvalidRequest,
                "invalid request",
            );
        }
    }
    guard
        .saved_shares
        .retain(|r| r.is_favorite || r.last_connected_at > 0);
    json_envelope(
        StatusCode::OK,
        serde_json::to_value(ok_envelope(EmptyData {})).unwrap(),
    )
}

async fn room_share_upload_control_mode(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: axum::http::Uri,
    body: Bytes,
) -> Response {
    let pq = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    if let Some(resp) = check_http_preconditions(&state, &Method::POST, pq, &headers, &body) {
        return resp;
    }
    let Ok(req) = serde_json::from_slice::<UploadControlModeRequest>(&body) else {
        return fake_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            ErrorCode::InvalidRequest,
            "invalid request",
        );
    };
    let mut guard = state.inner.lock().expect("fakecloud lock poisoned");
    if bearer_valid(&guard, &headers).is_none() {
        return token_invalid_resp();
    }
    if let Some(tx) = guard.control_mode_waiters.remove(&req.control_id) {
        let _ = tx.send(req);
    }
    json_envelope(
        StatusCode::OK,
        serde_json::to_value(ok_envelope(EmptyData {})).unwrap(),
    )
}

async fn room_share_upload_sign(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: axum::http::Uri,
    body: Bytes,
) -> Response {
    let pq = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    if let Some(resp) = check_http_preconditions(&state, &Method::POST, pq, &headers, &body) {
        return resp;
    }
    let Ok(req) = serde_json::from_slice::<UploadShareSignRequest>(&body) else {
        return fake_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            ErrorCode::InvalidRequest,
            "invalid request",
        );
    };
    let mut guard = state.inner.lock().expect("fakecloud lock poisoned");
    if bearer_valid(&guard, &headers).is_none() {
        return token_invalid_resp();
    }
    if let Some(tx) = guard.sign_waiters.remove(&req.control_id) {
        let _ = tx.send(req);
    }
    json_envelope(
        StatusCode::OK,
        serde_json::to_value(ok_envelope(EmptyData {})).unwrap(),
    )
}

async fn room_share_confirmation(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: axum::http::Uri,
    body: Bytes,
) -> Response {
    let pq = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    if let Some(resp) = check_http_preconditions(&state, &Method::POST, pq, &headers, &body) {
        return resp;
    }
    let Ok(req) = serde_json::from_slice::<ShareConfirmationRequest>(&body) else {
        return fake_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            ErrorCode::InvalidRequest,
            "invalid request",
        );
    };
    let mut guard = state.inner.lock().expect("fakecloud lock poisoned");
    if bearer_valid(&guard, &headers).is_none() {
        return token_invalid_resp();
    }
    if let Some(conf) = guard.confirmations.get_mut(&req.control_id) {
        conf.status = if req.allow {
            FakeConfirmStatus::Approved
        } else {
            FakeConfirmStatus::Denied
        };
    }
    json_envelope(
        StatusCode::OK,
        serde_json::to_value(ok_envelope(EmptyData {})).unwrap(),
    )
}

fn resolve_fake_share_target(
    guard: &CloudState,
    connect_id: &str,
) -> Result<(DeviceRecord, mpsc::Sender<Vec<WsOut>>), Box<Response>> {
    let Some(dev_id) = guard.connect_id_to_device.get(connect_id) else {
        return Err(Box::new(fake_error(
            StatusCode::BAD_REQUEST,
            ErrorCode::IncorrectShareCode,
            "incorrect code",
        )));
    };
    let Some(dev) = guard.devices_by_id.get(dev_id).cloned() else {
        return Err(Box::new(fake_error(
            StatusCode::BAD_REQUEST,
            ErrorCode::IncorrectShareCode,
            "incorrect code",
        )));
    };
    if dev.owner.as_deref() == Some("1") {
        return Err(Box::new(fake_error(
            StatusCode::BAD_REQUEST,
            ErrorCode::CannotInitiateRemoteConnection,
            "same account",
        )));
    }
    if !guard.shares_by_device.get(dev_id).is_some_and(|s| s.active) {
        return Err(Box::new(fake_error(
            StatusCode::BAD_REQUEST,
            ErrorCode::CannotInitiateRemoteConnection,
            "assist disabled",
        )));
    }
    let Some(host_tx) = guard.hosts_ws.get(dev_id).cloned() else {
        return Err(Box::new(fake_error(
            StatusCode::BAD_REQUEST,
            ErrorCode::DeviceOffline,
            "device offline",
        )));
    };
    Ok((dev, host_tx))
}

fn build_fake_assist_room(
    state: &AppState,
    guard: &mut CloudState,
    dev: &DeviceRecord,
    connect_id: &str,
    force_join: bool,
) -> Value {
    let seq = guard.next_id;
    guard.next_id += 1;
    let ticket = format!("fake-assist-viewer-{seq}");
    guard.room_tickets.insert(
        ticket.clone(),
        RoomTicket {
            device_id: dev.device_id.clone(),
            role: FakeRole::Viewer,
            force_join,
            shared: true,
        },
    );
    if let Some(existing) = guard
        .saved_shares
        .iter_mut()
        .find(|r| r.publisher_device_id == dev.device_id)
    {
        existing.connect_id = connect_id.to_owned();
        existing.last_connected_at = seq;
    } else {
        guard.saved_shares.push(SavedShareRecord {
            publisher_device_id: dev.device_id.clone(),
            connect_id: connect_id.to_owned(),
            platform: dev.platform,
            remark: dev.name.clone(),
            is_favorite: false,
            last_connected_at: seq,
            favorited_at: 0,
        });
    }
    json!({
        "signaling_server": state.ws_url,
        "signaling_list": [state.ws_url],
        "sni_required_signaling_list": [],
        "token": ticket,
        "ws_connect_timeout_ms": 16_000,
        "streamer_retry_delta_ms": 1_000,
        "max_reconnect_delta": 300,
        "report_token": "",
        "report_url": "",
        "report_server_address": "",
        "share_id": connect_id,
        "device_name": dev.name,
        "publisher_version_name": dev.version_name,
        "publisher_platform": dev.platform,
        "publisher_device_id": dev.device_id,
        "international_connect": false,
        "device_binding": if dev.owner.is_some() { "user_device" } else { "" },
        "streamer_config": "{}",
    })
}

async fn room_share_control_mode(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: axum::http::Uri,
    body: Bytes,
) -> Response {
    let pq = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    if let Some(resp) = check_http_preconditions(&state, &Method::POST, pq, &headers, &body) {
        return resp;
    }
    let Ok(req) = serde_json::from_slice::<ShareControlModeRequest>(&body) else {
        return fake_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            ErrorCode::InvalidRequest,
            "invalid request",
        );
    };
    let (dev, host_tx, control_id, rx) = {
        let mut guard = state.inner.lock().expect("fakecloud lock poisoned");
        if bearer_valid(&guard, &headers).is_none() {
            return token_invalid_resp();
        }
        let (dev, host_tx) = match resolve_fake_share_target(&guard, &req.connect_id) {
            Ok(ok) => ok,
            Err(resp) => return *resp,
        };
        let seq = guard.next_id;
        guard.next_id += 1;
        let control_id = format!("fake-ctrl-mode-{seq}");
        let (tx, rx) = oneshot::channel();
        guard.control_mode_waiters.insert(control_id.clone(), tx);
        (dev, host_tx, control_id, rx)
    };

    let push = encode_event(
        "bmsg_push",
        &[json!({ "type": "get_control_mode", "data": { "control_id": control_id } })],
        None,
    );
    if host_tx.send(vec![WsOut::Text(push)]).await.is_err() {
        return fake_error(
            StatusCode::BAD_REQUEST,
            ErrorCode::DeviceOffline,
            "device offline",
        );
    }

    let timeout = if dev.platform == 4 {
        std::time::Duration::from_millis(800)
    } else {
        std::time::Duration::from_secs(8)
    };
    match tokio::time::timeout(timeout, rx).await {
        Ok(Ok(reply)) => {
            let need_password = reply.allow_control && reply.control_mode.needs_password();
            json_envelope(
                StatusCode::OK,
                serde_json::to_value(ok_envelope(json!({
                    "can_remote_control": reply.allow_control,
                    "allow_control": reply.allow_control,
                    "control_mode": reply.control_mode,
                    "need_password": need_password,
                    "control_id": reply.control_id,
                })))
                .unwrap(),
            )
        }
        _ => {
            state
                .inner
                .lock()
                .expect("fakecloud lock poisoned")
                .control_mode_waiters
                .remove(&control_id);
            if dev.platform == 4 {
                json_envelope(
                    StatusCode::OK,
                    serde_json::to_value(ok_envelope(json!({
                        "can_remote_control": dev.controllable,
                        "allow_control": dev.controllable,
                        "control_mode": ShareControlMode::ByPassword,
                        "need_password": dev.controllable,
                        "control_id": control_id,
                    })))
                    .unwrap(),
                )
            } else {
                fake_error(
                    StatusCode::BAD_REQUEST,
                    ErrorCode::CannotInitiateRemoteConnection,
                    "assist disabled",
                )
            }
        }
    }
}

async fn room_join_share_by_code(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: axum::http::Uri,
    body: Bytes,
) -> Response {
    let pq = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    if let Some(resp) = check_http_preconditions(&state, &Method::POST, pq, &headers, &body) {
        return resp;
    }
    let Ok(req) = serde_json::from_slice::<JoinShareByCodeRequest>(&body) else {
        return fake_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            ErrorCode::InvalidRequest,
            "invalid request",
        );
    };
    if req.connect_id.trim().is_empty() || req.connect_code.trim().is_empty() {
        return fake_error(
            StatusCode::BAD_REQUEST,
            ErrorCode::IncorrectShareCode,
            "incorrect code",
        );
    }
    let (dev, host_tx, control_id, salt, rx) = {
        let mut guard = state.inner.lock().expect("fakecloud lock poisoned");
        if bearer_valid(&guard, &headers).is_none() {
            return token_invalid_resp();
        }
        let (dev, host_tx) = match resolve_fake_share_target(&guard, &req.connect_id) {
            Ok(ok) => ok,
            Err(resp) => return *resp,
        };
        if !req.force_join && guard.sessions.values().any(|(d, _, _)| d == &dev.device_id) {
            return fake_error(
                StatusCode::BAD_REQUEST,
                ErrorCode::BeingControlled,
                "being controlled",
            );
        }
        let seq = guard.next_id;
        guard.next_id += 1;
        let control_id = format!("fake-sign-{seq}");
        let salt = format!("fake-salt-{seq}");
        let (tx, rx) = oneshot::channel();
        guard.sign_waiters.insert(control_id.clone(), tx);
        (dev, host_tx, control_id, salt, rx)
    };

    let push = encode_event(
        "bmsg_push",
        &[json!({ "type": "remote_control", "data": { "control_id": control_id, "salt": salt } })],
        None,
    );
    if host_tx.send(vec![WsOut::Text(push)]).await.is_err() {
        return fake_error(
            StatusCode::BAD_REQUEST,
            ErrorCode::DeviceOffline,
            "device offline",
        );
    }

    let reply = match tokio::time::timeout(std::time::Duration::from_secs(8), rx).await {
        Ok(Ok(reply)) => reply,
        _ => {
            state
                .inner
                .lock()
                .expect("fakecloud lock poisoned")
                .sign_waiters
                .remove(&control_id);
            return fake_error(
                StatusCode::BAD_REQUEST,
                ErrorCode::CannotInitiateRemoteConnection,
                "assist disabled",
            );
        }
    };
    if !reply.can_remote_control {
        return fake_error(
            StatusCode::BAD_REQUEST,
            ErrorCode::CannotInitiateRemoteConnection,
            "assist disabled",
        );
    }
    let primary_ok = verify_share_code_digest(&salt, &req.connect_code, &reply.sign);
    let backup_ok = !reply.backup_sign.is_empty()
        && verify_share_code_digest(&salt, &req.connect_code, &reply.backup_sign);
    if !primary_ok && !backup_ok {
        return fake_error(
            StatusCode::BAD_REQUEST,
            ErrorCode::IncorrectShareCode,
            "incorrect code",
        );
    }

    let mut guard = state.inner.lock().expect("fakecloud lock poisoned");
    let room = build_fake_assist_room(&state, &mut guard, &dev, &req.connect_id, req.force_join);
    json_envelope(
        StatusCode::OK,
        serde_json::to_value(ok_envelope(room)).unwrap(),
    )
}

async fn room_join_share_by_confirmation(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: axum::http::Uri,
    body: Bytes,
) -> Response {
    let pq = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    if let Some(resp) = check_http_preconditions(&state, &Method::POST, pq, &headers, &body) {
        return resp;
    }
    let Ok(req) = serde_json::from_slice::<JoinShareByConfirmationRequest>(&body) else {
        return fake_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            ErrorCode::InvalidRequest,
            "invalid request",
        );
    };
    enum ConfirmStep {
        Respond(Response),
        PushAndPending {
            host_tx: mpsc::Sender<Vec<WsOut>>,
            control_id: String,
        },
    }
    let step = {
        let mut guard = state.inner.lock().expect("fakecloud lock poisoned");
        if bearer_valid(&guard, &headers).is_none() {
            return token_invalid_resp();
        }
        let (dev, host_tx) = match resolve_fake_share_target(&guard, &req.connect_id) {
            Ok(ok) => ok,
            Err(resp) => return *resp,
        };
        if !req.force_join && guard.sessions.values().any(|(d, _, _)| d == &dev.device_id) {
            return fake_error(
                StatusCode::BAD_REQUEST,
                ErrorCode::BeingControlled,
                "being controlled",
            );
        }
        if let Some(cid) = req.control_id.as_deref().filter(|s| !s.is_empty())
            && let Some(conf) = guard.confirmations.get(cid)
            && conf.connect_id == req.connect_id
        {
            match conf.status {
                FakeConfirmStatus::Pending => ConfirmStep::Respond(json_envelope(
                    StatusCode::BAD_REQUEST,
                    json!({
                        "code": ErrorCode::ShareConfirmationRequired.code(),
                        "msg": "waiting",
                        "data": { "control_id": cid },
                    }),
                )),
                FakeConfirmStatus::Denied => {
                    guard.confirmations.remove(cid);
                    ConfirmStep::Respond(fake_error(
                        StatusCode::BAD_REQUEST,
                        ErrorCode::ShareConfirmationDenied,
                        "denied",
                    ))
                }
                FakeConfirmStatus::Approved => {
                    guard.confirmations.remove(cid);
                    let room = build_fake_assist_room(
                        &state,
                        &mut guard,
                        &dev,
                        &req.connect_id,
                        req.force_join,
                    );
                    ConfirmStep::Respond(json_envelope(
                        StatusCode::OK,
                        serde_json::to_value(ok_envelope(room)).unwrap(),
                    ))
                }
            }
        } else {
            let seq = guard.next_id;
            guard.next_id += 1;
            let control_id = format!("fake-confirm-{seq}");
            guard.confirmations.insert(
                control_id.clone(),
                FakeConfirmation {
                    device_id: dev.device_id,
                    connect_id: req.connect_id.clone(),
                    status: FakeConfirmStatus::Pending,
                },
            );
            ConfirmStep::PushAndPending {
                host_tx,
                control_id,
            }
        }
    };

    match step {
        ConfirmStep::Respond(resp) => resp,
        ConfirmStep::PushAndPending {
            host_tx,
            control_id,
        } => {
            let push = encode_event(
                "bmsg_push",
                &[json!({
                    "type": "remote_control_by_confirmation",
                    "data": { "control_id": control_id, "device_name": "viewer", "user_id": "1" },
                })],
                None,
            );
            let _ = host_tx.send(vec![WsOut::Text(push)]).await;
            json_envelope(
                StatusCode::BAD_REQUEST,
                json!({
                    "code": ErrorCode::ShareConfirmationRequired.code(),
                    "msg": "waiting",
                    "data": { "control_id": control_id },
                }),
            )
        }
    }
}

async fn room_share_cancel_remote_assist(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: axum::http::Uri,
    body: Bytes,
) -> Response {
    let pq = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    if let Some(resp) = check_http_preconditions(&state, &Method::POST, pq, &headers, &body) {
        return resp;
    }
    let Ok(req) = serde_json::from_slice::<CancelRemoteAssistRequest>(&body) else {
        return fake_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            ErrorCode::InvalidRequest,
            "invalid request",
        );
    };
    let to_notify: Vec<(mpsc::Sender<Vec<WsOut>>, String)> = {
        let mut guard = state.inner.lock().expect("fakecloud lock poisoned");
        if bearer_valid(&guard, &headers).is_none() {
            return token_invalid_resp();
        }
        let mut cancelled = Vec::new();
        guard.confirmations.retain(|cid, conf| {
            let matches = conf.connect_id == req.connect_id
                && req.control_id.as_deref().is_none_or(|want| want == cid);
            if matches {
                cancelled.push((conf.device_id.clone(), cid.clone()));
                false
            } else {
                true
            }
        });
        cancelled
            .into_iter()
            .filter_map(|(dev_id, cid)| guard.hosts_ws.get(&dev_id).cloned().map(|tx| (tx, cid)))
            .collect()
    };
    for (tx, cid) in to_notify {
        let push = encode_event(
            "bmsg_push",
            &[json!({
                "type": "remote_control_by_confirmation_cancel",
                "data": { "control_id": cid },
            })],
            None,
        );
        let _ = tx.send(vec![WsOut::Text(push)]).await;
    }
    json_envelope(
        StatusCode::OK,
        serde_json::to_value(ok_envelope(EmptyData {})).unwrap(),
    )
}

#[derive(Deserialize, Default)]
struct ReleaseUpdateQuery {
    #[serde(default)]
    version_name: Option<String>,
}

fn no_update_json(version_name: &str, version_code: u64) -> Value {
    json!({
        "url": "",
        "md5": "",
        "version_name": version_name,
        "version_code": version_code,
        "change_logs": [],
        "subtitle": "",
        "user_external_force_upgrade": false,
        "upgrade_uri": {},
        "update_type": "NoUpdate",
        "update_prompt_type": "",
    })
}

async fn release_fetch_update(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: axum::http::Uri,
) -> Response {
    let pq = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    if let Some(resp) = check_http_preconditions(&state, &Method::GET, pq, &headers, &[]) {
        return resp;
    }
    let guard = state.inner.lock().expect("fakecloud lock poisoned");
    if bearer_valid(&guard, &headers).is_none() {
        return token_invalid_resp();
    }
    let version_name = headers
        .get("x-param-vn")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .unwrap_or("");
    let version_code = headers
        .get("x-param-vc")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0);
    json_envelope(
        StatusCode::OK,
        serde_json::to_value(ok_envelope(no_update_json(version_name, version_code))).unwrap(),
    )
}

async fn release_fetch_update_device(
    State(state): State<AppState>,
    Path(device_id): Path<String>,
    Query(query): Query<ReleaseUpdateQuery>,
    headers: HeaderMap,
    uri: axum::http::Uri,
) -> Response {
    let pq = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    if let Some(resp) = check_http_preconditions(&state, &Method::GET, pq, &headers, &[]) {
        return resp;
    }
    let guard = state.inner.lock().expect("fakecloud lock poisoned");
    if bearer_valid(&guard, &headers).is_none() {
        return token_invalid_resp();
    }
    let Some(dev) = guard
        .devices_by_id
        .get(&device_id)
        .filter(|d| d.owner.as_deref() == Some("1"))
    else {
        return json_envelope(
            StatusCode::BAD_REQUEST,
            serde_json::to_value(error_envelope(ErrorCode::UnknownDevice, "no such device"))
                .unwrap(),
        );
    };
    let version_name = query
        .version_name
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| dev.version_name.clone());
    json_envelope(
        StatusCode::OK,
        serde_json::to_value(ok_envelope(no_update_json(&version_name, 0))).unwrap(),
    )
}

async fn device_unbind(
    State(state): State<AppState>,
    Path(device_id): Path<String>,
    headers: HeaderMap,
    uri: axum::http::Uri,
    body: Bytes,
) -> Response {
    let pq = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    if let Some(resp) = check_http_preconditions(&state, &Method::POST, pq, &headers, &body) {
        return resp;
    }
    let (kick_tx, viewer_lefts) = {
        let mut guard = state.inner.lock().expect("fakecloud lock poisoned");
        if bearer_valid(&guard, &headers).is_none() {
            return token_invalid_resp();
        }
        let Some(dev) = guard.devices_by_id.get_mut(&device_id) else {
            return json_envelope(
                StatusCode::BAD_REQUEST,
                serde_json::to_value(error_envelope(ErrorCode::UnknownDevice, "no such device"))
                    .unwrap(),
            );
        };
        if dev.owner.as_deref() != Some("1") {
            return json_envelope(
                StatusCode::BAD_REQUEST,
                serde_json::to_value(error_envelope(ErrorCode::UnknownDevice, "no such device"))
                    .unwrap(),
            );
        }
        dev.owner = None;
        let client_id = dev.client_id.clone();
        if let Some(by_client) = guard.devices_by_client.get_mut(&client_id) {
            by_client.owner = None;
        }
        if let Some(bound_tokens) = guard.device_user_tokens.remove(&device_id) {
            for tok in bound_tokens {
                guard.user_tokens.remove(&tok);
            }
        }
        if let Some(share) = guard.shares_by_device.remove(&device_id) {
            guard.connect_id_to_device.remove(&share.connect_id);
        }
        guard
            .saved_shares
            .retain(|r| r.publisher_device_id != device_id);
        guard.wallpapers.remove(&device_id);
        guard
            .files
            .retain(|f| f.device_id.as_deref() != Some(device_id.as_str()));
        let mut removed_sessions = Vec::new();
        guard.sessions.retain(|ice_id, (dev_id, viewer_id, _)| {
            if dev_id == &device_id {
                removed_sessions.push((ice_id.clone(), viewer_id.clone()));
                false
            } else {
                true
            }
        });
        let viewer_lefts: Vec<_> = removed_sessions
            .into_iter()
            .filter_map(|(ice_id, viewer_id)| {
                guard
                    .viewers_ws
                    .get(&viewer_id)
                    .cloned()
                    .map(|tx| (ice_id, tx))
            })
            .collect();
        (guard.hosts_ws.remove(&device_id), viewer_lefts)
    };
    if let Some(tx) = kick_tx {
        let left = encode_event("left", &[json!({})], None);
        let _ = tx.send(vec![WsOut::Text(left), WsOut::Close]).await;
    }
    for (ice_id, v_tx) in viewer_lefts {
        let left = encode_event(
            "left",
            &[json!({ "ice_id": ice_id, "client_id": "fake-host" })],
            None,
        );
        let _ = v_tx.send(vec![WsOut::Text(left)]).await;
    }
    json_envelope(
        StatusCode::OK,
        serde_json::to_value(ok_envelope(EmptyData {})).unwrap(),
    )
}

async fn room_create(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: axum::http::Uri,
    body: Bytes,
) -> Response {
    let pq = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    if let Some(resp) = check_http_preconditions(&state, &Method::POST, pq, &headers, &body) {
        return resp;
    }
    let mut guard = state.inner.lock().expect("fakecloud lock poisoned");
    let Some(user_tok) = bearer_valid(&guard, &headers) else {
        return token_invalid_resp();
    };
    let Some(device_id) = headers
        .get("x-param-device-id")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
    else {
        return json_envelope(
            StatusCode::UNPROCESSABLE_ENTITY,
            serde_json::to_value(error_envelope(
                ErrorCode::InvalidRequest,
                "missing device_id",
            ))
            .unwrap(),
        );
    };
    let Some(dev) = guard.devices_by_id.get_mut(&device_id) else {
        return json_envelope(
            StatusCode::UNPROCESSABLE_ENTITY,
            serde_json::to_value(error_envelope(ErrorCode::InvalidRequest, "unknown device"))
                .unwrap(),
        );
    };
    dev.owner = Some("1".into());
    let client_id = dev.client_id.clone();
    if let Some(by_client) = guard.devices_by_client.get_mut(&client_id) {
        by_client.owner = Some("1".into());
    }
    guard
        .device_user_tokens
        .entry(device_id.clone())
        .or_default()
        .push(user_tok);
    let seq = guard.next_id;
    guard.next_id += 1;
    let ticket = format!("fake-room-host-{seq}");
    guard.room_tickets.insert(
        ticket.clone(),
        RoomTicket {
            device_id,
            role: FakeRole::Host,
            force_join: false,
            shared: false,
        },
    );
    json_envelope(
        StatusCode::OK,
        serde_json::to_value(ok_envelope(json!({
            "signaling_server": state.ws_url,
            "signaling_list": [state.ws_url],
            "sni_required_signaling_list": [],
            "streamer_config": "{}",
            "token": ticket,
            "ws_connect_timeout_ms": 16_000,
            "max_reconnect_delta": 300,
            "streamer_retry_delta_ms": 1_000,
            "international_connect": false
        })))
        .unwrap(),
    )
}

async fn room_join(
    State(state): State<AppState>,
    Path(device_id): Path<String>,
    headers: HeaderMap,
    uri: axum::http::Uri,
    body: Bytes,
) -> Response {
    let pq = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    if let Some(resp) = check_http_preconditions(&state, &Method::POST, pq, &headers, &body) {
        return resp;
    }
    let Ok(req_val) = serde_json::from_slice::<Value>(&body) else {
        return json_envelope(
            StatusCode::UNPROCESSABLE_ENTITY,
            serde_json::to_value(error_envelope(ErrorCode::InvalidRequest, "invalid body"))
                .unwrap(),
        );
    };
    let force_join = req_val
        .get("force_join")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let mut guard = state.inner.lock().expect("fakecloud lock poisoned");
    if bearer_valid(&guard, &headers).is_none() {
        return token_invalid_resp();
    }
    let Some(dev) = guard.devices_by_id.get(&device_id) else {
        return json_envelope(
            StatusCode::BAD_REQUEST,
            serde_json::to_value(error_envelope(ErrorCode::UnknownDevice, "no such device"))
                .unwrap(),
        );
    };
    if dev.owner.as_deref() != Some("1") {
        return json_envelope(
            StatusCode::BAD_REQUEST,
            serde_json::to_value(error_envelope(ErrorCode::UnknownDevice, "no such device"))
                .unwrap(),
        );
    }
    let platform = dev.platform;
    if !guard.hosts_ws.contains_key(&device_id) {
        return json_envelope(
            StatusCode::BAD_REQUEST,
            serde_json::to_value(error_envelope(ErrorCode::DeviceOffline, "device offline"))
                .unwrap(),
        );
    }
    if !force_join && guard.sessions.values().any(|(d, _, _)| d == &device_id) {
        return json_envelope(
            StatusCode::BAD_REQUEST,
            serde_json::to_value(error_envelope(
                ErrorCode::BeingControlled,
                "being controlled",
            ))
            .unwrap(),
        );
    }
    let seq = guard.next_id;
    guard.next_id += 1;
    let ticket = format!("fake-room-viewer-{seq}");
    guard.room_tickets.insert(
        ticket.clone(),
        RoomTicket {
            device_id,
            role: FakeRole::Viewer,
            force_join,
            shared: false,
        },
    );
    json_envelope(
        StatusCode::OK,
        serde_json::to_value(ok_envelope(json!({
            "signaling_server": state.ws_url,
            "signaling_list": [state.ws_url],
            "sni_required_signaling_list": [],
            "streamer_config": "{}",
            "token": ticket,
            "ws_connect_timeout_ms": 16_000,
            "max_reconnect_delta": 300,
            "streamer_retry_delta_ms": 1_000,
            "international_connect": false,
            "report_token": "",
            "report_url": "",
            "report_server_address": "",
            "publisher_platform": platform
        })))
        .unwrap(),
    )
}

#[derive(Deserialize)]
struct WsQuery {
    #[serde(rename = "EIO")]
    eio: String,
    transport: String,
}

async fn ws_connect(
    ws: WebSocketUpgrade,
    Query(q): Query<WsQuery>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    if q.eio != "4" || q.transport != "websocket" {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let token = headers
        .get("x-nrd-auth")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let ticket = {
        let guard = state.inner.lock().expect("fakecloud lock poisoned");
        guard.room_tickets.get(token).cloned()
    };
    let Some(ticket) = ticket else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    ws.on_upgrade(move |socket| serve_ws(socket, ticket, state))
}

static WS_CONN_SEQ: AtomicU64 = AtomicU64::new(1);

async fn serve_ws(mut socket: WebSocket, ticket: RoomTicket, state: AppState) {
    let conn_id = format!("fake-conn-{}", WS_CONN_SEQ.fetch_add(1, Ordering::Relaxed));
    let (tx, mut rx) = mpsc::channel::<Vec<WsOut>>(64);
    {
        let mut guard = state.inner.lock().expect("fakecloud lock poisoned");
        match ticket.role {
            FakeRole::Host => {
                if let Some(old) = guard.hosts_ws.insert(ticket.device_id.clone(), tx.clone()) {
                    let left = encode_event("left", &[json!({})], None);
                    let _ = old.try_send(vec![WsOut::Text(left), WsOut::Close]);
                }
            }
            FakeRole::Viewer => {
                guard.viewers_ws.insert(conn_id.clone(), tx.clone());
            }
        }
    }

    let open = Handshake {
        sid: conn_id.clone(),
        ping_interval: 25_000,
        ping_timeout: 20_000,
    }
    .encode();
    if socket.send(Message::Text(open.into())).await.is_err() {
        cleanup_ws(&state, &ticket, &conn_id).await;
        return;
    }

    let mut decoder = Decoder::new();
    loop {
        tokio::select! {
            inbound = socket.recv() => {
                let packet = match inbound {
                    Some(Ok(Message::Text(t))) => decoder.text(&t),
                    Some(Ok(Message::Binary(b))) => decoder.binary(&b),
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                };
                match packet {
                    Ok(Some(Packet::Connected)) => {
                        let _ = socket.send(Message::Text(encode_connected(&conn_id).into())).await;
                    }
                    Ok(Some(Packet::Event(ev))) => {
                        let ack_id = ev.ack_id;
                        let sig = SignalEvent::from_event(&ev);
                        handle_ws_event(&state, &ticket, &conn_id, ack_id, sig, &tx).await;
                    }
                    _ => {}
                }
            }
            outbound = rx.recv() => {
                let Some(batch) = outbound else { break; };
                for item in batch {
                    match item {
                        WsOut::Text(t) => {
                            if socket.send(Message::Text(t.into())).await.is_err() {
                                break;
                            }
                        }
                        WsOut::Binary(b) => {
                            if socket.send(Message::Binary(b.into())).await.is_err() {
                                break;
                            }
                        }
                        WsOut::Close => {
                            let _ = socket.close().await;
                            cleanup_ws(&state, &ticket, &conn_id).await;
                            return;
                        }
                    }
                }
            }
        }
    }

    cleanup_ws(&state, &ticket, &conn_id).await;
}

async fn cleanup_ws(state: &AppState, ticket: &RoomTicket, conn_id: &str) {
    let mut to_notify = Vec::new();
    {
        let mut guard = state.inner.lock().expect("fakecloud lock poisoned");
        match ticket.role {
            FakeRole::Host => {
                guard.hosts_ws.remove(&ticket.device_id);
            }
            FakeRole::Viewer => {
                guard.viewers_ws.remove(conn_id);
                let mut released = Vec::new();
                guard.sessions.retain(|ice_id, (dev, viewer, _)| {
                    if viewer == conn_id && dev == &ticket.device_id {
                        released.push(ice_id.clone());
                        false
                    } else {
                        true
                    }
                });
                if let Some(host_tx) = guard.hosts_ws.get(&ticket.device_id).cloned() {
                    for ice_id in released {
                        let msg = encode_event(
                            "released",
                            &[json!({ "ice_id": ice_id, "client_id": conn_id })],
                            None,
                        );
                        to_notify.push((host_tx.clone(), vec![WsOut::Text(msg)]));
                    }
                }
            }
        }
    }
    for (tx, batch) in to_notify {
        let _ = tx.send(batch).await;
    }
}

async fn handle_ws_event(
    state: &AppState,
    ticket: &RoomTicket,
    conn_id: &str,
    ack_id: Option<u64>,
    event: SignalEvent,
    self_tx: &mpsc::Sender<Vec<WsOut>>,
) {
    match (&ticket.role, event) {
        (FakeRole::Host, SignalEvent::RoomInfo(_)) => {
            let mut batch = Vec::new();
            if let Some(id) = ack_id {
                batch.push(WsOut::Text(encode_ack(id, &[json!({})])));
            }
            batch.push(WsOut::Text(encode_event(
                "bmsg_push",
                &[json!({ "data": { "status": "online", "publisher_availability_status": "ready" } })],
                None,
            )));
            let _ = self_tx.send(batch).await;
        }
        (_, SignalEvent::RefreshReconnectKey(_)) => {
            if let Some(id) = ack_id {
                let ack = {
                    let mut guard = state.inner.lock().expect("fakecloud lock poisoned");
                    let seq = guard.next_id;
                    guard.next_id += 1;
                    encode_ack(
                        id,
                        &[
                            json!("success"),
                            json!({ "reconnect_key": format!("fake-rk-{seq}") }),
                        ],
                    )
                };
                let _ = self_tx.send(vec![WsOut::Text(ack)]).await;
            }
        }
        (FakeRole::Viewer, SignalEvent::Control(ctrl)) => {
            struct Eviction {
                ice_id: String,
                viewer_id: String,
                viewer_tx: Option<mpsc::Sender<Vec<WsOut>>>,
            }
            enum ControlStep {
                Ack(Vec<Value>),
                Forward {
                    host_tx: mpsc::Sender<Vec<WsOut>>,
                    evictions: Vec<Eviction>,
                    batch_host: Vec<WsOut>,
                    ack_args: Vec<Value>,
                },
            }
            let action = {
                let mut guard = state.inner.lock().expect("fakecloud lock poisoned");
                if let Some(host_tx) = guard.hosts_ws.get(&ticket.device_id).cloned() {
                    if !ticket.force_join
                        && guard
                            .sessions
                            .values()
                            .any(|(d, _, _)| d == &ticket.device_id)
                    {
                        ControlStep::Ack(vec![
                            json!("failure"),
                            json!({ "reason": "controlled from another device" }),
                        ])
                    } else {
                        let mut evicted = Vec::new();
                        if ticket.force_join {
                            guard.sessions.retain(|ice_id, (dev_id, viewer_id, _)| {
                                if dev_id == &ticket.device_id {
                                    evicted.push((ice_id.clone(), viewer_id.clone()));
                                    false
                                } else {
                                    true
                                }
                            });
                        }
                        let evictions: Vec<Eviction> = evicted
                            .into_iter()
                            .map(|(ice_id, viewer_id)| {
                                let viewer_tx = guard.viewers_ws.get(&viewer_id).cloned();
                                Eviction {
                                    ice_id,
                                    viewer_id,
                                    viewer_tx,
                                }
                            })
                            .collect();

                        let seq = guard.next_id;
                        guard.next_id += 1;
                        let ice_id = format!("fake-ice-{seq}");
                        guard.sessions.insert(
                            ice_id.clone(),
                            (ticket.device_id.clone(), conn_id.to_owned(), ticket.shared),
                        );
                        let mut bc = json!({
                            "ice_id": ice_id,
                            "client_id": conn_id,
                            "app_control_id": ctrl.app_control_id,
                            "iceServers": [],
                            "force_relay": false,
                            "app_data": placeholder(0),
                        });
                        if let Some(platform) = controller_platform(&ctrl.connect_options) {
                            bc["controller_platform"] = platform.into();
                        }
                        let batch_host = vec![
                            WsOut::Text(encode_binary_event("be-controlled", &[bc], 1, None)),
                            WsOut::Binary(ctrl.connect_options),
                        ];
                        let ack_args = vec![
                            json!("success"),
                            json!({
                                "ice_id": ice_id,
                                "client_id": "fake-host",
                                "app_control_id": ctrl.app_control_id,
                                "iceServers": [],
                                "force_relay": false,
                            }),
                        ];
                        ControlStep::Forward {
                            host_tx,
                            evictions,
                            batch_host,
                            ack_args,
                        }
                    }
                } else {
                    ControlStep::Ack(vec![json!("failure"), json!({ "reason": "host offline" })])
                }
            };
            match action {
                ControlStep::Ack(args) => {
                    if let Some(id) = ack_id {
                        let _ = self_tx.send(vec![WsOut::Text(encode_ack(id, &args))]).await;
                    }
                }
                ControlStep::Forward {
                    host_tx,
                    evictions,
                    batch_host,
                    ack_args,
                } => {
                    for ev in evictions {
                        let released = encode_event(
                            "released",
                            &[json!({ "ice_id": ev.ice_id, "client_id": ev.viewer_id })],
                            None,
                        );
                        let _ = host_tx.send(vec![WsOut::Text(released)]).await;
                        if let Some(v_tx) = ev.viewer_tx {
                            let left = encode_event(
                                "left",
                                &[json!({ "ice_id": ev.ice_id, "client_id": "fake-host" })],
                                None,
                            );
                            let _ = v_tx.send(vec![WsOut::Text(left)]).await;
                        }
                    }
                    let _ = host_tx.send(batch_host).await;
                    if let Some(id) = ack_id {
                        let _ = self_tx
                            .send(vec![WsOut::Text(encode_ack(id, &ack_args))])
                            .await;
                    }
                }
            }
        }
        (_, SignalEvent::Soac(mut soac)) => {
            let target = {
                let guard = state.inner.lock().expect("fakecloud lock poisoned");
                match ticket.role {
                    FakeRole::Host => soac
                        .client_id
                        .as_deref()
                        .and_then(|id| guard.viewers_ws.get(id))
                        .cloned(),
                    FakeRole::Viewer => guard.hosts_ws.get(&ticket.device_id).cloned(),
                }
            };
            if let Some(tx) = target {
                soac.client_id = Some(conn_id.to_owned());
                let body = serde_json::to_value(soac).expect("soac serializes");
                let _ = tx
                    .send(vec![WsOut::Text(encode_event("soac", &[body], None))])
                    .await;
            }
        }
        (_, SignalEvent::ForwardSetting(mut fs)) => {
            let target = {
                let guard = state.inner.lock().expect("fakecloud lock poisoned");
                match ticket.role {
                    FakeRole::Host => fs
                        .client_id
                        .as_deref()
                        .and_then(|id| guard.viewers_ws.get(id))
                        .cloned(),
                    FakeRole::Viewer => guard.hosts_ws.get(&ticket.device_id).cloned(),
                }
            };
            if let Some(tx) = target {
                fs.client_id = Some(conn_id.to_owned());
                let body = serde_json::to_value(fs).expect("forward_setting serializes");
                let _ = tx
                    .send(vec![WsOut::Text(encode_event(
                        "forward_setting",
                        &[body],
                        None,
                    ))])
                    .await;
            }
        }
        _ => {}
    }
}

const FAKE_MAX_FILE_SIZE: usize = 10 * 1024 * 1024;
const FAKE_KEPT_UPLOADS: usize = 4;

fn fake_image_type(content: &[u8]) -> Option<&'static str> {
    if content.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if content.starts_with(b"\xff\xd8\xff") {
        Some("image/jpeg")
    } else if content.starts_with(b"RIFF") && content.get(8..12) == Some(b"WEBP") {
        Some("image/webp")
    } else if content.starts_with(b"GIF87a") || content.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if content.starts_with(b"BM") && content.len() > 14 {
        Some("image/bmp")
    } else {
        None
    }
}

fn fake_parse_file_id_from_url(url: &str) -> Option<String> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let (_, path) = rest.split_once('/')?;
    let path = path.split(['?', '#']).next()?;
    let id = path.strip_prefix("api/files/")?;
    let valid = id.len() == 32 && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    valid.then(|| id.to_owned())
}

fn extract_multipart_first_file(content_type: &str, body: &[u8]) -> Option<Vec<u8>> {
    let boundary = content_type
        .split(';')
        .map(str::trim)
        .find_map(|part| part.strip_prefix("boundary="))?
        .trim_matches('"');
    if boundary.is_empty() {
        return None;
    }
    let delim = format!("--{boundary}");
    let delim_bytes = delim.as_bytes();
    let start = body
        .windows(delim_bytes.len())
        .position(|w| w == delim_bytes)?;
    let mut rest = &body[start + delim_bytes.len()..];
    if rest.starts_with(b"\r\n") {
        rest = &rest[2..];
    } else if rest.starts_with(b"\n") {
        rest = &rest[1..];
    }
    let header_end = rest.windows(4).position(|w| w == b"\r\n\r\n")?;
    let after_headers = &rest[header_end + 4..];
    let next_delim = after_headers
        .windows(delim_bytes.len())
        .position(|w| w == delim_bytes)?;
    let mut part = &after_headers[..next_delim];
    if part.ends_with(b"\r\n") {
        part = &part[..part.len() - 2];
    } else if part.ends_with(b"\n") {
        part = &part[..part.len() - 1];
    }
    Some(part.to_vec())
}

async fn fp_token(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: axum::http::Uri,
) -> Response {
    let pq = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    if let Some(resp) = check_http_preconditions(&state, &Method::GET, pq, &headers, &[]) {
        return resp;
    }
    let mut guard = state.inner.lock().expect("fakecloud lock poisoned");
    let Some(user_tok) = bearer_valid(&guard, &headers) else {
        return token_invalid_resp();
    };
    let caller_id = headers
        .get("x-param-device-id")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
        .filter(|id| {
            guard
                .devices_by_id
                .get(*id)
                .is_some_and(|d| d.owner.as_deref() == Some("1"))
        })
        .map(str::to_owned);
    let seq = guard.next_id;
    guard.next_id += 1;
    let upload_tok = format!("fake.upload.{seq}");
    guard
        .upload_tokens
        .insert(upload_tok.clone(), (user_tok, caller_id));
    json_envelope(
        StatusCode::OK,
        serde_json::to_value(ok_envelope(json!({
            "token": upload_tok,
            "req_url": format!("{}/api/files", state.base_url),
            "ttl": 3600,
            "max_size": FAKE_MAX_FILE_SIZE,
        })))
        .unwrap(),
    )
}

async fn upload_file(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: axum::http::Uri,
    body: Bytes,
) -> Response {
    let pq = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    {
        let mut guard = state.inner.lock().expect("fakecloud lock poisoned");
        guard.http_calls.push(RecordedHttpCall {
            method: "POST".to_owned(),
            path: pq.to_owned(),
            body: body.to_vec(),
        });
    }
    let raw_auth = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.strip_prefix("Bearer ").unwrap_or(s));
    let mut guard = state.inner.lock().expect("fakecloud lock poisoned");
    let Some((user_tok, device_id)) = raw_auth.and_then(|t| guard.upload_tokens.get(t)).cloned()
    else {
        return json_envelope(StatusCode::UNAUTHORIZED, json!({ "error": "unauthorized" }));
    };
    if !guard.user_tokens.contains(&user_tok) {
        return json_envelope(StatusCode::UNAUTHORIZED, json!({ "error": "unauthorized" }));
    }

    let content = match headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
    {
        Some(ct) if ct.starts_with("multipart/form-data") => {
            match extract_multipart_first_file(ct, &body) {
                Some(bytes) => bytes,
                None => return json_envelope(StatusCode::BAD_REQUEST, json!({ "error": "empty" })),
            }
        }
        _ => body.to_vec(),
    };

    if content.is_empty() {
        return json_envelope(StatusCode::BAD_REQUEST, json!({ "error": "empty" }));
    }
    if content.len() > FAKE_MAX_FILE_SIZE {
        return json_envelope(
            StatusCode::PAYLOAD_TOO_LARGE,
            json!({ "error": "too_large" }),
        );
    }
    let Some(mime) = fake_image_type(&content) else {
        return json_envelope(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            json!({ "error": "not_an_image" }),
        );
    };
    let md5 = hex::encode(Md5::digest(&content));
    let seq = guard.next_id;
    guard.next_id += 1;
    let id = format!("{seq:032x}");
    let fsize = content.len();
    guard.files.push(StoredFile {
        id: id.clone(),
        device_id: device_id.clone(),
        mime: mime.to_owned(),
        content,
    });

    // Retain the latest FAKE_KEPT_UPLOADS per device_id plus any active wallpaper.
    let active_wallpapers: HashSet<String> = guard.wallpapers.values().cloned().collect();
    let mut kept_for_uploader = 0usize;
    let mut keep_ids: HashSet<String> = HashSet::new();
    for file in guard.files.iter().rev() {
        if file.device_id == device_id {
            if kept_for_uploader < FAKE_KEPT_UPLOADS {
                keep_ids.insert(file.id.clone());
                kept_for_uploader += 1;
            }
        } else {
            keep_ids.insert(file.id.clone());
        }
    }
    guard
        .files
        .retain(|f| keep_ids.contains(&f.id) || active_wallpapers.contains(&f.id));

    json_envelope(
        StatusCode::OK,
        json!({
            "url": format!("{}/api/files/{id}", state.base_url),
            "fsize": fsize,
            "md5": md5,
            "mime": mime,
        }),
    )
}

async fn serve_file(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let guard = state.inner.lock().expect("fakecloud lock poisoned");
    let Some(file) = guard.files.iter().find(|f| f.id == id).cloned() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let content_type = axum::http::HeaderValue::from_str(&file.mime)
        .unwrap_or_else(|_| axum::http::HeaderValue::from_static("application/octet-stream"));
    (
        [
            (axum::http::header::CONTENT_TYPE, content_type),
            (
                axum::http::header::CACHE_CONTROL,
                axum::http::HeaderValue::from_static("public, max-age=31536000, immutable"),
            ),
            (
                axum::http::header::X_CONTENT_TYPE_OPTIONS,
                axum::http::HeaderValue::from_static("nosniff"),
            ),
            (
                axum::http::header::CONTENT_SECURITY_POLICY,
                axum::http::HeaderValue::from_static("default-src 'none'; sandbox"),
            ),
        ],
        file.content,
    )
        .into_response()
}

#[derive(Deserialize)]
struct FakeWallpaperRequest {
    wallpaper_url: String,
}

async fn set_wallpaper(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: axum::http::Uri,
    body: Bytes,
) -> Response {
    let pq = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    if let Some(resp) = check_http_preconditions(&state, &Method::POST, pq, &headers, &body) {
        return resp;
    }
    let mut guard = state.inner.lock().expect("fakecloud lock poisoned");
    if bearer_valid(&guard, &headers).is_none() {
        return token_invalid_resp();
    }
    let Some(device_id) = headers
        .get("x-param-device-id")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
    else {
        return json_envelope(
            StatusCode::BAD_REQUEST,
            serde_json::to_value(error_envelope(ErrorCode::UnknownDevice, "no such device"))
                .unwrap(),
        );
    };
    if !guard
        .devices_by_id
        .get(&device_id)
        .is_some_and(|d| d.owner.as_deref() == Some("1"))
    {
        return json_envelope(
            StatusCode::BAD_REQUEST,
            serde_json::to_value(error_envelope(ErrorCode::UnknownDevice, "no such device"))
                .unwrap(),
        );
    }
    let Ok(req) = serde_json::from_slice::<FakeWallpaperRequest>(&body) else {
        return json_envelope(
            StatusCode::UNPROCESSABLE_ENTITY,
            serde_json::to_value(error_envelope(ErrorCode::InvalidRequest, "invalid request"))
                .unwrap(),
        );
    };
    if req.wallpaper_url.is_empty() {
        guard.wallpapers.remove(&device_id);
        return json_envelope(
            StatusCode::OK,
            serde_json::to_value(ok_envelope(EmptyData {})).unwrap(),
        );
    }
    let Some(file_id) = fake_parse_file_id_from_url(&req.wallpaper_url) else {
        return json_envelope(
            StatusCode::UNPROCESSABLE_ENTITY,
            serde_json::to_value(error_envelope(ErrorCode::InvalidRequest, "invalid request"))
                .unwrap(),
        );
    };
    if !guard.files.iter().any(|f| f.id == file_id) {
        return json_envelope(
            StatusCode::UNPROCESSABLE_ENTITY,
            serde_json::to_value(error_envelope(ErrorCode::InvalidRequest, "invalid request"))
                .unwrap(),
        );
    }
    guard.wallpapers.insert(device_id, file_id);
    json_envelope(
        StatusCode::OK,
        serde_json::to_value(ok_envelope(EmptyData {})).unwrap(),
    )
}

fn controller_platform(connect_options: &[u8]) -> Option<u8> {
    let options = remora_proto::pb::ConnectOptions::decode(connect_options).ok()?;
    Some(match options.client_type {
        3 => 1, // Windows
        2 => 2, // Android
        1 => 3, // iOS
        4 => 4, // macOS
        _ => return None,
    })
}
