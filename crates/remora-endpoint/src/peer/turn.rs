//! Minimal RFC 5389 / RFC 5766 STUN & TURN UDP client for ICE gathering and relay transport.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use hmac::{Hmac, KeyInit, Mac};
use md5::{Digest as _, Md5};
use rand::Rng;
use remora_proto::machine::peer::IceServerSpec;
use sha1::Sha1;
use tokio::net::UdpSocket;

const MAGIC_COOKIE: u32 = 0x2112_A442;
const STUN_HEADER_LEN: usize = 20;

const MSG_BINDING_REQUEST: u16 = 0x0001;
const MSG_BINDING_RESPONSE: u16 = 0x0101;
const MSG_ALLOCATE_REQUEST: u16 = 0x0003;
const MSG_ALLOCATE_RESPONSE: u16 = 0x0103;
const MSG_ALLOCATE_ERROR: u16 = 0x0113;
const MSG_REFRESH_REQUEST: u16 = 0x0004;
const MSG_REFRESH_RESPONSE: u16 = 0x0104;
const MSG_REFRESH_ERROR: u16 = 0x0114;
const MSG_CREATE_PERMISSION_REQUEST: u16 = 0x0008;
const MSG_CREATE_PERMISSION_RESPONSE: u16 = 0x0108;
const MSG_CREATE_PERMISSION_ERROR: u16 = 0x0118;
const MSG_SEND_INDICATION: u16 = 0x0016;
const MSG_DATA_INDICATION: u16 = 0x0017;

const ATTR_USERNAME: u16 = 0x0006;
const ATTR_MESSAGE_INTEGRITY: u16 = 0x0008;
const ATTR_ERROR_CODE: u16 = 0x0009;
const ATTR_LIFETIME: u16 = 0x000d;
const ATTR_XOR_PEER_ADDRESS: u16 = 0x0012;
const ATTR_DATA: u16 = 0x0013;
const ATTR_REALM: u16 = 0x0014;
const ATTR_NONCE: u16 = 0x0015;
const ATTR_XOR_RELAYED_ADDRESS: u16 = 0x0016;
const ATTR_REQUESTED_TRANSPORT: u16 = 0x0019;
const ATTR_XOR_MAPPED_ADDRESS: u16 = 0x0020;
const ATTR_FINGERPRINT: u16 = 0x8028;

const FINGERPRINT_XOR: u32 = 0x5354_554e;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_PENDING_PER_PEER: usize = 64;

/// Error returned by STUN/TURN operations.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum TurnError {
    InvalidUrl(String),
    Io(String),
    Timeout,
    Protocol(String),
    Rejected { code: u16, reason: String },
}

impl fmt::Display for TurnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidUrl(msg) => write!(f, "invalid ICE server URL: {msg}"),
            Self::Io(msg) => write!(f, "ICE socket error: {msg}"),
            Self::Timeout => f.write_str("STUN/TURN request timed out"),
            Self::Protocol(msg) => write!(f, "STUN/TURN protocol error: {msg}"),
            Self::Rejected { code, reason } => write!(f, "TURN error {code}: {reason}"),
        }
    }
}

impl std::error::Error for TurnError {}

impl From<std::io::Error> for TurnError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err.to_string())
    }
}

#[derive(Clone)]
struct LongTermAuth {
    username: String,
    realm: String,
    nonce: String,
    key: [u8; 16],
}

impl fmt::Debug for LongTermAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LongTermAuth")
            .field("realm", &self.realm)
            .field("has_nonce", &(!self.nonce.is_empty()))
            .finish()
    }
}

impl LongTermAuth {
    fn new(username: String, realm: String, nonce: String, password: &str) -> Self {
        let mut md5 = Md5::new();
        md5.update(username.as_bytes());
        md5.update(b":");
        md5.update(realm.as_bytes());
        md5.update(b":");
        md5.update(password.as_bytes());
        let key: [u8; 16] = md5.finalize().into();
        Self {
            username,
            realm,
            nonce,
            key,
        }
    }
}

#[derive(Debug, Clone)]
enum ParsedIceUrl {
    Stun { host: String, port: u16 },
    Turn { host: String, port: u16 },
}

