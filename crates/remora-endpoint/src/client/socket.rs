//! Engine.IO v4 / Socket.IO WebSocket transport, handshake, heartbeat, and reconnect backoff.

use std::fmt;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use http::{HeaderName, HeaderValue, Request};
use remora_proto::machine::signaling::SignalingTransmit;
use remora_proto::profile::{Profile, SignalProfile};
use rustls::pki_types::ServerName;
use rustls_platform_verifier::ConfigVerifierExt;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::{WebSocketStream, client_async};
use url::Url;

use crate::client::driver::MachineSocket;

#[cfg(feature = "record")]
use crate::record::{TraceDirection, TraceRecorder};

const AUTH_HEADER: &str = "x-nrd-auth";

/// Transport and timing settings derived from a client [`Profile`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SocketConfig {
    pub signal: SignalProfile,
    pub plat: &'static str,
    pub version: &'static str,
    pub build: u32,
}

impl SocketConfig {
    pub fn from_profile(profile: &Profile) -> Self {
        Self {
            signal: profile.signal,
            plat: profile.plat,
            version: profile.version,
            build: profile.build,
        }
    }
}

/// Exponential reconnect backoff parameterized by [`Profile::signal`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReconnectBackoff {
    initial: Duration,
    max: Duration,
    attempt: u32,
}

impl ReconnectBackoff {
    pub fn from_profile(profile: &Profile) -> Self {
        Self::from_signal(&profile.signal)
    }

    pub fn from_signal(signal: &SignalProfile) -> Self {
        let initial = signal.reconnect_initial();
        let max = signal.reconnect_max().max(initial);
        Self {
            initial,
            max,
            attempt: 0,
        }
    }

    /// Returns the next backoff delay and advances the attempt counter.
    pub fn next_delay(&mut self) -> Duration {
        let shift = self.attempt.min(30);
        let factor = 1u32.checked_shl(shift).unwrap_or(u32::MAX);
        let delay = self.initial.saturating_mul(factor).min(self.max);
        self.attempt = self.attempt.saturating_add(1);
        delay
    }

    /// Resets the backoff state after a successful connection.
    pub fn reset(&mut self) {
        self.attempt = 0;
    }

    pub fn attempt(&self) -> u32 {
        self.attempt
    }
}

/// Error returned by [`SignalingSocket`] operations.
#[derive(Debug)]
#[non_exhaustive]
pub enum SocketError {
    InvalidUrl(String),
    InvalidHeader,
    ConnectTimeout,
    Io(io::Error),
    Tls(String),
    WebSocket(tokio_tungstenite::tungstenite::Error),
}

impl fmt::Display for SocketError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidUrl(msg) => write!(f, "invalid signaling URL: {msg}"),
            Self::InvalidHeader => f.write_str("invalid signaling header"),
            Self::ConnectTimeout => f.write_str("signaling WebSocket connection timed out"),
            Self::Io(err) => write!(f, "signaling I/O error: {err}"),
            Self::Tls(msg) => write!(f, "signaling TLS error: {msg}"),
            Self::WebSocket(err) => write!(f, "signaling WebSocket error: {err}"),
        }
    }
}

impl std::error::Error for SocketError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            Self::WebSocket(err) => Some(err),
            Self::InvalidUrl(_) | Self::InvalidHeader | Self::ConnectTimeout | Self::Tls(_) => None,
        }
    }
}

impl From<io::Error> for SocketError {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

impl From<tokio_tungstenite::tungstenite::Error> for SocketError {
    fn from(err: tokio_tungstenite::tungstenite::Error) -> Self {
        Self::WebSocket(err)
    }
}

/// Underlying TCP or rustls-over-TCP stream for [`SignalingSocket`].
pub enum MaybeTlsStream {
    Plain(TcpStream),
    Tls(Box<tokio_rustls::client::TlsStream<TcpStream>>),
}

impl AsyncRead for MaybeTlsStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(s) => Pin::new(s).poll_read(cx, buf),
            Self::Tls(s) => Pin::new(s).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for MaybeTlsStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Self::Plain(s) => Pin::new(s).poll_write(cx, buf),
            Self::Tls(s) => Pin::new(s).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(s) => Pin::new(s).poll_flush(cx),
            Self::Tls(s) => Pin::new(s).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(s) => Pin::new(s).poll_shutdown(cx),
            Self::Tls(s) => Pin::new(s).poll_shutdown(cx),
        }
    }
}

