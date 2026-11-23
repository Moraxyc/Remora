//! Signed cloud control-plane HTTP client returning `remora_proto::control` types directly.

use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use rand::Rng;
use remora_proto::control::signing::{PARAM_PREFIX, SIGN_HEADER, SignKey, sign};
use remora_proto::control::{
    ApiError, AssistDeviceRecordPage, AssistFavoritesRequest, AssistRoomConfig,
    CancelRemoteAssistRequest, DeviceGroupsResponse, DeviceInitRequest, DeviceInitResponse,
    DeviceListResponse, DevicePreferencesResponse, DeviceShareInfoResponse, GuestCreateResponse,
    JoinShareByCodeRequest, JoinShareByConfirmationRequest, JoinShareByConfirmationResult,
    LoginByQrcodeRequest, LoginResponse, MacControllableRequest, QrLoginGenResponse,
    QrLoginStatusRequest, QrLoginStatusResponse, ReleaseUpdateResponse, RoomCreateRequest,
    RoomCreateResponse, RoomJoinRequest, RoomJoinResponse, SavedShareKind,
    ShareConfirmationRequest, ShareControlModeRequest, ShareControlModeResponse,
    UploadControlModeRequest, UploadShareSignRequest, UserInfoResponse,
    parse_join_confirmation_response, parse_response,
};
use remora_proto::machine::auth::SignedHttpRequest;
use remora_proto::profile::Profile;
use remora_proto::server::EmptyData;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use reqwest::{Client, Method};
use serde::Serialize;
use serde::de::DeserializeOwned;
use url::Url;

#[cfg(feature = "record")]
use std::sync::Arc;

#[cfg(feature = "record")]
use crate::record::TraceRecorder;

/// Error returned by [`ControlClient`] calls.
#[derive(Debug)]
#[non_exhaustive]
pub enum HttpError {
    /// Cloud returned a protocol error envelope or malformed JSON.
    Api(ApiError),
    /// Network, TLS, or HTTP transport error (without secret URL details).
    Transport(String),
    /// Endpoint path could not be joined onto the base URL.
    InvalidUrl(url::ParseError),
    /// Request header name or value was invalid.
    InvalidHeader,
    /// Request body serialization failed.
    Serialize(serde_json::Error),
}

impl fmt::Display for HttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Api(err) => err.fmt(f),
            Self::Transport(msg) => write!(f, "control transport error: {msg}"),
            Self::InvalidUrl(err) => write!(f, "invalid control URL: {err}"),
            Self::InvalidHeader => f.write_str("invalid HTTP header"),
            Self::Serialize(err) => write!(f, "request serialization error: {err}"),
        }
    }
}

impl std::error::Error for HttpError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Api(err) => Some(err),
            Self::InvalidUrl(err) => Some(err),
            Self::Serialize(err) => Some(err),
            Self::Transport(_) | Self::InvalidHeader => None,
        }
    }
}

impl From<ApiError> for HttpError {
    fn from(err: ApiError) -> Self {
        Self::Api(err)
    }
}

impl From<reqwest::Error> for HttpError {
    fn from(err: reqwest::Error) -> Self {
        Self::Transport(err.without_url().to_string())
    }
}

/// Signed control-plane HTTP client using `reqwest` with OS trust-store TLS verification.
#[derive(Clone)]
pub struct ControlClient {
    client: Client,
    base_url: Url,
    profile: &'static Profile,
    sign_key: SignKey,
    #[cfg(feature = "record")]
    recorder: Option<Arc<TraceRecorder>>,
}

impl fmt::Debug for ControlClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ControlClient")
            .field("base_url", &self.base_url.as_str())
            .field("profile", &self.profile.id)
            .field("sign_key", &self.sign_key)
            .finish()
    }
}