fn parse_ice_url(raw: &str) -> Result<Option<ParsedIceUrl>, TurnError> {
    let (scheme, rest) = raw
        .split_once(':')
        .ok_or_else(|| TurnError::InvalidUrl(raw.to_owned()))?;
    let (host_port, query) = rest.split_once('?').unwrap_or((rest, ""));
    let transport_is_tcp = query
        .split('&')
        .any(|kv| kv.eq_ignore_ascii_case("transport=tcp"));
    // Plain UDP is supported natively here; skip TLS/TCP-only schemes if any.
    match scheme {
        "stun" => {
            let (host, port) = split_host_port(host_port, 3478)?;
            Ok(Some(ParsedIceUrl::Stun { host, port }))
        }
        "turn" if !transport_is_tcp => {
            let (host, port) = split_host_port(host_port, 3478)?;
            Ok(Some(ParsedIceUrl::Turn { host, port }))
        }
        "turn" | "turns" | "stuns" => Ok(None),
        _ => Err(TurnError::InvalidUrl(raw.to_owned())),
    }
}

fn split_host_port(input: &str, default_port: u16) -> Result<(String, u16), TurnError> {
    if let Some(rest) = input.strip_prefix('[') {
        let (host, after) = rest
            .split_once(']')
            .ok_or_else(|| TurnError::InvalidUrl(input.to_owned()))?;
        if let Some(port_str) = after.strip_prefix(':') {
            let port = port_str
                .parse()
                .map_err(|_| TurnError::InvalidUrl(input.to_owned()))?;
            return Ok((host.to_owned(), port));
        }
        return Ok((host.to_owned(), default_port));
    }
    if let Some((host, port_str)) = input.rsplit_once(':')
        && !host.contains(':')
    {
        let port = port_str
            .parse()
            .map_err(|_| TurnError::InvalidUrl(input.to_owned()))?;
        return Ok((host.to_owned(), port));
    }
    if input.is_empty() {
        return Err(TurnError::InvalidUrl("empty host".into()));
    }
    Ok((input.to_owned(), default_port))
}

async fn resolve_udp_target(host: &str, port: u16) -> Result<SocketAddr, TurnError> {
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(SocketAddr::new(ip, port));
    }
    let mut addrs = tokio::net::lookup_host((host, port))
        .await
        .map_err(|e| TurnError::Io(e.to_string()))?;
    // Prefer IPv4 when available for widest coturn/VM compatibility, falling back to first address.
    let mut first = None;
    for addr in &mut addrs {
        if first.is_none() {
            first = Some(addr);
        }
        if addr.is_ipv4() {
            return Ok(addr);
        }
    }
    first.ok_or_else(|| TurnError::Io(format!("no addresses resolved for {host}:{port}")))
}

/// Discovers a server-reflexive address from a `stun:` URL using `socket`.
pub async fn stun_binding_address(
    socket: &UdpSocket,
    stun_url: &str,
) -> Result<Option<SocketAddr>, TurnError> {
    let Some(ParsedIceUrl::Stun { host, port }) = parse_ice_url(stun_url)? else {
        return Ok(None);
    };
    let server_addr = resolve_udp_target(&host, port).await?;
    let tx_id = random_tx_id();
    let packet = encode_stun_message(MSG_BINDING_REQUEST, tx_id, &[], None, true);
    socket.send_to(&packet, server_addr).await?;

    let mut buf = [0u8; 1500];
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(TurnError::Timeout);
        }
        let (n, _from) = tokio::time::timeout(remaining, socket.recv_from(&mut buf))
            .await
            .map_err(|_| TurnError::Timeout)??;
        let Ok(msg) = decode_stun_message(&buf[..n]) else {
            continue;
        };
        if msg.tx_id == tx_id && msg.msg_type == MSG_BINDING_RESPONSE {
            return Ok(msg.xor_mapped_address);
        }
    }
}

/// Active TURN UDP allocation bound to a local socket.
pub struct TurnAllocation {
    socket: Arc<UdpSocket>,
    server_addr: SocketAddr,
    local_addr: SocketAddr,
    relayed_addr: SocketAddr,
    password: String,
    auth: LongTermAuth,
    refresh_interval: Duration,
    next_refresh: Instant,
    permitted_ips: HashSet<IpAddr>,
    inflight_permissions: HashMap<[u8; 12], IpAddr>,
    pending_ips: HashSet<IpAddr>,
    pending_packets: HashMap<IpAddr, VecDeque<(SocketAddr, Vec<u8>)>>,
    inflight_refresh: Option<[u8; 12]>,
}