/// WebSocket transport implementing [`MachineSocket<SignalingTransmit>`] with Engine.IO v4 URL,
/// auth headers, profile identification headers, and OS-verified TLS.
pub struct SignalingSocket {
    ws: WebSocketStream<MaybeTlsStream>,
    config: SocketConfig,
    closed: bool,
    #[cfg(feature = "record")]
    recorder: Option<Arc<TraceRecorder>>,
}

impl fmt::Debug for SignalingSocket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SignalingSocket")
            .field("closed", &self.closed)
            .field("config", &self.config)
            .finish()
    }
}

impl SignalingSocket {
    /// Connects to the signaling WebSocket endpoint using parameters from `profile`.
    pub async fn connect(
        server_url: &str,
        room_token: &str,
        profile: &'static Profile,
        tls_config: Option<Arc<rustls::ClientConfig>>,
        #[cfg(feature = "record")] recorder: Option<Arc<TraceRecorder>>,
    ) -> Result<Self, SocketError> {
        let config = SocketConfig::from_profile(profile);
        let ws_url = build_signaling_url(server_url, config.signal.path)?;
        let request = build_upgrade_request(&ws_url, room_token, &config)?;

        let host = ws_url
            .host_str()
            .ok_or_else(|| SocketError::InvalidUrl("missing host".into()))?
            .to_owned();
        let port = ws_url
            .port_or_known_default()
            .ok_or_else(|| SocketError::InvalidUrl("missing port".into()))?;
        let use_tls = match ws_url.scheme() {
            "wss" => true,
            "ws" => false,
            other => {
                return Err(SocketError::InvalidUrl(format!(
                    "unsupported scheme {other}"
                )));
            }
        };

        let timeout = config.signal.handshake_timeout();
        let tcp = tokio::time::timeout(timeout, TcpStream::connect((host.as_str(), port)))
            .await
            .map_err(|_| SocketError::ConnectTimeout)??;
        let _ = tcp.set_nodelay(true);

        let stream = if use_tls {
            let tls_cfg = match tls_config {
                Some(cfg) => cfg,
                None => Arc::new(
                    rustls::ClientConfig::with_platform_verifier()
                        .map_err(|err| SocketError::Tls(err.to_string()))?,
                ),
            };
            let server_name =
                ServerName::try_from(host).map_err(|err| SocketError::Tls(err.to_string()))?;
            let connector = TlsConnector::from(tls_cfg);
            let tls_stream = tokio::time::timeout(timeout, connector.connect(server_name, tcp))
                .await
                .map_err(|_| SocketError::ConnectTimeout)??;
            MaybeTlsStream::Tls(Box::new(tls_stream))
        } else {
            MaybeTlsStream::Plain(tcp)
        };

        let (ws, _) = tokio::time::timeout(timeout, client_async(request, stream))
            .await
            .map_err(|_| SocketError::ConnectTimeout)??;

        Ok(Self {
            ws,
            config,
            closed: false,
            #[cfg(feature = "record")]
            recorder,
        })
    }

    pub fn config(&self) -> &SocketConfig {
        &self.config
    }
}

impl MachineSocket<SignalingTransmit> for SignalingSocket {
    type Error = SocketError;

    async fn send(&mut self, transmit: SignalingTransmit) -> Result<(), Self::Error> {
        if self.closed {
            return Ok(());
        }
        match transmit {
            SignalingTransmit::Text(text) => {
                #[cfg(feature = "record")]
                if let Some(rec) = &self.recorder {
                    rec.record_ws_text(TraceDirection::Out, &text);
                }
                self.ws.send(Message::Text(text.into())).await?;
            }
            SignalingTransmit::Binary(bytes) => {
                #[cfg(feature = "record")]
                if let Some(rec) = &self.recorder {
                    rec.record_ws_binary(TraceDirection::Out, &bytes);
                }
                self.ws.send(Message::Binary(bytes.into())).await?;
            }
            SignalingTransmit::Close => {
                self.closed = true;
                let _ = self.ws.send(Message::Close(None)).await;
                let _ = self.ws.close(None).await;
            }
            _ => {}
        }
        Ok(())
    }