impl ControlClient {
    /// Builds a control client with a default `reqwest::Client` verifying TLS via OS roots.
    pub fn new(
        base_url: Url,
        profile: &'static Profile,
        sign_key: SignKey,
    ) -> Result<Self, HttpError> {
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self::with_http_client(base_url, profile, sign_key, client))
    }

    /// Builds a control client around a caller-supplied `reqwest::Client`.
    pub fn with_http_client(
        base_url: Url,
        profile: &'static Profile,
        sign_key: SignKey,
        client: Client,
    ) -> Self {
        Self {
            client,
            base_url,
            profile,
            sign_key,
            #[cfg(feature = "record")]
            recorder: None,
        }
    }

    /// Attaches a JSONL trace recorder.
    #[cfg(feature = "record")]
    pub fn with_recorder(mut self, recorder: Arc<TraceRecorder>) -> Self {
        self.recorder = Some(recorder);
        self
    }

    pub fn base_url(&self) -> &Url {
        &self.base_url
    }

    pub fn profile(&self) -> &'static Profile {
        self.profile
    }

    pub fn sign_key(&self) -> &SignKey {
        &self.sign_key
    }

    /// Registers or renames this installation (`POST {profile.init_endpoint}`).
    pub async fn device_init(
        &self,
        req: &DeviceInitRequest,
    ) -> Result<DeviceInitResponse, HttpError> {
        self.post_json(&self.profile.init_endpoint, None, None, Some(req))
            .await
    }

    /// Generates a QR login challenge (`POST /api/v1/qrcode/gen/login`).
    pub async fn qr_login_generate(
        &self,
        device_id: Option<&str>,
    ) -> Result<QrLoginGenResponse, HttpError> {
        self.post_json::<(), _>("/api/v1/qrcode/gen/login", None, device_id, None)
            .await
    }

    /// Polls QR login progress (`POST /api/v1/qrcode/login/status`).
    pub async fn qr_login_status(
        &self,
        req: &QrLoginStatusRequest,
    ) -> Result<QrLoginStatusResponse, HttpError> {
        self.post_json("/api/v1/qrcode/login/status", None, None, Some(req))
            .await
    }

    /// Redeems a confirmed QR login for a user token (`POST /api/v1/login/by_qrcode`).
    pub async fn login_by_qrcode(
        &self,
        req: &LoginByQrcodeRequest,
    ) -> Result<LoginResponse, HttpError> {
        self.post_json("/api/v1/login/by_qrcode", None, None, Some(req))
            .await
    }

    /// Fetches the authenticated user's profile (`GET /api/v1/user/info`).
    pub async fn user_info(
        &self,
        user_token: &str,
        device_id: Option<&str>,
    ) -> Result<UserInfoResponse, HttpError> {
        self.get("/api/v1/user/info", Some(user_token), device_id)
            .await
    }

    /// Ends the current user session (`POST /api/v1/user/logout`).
    pub async fn user_logout(
        &self,
        user_token: &str,
        device_id: Option<&str>,
    ) -> Result<EmptyData, HttpError> {
        self.post_json::<(), _>("/api/v1/user/logout", Some(user_token), device_id, None)
            .await
    }

    /// Lists owned and current devices (`GET /api/v1/device/list`).
    pub async fn device_list(
        &self,
        user_token: &str,
        device_id: Option<&str>,
    ) -> Result<DeviceListResponse, HttpError> {
        self.get("/api/v1/device/list", Some(user_token), device_id)
            .await
    }

    /// Lists owned devices grouped by form factor (`GET /api/v1/device/groups/of/my`).
    pub async fn device_groups(
        &self,
        user_token: &str,
        device_id: Option<&str>,
    ) -> Result<DeviceGroupsResponse, HttpError> {
        self.get("/api/v1/device/groups/of/my", Some(user_token), device_id)
            .await
    }

    /// Fetches per-device preferences (`GET /api/v1/device/{id}/preferences`).
    pub async fn device_preferences(
        &self,
        user_token: &str,
        target_device_id: &str,
        caller_device_id: Option<&str>,
    ) -> Result<DevicePreferencesResponse, HttpError> {
        let path = format!("/api/v1/device/{target_device_id}/preferences");
        self.get(&path, Some(user_token), caller_device_id).await
    }

    /// Updates whether the calling macOS host allows incoming control
    /// (`POST /api/v1/device/mac_controllable`).
    pub async fn mac_controllable(
        &self,
        user_token: &str,
        device_id: &str,
        controllable: bool,
    ) -> Result<EmptyData, HttpError> {
        let body = MacControllableRequest { controllable };
        self.post_json(
            "/api/v1/device/mac_controllable",
            Some(user_token),
            Some(device_id),
            Some(&body),
        )
        .await
    }

    /// Fetches the calling device's share ID and display alias (`GET /api/v1/device/share/info`).
    pub async fn device_share_info(
        &self,
        user_token: &str,
        device_id: &str,
    ) -> Result<DeviceShareInfoResponse, HttpError> {
        self.get(
            "/api/v1/device/share/info",
            Some(user_token),
            Some(device_id),
        )
        .await
    }

    /// Rotates the calling device's share ID (`POST /api/v1/device/share/info/refresh_id`).
    pub async fn device_share_info_refresh(
        &self,
        user_token: &str,
        device_id: &str,
    ) -> Result<DeviceShareInfoResponse, HttpError> {
        self.post_json::<(), _>(
            "/api/v1/device/share/info/refresh_id",
            Some(user_token),
            Some(device_id),
            None,
        )
        .await
    }

    /// Cancels active remote-assistance sharing on the calling device
    /// (`POST /api/v1/device/share/cancel`).
    pub async fn device_share_cancel(
        &self,
        user_token: &str,
        device_id: &str,
    ) -> Result<EmptyData, HttpError> {
        self.post_json::<(), _>(
            "/api/v1/device/share/cancel",
            Some(user_token),
            Some(device_id),
            None,
        )
        .await
    }

    /// Removes a shared device from the caller's list (`POST /api/v1/device/share/{id}/unbind`).
    pub async fn device_share_unbind(
        &self,
        user_token: &str,
        target_id: &str,
        caller_device_id: Option<&str>,
    ) -> Result<EmptyData, HttpError> {
        let path = format!("/api/v1/device/share/{target_id}/unbind");
        self.post_json::<(), _>(&path, Some(user_token), caller_device_id, None)
            .await
    }

    /// Lists saved assisted devices (`GET /api/v1/device/share/saved` or `/saved/{kind}`).
    pub async fn device_share_saved(
        &self,
        user_token: &str,
        kind: Option<SavedShareKind>,
        cursor: Option<&str>,
        limit: Option<usize>,
        caller_device_id: Option<&str>,
    ) -> Result<AssistDeviceRecordPage, HttpError> {
        let base = match kind {
            Some(k) => format!("/api/v1/device/share/saved/{}", k.as_str()),
            None => "/api/v1/device/share/saved".to_owned(),
        };
        let query = {
            let mut qs = url::form_urlencoded::Serializer::new(String::new());
            if let Some(c) = cursor {
                qs.append_pair("cursor", c);
            }
            if let Some(l) = limit {
                qs.append_pair("limit", &l.to_string());
            }
            qs.finish()
        };
        let path = if query.is_empty() {
            base
        } else {
            format!("{base}?{query}")
        };
        self.get(&path, Some(user_token), caller_device_id).await
    }

    /// Upserts favorite assisted devices (`POST /api/v1/device/share/saved/favorites`).
    pub async fn device_share_saved_favorites_post(
        &self,
        user_token: &str,
        caller_device_id: Option<&str>,
        req: &AssistFavoritesRequest,
    ) -> Result<EmptyData, HttpError> {
        self.post_json(
            "/api/v1/device/share/saved/favorites",
            Some(user_token),
            caller_device_id,
            Some(req),
        )
        .await
    }

    /// Deletes a saved assisted device record
    /// (`DELETE /api/v1/device/share/saved/{kind}/{publisher_device_id}`).
    pub async fn device_share_saved_delete(
        &self,
        user_token: &str,
        kind: SavedShareKind,
        publisher_device_id: &str,
        caller_device_id: Option<&str>,
    ) -> Result<EmptyData, HttpError> {
        let path = format!(
            "/api/v1/device/share/saved/{}/{publisher_device_id}",
            kind.as_str()
        );
        self.delete(&path, Some(user_token), caller_device_id).await
    }

    /// Creates a guest identity for an already-registered device (`POST /api/v1/guest/create`).
    pub async fn guest_create(&self, device_id: &str) -> Result<GuestCreateResponse, HttpError> {
        self.post_json::<(), _>("/api/v1/guest/create", None, Some(device_id), None)
            .await
    }

    /// Creates or renews a signaling room for a guest host (`POST /api/v1/guest/room/create`).
    pub async fn guest_room_create(
        &self,
        guest_token: &str,
        device_id: &str,
        guest_id: Option<&str>,
    ) -> Result<RoomCreateResponse, HttpError> {
        let body =
            serde_json::to_vec(&RoomCreateRequest::default()).map_err(HttpError::Serialize)?;
        let extra = guest_id_param(guest_id);
        self.send_signed_with_extra(
            Method::POST,
            "/api/v1/guest/room/create",
            Some(guest_token),
            Some(device_id),
            &extra,
            body,
        )
        .await
    }

    /// Publishes a guest host's share ID once its room exists (`POST /api/v1/guest/share/info`).
    pub async fn guest_share_info(
        &self,
        guest_token: &str,
        device_id: &str,
        guest_id: Option<&str>,
    ) -> Result<DeviceShareInfoResponse, HttpError> {
        let extra = guest_id_param(guest_id);
        self.send_signed_with_extra(
            Method::POST,
            "/api/v1/guest/share/info",
            Some(guest_token),
            Some(device_id),
            &extra,
            Vec::new(),
        )
        .await
    }

    /// Cancels a guest host's share session (`POST /api/v1/guest/share/cancel`).
    pub async fn guest_share_cancel(
        &self,
        guest_token: &str,
        device_id: &str,
        guest_id: Option<&str>,
    ) -> Result<EmptyData, HttpError> {
        let extra = guest_id_param(guest_id);
        self.send_signed_with_extra(
            Method::POST,
            "/api/v1/guest/share/cancel",
            Some(guest_token),
            Some(device_id),
            &extra,
            Vec::new(),
        )
        .await
    }

    /// Uploads a guest host's verification mode (`POST /api/v1/guest/room/share/upload_control_mode`).
    pub async fn guest_upload_control_mode(
        &self,
        guest_token: &str,
        device_id: &str,
        guest_id: Option<&str>,
        req: &UploadControlModeRequest,
    ) -> Result<EmptyData, HttpError> {
        let body = serde_json::to_vec(req).map_err(HttpError::Serialize)?;
        let extra = guest_id_param(guest_id);
        self.send_signed_with_extra(
            Method::POST,
            "/api/v1/guest/room/share/upload_control_mode",
            Some(guest_token),
            Some(device_id),
            &extra,
            body,
        )
        .await
    }

    /// Uploads a guest host's salted verification code digest
    /// (`POST /api/v1/guest/room/share/upload/sign`).
    pub async fn guest_upload_sign(
        &self,
        guest_token: &str,
        device_id: &str,
        guest_id: Option<&str>,
        req: &UploadShareSignRequest,
    ) -> Result<EmptyData, HttpError> {
        let body = serde_json::to_vec(req).map_err(HttpError::Serialize)?;
        let extra = guest_id_param(guest_id);
        self.send_signed_with_extra(
            Method::POST,
            "/api/v1/guest/room/share/upload/sign",
            Some(guest_token),
            Some(device_id),
            &extra,
            body,
        )
        .await
    }

    /// Approves or denies a confirmation prompt on a guest host
    /// (`POST /api/v1/guest/room/share/confirmation`).
    pub async fn guest_confirmation(
        &self,
        guest_token: &str,
        device_id: &str,
        guest_id: Option<&str>,
        req: &ShareConfirmationRequest,
    ) -> Result<EmptyData, HttpError> {
        let body = serde_json::to_vec(req).map_err(HttpError::Serialize)?;
        let extra = guest_id_param(guest_id);
        self.send_signed_with_extra(
            Method::POST,
            "/api/v1/guest/room/share/confirmation",
            Some(guest_token),
            Some(device_id),
            &extra,
            body,
        )
        .await
    }

    /// Queries a remote-assistance target's verification mode (`POST /api/v2/room/share/control_mode`).
    pub async fn room_share_control_mode(
        &self,
        user_token: &str,
        caller_device_id: Option<&str>,
        req: &ShareControlModeRequest,
    ) -> Result<ShareControlModeResponse, HttpError> {
        self.post_json(
            "/api/v2/room/share/control_mode",
            Some(user_token),
            caller_device_id,
            Some(req),
        )
        .await
    }

    /// Uploads an account host's verification mode (`POST /api/v2/room/share/upload_control_mode`).
    pub async fn room_share_upload_control_mode(
        &self,
        user_token: &str,
        device_id: &str,
        req: &UploadControlModeRequest,
    ) -> Result<EmptyData, HttpError> {
        self.post_json(
            "/api/v2/room/share/upload_control_mode",
            Some(user_token),
            Some(device_id),
            Some(req),
        )
        .await
    }

    /// Uploads an account host's salted code digest (`POST /api/v2/room/share/upload_sign`).
    pub async fn room_share_upload_sign(
        &self,
        user_token: &str,
        device_id: &str,
        req: &UploadShareSignRequest,
    ) -> Result<EmptyData, HttpError> {
        self.post_json(
            "/api/v2/room/share/upload_sign",
            Some(user_token),
            Some(device_id),
            Some(req),
        )
        .await
    }

    /// Approves or denies a confirmation prompt on an account host
    /// (`POST /api/v2/room/share/confirmation`).
    pub async fn room_share_confirmation(
        &self,
        user_token: &str,
        device_id: &str,
        req: &ShareConfirmationRequest,
    ) -> Result<EmptyData, HttpError> {
        self.post_json(
            "/api/v2/room/share/confirmation",
            Some(user_token),
            Some(device_id),
            Some(req),
        )
        .await
    }

    /// Joins a remote-assistance session using the target's verification code
    /// (`POST /api/v2/room/join/share/by_code`).
    pub async fn room_join_share_by_code(
        &self,
        user_token: &str,
        caller_device_id: Option<&str>,
        req: &JoinShareByCodeRequest,
    ) -> Result<AssistRoomConfig, HttpError> {
        self.post_json(
            "/api/v2/room/join/share/by_code",
            Some(user_token),
            caller_device_id,
            Some(req),
        )
        .await
    }

    /// Starts or polls a confirmation-based remote-assistance join
    /// (`POST /api/v2/room/join/share/by_confirmation`).
    pub async fn room_join_share_by_confirmation(
        &self,
        user_token: &str,
        caller_device_id: Option<&str>,
        req: &JoinShareByConfirmationRequest,
    ) -> Result<JoinShareByConfirmationResult, HttpError> {
        let body = serde_json::to_vec(req).map_err(HttpError::Serialize)?;
        let (status, bytes) = self
            .send_signed_raw(
                Method::POST,
                "/api/v2/room/join/share/by_confirmation",
                Some(user_token),
                caller_device_id,
                &[],
                body,
            )
            .await?;
        parse_join_confirmation_response(status, &bytes).map_err(HttpError::Api)
    }

    /// Cancels a pending confirmation request (`POST /api/v2/room/share/cancel_remote_assist`).
    pub async fn room_share_cancel_remote_assist(
        &self,
        user_token: &str,
        caller_device_id: Option<&str>,
        req: &CancelRemoteAssistRequest,
    ) -> Result<EmptyData, HttpError> {
        self.post_json(
            "/api/v2/room/share/cancel_remote_assist",
            Some(user_token),
            caller_device_id,
            Some(req),
        )
        .await
    }

    /// Queries release update metadata for the calling client (`GET /api/v1/release/fetch/update`).
    pub async fn release_fetch_update(
        &self,
        user_token: &str,
        device_id: Option<&str>,
    ) -> Result<ReleaseUpdateResponse, HttpError> {
        self.get("/api/v1/release/fetch/update", Some(user_token), device_id)
            .await
    }

    /// Queries release update metadata for a bound target device
    /// (`GET /api/v1/release/fetch/update/{id}`).
    pub async fn release_fetch_update_for_device(
        &self,
        user_token: &str,
        target_device_id: &str,
        caller_device_id: Option<&str>,
        platform: Option<u8>,
        version_name: Option<&str>,
    ) -> Result<ReleaseUpdateResponse, HttpError> {
        let query = {
            let mut qs = url::form_urlencoded::Serializer::new(String::new());
            if let Some(p) = platform {
                qs.append_pair("platform", &p.to_string());
            }
            if let Some(vn) = version_name {
                qs.append_pair("version_name", vn);
            }
            qs.finish()
        };
        let path = if query.is_empty() {
            format!("/api/v1/release/fetch/update/{target_device_id}")
        } else {
            format!("/api/v1/release/fetch/update/{target_device_id}?{query}")
        };
        self.get(&path, Some(user_token), caller_device_id).await
    }

    /// Unbinds `target_device_id` from the authenticated user (`POST /api/v1/device/{id}/unbind`).
    pub async fn device_unbind(
        &self,
        user_token: &str,
        target_device_id: &str,
        caller_device_id: Option<&str>,
    ) -> Result<EmptyData, HttpError> {
        let path = format!("/api/v1/device/{target_device_id}/unbind");
        self.post_json::<(), _>(&path, Some(user_token), caller_device_id, None)
            .await
    }

    /// Creates a host signaling room (`POST /api/v1/room/create`).
    pub async fn room_create(
        &self,
        user_token: &str,
        device_id: &str,
    ) -> Result<RoomCreateResponse, HttpError> {
        let empty = serde_json::json!({});
        self.post_json(
            "/api/v1/room/create",
            Some(user_token),
            Some(device_id),
            Some(&empty),
        )
        .await
    }

    /// Requests a viewer room ticket for `target_device_id` (`POST /api/v1/room/join/by_device/{id}`).
    pub async fn room_join_by_device(
        &self,
        user_token: &str,
        target_device_id: &str,
        caller_device_id: Option<&str>,
        req: &RoomJoinRequest,
    ) -> Result<RoomJoinResponse, HttpError> {
        let path = format!("/api/v1/room/join/by_device/{target_device_id}");
        self.post_json(&path, Some(user_token), caller_device_id, Some(req))
            .await
    }

    /// Executes a pre-signed HTTP request emitted by `remora_proto::machine::AuthMachine`.
    pub async fn execute_signed(
        &self,
        req: SignedHttpRequest,
    ) -> Result<(u16, Vec<u8>), HttpError> {
        let url = self.join_url(&req.path)?;
        let method =
            Method::from_bytes(req.method.as_bytes()).map_err(|_| HttpError::InvalidHeader)?;
        let mut headers = HeaderMap::new();
        for (name, value) in &req.headers {
            let h_name =
                HeaderName::from_bytes(name.as_bytes()).map_err(|_| HttpError::InvalidHeader)?;
            let h_val = HeaderValue::from_str(value).map_err(|_| HttpError::InvalidHeader)?;
            headers.insert(h_name, h_val);
        }
        #[cfg(feature = "record")]
        if let Some(rec) = &self.recorder {
            rec.record_http_request(req.method, &req.path, &req.body);
        }

        let response = self
            .client
            .request(method, url)
            .headers(headers)
            .body(req.body)
            .send()
            .await?;
        let status = response.status().as_u16();
        let bytes = response.bytes().await?.to_vec();

        #[cfg(feature = "record")]
        if let Some(rec) = &self.recorder {
            rec.record_http_response(req.method, &req.path, status, &bytes);
        }

        Ok((status, bytes))
    }

    async fn get<Resp: DeserializeOwned>(
        &self,
        path: &str,
        user_token: Option<&str>,
        device_id: Option<&str>,
    ) -> Result<Resp, HttpError> {
        self.send_signed_with_extra(Method::GET, path, user_token, device_id, &[], Vec::new())
            .await
    }

    async fn delete<Resp: DeserializeOwned>(
        &self,
        path: &str,
        user_token: Option<&str>,
        device_id: Option<&str>,
    ) -> Result<Resp, HttpError> {
        self.send_signed_with_extra(Method::DELETE, path, user_token, device_id, &[], Vec::new())
            .await
    }

    async fn post_json<Req: Serialize, Resp: DeserializeOwned>(
        &self,
        path: &str,
        user_token: Option<&str>,
        device_id: Option<&str>,
        body: Option<&Req>,
    ) -> Result<Resp, HttpError> {
        let bytes = match body {
            Some(payload) => serde_json::to_vec(payload).map_err(HttpError::Serialize)?,
            None => Vec::new(),
        };
        self.send_signed_with_extra(Method::POST, path, user_token, device_id, &[], bytes)
            .await
    }

    async fn send_signed_with_extra<Resp: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        user_token: Option<&str>,
        device_id: Option<&str>,
        extra_params: &[(&str, &str)],
        body: Vec<u8>,
    ) -> Result<Resp, HttpError> {
        let (status, bytes) = self
            .send_signed_raw(method, path, user_token, device_id, extra_params, body)
            .await?;
        parse_response(status, &bytes).map_err(HttpError::Api)
    }

    async fn send_signed_raw(
        &self,
        method: Method,
        path: &str,
        user_token: Option<&str>,
        device_id: Option<&str>,
        extra_params: &[(&str, &str)],
        body: Vec<u8>,
    ) -> Result<(u16, Vec<u8>), HttpError> {
        let url = self.join_url(path)?;
        let signed_path = match url.query() {
            Some(q) => format!("{}?{q}", url.path()),
            None => url.path().to_owned(),
        };
        let headers = self.build_signed_headers(
            method.as_str(),
            &signed_path,
            user_token,
            device_id,
            extra_params,
            &body,
        )?;

        #[cfg(feature = "record")]
        if let Some(rec) = &self.recorder {
            rec.record_http_request(method.as_str(), &signed_path, &body);
        }

        let response = self
            .client
            .request(method.clone(), url)
            .headers(headers)
            .body(body)
            .send()
            .await?;
        let status = response.status().as_u16();
        let bytes = response.bytes().await?.to_vec();

        #[cfg(feature = "record")]
        if let Some(rec) = &self.recorder {
            rec.record_http_response(method.as_str(), &signed_path, status, &bytes);
        }

        Ok((status, bytes))
    }

    fn join_url(&self, path: &str) -> Result<Url, HttpError> {
        self.base_url.join(path).map_err(HttpError::InvalidUrl)
    }

    fn build_signed_headers(
        &self,
        method: &str,
        path: &str,
        user_token: Option<&str>,
        device_id: Option<&str>,
        extra_params: &[(&str, &str)],
        body: &[u8],
    ) -> Result<HeaderMap, HttpError> {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            .to_string();
        let mut nonce_bytes = [0u8; 8];
        rand::rng().fill_bytes(&mut nonce_bytes);
        let nonce = hex::encode(nonce_bytes);
        let build = self.profile.build.to_string();

        let mut param_pairs: Vec<(String, String)> = vec![
            (format!("{PARAM_PREFIX}plat"), self.profile.plat.to_owned()),
            (format!("{PARAM_PREFIX}vn"), self.profile.version.to_owned()),
            (format!("{PARAM_PREFIX}vc"), build),
            (
                format!("{PARAM_PREFIX}chn"),
                self.profile.channel.to_owned(),
            ),
            (
                format!("{PARAM_PREFIX}pkgn"),
                self.profile.bundle_id.to_owned(),
            ),
            (format!("{PARAM_PREFIX}ts"), ts),
            (format!("{PARAM_PREFIX}nonce"), nonce),
        ];
        if let Some(dev) = device_id
            && !dev.is_empty()
        {
            param_pairs.push((format!("{PARAM_PREFIX}device-id"), dev.to_owned()));
        }
        for (suffix, value) in extra_params {
            if !value.is_empty() {
                param_pairs.push((format!("{PARAM_PREFIX}{suffix}"), (*value).to_owned()));
            }
        }

        let signature = sign(
            &self.sign_key,
            method,
            path,
            param_pairs.iter().map(|(k, v)| (k.as_str(), v.as_str())),
            body,
        );
        param_pairs.push((SIGN_HEADER.to_owned(), signature));

        let mut headers = HeaderMap::new();
        for (name, value) in param_pairs {
            let h_name =
                HeaderName::from_bytes(name.as_bytes()).map_err(|_| HttpError::InvalidHeader)?;
            let h_val = HeaderValue::from_str(&value).map_err(|_| HttpError::InvalidHeader)?;
            headers.insert(h_name, h_val);
        }
        if !body.is_empty() {
            headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        }
        if let Some(token) = user_token {
            let auth = HeaderValue::from_str(&format!("Bearer {token}"))
                .map_err(|_| HttpError::InvalidHeader)?;
            headers.insert(AUTHORIZATION, auth);
        }
        Ok(headers)
    }
}