impl fmt::Debug for TurnAllocation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TurnAllocation")
            .field("server_addr", &self.server_addr)
            .field("local_addr", &self.local_addr)
            .field("relayed_addr", &self.relayed_addr)
            .field("permitted_ips", &self.permitted_ips.len())
            .finish()
    }
}

impl TurnAllocation {
    /// Attempts to allocate a UDP relay address from the first reachable TURN server in `servers`.
    pub async fn allocate_first(servers: &[IceServerSpec]) -> Result<Option<Self>, TurnError> {
        let mut last_err = None;
        for server in servers {
            for url in &server.urls {
                let parsed = match parse_ice_url(url) {
                    Ok(Some(ParsedIceUrl::Turn { host, port })) => (host, port),
                    Ok(_) => continue,
                    Err(err) => {
                        last_err = Some(err);
                        continue;
                    }
                };
                match Self::allocate(&parsed.0, parsed.1, &server.username, &server.credential)
                    .await
                {
                    Ok(alloc) => return Ok(Some(alloc)),
                    Err(err) => last_err = Some(err),
                }
            }
        }
        if let Some(err) = last_err {
            Err(err)
        } else {
            Ok(None)
        }
    }

    /// Allocates a TURN relay on `host:port` using long-term credentials.
    pub async fn allocate(
        host: &str,
        port: u16,
        username: &str,
        password: &str,
    ) -> Result<Self, TurnError> {
        let server_addr = resolve_udp_target(host, port).await?;
        let bind_addr: SocketAddr = if server_addr.is_ipv6() {
            "[::]:0".parse().expect("valid ipv6 addr")
        } else {
            "0.0.0.0:0".parse().expect("valid ipv4 addr")
        };
        let socket = Arc::new(UdpSocket::bind(bind_addr).await?);
        socket.connect(server_addr).await?;
        let local_addr = socket.local_addr()?;

        // Step 1: unauthenticated Allocate to retrieve REALM + NONCE from 401.
        let tx1 = random_tx_id();
        let req1 = encode_stun_message(
            MSG_ALLOCATE_REQUEST,
            tx1,
            &[(ATTR_REQUESTED_TRANSPORT, vec![17, 0, 0, 0])],
            None,
            true,
        );
        socket.send(&req1).await?;

        let mut buf = [0u8; 2048];
        let challenge = recv_matching(&socket, &mut buf, tx1, REQUEST_TIMEOUT).await?;
        if challenge.msg_type == MSG_ALLOCATE_RESPONSE {
            let relayed_addr = challenge
                .xor_relayed_address
                .ok_or_else(|| TurnError::Protocol("missing XOR-RELAYED-ADDRESS".into()))?;
            let lifetime = challenge.lifetime.unwrap_or(600);
            let refresh_interval = refresh_interval_from_lifetime(lifetime);
            let auth =
                LongTermAuth::new(username.to_owned(), String::new(), String::new(), password);
            return Ok(Self {
                socket,
                server_addr,
                local_addr,
                relayed_addr,
                password: password.to_owned(),
                auth,
                refresh_interval,
                next_refresh: Instant::now() + refresh_interval,
                permitted_ips: HashSet::new(),
                inflight_permissions: HashMap::new(),
                pending_ips: HashSet::new(),
                pending_packets: HashMap::new(),
                inflight_refresh: None,
            });
        }
        if challenge.msg_type != MSG_ALLOCATE_ERROR {
            return Err(TurnError::Protocol(format!(
                "unexpected Allocate reply 0x{:04x}",
                challenge.msg_type
            )));
        }
        let realm = challenge
            .realm
            .ok_or_else(|| TurnError::Protocol("401 challenge missing REALM".into()))?;
        let nonce = challenge
            .nonce
            .ok_or_else(|| TurnError::Protocol("401 challenge missing NONCE".into()))?;
        let auth = LongTermAuth::new(username.to_owned(), realm, nonce, password);

        // Step 2: authenticated Allocate.
        let tx2 = random_tx_id();
        let req2 = encode_stun_message(
            MSG_ALLOCATE_REQUEST,
            tx2,
            &[
                (ATTR_REQUESTED_TRANSPORT, vec![17, 0, 0, 0]),
                (ATTR_USERNAME, auth.username.as_bytes().to_vec()),
                (ATTR_REALM, auth.realm.as_bytes().to_vec()),
                (ATTR_NONCE, auth.nonce.as_bytes().to_vec()),
            ],
            Some(&auth.key),
            true,
        );
        socket.send(&req2).await?;

        let resp = recv_matching(&socket, &mut buf, tx2, REQUEST_TIMEOUT).await?;
        match resp.msg_type {
            MSG_ALLOCATE_RESPONSE => {
                let relayed_addr = resp
                    .xor_relayed_address
                    .ok_or_else(|| TurnError::Protocol("missing XOR-RELAYED-ADDRESS".into()))?;
                let lifetime = resp.lifetime.unwrap_or(600);
                let refresh_interval = refresh_interval_from_lifetime(lifetime);
                Ok(Self {
                    socket,
                    server_addr,
                    local_addr,
                    relayed_addr,
                    password: password.to_owned(),
                    auth,
                    refresh_interval,
                    next_refresh: Instant::now() + refresh_interval,
                    permitted_ips: HashSet::new(),
                    inflight_permissions: HashMap::new(),
                    pending_ips: HashSet::new(),
                    pending_packets: HashMap::new(),
                    inflight_refresh: None,
                })
            }
            MSG_ALLOCATE_ERROR => Err(TurnError::Rejected {
                code: resp.error_code.unwrap_or(500),
                reason: resp.error_reason.unwrap_or_default(),
            }),
            other => Err(TurnError::Protocol(format!(
                "unexpected Allocate reply 0x{other:04x}"
            ))),
        }
    }