    async fn recv(&mut self) -> Result<Option<Vec<u8>>, Self::Error> {
        if self.closed {
            return Ok(None);
        }
        while let Some(item) = self.ws.next().await {
            match item? {
                Message::Text(text) => {
                    #[cfg(feature = "record")]
                    if let Some(rec) = &self.recorder {
                        rec.record_ws_text(TraceDirection::In, &text);
                    }
                    return Ok(Some(text.as_bytes().to_vec()));
                }
                Message::Binary(bytes) => {
                    #[cfg(feature = "record")]
                    if let Some(rec) = &self.recorder {
                        rec.record_ws_binary(TraceDirection::In, &bytes);
                    }
                    return Ok(Some(bytes.to_vec()));
                }
                Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => continue,
                Message::Close(_) => {
                    self.closed = true;
                    return Ok(None);
                }
            }
        }
        self.closed = true;
        Ok(None)
    }
}

/// Builds the full Engine.IO v4 WebSocket URL from a signaling server base URL and `profile.signal.path`.
pub fn build_signaling_url(server_url: &str, signal_path: &str) -> Result<Url, SocketError> {
    let mut url = Url::parse(server_url).map_err(|err| SocketError::InvalidUrl(err.to_string()))?;
    match url.scheme() {
        "http" => {
            let _ = url.set_scheme("ws");
        }
        "https" => {
            let _ = url.set_scheme("wss");
        }
        "ws" | "wss" => {}
        other => {
            return Err(SocketError::InvalidUrl(format!(
                "unsupported scheme {other}"
            )));
        }
    }
    let joined = url
        .join(signal_path)
        .map_err(|err| SocketError::InvalidUrl(err.to_string()))?;
    Ok(joined)
}

fn build_upgrade_request(
    ws_url: &Url,
    room_token: &str,
    config: &SocketConfig,
) -> Result<Request<()>, SocketError> {
    let mut req = ws_url
        .as_str()
        .into_client_request()
        .map_err(SocketError::WebSocket)?;
    let headers = req.headers_mut();
    headers.insert(
        HeaderName::from_static(AUTH_HEADER),
        HeaderValue::from_str(room_token).map_err(|_| SocketError::InvalidHeader)?,
    );
    headers.insert(
        HeaderName::from_static("x-param-plat"),
        HeaderValue::from_str(config.plat).map_err(|_| SocketError::InvalidHeader)?,
    );
    headers.insert(
        HeaderName::from_static("x-param-vn"),
        HeaderValue::from_str(config.version).map_err(|_| SocketError::InvalidHeader)?,
    );
    headers.insert(
        HeaderName::from_static("x-param-vc"),
        HeaderValue::from_str(&config.build.to_string()).map_err(|_| SocketError::InvalidHeader)?,
    );
    Ok(req)
}

#[cfg(test)]
mod tests {
    use rand::RngCore;
    use remora_proto::machine::Timestamp;
    use remora_proto::machine::signaling::{SignalingEvent, SignalingMachine, SignalingState};
    use remora_proto::profile;
    use remora_proto::signal::{Handshake, PING, PONG, encode_connected};
    use tokio::net::TcpListener;
    use tokio::sync::mpsc;
    use tokio::time::Instant;
    use tokio_tungstenite::accept_hdr_async;
    use tokio_tungstenite::tungstenite::handshake::server::{Request as WsReq, Response as WsResp};

    use super::*;
    use crate::client::driver::drive_until_closed;

    struct DeterministicRng(u64);