fn guest_id_param(guest_id: Option<&str>) -> [(&'static str, &str); 1] {
    [("guest-id", guest_id.unwrap_or(""))]
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use remora_proto::control::DeviceStatus;
    use remora_proto::profile;
    use remora_proto::server::verify_request;
    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;

    type Handler =
        Arc<dyn Fn(&str, &str, &HashMap<String, String>, &[u8]) -> (u16, String) + Send + Sync>;

    async fn spawn_fake_http(handler: Handler) -> Url {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let handler = Arc::clone(&handler);
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut tmp = [0u8; 4096];
                    let head_end;
                    loop {
                        let n = stream.read(&mut tmp).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&tmp[..n]);
                        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            head_end = pos;
                            break;
                        }
                    }
                    let head_str = std::str::from_utf8(&buf[..head_end]).unwrap();
                    let mut lines = head_str.lines();
                    let req_line = lines.next().unwrap();
                    let mut parts = req_line.split_whitespace();
                    let method = parts.next().unwrap().to_owned();
                    let path = parts.next().unwrap().to_owned();

                    let mut headers = HashMap::new();
                    let mut content_len = 0usize;
                    for line in lines {
                        if let Some((k, v)) = line.split_once(':') {
                            let k_lower = k.trim().to_ascii_lowercase();
                            let v_trim = v.trim().to_owned();
                            if k_lower == "content-length" {
                                content_len = v_trim.parse().unwrap_or(0);
                            }
                            headers.insert(k_lower, v_trim);
                        }
                    }
                    let body_start = head_end + 4;
                    while buf.len() < body_start + content_len {
                        let n = stream.read(&mut tmp).await.unwrap_or(0);
                        if n == 0 {
                            break;
                        }
                        buf.extend_from_slice(&tmp[..n]);
                    }
                    let body = &buf[body_start..body_start + content_len];
                    let (status, resp_body) = handler(&method, &path, &headers, body);
                    let resp = format!(
                        "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{resp_body}",
                        resp_body.len()
                    );
                    let _ = stream.write_all(resp.as_bytes()).await;
                });
            }
        });
        Url::parse(&format!("http://{addr}")).unwrap()
    }

    #[tokio::test]
    async fn signs_requests_and_returns_proto_types() {
        let sign_key = SignKey::new("test-sign-key");
        let seen_paths = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen_clone = Arc::clone(&seen_paths);
        let key_clone = sign_key.clone();

        let url = spawn_fake_http(Arc::new(move |method, path, headers, body| {
            let hdr_iter = headers.iter().map(|(k, v)| (k.as_str(), v.as_str()));
            assert_eq!(
                verify_request(&key_clone, method, path, hdr_iter, body),
                Ok(())
            );
            assert_eq!(headers.get("x-param-plat").map(String::as_str), Some("1"));
            assert_eq!(
                headers.get("x-param-vn").map(String::as_str),
                Some("4.48.2.5754")
            );
            seen_clone.lock().unwrap().push(format!("{method} {path}"));

            match (method, path) {
                ("POST", "/api/v1/device/windows/init") => (
                    200,
                    json!({"code": 0, "msg": "ok", "data": {"device_id": "dev-1"}}).to_string(),
                ),
                ("POST", "/api/v1/qrcode/gen/login") => (
                    200,
                    json!({
                        "code": 0,
                        "msg": "ok",
                        "data": {
                            "qrcode_jump_url": "https://remora.test/login/abc",
                            "status_query_ticket": "ticket-1"
                        }
                    })
                    .to_string(),
                ),
                ("POST", "/api/v1/qrcode/login/status") => (
                    200,
                    json!({"code": 0, "msg": "ok", "data": {"status": 4}}).to_string(),
                ),
                ("POST", "/api/v1/login/by_qrcode") => (
                    200,
                    json!({
                        "code": 0,
                        "msg": "ok",
                        "data": {"token": "jwt-1", "user_id": "1", "nickname": "Owner"}
                    })
                    .to_string(),
                ),
                ("GET", "/api/v1/device/list") => {
                    assert_eq!(
                        headers.get("authorization").map(String::as_str),
                        Some("Bearer jwt-1")
                    );
                    assert_eq!(
                        headers.get("x-param-device-id").map(String::as_str),
                        Some("dev-1")
                    );
                    (
                        200,
                        json!({
                            "code": 0,
                            "msg": "ok",
                            "data": {
                                "current_device": null,
                                "my_binded_devices": [{
                                    "device_id": "dev-1",
                                    "alias": "Mac",
                                    "platform": 1,
                                    "status": "CONNECTED"
                                }],
                                "others_shared_devices": []
                            }
                        })
                        .to_string(),
                    )
                }
                ("GET", "/api/v1/device/groups/of/my") => (
                    200,
                    json!({
                        "code": 0,
                        "msg": "ok",
                        "data": {
                            "current_device_id": "dev-1",
                            "mobile_devices": [],
                            "desktop_devices": [{
                                "device_id": "dev-1",
                                "alias": "Mac",
                                "platform": 4,
                                "status": "CONNECTED",
                                "controllable": true
                            }],
                            "tv_devices": []
                        }
                    })
                    .to_string(),
                ),
                ("POST", "/api/v1/device/mac_controllable") => {
                    (200, json!({"code": 0, "msg": "ok", "data": {}}).to_string())
                }
                ("GET", "/api/v1/device/dev-1/preferences") => (
                    200,
                    json!({
                        "code": 0,
                        "msg": "ok",
                        "data": {
                            "device_preferences": {
                                "device_id": "dev-1",
                                "tools": { "sequence": ["file_transfer"] }
                            }
                        }
                    })
                    .to_string(),
                ),
                ("GET", "/api/v1/device/share/info") => (
                    200,
                    json!({
                        "code": 0,
                        "msg": "ok",
                        "data": {
                            "connect_id": "123456789",
                            "connect_code": "",
                            "alias": "Mac"
                        }
                    })
                    .to_string(),
                ),
                ("GET", "/api/v1/release/fetch/update") => (
                    200,
                    json!({
                        "code": 0,
                        "msg": "ok",
                        "data": {
                            "url": "",
                            "md5": "",
                            "version_name": "4.48.2",
                            "version_code": 635,
                            "change_logs": [],
                            "subtitle": "",
                            "user_external_force_upgrade": false,
                            "upgrade_uri": {},
                            "update_type": "NoUpdate",
                            "update_prompt_type": ""
                        }
                    })
                    .to_string(),
                ),
                (
                    "GET",
                    "/api/v1/release/fetch/update/dev-1?platform=1&version_name=4.45.0.4318",
                ) => (
                    200,
                    json!({
                        "code": 0,
                        "msg": "ok",
                        "data": {
                            "url": "",
                            "md5": "",
                            "version_name": "4.45.0.4318",
                            "version_code": 0,
                            "change_logs": [],
                            "subtitle": "",
                            "user_external_force_upgrade": false,
                            "upgrade_uri": {},
                            "update_type": "NoUpdate",
                            "update_prompt_type": ""
                        }
                    })
                    .to_string(),
                ),
                ("POST", "/api/v1/room/create") => (
                    200,
                    json!({
                        "code": 0,
                        "msg": "ok",
                        "data": {
                            "signaling_server": "ws://127.0.0.1:9999",
                            "signaling_list": ["ws://127.0.0.1:9999"],
                            "token": "room-tok",
                            "ws_connect_timeout_ms": 10000
                        }
                    })
                    .to_string(),
                ),
                ("POST", "/api/v1/room/join/by_device/dev-1") => (
                    200,
                    json!({
                        "code": 0,
                        "msg": "ok",
                        "data": {
                            "signaling_list": ["ws://127.0.0.1:9999"],
                            "token": "viewer-room-tok",
                            "ws_connect_timeout_ms": 10000,
                            "report_token": "",
                            "report_url": "",
                            "publisher_platform": 1
                        }
                    })
                    .to_string(),
                ),
                ("GET", "/api/v1/user/info") => (
                    422,
                    json!({"code": 1120, "msg": "login expired", "data": {}}).to_string(),
                ),
                _ => (404, json!({"code": 1001, "msg": "not found"}).to_string()),
            }
        }))
        .await;

        let profile = profile::by_id("windows-4.48.2").unwrap();
        let client = ControlClient::new(url, profile, sign_key).unwrap();

        let init = client
            .device_init(&DeviceInitRequest {
                client_id: "cid-1".into(),
                name: "Host".into(),
            })
            .await
            .unwrap();
        assert_eq!(init.device_id, "dev-1");

        let qr = client.qr_login_generate(Some("dev-1")).await.unwrap();
        let status = client
            .qr_login_status(&QrLoginStatusRequest {
                token: qr.status_query_ticket.clone(),
                qrcode_jump_url: qr.qrcode_jump_url.clone(),
                login_status: 1,
            })
            .await
            .unwrap();
        assert_eq!(status.status, 4);

        let login = client
            .login_by_qrcode(&LoginByQrcodeRequest {
                token: qr.status_query_ticket,
                qrcode_jump_url: qr.qrcode_jump_url,
            })
            .await
            .unwrap();
        assert_eq!(login.user_id, "1");

        let devices = client
            .device_list(&login.token, Some("dev-1"))
            .await
            .unwrap();
        assert_eq!(devices.my_binded_devices.len(), 1);
        assert_eq!(devices.my_binded_devices[0].status, DeviceStatus::Connected);

        let groups = client
            .device_groups(&login.token, Some("dev-1"))
            .await
            .unwrap();
        assert_eq!(groups.desktop_devices.len(), 1);
        assert_eq!(groups.desktop_devices[0].controllable, Some(true));

        client
            .mac_controllable(&login.token, "dev-1", false)
            .await
            .unwrap();
        let prefs = client
            .device_preferences(&login.token, "dev-1", Some("dev-1"))
            .await
            .unwrap();
        assert_eq!(prefs.device_preferences.device_id, "dev-1");

        let share = client
            .device_share_info(&login.token, "dev-1")
            .await
            .unwrap();
        assert_eq!(share.connect_id, "123456789");

        let upd = client
            .release_fetch_update(&login.token, Some("dev-1"))
            .await
            .unwrap();
        assert_eq!(upd.update_type, "NoUpdate");

        let dev_upd = client
            .release_fetch_update_for_device(
                &login.token,
                "dev-1",
                Some("dev-1"),
                Some(1),
                Some("4.45.0.4318"),
            )
            .await
            .unwrap();
        assert_eq!(dev_upd.version_name, "4.45.0.4318");

        let created = client.room_create(&login.token, "dev-1").await.unwrap();
        assert_eq!(created.ws_connect_timeout_ms, 10000);

        let joined = client
            .room_join_by_device(
                &login.token,
                "dev-1",
                None,
                &RoomJoinRequest { force_join: false },
            )
            .await
            .unwrap();
        assert_eq!(joined.publisher_platform, 1);

        let err = client.user_info(&login.token, None).await.unwrap_err();
        assert!(matches!(err, HttpError::Api(ApiError::TokenExpired)));
        assert!(!seen_paths.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn guest_and_share_requests_sign_headers_and_parse_confirmation() {
        let sign_key = SignKey::new("test-sign-key");
        let key_clone = sign_key.clone();
        let poll_count = Arc::new(Mutex::new(0u8));
        let poll_clone = Arc::clone(&poll_count);

        let url = spawn_fake_http(Arc::new(move |method, path, headers, body| {
            let hdr_iter = headers.iter().map(|(k, v)| (k.as_str(), v.as_str()));
            assert_eq!(
                verify_request(&key_clone, method, path, hdr_iter, body),
                Ok(())
            );
            match (method, path) {
                ("POST", "/api/v1/guest/create") => {
                    assert_eq!(
                        headers.get("x-param-device-id").map(String::as_str),
                        Some("dev-host")
                    );
                    (
                        200,
                        json!({
                            "code": 0,
                            "msg": "ok",
                            "data": { "guest_id": "guest-1", "token": "guest-jwt" }
                        })
                        .to_string(),
                    )
                }
                ("POST", "/api/v1/guest/room/create") => {
                    assert_eq!(
                        headers.get("authorization").map(String::as_str),
                        Some("Bearer guest-jwt")
                    );
                    assert_eq!(
                        headers.get("x-param-guest-id").map(String::as_str),
                        Some("guest-1")
                    );
                    (
                        200,
                        json!({
                            "code": 0,
                            "msg": "ok",
                            "data": {
                                "signaling_server": "ws://127.0.0.1:9999",
                                "signaling_list": ["ws://127.0.0.1:9999"],
                                "token": "guest-room-tok",
                                "ws_connect_timeout_ms": 16000
                            }
                        })
                        .to_string(),
                    )
                }
                ("POST", "/api/v1/guest/share/info") => (
                    200,
                    json!({
                        "code": 0,
                        "msg": "ok",
                        "data": { "connect_id": "243829726", "connect_code": "", "alias": "Host" }
                    })
                    .to_string(),
                ),
                ("POST", "/api/v2/room/join/share/by_confirmation") => {
                    let mut count = poll_clone.lock().unwrap();
                    *count += 1;
                    if *count == 1 {
                        (
                            400,
                            json!({
                                "code": 1136,
                                "msg": "waiting",
                                "data": { "control_id": "ctrl-1" }
                            })
                            .to_string(),
                        )
                    } else {
                        (
                            200,
                            json!({
                                "code": 0,
                                "msg": "ok",
                                "data": {
                                    "signaling_server": "ws://127.0.0.1:9999",
                                    "signaling_list": ["ws://127.0.0.1:9999"],
                                    "token": "assist-room-tok",
                                    "ws_connect_timeout_ms": 16000,
                                    "streamer_retry_delta_ms": 1000,
                                    "max_reconnect_delta": 300,
                                    "share_id": "243829726",
                                    "publisher_platform": 1,
                                    "publisher_device_id": "dev-host"
                                }
                            })
                            .to_string(),
                        )
                    }
                }
                ("GET", "/api/v1/device/share/saved/favorites?limit=10") => (
                    200,
                    json!({
                        "code": 0,
                        "msg": "ok",
                        "data": { "saved": [], "next": null }
                    })
                    .to_string(),
                ),
                ("DELETE", "/api/v1/device/share/saved/recent/dev-host") => (
                    200,
                    json!({ "code": 0, "msg": "ok", "data": {} }).to_string(),
                ),
                _ => (404, json!({"code": 1001, "msg": "not found"}).to_string()),
            }
        }))
        .await;

        let profile = profile::by_id("windows-4.48.2").unwrap();
        let client = ControlClient::new(url, profile, sign_key).unwrap();

        let guest = client.guest_create("dev-host").await.unwrap();
        let room = client
            .guest_room_create(&guest.token, "dev-host", Some(&guest.guest_id))
            .await
            .unwrap();
        assert_eq!(room.token, "guest-room-tok");

        let info = client
            .guest_share_info(&guest.token, "dev-host", Some(&guest.guest_id))
            .await
            .unwrap();
        assert_eq!(info.connect_id, "243829726");

        let pending = client
            .room_join_share_by_confirmation(
                "user-tok",
                None,
                &JoinShareByConfirmationRequest {
                    connect_id: info.connect_id.clone(),
                    control_id: None,
                    force_join: false,
                },
            )
            .await
            .unwrap();
        assert_eq!(
            pending,
            JoinShareByConfirmationResult::Pending {
                control_id: "ctrl-1".into()
            }
        );

        let approved = client
            .room_join_share_by_confirmation(
                "user-tok",
                None,
                &JoinShareByConfirmationRequest {
                    connect_id: info.connect_id,
                    control_id: Some("ctrl-1".into()),
                    force_join: false,
                },
            )
            .await
            .unwrap();
        assert!(matches!(
            approved,
            JoinShareByConfirmationResult::Approved(cfg) if cfg.token == "assist-room-tok"
        ));

        let favs = client
            .device_share_saved(
                "user-tok",
                Some(SavedShareKind::Favorites),
                None,
                Some(10),
                None,
            )
            .await
            .unwrap();
        assert!(favs.saved.is_empty());

        client
            .device_share_saved_delete("user-tok", SavedShareKind::Recent, "dev-host", None)
            .await
            .unwrap();
    }
}