    pub fn socket(&self) -> &Arc<UdpSocket> {
        &self.socket
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub fn relayed_addr(&self) -> SocketAddr {
        self.relayed_addr
    }

    pub fn next_refresh(&self) -> Instant {
        self.next_refresh
    }

    /// Requests a TURN CreatePermission for `peer_ip` if not already permitted or pending.
    pub async fn ensure_permission(&mut self, peer_ip: IpAddr) -> Result<(), TurnError> {
        if self.permitted_ips.contains(&peer_ip) || self.pending_ips.contains(&peer_ip) {
            return Ok(());
        }
        self.send_create_permission(peer_ip).await
    }

    async fn send_create_permission(&mut self, peer_ip: IpAddr) -> Result<(), TurnError> {
        let tx_id = random_tx_id();
        let peer_attr = encode_xor_addr(SocketAddr::new(peer_ip, 0), tx_id);
        let attrs = self.auth_attrs(vec![(ATTR_XOR_PEER_ADDRESS, peer_attr)]);
        let packet = encode_stun_message(
            MSG_CREATE_PERMISSION_REQUEST,
            tx_id,
            &attrs,
            Some(&self.auth.key),
            true,
        );
        self.pending_ips.insert(peer_ip);
        self.inflight_permissions.insert(tx_id, peer_ip);
        self.socket.send(&packet).await?;
        Ok(())
    }

    /// Relays an outbound packet to `peer_addr` via a TURN Send Indication.
    ///
    /// If permission for `peer_addr.ip()` is not yet acknowledged by the TURN server, the
    /// packet is queued and a `CreatePermission` request is sent first.
    pub async fn send_to_peer(
        &mut self,
        peer_addr: SocketAddr,
        data: &[u8],
    ) -> Result<(), TurnError> {
        let ip = peer_addr.ip();
        if !self.permitted_ips.contains(&ip) {
            self.ensure_permission(ip).await?;
            let queue = self.pending_packets.entry(ip).or_default();
            if queue.len() >= MAX_PENDING_PER_PEER {
                queue.pop_front();
            }
            queue.push_back((peer_addr, data.to_vec()));
            return Ok(());
        }
        self.send_indication(peer_addr, data).await
    }

    async fn send_indication(&self, peer_addr: SocketAddr, data: &[u8]) -> Result<(), TurnError> {
        let tx_id = random_tx_id();
        let pkt = encode_stun_message(
            MSG_SEND_INDICATION,
            tx_id,
            &[
                (ATTR_XOR_PEER_ADDRESS, encode_xor_addr(peer_addr, tx_id)),
                (ATTR_DATA, data.to_vec()),
            ],
            None,
            false,
        );
        self.socket.send(&pkt).await?;
        Ok(())
    }

    /// Sends a TURN Refresh request when the refresh deadline is reached.
    pub async fn maybe_refresh(&mut self, now: Instant) -> Result<(), TurnError> {
        if now < self.next_refresh || self.inflight_refresh.is_some() {
            return Ok(());
        }
        let tx_id = random_tx_id();
        let attrs = self.auth_attrs(Vec::new());
        let pkt = encode_stun_message(
            MSG_REFRESH_REQUEST,
            tx_id,
            &attrs,
            Some(&self.auth.key),
            true,
        );
        self.inflight_refresh = Some(tx_id);
        self.next_refresh = now + self.refresh_interval;
        self.socket.send(&pkt).await?;
        Ok(())
    }

    /// Processes a packet received on the TURN UDP socket.
    ///
    /// Returns `Some((peer_addr, payload))` when a TURN `Data Indication` arrives.
    pub async fn handle_socket_packet(
        &mut self,
        packet: &[u8],
    ) -> Result<Option<(SocketAddr, Vec<u8>)>, TurnError> {
        let Ok(msg) = decode_stun_message(packet) else {
            return Ok(None);
        };
        match msg.msg_type {
            MSG_DATA_INDICATION => {
                if let (Some(peer_addr), Some(data)) = (msg.xor_peer_address, msg.data) {
                    return Ok(Some((peer_addr, data)));
                }
            }
            MSG_CREATE_PERMISSION_RESPONSE => {
                if let Some(ip) = self.inflight_permissions.remove(&msg.tx_id) {
                    self.pending_ips.remove(&ip);
                    self.permitted_ips.insert(ip);
                    if let Some(mut queued) = self.pending_packets.remove(&ip) {
                        while let Some((addr, payload)) = queued.pop_front() {
                            let _ = self.send_indication(addr, &payload).await;
                        }
                    }
                }
            }
            MSG_CREATE_PERMISSION_ERROR => {
                if let Some(ip) = self.inflight_permissions.remove(&msg.tx_id) {
                    self.pending_ips.remove(&ip);
                    if msg.error_code == Some(438)
                        && let (Some(realm), Some(nonce)) = (msg.realm, msg.nonce)
                    {
                        self.auth = LongTermAuth::new(
                            self.auth.username.clone(),
                            realm,
                            nonce,
                            &self.password,
                        );
                        let _ = self.send_create_permission(ip).await;
                    }
                }
            }
            MSG_REFRESH_RESPONSE => {
                if self.inflight_refresh == Some(msg.tx_id) {
                    self.inflight_refresh = None;
                    if let Some(lifetime) = msg.lifetime {
                        self.refresh_interval = refresh_interval_from_lifetime(lifetime);
                        self.next_refresh = Instant::now() + self.refresh_interval;
                    }
                }
            }
            MSG_REFRESH_ERROR if self.inflight_refresh == Some(msg.tx_id) => {
                self.inflight_refresh = None;
                if msg.error_code == Some(438)
                    && let (Some(realm), Some(nonce)) = (msg.realm, msg.nonce)
                {
                    self.auth =
                        LongTermAuth::new(self.auth.username.clone(), realm, nonce, &self.password);
                }
            }
            _ => {}
        }
        Ok(None)
    }

    fn auth_attrs(&self, mut extra: Vec<(u16, Vec<u8>)>) -> Vec<(u16, Vec<u8>)> {
        if !self.auth.username.is_empty() {
            extra.push((ATTR_USERNAME, self.auth.username.as_bytes().to_vec()));
        }
        if !self.auth.realm.is_empty() {
            extra.push((ATTR_REALM, self.auth.realm.as_bytes().to_vec()));
        }
        if !self.auth.nonce.is_empty() {
            extra.push((ATTR_NONCE, self.auth.nonce.as_bytes().to_vec()));
        }
        extra
    }
}

fn refresh_interval_from_lifetime(lifetime_secs: u32) -> Duration {
    let half = (u64::from(lifetime_secs) / 2).clamp(30, 300);
    Duration::from_secs(half)
}

async fn recv_matching(
    socket: &UdpSocket,
    buf: &mut [u8],
    tx_id: [u8; 12],
    timeout: Duration,
) -> Result<DecodedStun, TurnError> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(TurnError::Timeout);
        }
        let n = tokio::time::timeout(remaining, socket.recv(buf))
            .await
            .map_err(|_| TurnError::Timeout)??;
        let Ok(msg) = decode_stun_message(&buf[..n]) else {
            continue;
        };
        if msg.tx_id == tx_id {
            return Ok(msg);
        }
    }
}

