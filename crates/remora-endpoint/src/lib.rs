//! High-level UU Remote endpoint SDK built on [`remora_proto`].

pub use remora_proto;

#[cfg(feature = "client")]
pub mod client;
#[cfg(feature = "peer")]
pub mod peer;
pub mod platform;
#[cfg(feature = "record")]
pub mod record;
#[cfg(all(feature = "client", feature = "peer"))]
pub mod role;

#[cfg(feature = "client")]
mod sdk {
    use std::fmt;
    use std::ops::ControlFlow;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::task::{Context, Poll};
    use std::time::Duration;

    use futures_core::Stream;
    use rand::RngCore;
    use rand::SeedableRng;
    use remora_proto::control::{
        ApiError, DeviceGroupsResponse, DeviceInitRequest, DeviceInitResponse, DeviceListResponse,
        DevicePreferencesResponse, LoginByQrcodeRequest, LoginResponse, QrCodeStatus,
        QrLoginGenResponse, QrLoginStatusRequest, RoomJoinRequest, SignKey, UserInfoResponse,
    };
    use remora_proto::machine::Timestamp;
    use remora_proto::machine::signaling::{SignalingEvent, SignalingMachine};
    use remora_proto::profile::Profile;
    use remora_proto::signal::SignalEvent;
    use serde_json::Value;
    use tokio::sync::{Mutex, RwLock, mpsc, oneshot};
    use tokio::time::Instant;
    use url::Url;

    use crate::client::driver::drive_machine_with;
    use crate::client::http::{ControlClient, HttpError};
    use crate::client::socket::{ReconnectBackoff, SignalingSocket, SocketError};

    #[cfg(feature = "record")]
    use crate::record::{TracePlane, TraceRecorder};

    const COMMAND_CHANNEL_CAP: usize = 64;
    const EVENT_CHANNEL_CAP: usize = 128;

    /// Role an endpoint plays in a signaling room.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    #[non_exhaustive]
    pub enum RoomRole {
        Host,
        Viewer,
    }

    /// Error returned by high-level [`Endpoint`] and [`RoomHandle`] operations.
    #[derive(Debug)]
    #[non_exhaustive]
    pub enum EndpointError {
        MissingBaseUrl,
        MissingUserToken,
        InvalidBaseUrl(url::ParseError),
        Http(HttpError),
        Socket(SocketError),
        QrExpired,
        NoSignalingServer,
        SignalingClosed,
        ControlRefused(String),
        #[cfg(feature = "peer")]
        Peer(crate::peer::PeerSessionError),
    }

