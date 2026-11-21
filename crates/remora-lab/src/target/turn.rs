//! Lightweight in-process RFC 5766 UDP TURN server for local conformance tests.
//!
//! Validates temporary TURN REST API credentials (`<expiry>:<user_id>` + HMAC-SHA1) issued
//! by `remora-cloud` so forced-relay scenarios can exercise real TURN allocations without
//! external infrastructure.

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use hmac::{Hmac, KeyInit, Mac};
use md5::{Digest as _, Md5};
use rand::RngCore as _;
use sha1::Sha1;
use tokio::net::UdpSocket;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use crate::error::LabError;

const MAGIC_COOKIE: u32 = 0x2112_A442;
const STUN_HEADER_LEN: usize = 20;

const MSG_ALLOCATE_REQUEST: u16 = 0x0003;
const MSG_ALLOCATE_RESPONSE: u16 = 0x0103;
const MSG_ALLOCATE_ERROR: u16 = 0x0113;
const MSG_REFRESH_REQUEST: u16 = 0x0004;
const MSG_REFRESH_RESPONSE: u16 = 0x0104;
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
const ATTR_XOR_MAPPED_ADDRESS: u16 = 0x0020;
const ATTR_FINGERPRINT: u16 = 0x8028;

const FINGERPRINT_XOR: u32 = 0x5354_554e;
const REALM: &str = "remora.lab";
const NONCE: &str = "remora-lab-nonce";

/// Running in-process UDP TURN server for local forced-relay lab testing.
pub struct TestTurnServer {
    addr: SocketAddr,
    tasks: Arc<Mutex<Vec<JoinHandle<()>>>>,
    main_task: JoinHandle<()>,
}

impl Drop for TestTurnServer {
    fn drop(&mut self) {
        self.main_task.abort();
        if let Ok(mut list) = self.tasks.try_lock() {
            for t in list.drain(..) {
                t.abort();
            }
        }
    }
}

impl TestTurnServer {
    pub async fn spawn(secret: impl Into<Vec<u8>>) -> Result<Self, LabError> {
        let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await?);
        let addr = socket.local_addr()?;
        let secret = secret.into();
        let tasks = Arc::new(Mutex::new(Vec::new()));
        let tasks_ref = Arc::clone(&tasks);

        let main_task = tokio::spawn(run_turn_server(socket, secret, tasks_ref));

        Ok(Self {
            addr,
            tasks,
            main_task,
        })
    }

    pub fn turn_url(&self) -> String {
        format!("turn:{}", self.addr)
    }
}

struct ClientAlloc {
    relay_socket: Arc<UdpSocket>,
    relayed_addr: SocketAddr,
    permitted_ips: HashSet<IpAddr>,
}

