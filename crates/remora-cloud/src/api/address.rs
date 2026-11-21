//! Where a request came from. Behind a reverse proxy, the connection comes from the proxy, so
//! the client's address is taken from `X-Forwarded-For` when a trusted proxy sent it.

use std::convert::Infallible;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use axum::extract::connect_info::Connected;
use axum::extract::{ConnectInfo, FromRef, FromRequestParts};
use axum::http::HeaderMap;
use axum::http::request::Parts;
use axum::serve::IncomingStream;
use ipnet::IpNet;
use serde::Deserialize;
use tokio::net::{TcpListener, UnixListener};

const FORWARDED_FOR: &str = "x-forwarded-for";

/// The proxies whose `X-Forwarded-For` is believed. Without any, the connection's address is
/// the client's.
#[derive(Debug, Clone, Default)]
pub struct TrustedProxies(Arc<[IpNet]>);

impl TrustedProxies {
    pub fn new(proxies: impl IntoIterator<Item = TrustedProxy>) -> Self {
        Self(proxies.into_iter().map(|TrustedProxy(net)| net).collect())
    }

    fn trusts(&self, address: IpAddr) -> bool {
        self.0.iter().any(|net| net.contains(&address))
    }

    /// The client behind a connection from `peer`. Connections over a Unix socket and from
    /// `trusted_proxies` are trusted to name the client in `X-Forwarded-For`; each trusted proxy
    /// appends the address it was connected from, so the client is the last one, from the right,
    /// that no trusted proxy owns.
    fn client(&self, peer: PeerAddress, headers: &HeaderMap) -> Option<IpAddr> {
        let (trusted, mut client) = match peer {
            PeerAddress::Tcp(address) => {
                let ip = address.ip().to_canonical();
                (self.trusts(ip), Some(ip))
            }
            PeerAddress::Unix => (true, None),
        };
        if !trusted {
            return client;
        }
        let hops: Vec<&str> = headers
            .get_all(FORWARDED_FOR)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .flat_map(|value| value.split(','))
            .collect();
        for hop in hops.into_iter().rev() {
            // What a trusted proxy cannot name, it cannot vouch for.
            let Some(address) = parse_hop(hop.trim()) else {
                break;
            };
            client = Some(address);
            if !self.trusts(address) {
                break;
            }
        }
        client
    }
}

/// An address, or an address with a port, as proxies write them.
fn parse_hop(hop: &str) -> Option<IpAddr> {
    hop.parse::<IpAddr>()
        .or_else(|_| hop.parse::<SocketAddr>().map(|address| address.ip()))
        .ok()
        .map(|address| address.to_canonical())
}

/// One entry of `trusted_proxies`: an address, such as `127.0.0.1`, or a network, such as
/// `10.0.0.0/8`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(try_from = "String")]
pub struct TrustedProxy(IpNet);

impl TryFrom<String> for TrustedProxy {
    type Error = String;

    fn try_from(text: String) -> Result<Self, String> {
        text.parse::<IpNet>()
            .or_else(|_| text.parse::<IpAddr>().map(IpNet::from))
            .map(|net| Self(net.trunc()))
            .map_err(|_| format!("expected an IP address or network, not {text:?}"))
    }
}

/// How a client connected to the server: over TCP or over a Unix domain socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerAddress {
    Tcp(SocketAddr),
    Unix,
}

impl From<SocketAddr> for PeerAddress {
    fn from(address: SocketAddr) -> Self {
        Self::Tcp(address)
    }
}

impl Connected<IncomingStream<'_, TcpListener>> for PeerAddress {
    fn connect_info(stream: IncomingStream<'_, TcpListener>) -> Self {
        Self::Tcp(*stream.remote_addr())
    }
}

impl Connected<IncomingStream<'_, UnixListener>> for PeerAddress {
    fn connect_info(_stream: IncomingStream<'_, UnixListener>) -> Self {
        Self::Unix
    }
}

/// The address a request came from, when the server knows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClientAddress(pub Option<IpAddr>);