    impl fmt::Display for EndpointError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                Self::MissingBaseUrl => f.write_str("cloud base URL is required"),
                Self::MissingUserToken => f.write_str("user is not logged in"),
                Self::InvalidBaseUrl(err) => write!(f, "invalid cloud base URL: {err}"),
                Self::Http(err) => err.fmt(f),
                Self::Socket(err) => err.fmt(f),
                Self::QrExpired => f.write_str("QR login expired"),
                Self::NoSignalingServer => f.write_str("cloud returned no signaling server URL"),
                Self::SignalingClosed => f.write_str("signaling room handle is closed"),
                Self::ControlRefused(reason) => write!(f, "control request refused: {reason}"),
                #[cfg(feature = "peer")]
                Self::Peer(err) => err.fmt(f),
            }
        }
    }

    impl std::error::Error for EndpointError {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            match self {
                Self::InvalidBaseUrl(err) => Some(err),
                Self::Http(err) => Some(err),
                Self::Socket(err) => Some(err),
                Self::MissingBaseUrl
                | Self::MissingUserToken
                | Self::QrExpired
                | Self::NoSignalingServer
                | Self::SignalingClosed
                | Self::ControlRefused(_) => None,
                #[cfg(feature = "peer")]
                Self::Peer(err) => Some(err),
            }
        }
    }

    impl From<HttpError> for EndpointError {
        fn from(err: HttpError) -> Self {
            match err {
                HttpError::Api(ApiError::QrCodeExpired) => Self::QrExpired,
                other => Self::Http(other),
            }
        }
    }

    impl From<SocketError> for EndpointError {
        fn from(err: SocketError) -> Self {
            Self::Socket(err)
        }
    }

    #[cfg(feature = "peer")]
    impl From<crate::peer::PeerSessionError> for EndpointError {
        fn from(err: crate::peer::PeerSessionError) -> Self {
            Self::Peer(err)
        }
    }

    /// Builder for configuring an [`Endpoint`].
    pub struct EndpointBuilder {
        profile: &'static Profile,
        base_url: Option<String>,
        sign_key: SignKey,
        client_id: Option<String>,
        device_name: String,
        device_id: Option<String>,
        user_token: Option<String>,
        http_client: Option<reqwest::Client>,
        tls_config: Option<Arc<rustls::ClientConfig>>,
        auto_reconnect: bool,
        #[cfg(feature = "record")]
        recorder: Option<Arc<TraceRecorder>>,
    }

    impl fmt::Debug for EndpointBuilder {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("EndpointBuilder")
                .field("profile", &self.profile.id)
                .field("base_url", &self.base_url)
                .field("device_name", &self.device_name)
                .field("has_device_id", &self.device_id.is_some())
                .field("has_user_token", &self.user_token.is_some())
                .field("auto_reconnect", &self.auto_reconnect)
                .finish()
        }
    }

    impl EndpointBuilder {
        /// Sets the cloud REST API base URL (e.g. `https://cloud.example.com`).
        pub fn base_url(mut self, url: impl Into<String>) -> Self {
            self.base_url = Some(url.into());
            self
        }

        /// Sets the HMAC request-signing key shared with the cloud.
        pub fn sign_key(mut self, key: impl Into<SignKey>) -> Self {
            self.sign_key = key.into();
            self
        }

        /// Sets the persistent installation identifier sent to device initialization.
        pub fn client_id(mut self, id: impl Into<String>) -> Self {
            self.client_id = Some(id.into());
            self
        }

        /// Sets the human-readable device name sent during device registration.
        pub fn device_name(mut self, name: impl Into<String>) -> Self {
            self.device_name = name.into();
            self
        }

        /// Pre-populates a previously persisted `device_id`.
        pub fn device_id(mut self, id: impl Into<String>) -> Self {
            self.device_id = Some(id.into());
            self
        }

        /// Pre-populates a previously persisted `user_token`.
        pub fn user_token(mut self, token: impl Into<String>) -> Self {
            self.user_token = Some(token.into());
            self
        }

        /// Supplies a custom `reqwest::Client` for the control plane.
        pub fn http_client(mut self, client: reqwest::Client) -> Self {
            self.http_client = Some(client);
            self
        }

        /// Supplies a custom `rustls::ClientConfig` for `wss://` signaling connections.
        pub fn tls_config(mut self, config: Arc<rustls::ClientConfig>) -> Self {
            self.tls_config = Some(config);
            self
        }

        /// Enables or disables automatic signaling reconnect using `Profile` backoff (default `true`).
        pub fn auto_reconnect(mut self, enabled: bool) -> Self {
            self.auto_reconnect = enabled;
            self
        }

        /// Attaches a JSONL trace recorder for control and signaling traffic.
        #[cfg(feature = "record")]
        pub fn recorder(mut self, recorder: Arc<TraceRecorder>) -> Self {
            self.recorder = Some(recorder);
            self
        }

        /// Constructs the [`Endpoint`].
        pub fn build(self) -> Result<Endpoint, EndpointError> {
            let raw_url = self.base_url.ok_or(EndpointError::MissingBaseUrl)?;
            let base_url = Url::parse(&raw_url).map_err(EndpointError::InvalidBaseUrl)?;

            #[cfg_attr(not(feature = "record"), allow(unused_mut))]
            let mut http = match self.http_client {
                Some(client) => {
                    ControlClient::with_http_client(base_url, self.profile, self.sign_key, client)
                }
                None => ControlClient::new(base_url, self.profile, self.sign_key)?,
            };
            #[cfg(feature = "record")]
            if let Some(ref rec) = self.recorder {
                http = http.with_recorder(Arc::clone(rec));
            }

            let client_id = self.client_id.unwrap_or_else(random_hex_id);

            Ok(Endpoint {
                inner: Arc::new(EndpointInner {
                    profile: self.profile,
                    http,
                    client_id,
                    device_name: self.device_name,
                    tls_config: self.tls_config,
                    auto_reconnect: self.auto_reconnect,
                    #[cfg(feature = "record")]
                    recorder: self.recorder,
                    state: RwLock::new(EndpointState {
                        device_id: self.device_id,
                        user_token: self.user_token,
                    }),
                }),
            })
        }
    }

    struct EndpointState {
        device_id: Option<String>,
        user_token: Option<String>,
    }

    struct EndpointInner {
        profile: &'static Profile,
        http: ControlClient,
        client_id: String,
        device_name: String,
        tls_config: Option<Arc<rustls::ClientConfig>>,
        auto_reconnect: bool,
        #[cfg(feature = "record")]
        recorder: Option<Arc<TraceRecorder>>,
        state: RwLock<EndpointState>,
    }

    /// High-level UU Remote endpoint client.
    #[derive(Clone)]
    pub struct Endpoint {
        inner: Arc<EndpointInner>,
    }

    impl fmt::Debug for Endpoint {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("Endpoint")
                .field("profile", &self.inner.profile.id)
                .field("base_url", &self.inner.http.base_url().as_str())
                .field("auto_reconnect", &self.inner.auto_reconnect)
                .finish()
        }
    }

    impl Endpoint {
        /// Starts configuring a new endpoint for the given client [`Profile`].
        pub fn builder(profile: &'static Profile) -> EndpointBuilder {
            EndpointBuilder {
                profile,
                base_url: None,
                sign_key: SignKey::new(""),
                client_id: None,
                device_name: "remora-endpoint".to_owned(),
                device_id: None,
                user_token: None,
                http_client: None,
                tls_config: None,
                auto_reconnect: true,
                #[cfg(feature = "record")]
                recorder: None,
            }
        }

        pub fn profile(&self) -> &'static Profile {
            self.inner.profile
        }

        pub fn http(&self) -> &ControlClient {
            &self.inner.http
        }

        pub async fn device_id(&self) -> Option<String> {
            self.inner.state.read().await.device_id.clone()
        }

        pub async fn user_token(&self) -> Option<String> {
            self.inner.state.read().await.user_token.clone()
        }

        pub async fn set_device_id(&self, id: impl Into<String>) {
            self.inner.state.write().await.device_id = Some(id.into());
        }

        pub async fn set_user_token(&self, token: impl Into<String>) {
            self.inner.state.write().await.user_token = Some(token.into());
        }

        /// Initiates a QR login challenge, returning a session handle whose URL can be displayed
        /// before calling [`QrLoginSession::wait`].
        pub async fn start_qr_login(&self) -> Result<QrLoginSession, EndpointError> {
            let device_id = self.device_id().await;
            let challenge = self
                .inner
                .http
                .qr_login_generate(device_id.as_deref())
                .await?;
            Ok(QrLoginSession {
                endpoint: self.clone(),
                challenge,
            })
        }

        /// Performs a complete QR login (generate, poll until confirmed, redeem, and store the
        /// resulting user token).
        pub async fn login(&self) -> Result<LoginResponse, EndpointError> {
            self.start_qr_login().await?.wait().await
        }

        /// Fetches the authenticated user's profile (`GET /api/v1/user/info`).
        pub async fn user_info(&self) -> Result<UserInfoResponse, EndpointError> {
            let (token, device_id) = self.require_auth().await?;
            Ok(self
                .inner
                .http
                .user_info(&token, device_id.as_deref())
                .await?)
        }

        /// Logs out the current user session and clears the stored user token.
        pub async fn logout(&self) -> Result<(), EndpointError> {
            let (token, device_id) = self.require_auth().await?;
            self.inner
                .http
                .user_logout(&token, device_id.as_deref())
                .await?;
            self.inner.state.write().await.user_token = None;
            Ok(())
        }

        /// Lists devices owned by the authenticated user (`GET /api/v1/device/list`).
        pub async fn devices(&self) -> Result<DeviceListResponse, EndpointError> {
            let (token, device_id) = self.require_auth().await?;
            Ok(self
                .inner
                .http
                .device_list(&token, device_id.as_deref())
                .await?)
        }

        /// Lists owned devices grouped by form factor (`GET /api/v1/device/groups/of/my`).
        pub async fn device_groups(&self) -> Result<DeviceGroupsResponse, EndpointError> {
            let (token, device_id) = self.require_auth().await?;
            Ok(self
                .inner
                .http
                .device_groups(&token, device_id.as_deref())
                .await?)
        }

        /// Fetches per-device preferences (`GET /api/v1/device/{id}/preferences`).
        pub async fn device_preferences(
            &self,
            target_device_id: impl AsRef<str>,
        ) -> Result<DevicePreferencesResponse, EndpointError> {
            let (token, device_id) = self.require_auth().await?;
            Ok(self
                .inner
                .http
                .device_preferences(&token, target_device_id.as_ref(), device_id.as_deref())
                .await?)
        }

        /// Updates whether this macOS installation allows incoming remote control.
        pub async fn set_controllable(&self, controllable: bool) -> Result<(), EndpointError> {
            let device_id = match self.device_id().await {
                Some(id) => id,
                None => self.register_device().await?.device_id,
            };
            let token = self.require_token().await?;
            self.inner
                .http
                .mac_controllable(&token, &device_id, controllable)
                .await?;
            Ok(())
        }

        /// Registers or updates this installation's device record and stores `device_id`.
        pub async fn register_device(&self) -> Result<DeviceInitResponse, EndpointError> {
            let resp = self
                .inner
                .http
                .device_init(&DeviceInitRequest {
                    client_id: self.inner.client_id.clone(),
                    name: self.inner.device_name.clone(),
                })
                .await?;
            self.inner.state.write().await.device_id = Some(resp.device_id.clone());
            Ok(resp)
        }

        /// Ensures this device is registered, creates a host room, connects to signaling, and
        /// returns a [`RoomHandle`] that automatically announces `room_info` upon connect.
        pub async fn register_host(&self) -> Result<RoomHandle, EndpointError> {
            let device_id = match self.device_id().await {
                Some(id) => id,
                None => self.register_device().await?.device_id,
            };
            let token = self.require_token().await?;
            let created = self.inner.http.room_create(&token, &device_id).await?;
            let ws_url = created
                .signaling_list
                .first()
                .filter(|u| !u.is_empty())
                .cloned()
                .unwrap_or(created.signaling_server);
            if ws_url.is_empty() {
                return Err(EndpointError::NoSignalingServer);
            }

            let socket = SignalingSocket::connect(
                &ws_url,
                &created.token,
                self.inner.profile,
                self.inner.tls_config.clone(),
                #[cfg(feature = "record")]
                self.inner.recorder.clone(),
            )
            .await?;

            Ok(spawn_room_handle(
                self.clone(),
                RoomRole::Host,
                device_id,
                false,
                socket,
            ))
        }

        /// Joins `device_id`'s signaling room as a viewer and returns a [`RoomHandle`].
        pub async fn connect(
            &self,
            device_id: impl Into<String>,
        ) -> Result<RoomHandle, EndpointError> {
            self.connect_with_force(device_id, false).await
        }

        /// Joins `device_id`'s signaling room as a viewer, taking over from any existing viewer if
        /// `force_join` is `true`.
        pub async fn connect_force(
            &self,
            device_id: impl Into<String>,
        ) -> Result<RoomHandle, EndpointError> {
            self.connect_with_force(device_id, true).await
        }

        async fn connect_with_force(
            &self,
            device_id: impl Into<String>,
            force_join: bool,
        ) -> Result<RoomHandle, EndpointError> {
            let target_device_id = device_id.into();
            let (token, caller_device_id) = self.require_auth().await?;
            let joined = self
                .inner
                .http
                .room_join_by_device(
                    &token,
                    &target_device_id,
                    caller_device_id.as_deref(),
                    &RoomJoinRequest { force_join },
                )
                .await?;
            let ws_url = joined
                .signaling_list
                .first()
                .filter(|u| !u.is_empty())
                .cloned()
                .ok_or(EndpointError::NoSignalingServer)?;

            let socket = SignalingSocket::connect(
                &ws_url,
                &joined.token,
                self.inner.profile,
                self.inner.tls_config.clone(),
                #[cfg(feature = "record")]
                self.inner.recorder.clone(),
            )
            .await?;

            Ok(spawn_room_handle(
                self.clone(),
                RoomRole::Viewer,
                target_device_id,
                force_join,
                socket,
            ))
        }

        async fn require_token(&self) -> Result<String, EndpointError> {
            self.user_token()
                .await
                .ok_or(EndpointError::MissingUserToken)
        }

        async fn require_auth(&self) -> Result<(String, Option<String>), EndpointError> {
            let state = self.inner.state.read().await;
            let token = state
                .user_token
                .clone()
                .ok_or(EndpointError::MissingUserToken)?;
            Ok((token, state.device_id.clone()))
        }

        async fn fetch_fresh_ticket(
            &self,
            role: RoomRole,
            device_id: &str,
            force_join: bool,
        ) -> Result<(String, String), EndpointError> {
            let (token, caller_device_id) = self.require_auth().await?;
            match role {
                RoomRole::Host => {
                    let created = self.inner.http.room_create(&token, device_id).await?;
                    let ws_url = created
                        .signaling_list
                        .first()
                        .filter(|u| !u.is_empty())
                        .cloned()
                        .unwrap_or(created.signaling_server);
                    if ws_url.is_empty() {
                        return Err(EndpointError::NoSignalingServer);
                    }
                    Ok((ws_url, created.token))
                }
                RoomRole::Viewer => {
                    let joined = self
                        .inner
                        .http
                        .room_join_by_device(
                            &token,
                            device_id,
                            caller_device_id.as_deref(),
                            &RoomJoinRequest { force_join },
                        )
                        .await?;
                    let ws_url = joined
                        .signaling_list
                        .first()
                        .filter(|u| !u.is_empty())
                        .cloned()
                        .ok_or(EndpointError::NoSignalingServer)?;
                    Ok((ws_url, joined.token))
                }
            }
        }
    }

    /// Pending QR login challenge returned by [`Endpoint::start_qr_login`].
    pub struct QrLoginSession {
        endpoint: Endpoint,
        challenge: QrLoginGenResponse,
    }

    impl fmt::Debug for QrLoginSession {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("QrLoginSession")
                .field("challenge", &self.challenge)
                .finish()
        }
    }

    impl QrLoginSession {
        pub fn qrcode_jump_url(&self) -> &str {
            &self.challenge.qrcode_jump_url
        }

        pub fn status_query_ticket(&self) -> &str {
            &self.challenge.status_query_ticket
        }

        /// Polls until the QR code is confirmed or expired, redeems it, and stores the user token.
        pub async fn wait(self) -> Result<LoginResponse, EndpointError> {
            let mut last_status = u8::from(QrCodeStatus::Waiting);
            loop {
                let status_resp = self
                    .endpoint
                    .inner
                    .http
                    .qr_login_status(&QrLoginStatusRequest {
                        token: self.challenge.status_query_ticket.clone(),
                        qrcode_jump_url: self.challenge.qrcode_jump_url.clone(),
                        login_status: last_status,
                    })
                    .await?;
                match QrCodeStatus::from(status_resp.status) {
                    QrCodeStatus::Confirmed => break,
                    QrCodeStatus::Expired => return Err(EndpointError::QrExpired),
                    _ => {
                        if status_resp.status == last_status {
                            tokio::time::sleep(Duration::from_millis(200)).await;
                        }
                        last_status = status_resp.status;
                    }
                }
            }

            let login = self
                .endpoint
                .inner
                .http
                .login_by_qrcode(&LoginByQrcodeRequest {
                    token: self.challenge.status_query_ticket,
                    qrcode_jump_url: self.challenge.qrcode_jump_url,
                })
                .await?;
            self.endpoint.set_user_token(login.token.clone()).await;
            Ok(login)
        }
    }

    enum RoomCommand {
        SendRoomInfo(oneshot::Sender<u64>),
        SendControl {
            header: Value,
            connect_options: Vec<u8>,
            reply: oneshot::Sender<u64>,
        },
        SendSoac(Value),
        SendForwardSetting(Value),
        SendIceFinished {
            ice_id: String,
            app_control_id: String,
        },
        RefreshReconnectKey(oneshot::Sender<u64>),
        Close,
    }

    /// Clonable command handle for sending signaling messages into a room.
    #[derive(Clone)]
    pub struct RoomSender {
        role: RoomRole,
        device_id: String,
        cmd_tx: mpsc::Sender<RoomCommand>,
    }

    impl fmt::Debug for RoomSender {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("RoomSender")
                .field("role", &self.role)
                .field("device_id", &self.device_id)
                .finish()
        }
    }

    impl RoomSender {
        pub fn role(&self) -> RoomRole {
            self.role
        }

        pub fn device_id(&self) -> &str {
            &self.device_id
        }

        pub async fn send_room_info(&self) -> Result<u64, EndpointError> {
            let (tx, rx) = oneshot::channel();
            self.cmd_tx
                .send(RoomCommand::SendRoomInfo(tx))
                .await
                .map_err(|_| EndpointError::SignalingClosed)?;
            rx.await.map_err(|_| EndpointError::SignalingClosed)
        }

        pub async fn send_control(
            &self,
            header: Value,
            connect_options: Vec<u8>,
        ) -> Result<u64, EndpointError> {
            let (tx, rx) = oneshot::channel();
            self.cmd_tx
                .send(RoomCommand::SendControl {
                    header,
                    connect_options,
                    reply: tx,
                })
                .await
                .map_err(|_| EndpointError::SignalingClosed)?;
            rx.await.map_err(|_| EndpointError::SignalingClosed)
        }

        pub async fn send_soac(&self, payload: Value) -> Result<(), EndpointError> {
            self.cmd_tx
                .send(RoomCommand::SendSoac(payload))
                .await
                .map_err(|_| EndpointError::SignalingClosed)
        }

        pub async fn send_forward_setting(&self, payload: Value) -> Result<(), EndpointError> {
            self.cmd_tx
                .send(RoomCommand::SendForwardSetting(payload))
                .await
                .map_err(|_| EndpointError::SignalingClosed)
        }

        pub async fn send_ice_finished(
            &self,
            ice_id: impl Into<String>,
            app_control_id: impl Into<String>,
        ) -> Result<(), EndpointError> {
            self.cmd_tx
                .send(RoomCommand::SendIceFinished {
                    ice_id: ice_id.into(),
                    app_control_id: app_control_id.into(),
                })
                .await
                .map_err(|_| EndpointError::SignalingClosed)
        }

        pub async fn refresh_reconnect_key(&self) -> Result<u64, EndpointError> {
            let (tx, rx) = oneshot::channel();
            self.cmd_tx
                .send(RoomCommand::RefreshReconnectKey(tx))
                .await
                .map_err(|_| EndpointError::SignalingClosed)?;
            rx.await.map_err(|_| EndpointError::SignalingClosed)
        }

        pub async fn close(&self) -> Result<(), EndpointError> {
            self.cmd_tx
                .send(RoomCommand::Close)
                .await
                .map_err(|_| EndpointError::SignalingClosed)
        }
    }

    /// Stream of inbound [`SignalingEvent`]s from a signaling room.
    pub struct RoomEventStream {
        rx: mpsc::Receiver<SignalingEvent>,
    }

    impl fmt::Debug for RoomEventStream {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("RoomEventStream").finish()
        }
    }

    impl RoomEventStream {
        pub async fn next(&mut self) -> Option<SignalingEvent> {
            self.rx.recv().await
        }
    }

    impl Stream for RoomEventStream {
        type Item = SignalingEvent;

        fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            self.rx.poll_recv(cx)
        }
    }

    /// Active signaling room connection exposing commands and an event [`Stream`].
    pub struct RoomHandle {
        sender: RoomSender,
        events: RoomEventStream,
    }

    impl fmt::Debug for RoomHandle {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("RoomHandle")
                .field("sender", &self.sender)
                .finish()
        }
    }

    impl RoomHandle {
        pub fn role(&self) -> RoomRole {
            self.sender.role()
        }

        pub fn device_id(&self) -> &str {
            self.sender.device_id()
        }

        pub fn sender(&self) -> RoomSender {
            self.sender.clone()
        }

        pub fn split(self) -> (RoomSender, RoomEventStream) {
            (self.sender, self.events)
        }

        pub async fn next_event(&mut self) -> Option<SignalingEvent> {
            self.events.next().await
        }

        pub async fn send_room_info(&self) -> Result<u64, EndpointError> {
            self.sender.send_room_info().await
        }

        pub async fn send_control(
            &self,
            header: Value,
            connect_options: Vec<u8>,
        ) -> Result<u64, EndpointError> {
            self.sender.send_control(header, connect_options).await
        }

        pub async fn send_soac(&self, payload: Value) -> Result<(), EndpointError> {
            self.sender.send_soac(payload).await
        }

        pub async fn send_forward_setting(&self, payload: Value) -> Result<(), EndpointError> {
            self.sender.send_forward_setting(payload).await
        }

        pub async fn send_ice_finished(
            &self,
            ice_id: impl Into<String>,
            app_control_id: impl Into<String>,
        ) -> Result<(), EndpointError> {
            self.sender.send_ice_finished(ice_id, app_control_id).await
        }

        pub async fn refresh_reconnect_key(&self) -> Result<u64, EndpointError> {
            self.sender.refresh_reconnect_key().await
        }

        pub async fn close(&self) -> Result<(), EndpointError> {
            self.sender.close().await
        }
    }

    impl Stream for RoomHandle {
        type Item = SignalingEvent;

        fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            Pin::new(&mut self.events).poll_next(cx)
        }
    }

    fn spawn_room_handle(
        endpoint: Endpoint,
        role: RoomRole,
        device_id: String,
        force_join: bool,
        initial_socket: SignalingSocket,
    ) -> RoomHandle {
        let (cmd_tx, cmd_rx) = mpsc::channel(COMMAND_CHANNEL_CAP);
        let (ev_tx, ev_rx) = mpsc::channel(EVENT_CHANNEL_CAP);

        let sender = RoomSender {
            role,
            device_id: device_id.clone(),
            cmd_tx,
        };

        tokio::spawn(run_room_loop(
            endpoint,
            role,
            device_id,
            force_join,
            initial_socket,
            cmd_rx,
            ev_tx,
        ));

        RoomHandle {
            sender,
            events: RoomEventStream { rx: ev_rx },
        }
    }

    async fn run_room_loop(
        endpoint: Endpoint,
        role: RoomRole,
        device_id: String,
        force_join: bool,
        initial_socket: SignalingSocket,
        mut cmd_rx: mpsc::Receiver<RoomCommand>,
        ev_tx: mpsc::Sender<SignalingEvent>,
    ) {
        let mut maybe_socket = Some(initial_socket);
        let mut backoff = ReconnectBackoff::from_profile(endpoint.profile());
        let backoff_lock = Arc::new(Mutex::new(()));
        let _ = &backoff_lock;

        loop {
            let mut socket = match maybe_socket.take() {
                Some(sock) => sock,
                None => {
                    if !endpoint.inner.auto_reconnect || ev_tx.is_closed() {
                        return;
                    }
                    let delay = backoff.next_delay();
                    let sleep = tokio::time::sleep(delay);
                    tokio::pin!(sleep);
                    loop {
                        tokio::select! {
                            () = &mut sleep => break,
                            cmd = cmd_rx.recv() => {
                                match cmd {
                                    Some(RoomCommand::Close) | None => return,
                                    Some(_) => {}
                                }
                            }
                        }
                    }

                    let Ok((ws_url, room_token)) = endpoint
                        .fetch_fresh_ticket(role, &device_id, force_join)
                        .await
                    else {
                        continue;
                    };

                    match SignalingSocket::connect(
                        &ws_url,
                        &room_token,
                        endpoint.inner.profile,
                        endpoint.inner.tls_config.clone(),
                        #[cfg(feature = "record")]
                        endpoint.inner.recorder.clone(),
                    )
                    .await
                    {
                        Ok(sock) => sock,
                        Err(_) => continue,
                    }
                }
            };

            let anchor = Instant::now();
            let mut machine = SignalingMachine::with_signal_profile(
                rand::rngs::StdRng::from_os_rng(),
                &endpoint.inner.profile.signal,
            );
            machine.connect(Timestamp::ZERO);

            let mut user_closed = false;
            let mut fatal_kick_or_auth = false;
            let mut connected_this_run = false;

            let result = drive_machine_with(
                &mut machine,
                &mut socket,
                anchor,
                &mut cmd_rx,
                &ev_tx,
                |m, _now, cmd| match cmd {
                    RoomCommand::SendRoomInfo(reply) => {
                        let _ = reply.send(m.send_room_info());
                        ControlFlow::Continue(())
                    }
                    RoomCommand::SendControl {
                        header,
                        connect_options,
                        reply,
                    } => {
                        let _ = reply.send(m.send_control(header, connect_options));
                        ControlFlow::Continue(())
                    }
                    RoomCommand::SendSoac(payload) => {
                        m.send_soac(payload);
                        ControlFlow::Continue(())
                    }
                    RoomCommand::SendForwardSetting(payload) => {
                        m.send_forward_setting(payload);
                        ControlFlow::Continue(())
                    }
                    RoomCommand::SendIceFinished {
                        ice_id,
                        app_control_id,
                    } => {
                        m.send_ice_finished(&ice_id, &app_control_id);
                        ControlFlow::Continue(())
                    }
                    RoomCommand::RefreshReconnectKey(reply) => {
                        let _ = reply.send(m.send_refresh_reconnect_key());
                        ControlFlow::Continue(())
                    }
                    RoomCommand::Close => {
                        user_closed = true;
                        m.disconnect();
                        ControlFlow::Break(())
                    }
                },
                |m, _now, ev| match ev {
                    SignalingEvent::Connected => {
                        connected_this_run = true;
                        if role == RoomRole::Host {
                            m.send_room_info();
                        }
                    }
                    SignalingEvent::ConnectError(_) => {
                        fatal_kick_or_auth = true;
                    }
                    SignalingEvent::Signal(SignalEvent::Left(left)) if left.is_room_kick() => {
                        fatal_kick_or_auth = true;
                    }
                    SignalingEvent::TimedOut =>
                    {
                        #[cfg(feature = "record")]
                        if let Some(rec) = &endpoint.inner.recorder {
                            rec.record_timeout(TracePlane::Signal, "liveness");
                        }
                    }
                    _ => {}
                },
            )
            .await;

            if connected_this_run {
                backoff.reset();
            }

            if user_closed
                || fatal_kick_or_auth
                || !endpoint.inner.auto_reconnect
                || ev_tx.is_closed()
            {
                return;
            }
            if result.is_ok() && !connected_this_run {
                // Server closed cleanly before completing connect; still honor reconnect unless kicked.
            }
        }
    }

    fn random_hex_id() -> String {
        let mut bytes = [0u8; 16];
        rand::rng().fill_bytes(&mut bytes);
        hex::encode(bytes)
    }
}