pub(crate) fn random_tx_id() -> [u8; 12] {
    let mut id = [0u8; 12];
    rand::rng().fill_bytes(&mut id);
    id
}

#[derive(Debug, Default)]
pub(crate) struct DecodedStun {
    pub msg_type: u16,
    pub tx_id: [u8; 12],
    pub xor_mapped_address: Option<SocketAddr>,
    pub xor_relayed_address: Option<SocketAddr>,
    pub xor_peer_address: Option<SocketAddr>,
    pub data: Option<Vec<u8>>,
    pub realm: Option<String>,
    pub nonce: Option<String>,
    pub lifetime: Option<u32>,
    pub error_code: Option<u16>,
    pub error_reason: Option<String>,
}

pub(crate) fn encode_stun_message(
    msg_type: u16,
    tx_id: [u8; 12],
    attrs: &[(u16, Vec<u8>)],
    integrity_key: Option<&[u8]>,
    with_fingerprint: bool,
) -> Vec<u8> {
    let mut body = Vec::new();
    for (attr_type, value) in attrs {
        body.extend_from_slice(&attr_type.to_be_bytes());
        body.extend_from_slice(&(value.len() as u16).to_be_bytes());
        body.extend_from_slice(value);
        let pad = (4 - (value.len() % 4)) % 4;
        body.resize(body.len() + pad, 0);
    }

    let mut out = Vec::with_capacity(STUN_HEADER_LEN + body.len() + 32);
    out.extend_from_slice(&msg_type.to_be_bytes());
    out.extend_from_slice(&(body.len() as u16).to_be_bytes());
    out.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
    out.extend_from_slice(&tx_id);
    out.extend_from_slice(&body);

    if let Some(key) = integrity_key {
        let len_with_mi = (out.len() - STUN_HEADER_LEN + 24) as u16;
        out[2..4].copy_from_slice(&len_with_mi.to_be_bytes());
        let mut mac = Hmac::<Sha1>::new_from_slice(key).expect("HMAC accepts any key length");
        mac.update(&out);
        let tag = mac.finalize().into_bytes();
        out.extend_from_slice(&ATTR_MESSAGE_INTEGRITY.to_be_bytes());
        out.extend_from_slice(&20u16.to_be_bytes());
        out.extend_from_slice(&tag);
    }

    if with_fingerprint {
        let len_with_fp = (out.len() - STUN_HEADER_LEN + 8) as u16;
        out[2..4].copy_from_slice(&len_with_fp.to_be_bytes());
        let crc = crc32fast::hash(&out) ^ FINGERPRINT_XOR;
        out.extend_from_slice(&ATTR_FINGERPRINT.to_be_bytes());
        out.extend_from_slice(&4u16.to_be_bytes());
        out.extend_from_slice(&crc.to_be_bytes());
    }

    out
}