async fn run_turn_server(
    control_sock: Arc<UdpSocket>,
    secret: Vec<u8>,
    tasks: Arc<Mutex<Vec<JoinHandle<()>>>>,
) {
    let mut allocs: HashMap<SocketAddr, ClientAlloc> = HashMap::new();
    let mut buf = vec![0u8; 4096];

    while let Ok((n, client_addr)) = control_sock.recv_from(&mut buf).await {
        let pkt = &buf[..n];
        let Ok(parsed) = parse_stun(pkt) else {
            continue;
        };

        match parsed.msg_type {
            MSG_ALLOCATE_REQUEST => {
                if !verify_rest_auth(pkt, &parsed, &secret) {
                    let err = encode_stun(
                        MSG_ALLOCATE_ERROR,
                        parsed.tx_id,
                        &[
                            (ATTR_ERROR_CODE, vec![0, 0, 4, 1]),
                            (ATTR_REALM, REALM.as_bytes().to_vec()),
                            (ATTR_NONCE, NONCE.as_bytes().to_vec()),
                        ],
                        None,
                        true,
                    );
                    let _ = control_sock.send_to(&err, client_addr).await;
                    continue;
                }

                let entry = if let Some(existing) = allocs.get(&client_addr) {
                    existing.relayed_addr
                } else {
                    let Ok(relay_sock) = UdpSocket::bind("127.0.0.1:0").await else {
                        continue;
                    };
                    let Ok(relayed_addr) = relay_sock.local_addr() else {
                        continue;
                    };
                    let relay_sock = Arc::new(relay_sock);
                    let relay_rx = Arc::clone(&relay_sock);
                    let ctrl_tx = Arc::clone(&control_sock);
                    let relay_task = tokio::spawn(async move {
                        let mut rbuf = vec![0u8; 4096];
                        while let Ok((rn, peer_addr)) = relay_rx.recv_from(&mut rbuf).await {
                            let mut tx_id = [0u8; 12];
                            rand::rng().fill_bytes(&mut tx_id);
                            let ind = encode_stun(
                                MSG_DATA_INDICATION,
                                tx_id,
                                &[
                                    (ATTR_XOR_PEER_ADDRESS, encode_xor_addr(peer_addr, tx_id)),
                                    (ATTR_DATA, rbuf[..rn].to_vec()),
                                ],
                                None,
                                false,
                            );
                            if ctrl_tx.send_to(&ind, client_addr).await.is_err() {
                                break;
                            }
                        }
                    });
                    tasks.lock().await.push(relay_task);
                    allocs.insert(
                        client_addr,
                        ClientAlloc {
                            relay_socket: relay_sock,
                            relayed_addr,
                            permitted_ips: HashSet::new(),
                        },
                    );
                    relayed_addr
                };

                let key = long_term_key(parsed.username.as_deref().unwrap_or(""), &secret);
                let ok = encode_stun(
                    MSG_ALLOCATE_RESPONSE,
                    parsed.tx_id,
                    &[
                        (
                            ATTR_XOR_RELAYED_ADDRESS,
                            encode_xor_addr(entry, parsed.tx_id),
                        ),
                        (
                            ATTR_XOR_MAPPED_ADDRESS,
                            encode_xor_addr(client_addr, parsed.tx_id),
                        ),
                        (ATTR_LIFETIME, 600u32.to_be_bytes().to_vec()),
                    ],
                    key.as_ref().map(<[u8; 16]>::as_slice),
                    true,
                );
                let _ = control_sock.send_to(&ok, client_addr).await;
            }
            MSG_CREATE_PERMISSION_REQUEST => {
                if !verify_rest_auth(pkt, &parsed, &secret) {
                    let err = encode_stun(
                        MSG_CREATE_PERMISSION_ERROR,
                        parsed.tx_id,
                        &[(ATTR_ERROR_CODE, vec![0, 0, 4, 1])],
                        None,
                        true,
                    );
                    let _ = control_sock.send_to(&err, client_addr).await;
                    continue;
                }
                if let Some(alloc) = allocs.get_mut(&client_addr)
                    && let Some(peer) = parsed.xor_peer_address
                {
                    alloc.permitted_ips.insert(peer.ip());
                }
                let key = long_term_key(parsed.username.as_deref().unwrap_or(""), &secret);
                let ok = encode_stun(
                    MSG_CREATE_PERMISSION_RESPONSE,
                    parsed.tx_id,
                    &[],
                    key.as_ref().map(<[u8; 16]>::as_slice),
                    true,
                );
                let _ = control_sock.send_to(&ok, client_addr).await;
            }
            MSG_REFRESH_REQUEST => {
                let key = long_term_key(parsed.username.as_deref().unwrap_or(""), &secret);
                let ok = encode_stun(
                    MSG_REFRESH_RESPONSE,
                    parsed.tx_id,
                    &[(ATTR_LIFETIME, 600u32.to_be_bytes().to_vec())],
                    key.as_ref().map(<[u8; 16]>::as_slice),
                    true,
                );
                let _ = control_sock.send_to(&ok, client_addr).await;
            }
            MSG_SEND_INDICATION => {
                if let Some(alloc) = allocs.get(&client_addr)
                    && let (Some(peer), Some(data)) = (parsed.xor_peer_address, parsed.data)
                    && alloc.permitted_ips.contains(&peer.ip())
                {
                    let _ = alloc.relay_socket.send_to(&data, peer).await;
                }
            }
            _ => {}
        }
    }
}