#[cfg(feature = "client")]
pub use sdk::{
    Endpoint, EndpointBuilder, EndpointError, QrLoginSession, RoomEventStream, RoomHandle,
    RoomRole, RoomSender,
};

#[cfg(all(test, feature = "client"))]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use futures_util::{SinkExt, StreamExt};
    use remora_proto::control::{DeviceStatus, SignKey};
    use remora_proto::machine::signaling::SignalingEvent;
    use remora_proto::profile;
    use remora_proto::signal::{
        Handshake, Packet, SignalEvent, encode_ack, encode_connected, encode_event,
    };
    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio_tungstenite::accept_async;
    use tokio_tungstenite::tungstenite::Message;

    use super::*;

    fn decode_packet_text(text: &str) -> Packet {
        let mut dec = remora_proto::signal::Decoder::new();
        dec.text(text).unwrap().unwrap()
    }

    async fn spawn_fake_cloud() -> (String, Arc<AtomicUsize>) {
        // Bind WS server first so HTTP room_create / room_join can advertise it.
        let ws_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let ws_addr = ws_listener.local_addr().unwrap();
        let ws_connections = Arc::new(AtomicUsize::new(0));
        let ws_connections_clone = Arc::clone(&ws_connections);

        tokio::spawn(async move {
            while let Ok((tcp, _)) = ws_listener.accept().await {
                let count = ws_connections_clone.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    let mut ws = accept_async(tcp).await.unwrap();
                    let open = Handshake {
                        sid: format!("sid-{count}"),
                        ping_interval: 25_000,
                        ping_timeout: 20_000,
                    }
                    .encode();
                    ws.send(Message::Text(open.into())).await.unwrap();
                    let conn = ws.next().await.unwrap().unwrap();
                    assert_eq!(conn.into_text().unwrap().as_str(), "40");
                    ws.send(Message::Text(
                        encode_connected(&format!("sid-{count}")).into(),
                    ))
                    .await
                    .unwrap();

                    if count == 0 {
                        // Expect room_info from host on first connection, then drop to test reconnect.
                        let room_info = ws.next().await.unwrap().unwrap();
                        if let Packet::Event(ev) =
                            decode_packet_text(&room_info.into_text().unwrap())
                        {
                            assert_eq!(ev.name, "room_info");
                            if let Some(ack) = ev.ack_id {
                                ws.send(Message::Text(encode_ack(ack, &[json!({})]).into()))
                                    .await
                                    .unwrap();
                            }
                        }
                        // Drop connection abruptly to trigger client reconnect backoff.
                        return;
                    }

                    // Second connection (or viewer): send bmsg_push ready and wait for close.
                    ws.send(Message::Text(
                        encode_event(
                            "bmsg_push",
                            &[json!({"data": {"publisher_availability_status": "ready"}})],
                            None,
                        )
                        .into(),
                    ))
                    .await
                    .unwrap();

                    while let Some(Ok(msg)) = ws.next().await {
                        if matches!(msg, Message::Close(_)) {
                            break;
                        }
                    }
                });
            }
        });

        let http_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let http_addr = http_listener.local_addr().unwrap();
        let ws_url = format!("ws://{ws_addr}");

        tokio::spawn(async move {
            while let Ok((mut stream, _)) = http_listener.accept().await {
                let ws_url = ws_url.clone();
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
                    let body = match (method.as_str(), path.as_str()) {
                        ("POST", "/api/v1/device/macos/init") => json!({
                            "code": 0,
                            "msg": "ok",
                            "data": {"device_id": "dev-mac-1"}
                        }),
                        ("POST", "/api/v1/qrcode/gen/login") => json!({
                            "code": 0,
                            "msg": "ok",
                            "data": {
                                "qrcode_jump_url": "https://remora.test/login/1",
                                "status_query_ticket": "ticket-1"
                            }
                        }),
                        ("POST", "/api/v1/qrcode/login/status") => json!({
                            "code": 0,
                            "msg": "ok",
                            "data": {"status": 4}
                        }),
                        ("POST", "/api/v1/login/by_qrcode") => json!({
                            "code": 0,
                            "msg": "ok",
                            "data": {
                                "token": "user-jwt",
                                "user_id": "1",
                                "nickname": "Owner"
                            }
                        }),
                        ("GET", "/api/v1/device/list") => json!({
                            "code": 0,
                            "msg": "ok",
                            "data": {
                                "current_device": null,
                                "my_binded_devices": [{
                                    "device_id": "dev-mac-1",
                                    "alias": "Mac",
                                    "platform": 4,
                                    "status": "CONNECTED"
                                }],
                                "others_shared_devices": []
                            }
                        }),
                        ("GET", "/api/v1/device/groups/of/my") => json!({
                            "code": 0,
                            "msg": "ok",
                            "data": {
                                "current_device_id": "dev-mac-1",
                                "mobile_devices": [],
                                "desktop_devices": [{
                                    "device_id": "dev-mac-1",
                                    "alias": "Mac",
                                    "platform": 4,
                                    "status": "CONNECTED",
                                    "controllable": true
                                }],
                                "tv_devices": []
                            }
                        }),
                        ("POST", "/api/v1/device/mac_controllable") => json!({
                            "code": 0,
                            "msg": "ok",
                            "data": {}
                        }),
                        ("GET", "/api/v1/device/dev-mac-1/preferences") => json!({
                            "code": 0,
                            "msg": "ok",
                            "data": {
                                "device_preferences": {
                                    "device_id": "dev-mac-1",
                                    "tools": { "sequence": ["file_transfer"] }
                                }
                            }
                        }),
                        ("POST", "/api/v1/room/create") => json!({
                            "code": 0,
                            "msg": "ok",
                            "data": {
                                "signaling_server": ws_url,
                                "signaling_list": [ws_url],
                                "token": "host-room-jwt",
                                "ws_connect_timeout_ms": 10000
                            }
                        }),
                        ("POST", "/api/v1/room/join/by_device/dev-mac-1") => json!({
                            "code": 0,
                            "msg": "ok",
                            "data": {
                                "signaling_list": [ws_url],
                                "token": "viewer-room-jwt",
                                "ws_connect_timeout_ms": 10000,
                                "report_token": "",
                                "report_url": "",
                                "publisher_platform": 4
                            }
                        }),
                        _ => json!({"code": 1001, "msg": "unexpected"}),
                    };
                    let body_str = body.to_string();
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body_str}",
                        body_str.len()
                    );
                    let _ = stream.write_all(resp.as_bytes()).await;
                });
            }
        });

        (format!("http://{http_addr}"), ws_connections)
    }

    #[tokio::test]
    async fn endpoint_full_host_reconnect_and_viewer_connect_flow() {
        let (base_url, ws_count) = spawn_fake_cloud().await;
        let profile = profile::by_id("macos-4.48.2").unwrap();

        let endpoint = Endpoint::builder(profile)
            .base_url(base_url)
            .sign_key(SignKey::new("test-key"))
            .device_name("My Mac")
            .build()
            .unwrap();

        let login = endpoint.login().await.unwrap();
        assert_eq!(login.user_id, "1");

        let devices = endpoint.devices().await.unwrap();
        assert_eq!(devices.my_binded_devices[0].status, DeviceStatus::Connected);

        let groups = endpoint.device_groups().await.unwrap();
        assert_eq!(groups.desktop_devices.len(), 1);
        endpoint.set_controllable(false).await.unwrap();
        let prefs = endpoint.device_preferences("dev-mac-1").await.unwrap();
        assert_eq!(prefs.device_preferences.device_id, "dev-mac-1");

        let mut host_room = endpoint.register_host().await.unwrap();
        assert_eq!(host_room.role(), RoomRole::Host);
        assert_eq!(host_room.device_id(), "dev-mac-1");

        // First connection emits Connected + Ack(room_info), then drops and reconnects.
        assert_eq!(host_room.next().await, Some(SignalingEvent::Connected));
        assert!(matches!(
            host_room.next().await,
            Some(SignalingEvent::Ack { .. })
        ));
        // After reconnect: Connected + BmsgPush(ready).
        assert_eq!(host_room.next().await, Some(SignalingEvent::Connected));
        assert!(matches!(
            host_room.next().await,
            Some(SignalingEvent::Signal(SignalEvent::BmsgPush(_)))
        ));
        assert!(ws_count.load(Ordering::SeqCst) >= 2);
        host_room.close().await.unwrap();

        let mut viewer_room = endpoint.connect("dev-mac-1").await.unwrap();
        assert_eq!(viewer_room.role(), RoomRole::Viewer);
        assert_eq!(viewer_room.next().await, Some(SignalingEvent::Connected));
        viewer_room.close().await.unwrap();
    }
}