pub(crate) fn decode_stun_message(bytes: &[u8]) -> Result<DecodedStun, TurnError> {
    if bytes.len() < STUN_HEADER_LEN {
        return Err(TurnError::Protocol("STUN packet too short".into()));
    }
    if bytes[0] & 0xC0 != 0 {
        return Err(TurnError::Protocol("invalid STUN leading bits".into()));
    }
    let msg_type = u16::from_be_bytes([bytes[0], bytes[1]]);
    let msg_len = usize::from(u16::from_be_bytes([bytes[2], bytes[3]]));
    let cookie = u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
    if cookie != MAGIC_COOKIE {
        return Err(TurnError::Protocol("missing STUN magic cookie".into()));
    }
    if bytes.len() < STUN_HEADER_LEN + msg_len {
        return Err(TurnError::Protocol("truncated STUN message".into()));
    }
    let mut tx_id = [0u8; 12];
    tx_id.copy_from_slice(&bytes[8..20]);

    let mut out = DecodedStun {
        msg_type,
        tx_id,
        ..Default::default()
    };

    let mut cursor = STUN_HEADER_LEN;
    let end = STUN_HEADER_LEN + msg_len;
    while cursor + 4 <= end {
        let attr_type = u16::from_be_bytes([bytes[cursor], bytes[cursor + 1]]);
        let attr_len = usize::from(u16::from_be_bytes([bytes[cursor + 2], bytes[cursor + 3]]));
        cursor += 4;
        if cursor + attr_len > end {
            break;
        }
        let val = &bytes[cursor..cursor + attr_len];
        match attr_type {
            ATTR_XOR_MAPPED_ADDRESS => {
                out.xor_mapped_address = decode_xor_addr(val, tx_id);
            }
            ATTR_XOR_RELAYED_ADDRESS => {
                out.xor_relayed_address = decode_xor_addr(val, tx_id);
            }
            ATTR_XOR_PEER_ADDRESS => {
                out.xor_peer_address = decode_xor_addr(val, tx_id);
            }
            ATTR_DATA => {
                out.data = Some(val.to_vec());
            }
            ATTR_REALM => {
                out.realm = std::str::from_utf8(val).ok().map(str::to_owned);
            }
            ATTR_NONCE => {
                out.nonce = std::str::from_utf8(val).ok().map(str::to_owned);
            }
            ATTR_LIFETIME if val.len() == 4 => {
                out.lifetime = Some(u32::from_be_bytes([val[0], val[1], val[2], val[3]]));
            }
            ATTR_ERROR_CODE if val.len() >= 4 => {
                let class = u16::from(val[2] & 0x07);
                let number = u16::from(val[3]);
                out.error_code = Some(class * 100 + number);
                out.error_reason = std::str::from_utf8(&val[4..]).ok().map(str::to_owned);
            }
            _ => {}
        }
        let pad = (4 - (attr_len % 4)) % 4;
        cursor += attr_len + pad;
    }

    Ok(out)
}