    impl RngCore for DeterministicRng {
        fn next_u32(&mut self) -> u32 {
            self.next_u64() as u32
        }
        fn next_u64(&mut self) -> u64 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1);
            self.0
        }
        fn fill_bytes(&mut self, dest: &mut [u8]) {
            for chunk in dest.chunks_mut(8) {
                let bytes = self.next_u64().to_le_bytes();
                chunk.copy_from_slice(&bytes[..chunk.len()]);
            }
        }
    }

    #[test]
    fn builds_ws_urls_from_profile_path() {
        let p = profile::by_id("macos-4.48.2").unwrap();
        let url = build_signaling_url("https://cloud.example.com:8443", p.signal.path).unwrap();
        assert_eq!(
            url.as_str(),
            "wss://cloud.example.com:8443/socket.io/?EIO=4&transport=websocket"
        );
    }

    #[test]
    fn backoff_uses_profile_bounds() {
        let p = profile::by_id("macos-4.48.2").unwrap();
        let mut backoff = ReconnectBackoff::from_profile(p);
        assert_eq!(backoff.next_delay(), p.signal.reconnect_initial());
        assert_eq!(backoff.next_delay(), p.signal.reconnect_initial() * 2);
        assert_eq!(backoff.next_delay(), p.signal.reconnect_initial() * 4);
        for _ in 0..10 {
            let d = backoff.next_delay();
            assert!(d <= p.signal.reconnect_max());
        }
        assert_eq!(backoff.next_delay(), p.signal.reconnect_max());
        backoff.reset();
        assert_eq!(backoff.attempt(), 0);
        assert_eq!(backoff.next_delay(), p.signal.reconnect_initial());
    }

    #[tokio::test]
    async fn engineio_handshake_and_heartbeat_over_websocket() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut seen_auth = None;
            let mut seen_plat = None;
            #[allow(
                clippy::result_large_err,
                reason = "tokio-tungstenite ErrorResponse type"
            )]
            let mut ws = accept_hdr_async(tcp, |req: &WsReq, resp: WsResp| {
                seen_auth = req
                    .headers()
                    .get("x-nrd-auth")
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_owned);
                seen_plat = req
                    .headers()
                    .get("x-param-plat")
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_owned);
                Ok(resp)
            })
            .await
            .unwrap();

            assert_eq!(seen_auth.as_deref(), Some("room-jwt"));
            assert_eq!(seen_plat.as_deref(), Some("4"));

            let open = Handshake {
                sid: "sid-1".into(),
                ping_interval: 25_000,
                ping_timeout: 20_000,
            }
            .encode();
            ws.send(Message::Text(open.into())).await.unwrap();

            let msg = ws.next().await.unwrap().unwrap();
            assert_eq!(msg.into_text().unwrap().as_str(), "40");
            ws.send(Message::Text(encode_connected("sid-1").into()))
                .await
                .unwrap();

            ws.send(Message::Text(PING.into())).await.unwrap();
            let pong = ws.next().await.unwrap().unwrap();
            assert_eq!(pong.into_text().unwrap().as_str(), PONG);

            ws.send(Message::Text("1".into())).await.unwrap();
            ws.send(Message::Close(None)).await.unwrap();
        });

        let p = profile::by_id("macos-4.48.2").unwrap();
        let mut socket = SignalingSocket::connect(
            &format!("http://{addr}"),
            "room-jwt",
            p,
            None,
            #[cfg(feature = "record")]
            None,
        )
        .await
        .unwrap();

        let mut machine = SignalingMachine::with_signal_profile(DeterministicRng(1), &p.signal);
        machine.connect(Timestamp::ZERO);

        let (ev_tx, mut ev_rx) = mpsc::channel(8);
        drive_until_closed(&mut machine, &mut socket, Instant::now(), &ev_tx)
            .await
            .unwrap();
        drop(ev_tx);

        assert_eq!(ev_rx.recv().await, Some(SignalingEvent::Connected));
        assert_eq!(ev_rx.recv().await, Some(SignalingEvent::Closed));
        assert_eq!(ev_rx.recv().await, None);
        assert_eq!(machine.state(), SignalingState::Disconnected);
        server.await.unwrap();
    }
}