fn long_term_key(username: &str, secret: &[u8]) -> Option<[u8; 16]> {
    if username.is_empty() {
        return None;
    }
    let mut mac = Hmac::<Sha1>::new_from_slice(secret).ok()?;
    mac.update(username.as_bytes());
    let password = BASE64.encode(mac.finalize().into_bytes());
    let mut md5 = Md5::new();
    md5.update(username.as_bytes());
    md5.update(b":");
    md5.update(REALM.as_bytes());
    md5.update(b":");
    md5.update(password.as_bytes());
    Some(md5.finalize().into())
}

fn verify_rest_auth(pkt: &[u8], parsed: &ParsedStun, secret: &[u8]) -> bool {
    let (Some(username), Some((mi_offset, mi_tag))) =
        (parsed.username.as_deref(), parsed.message_integrity)
    else {
        return false;
    };
    let Some(key) = long_term_key(username, secret) else {
        return false;
    };
    let mut prefix = pkt[..mi_offset].to_vec();
    let adjusted_len = (mi_offset - STUN_HEADER_LEN + 24) as u16;
    prefix[2..4].copy_from_slice(&adjusted_len.to_be_bytes());
    let Ok(mut mac) = Hmac::<Sha1>::new_from_slice(&key) else {
        return false;
    };
    mac.update(&prefix);
    mac.verify_slice(&mi_tag).is_ok()
}

struct ParsedStun {
    msg_type: u16,
    tx_id: [u8; 12],
    username: Option<String>,
    xor_peer_address: Option<SocketAddr>,
    data: Option<Vec<u8>>,
    message_integrity: Option<(usize, [u8; 20])>,
}

fn parse_stun(bytes: &[u8]) -> Result<ParsedStun, ()> {
    if bytes.len() < STUN_HEADER_LEN || (bytes[0] & 0xC0) != 0 {
        return Err(());
    }
    let msg_type = u16::from_be_bytes([bytes[0], bytes[1]]);
    let msg_len = usize::from(u16::from_be_bytes([bytes[2], bytes[3]]));
    if u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) != MAGIC_COOKIE
        || bytes.len() < STUN_HEADER_LEN + msg_len
    {
        return Err(());
    }
    let mut tx_id = [0u8; 12];
    tx_id.copy_from_slice(&bytes[8..20]);

    let mut username = None;
    let mut xor_peer_address = None;
    let mut data = None;
    let mut message_integrity = None;

    let mut cursor = STUN_HEADER_LEN;
    let end = STUN_HEADER_LEN + msg_len;
    while cursor + 4 <= end {
        let attr_start = cursor;
        let attr_type = u16::from_be_bytes([bytes[cursor], bytes[cursor + 1]]);
        let attr_len = usize::from(u16::from_be_bytes([bytes[cursor + 2], bytes[cursor + 3]]));
        cursor += 4;
        if cursor + attr_len > end {
            break;
        }
        let val = &bytes[cursor..cursor + attr_len];
        match attr_type {
            ATTR_USERNAME => {
                username = std::str::from_utf8(val).ok().map(str::to_owned);
            }
            ATTR_XOR_PEER_ADDRESS => {
                xor_peer_address = decode_xor_addr(val, tx_id);
            }
            ATTR_DATA => {
                data = Some(val.to_vec());
            }
            ATTR_MESSAGE_INTEGRITY if val.len() == 20 => {
                let mut tag = [0u8; 20];
                tag.copy_from_slice(val);
                message_integrity = Some((attr_start, tag));
            }
            _ => {}
        }
        let pad = (4 - (attr_len % 4)) % 4;
        cursor += attr_len + pad;
    }

    Ok(ParsedStun {
        msg_type,
        tx_id,
        username,
        xor_peer_address,
        data,
        message_integrity,
    })
}

fn encode_stun(
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
        let mut mac = Hmac::<Sha1>::new_from_slice(key).expect("HMAC accepts any key");
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

fn encode_xor_addr(addr: SocketAddr, tx_id: [u8; 12]) -> Vec<u8> {
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

fn decode_xor_addr(val: &[u8], tx_id: [u8; 12]) -> Option<SocketAddr> {
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