pub(crate) fn encode_xor_addr(addr: SocketAddr, tx_id: [u8; 12]) -> Vec<u8> {
    let xport = addr.port() ^ ((MAGIC_COOKIE >> 16) as u16);
    match addr {
        SocketAddr::V4(v4) => {
            let mut out = Vec::with_capacity(8);
            out.push(0);
            out.push(1);
            out.extend_from_slice(&xport.to_be_bytes());
            let ip_u32 = u32::from(*v4.ip()) ^ MAGIC_COOKIE;
            out.extend_from_slice(&ip_u32.to_be_bytes());
            out
        }
        SocketAddr::V6(v6) => {
            let mut out = Vec::with_capacity(20);
            out.push(0);
            out.push(2);
            out.extend_from_slice(&xport.to_be_bytes());
            let mut mask = [0u8; 16];
            mask[..4].copy_from_slice(&MAGIC_COOKIE.to_be_bytes());
            mask[4..].copy_from_slice(&tx_id);
            let octets = v6.ip().octets();
            for i in 0..16 {
                out.push(octets[i] ^ mask[i]);
            }
            out
        }
    }
}

pub(crate) fn decode_xor_addr(val: &[u8], tx_id: [u8; 12]) -> Option<SocketAddr> {
    if val.len() < 4 {
        return None;
    }
    let family = val[1];
    let xport = u16::from_be_bytes([val[2], val[3]]);
    let port = xport ^ ((MAGIC_COOKIE >> 16) as u16);
    match family {
        1 if val.len() >= 8 => {
            let xip = u32::from_be_bytes([val[4], val[5], val[6], val[7]]);
            Some(SocketAddr::new(
                IpAddr::V4(Ipv4Addr::from(xip ^ MAGIC_COOKIE)),
                port,
            ))
        }
        2 if val.len() >= 20 => {
            let mut mask = [0u8; 16];
            mask[..4].copy_from_slice(&MAGIC_COOKIE.to_be_bytes());
            mask[4..].copy_from_slice(&tx_id);
            let mut ip = [0u8; 16];
            for i in 0..16 {
                ip[i] = val[4 + i] ^ mask[i];
            }
            Some(SocketAddr::new(IpAddr::V6(Ipv6Addr::from(ip)), port))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrips_xor_addresses_and_stun_messages() {
        let tx_id = [7u8; 12];
        let v4: SocketAddr = "203.0.113.9:49152".parse().unwrap();
        let v6: SocketAddr = "[2001:db8::1]:3478".parse().unwrap();
        assert_eq!(
            decode_xor_addr(&encode_xor_addr(v4, tx_id), tx_id),
            Some(v4)
        );
        assert_eq!(
            decode_xor_addr(&encode_xor_addr(v6, tx_id), tx_id),
            Some(v6)
        );

        let auth = LongTermAuth::new(
            "1700000000:user1".into(),
            "remora.test".into(),
            "n1".into(),
            "cred",
        );
        let pkt = encode_stun_message(
            MSG_ALLOCATE_REQUEST,
            tx_id,
            &[
                (ATTR_REQUESTED_TRANSPORT, vec![17, 0, 0, 0]),
                (ATTR_USERNAME, auth.username.as_bytes().to_vec()),
                (ATTR_REALM, auth.realm.as_bytes().to_vec()),
                (ATTR_NONCE, auth.nonce.as_bytes().to_vec()),
            ],
            Some(&auth.key),
            true,
        );
        let decoded = decode_stun_message(&pkt).unwrap();
        assert_eq!(decoded.msg_type, MSG_ALLOCATE_REQUEST);
        assert_eq!(decoded.tx_id, tx_id);
        assert_eq!(decoded.realm.as_deref(), Some("remora.test"));
        assert_eq!(decoded.nonce.as_deref(), Some("n1"));
    }

    #[tokio::test]
    async fn turn_allocation_create_permission_and_indication_roundtrip() {
        let server_sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let server_addr = server_sock.local_addr().unwrap();
        let relayed_addr: SocketAddr = "127.0.0.1:50001".parse().unwrap();

        let server = tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            // 1. Allocate -> 401
            let (n, client_addr) = server_sock.recv_from(&mut buf).await.unwrap();
            let req1 = decode_stun_message(&buf[..n]).unwrap();
            assert_eq!(req1.msg_type, MSG_ALLOCATE_REQUEST);
            let err401 = encode_stun_message(
                MSG_ALLOCATE_ERROR,
                req1.tx_id,
                &[
                    (ATTR_ERROR_CODE, vec![0, 0, 4, 1]),
                    (ATTR_REALM, b"test.realm".to_vec()),
                    (ATTR_NONCE, b"nonce-1".to_vec()),
                ],
                None,
                true,
            );
            server_sock.send_to(&err401, client_addr).await.unwrap();

            // 2. Authenticated Allocate -> success
            let (n, _) = server_sock.recv_from(&mut buf).await.unwrap();
            let req2 = decode_stun_message(&buf[..n]).unwrap();
            assert_eq!(req2.msg_type, MSG_ALLOCATE_REQUEST);
            let ok_alloc = encode_stun_message(
                MSG_ALLOCATE_RESPONSE,
                req2.tx_id,
                &[
                    (
                        ATTR_XOR_RELAYED_ADDRESS,
                        encode_xor_addr(relayed_addr, req2.tx_id),
                    ),
                    (ATTR_LIFETIME, 600u32.to_be_bytes().to_vec()),
                ],
                None,
                true,
            );
            server_sock.send_to(&ok_alloc, client_addr).await.unwrap();

            // 3. CreatePermission -> success
            let (n, _) = server_sock.recv_from(&mut buf).await.unwrap();
            let perm = decode_stun_message(&buf[..n]).unwrap();
            assert_eq!(perm.msg_type, MSG_CREATE_PERMISSION_REQUEST);
            let ok_perm =
                encode_stun_message(MSG_CREATE_PERMISSION_RESPONSE, perm.tx_id, &[], None, true);
            server_sock.send_to(&ok_perm, client_addr).await.unwrap();

            // 4. Send Indication -> echo back Data Indication
            let (n, _) = server_sock.recv_from(&mut buf).await.unwrap();
            let send_ind = decode_stun_message(&buf[..n]).unwrap();
            assert_eq!(send_ind.msg_type, MSG_SEND_INDICATION);
            let tx_out = random_tx_id();
            let data_ind = encode_stun_message(
                MSG_DATA_INDICATION,
                tx_out,
                &[
                    (
                        ATTR_XOR_PEER_ADDRESS,
                        encode_xor_addr(send_ind.xor_peer_address.unwrap(), tx_out),
                    ),
                    (ATTR_DATA, send_ind.data.unwrap()),
                ],
                None,
                false,
            );
            server_sock.send_to(&data_ind, client_addr).await.unwrap();
        });

        let mut alloc = TurnAllocation::allocate("127.0.0.1", server_addr.port(), "u", "p")
            .await
            .unwrap();
        assert_eq!(alloc.relayed_addr(), relayed_addr);

        let peer: SocketAddr = "198.51.100.2:4000".parse().unwrap();
        alloc.send_to_peer(peer, b"relayed-hello").await.unwrap();

        let mut recv_buf = [0u8; 2048];
        let mut got = None;
        for _ in 0..4 {
            let n = alloc.socket().recv(&mut recv_buf).await.unwrap();
            if let Some(item) = alloc.handle_socket_packet(&recv_buf[..n]).await.unwrap() {
                got = Some(item);
                break;
            }
        }
        assert_eq!(got, Some((peer, b"relayed-hello".to_vec())));
        server.await.unwrap();
    }
}