impl<S> FromRequestParts<S> for ClientAddress
where
    TrustedProxies: FromRef<S>,
    S: Send + Sync,
{
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Infallible> {
        let peer = parts
            .extensions
            .get::<ConnectInfo<PeerAddress>>()
            .map(|ConnectInfo(peer)| *peer)
            .or_else(|| {
                parts
                    .extensions
                    .get::<ConnectInfo<SocketAddr>>()
                    .map(|ConnectInfo(address)| PeerAddress::Tcp(*address))
            });
        let address =
            peer.and_then(|peer| TrustedProxies::from_ref(state).client(peer, &parts.headers));
        Ok(Self(address))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proxies(entries: &[&str]) -> TrustedProxies {
        TrustedProxies::new(
            entries
                .iter()
                .map(|entry| TrustedProxy::try_from(entry.to_string()).unwrap()),
        )
    }

    fn forwarded(values: &[&str]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for value in values {
            headers.append(FORWARDED_FOR, value.parse().unwrap());
        }
        headers
    }

    fn ip(address: &str) -> IpAddr {
        address.parse().unwrap()
    }

    fn tcp(address: &str) -> PeerAddress {
        PeerAddress::Tcp(SocketAddr::new(ip(address), 40000))
    }

    #[test]
    fn untrusted_peers_are_the_client() {
        let headers = forwarded(&["203.0.113.9"]);
        assert_eq!(
            TrustedProxies::default().client(tcp("198.51.100.1"), &headers),
            Some(ip("198.51.100.1"))
        );
        assert_eq!(
            proxies(&["127.0.0.1"]).client(tcp("198.51.100.1"), &headers),
            Some(ip("198.51.100.1"))
        );
    }

    #[test]
    fn a_trusted_proxy_names_the_client() {
        let trusted = proxies(&["127.0.0.1", "10.0.0.0/8"]);
        let client = |values: &[&str]| trusted.client(tcp("127.0.0.1"), &forwarded(values));
        assert_eq!(client(&["203.0.113.9"]), Some(ip("203.0.113.9")));
        // A client can claim any address; only what the proxies appended counts.
        assert_eq!(
            client(&["192.0.2.1, 203.0.113.9, 10.1.2.3"]),
            Some(ip("203.0.113.9"))
        );
        assert_eq!(
            client(&["192.0.2.1", "203.0.113.9"]),
            Some(ip("203.0.113.9"))
        );
        assert_eq!(client(&["203.0.113.9:4711"]), Some(ip("203.0.113.9")));
        assert_eq!(client(&["[2001:db8::1]:4711"]), Some(ip("2001:db8::1")));
        assert_eq!(client(&["::ffff:203.0.113.9"]), Some(ip("203.0.113.9")));
    }

    #[test]
    fn unix_peers_are_trusted_by_default() {
        let trusted = proxies(&["10.0.0.0/8"]);
        assert_eq!(
            TrustedProxies::default().client(PeerAddress::Unix, &forwarded(&["203.0.113.9"])),
            Some(ip("203.0.113.9"))
        );
        assert_eq!(
            trusted.client(PeerAddress::Unix, &forwarded(&["203.0.113.9, 10.1.2.3"])),
            Some(ip("203.0.113.9"))
        );
        assert_eq!(
            TrustedProxies::default().client(PeerAddress::Unix, &HeaderMap::new()),
            None
        );
    }

    #[test]
    fn what_proxies_cannot_name_stays_with_them() {
        let trusted = proxies(&["127.0.0.1", "10.0.0.0/8"]);
        let client = |values: &[&str]| trusted.client(tcp("127.0.0.1"), &forwarded(values));
        assert_eq!(client(&[]), Some(ip("127.0.0.1")));
        assert_eq!(client(&["unknown, 10.1.2.3"]), Some(ip("10.1.2.3")));
        assert_eq!(client(&["10.1.2.3"]), Some(ip("10.1.2.3")));
    }

    #[test]
    fn proxies_are_addresses_or_networks() {
        let parse = |text: &str| TrustedProxy::try_from(text.to_owned());
        assert_eq!(parse("::1").unwrap(), parse("::1/128").unwrap());
        assert_eq!(parse("10.1.2.3/8").unwrap(), parse("10.0.0.0/8").unwrap());
        assert!(parse("localhost").is_err());
        assert!(parse("10.0.0.0/33").is_err());
    }
}
