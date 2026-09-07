//! The host egress proxy: the only way out for a guest. An HTTP `CONNECT` proxy that looks up
//! the calling guest by source address (spoofing is dropped by the ruleset in [`crate::net`])
//! and tunnels only to hostnames covered by that deployment's `manifest.egress`.

use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use cloud_contract::ledger::Stage;
use cloud_contract::validate::{host_covered, is_hostname};
use futures::future::BoxFuture;
use parking_lot::RwLock;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::net::Ipv4Cidr;

/// Largest request head accepted.
pub const MAX_HEAD: usize = 8 * 1024;

/// What one guest may reach.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GuestPolicy {
    pub project: String,
    pub stage: Stage,
    /// `manifest.egress`: hostnames, a leading `*.` matching subdomains.
    pub allow: Vec<String>,
}

impl GuestPolicy {
    pub fn allows(&self, host: &str) -> bool {
        self.allow.iter().any(|pattern| host_covered(pattern, host))
    }
}

/// Policies by guest address, shared between the supervisor (writer) and the proxy (reader).
#[derive(Clone, Default)]
pub struct EgressPolicies {
    inner: Arc<RwLock<HashMap<IpAddr, Arc<GuestPolicy>>>>,
}

impl EgressPolicies {
    pub fn set(&self, guest: IpAddr, policy: GuestPolicy) {
        self.inner.write().insert(canonical(guest), Arc::new(policy));
    }

    pub fn remove(&self, guest: IpAddr) {
        self.inner.write().remove(&canonical(guest));
    }

    pub fn get(&self, guest: IpAddr) -> Option<Arc<GuestPolicy>> {
        self.inner.read().get(&canonical(guest)).cloned()
    }
}

/// IPv4-mapped IPv6 peers (dual-stack listeners) are the IPv4 guest.
fn canonical(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
        v4 => v4,
    }
}

/// Opens the upstream connection for an allowed tunnel.
pub trait Dialer: Send + Sync + 'static {
    /// `PermissionDenied` means the destination is off-limits (answered 403, not 502).
    fn dial<'a>(&'a self, host: &'a str, port: u16) -> BoxFuture<'a, io::Result<TcpStream>>;
}

/// Resolves with the system resolver and connects, refusing any address inside the host's
/// own networks (loopback, private, link-local incl. metadata, the guest subnet, …) so an
/// allowed hostname cannot be pointed back at our infrastructure.
#[derive(Clone, Debug)]
pub struct SystemDialer {
    pub guest_subnet: Ipv4Cidr,
    pub connect_timeout: Duration,
}

const SPECIAL_V4: [(Ipv4Addr, u8); 6] = [
    (Ipv4Addr::new(0, 0, 0, 0), 8),
    (Ipv4Addr::new(100, 64, 0, 0), 10),
    (Ipv4Addr::new(192, 0, 0, 0), 24),
    (Ipv4Addr::new(198, 18, 0, 0), 15),
    (Ipv4Addr::new(240, 0, 0, 0), 4),
    (Ipv4Addr::new(169, 254, 0, 0), 16),
];

/// Whether a tunnel to `ip` must be refused.
pub fn is_forbidden_destination(ip: IpAddr, guest_subnet: Ipv4Cidr) -> bool {
    let v4 = |a: Ipv4Addr| {
        a.is_loopback()
            || a.is_private()
            || a.is_multicast()
            || a.is_broadcast()
            || a.is_documentation()
            || guest_subnet.contains(a)
            || SPECIAL_V4.iter().any(|&(network, prefix_len)| Ipv4Cidr { network, prefix_len }.contains(a))
    };
    match ip {
        IpAddr::V4(a) => v4(a),
        IpAddr::V6(a) => match a.to_ipv4_mapped() {
            Some(mapped) => v4(mapped),
            None => {
                let first = a.segments()[0];
                a.is_loopback()
                    || a.is_unspecified()
                    || a.is_multicast()
                    || first & 0xfe00 == 0xfc00 // unique local
                    || first & 0xffc0 == 0xfe80 // link local
                    || first == 0x2001 && a.segments()[1] == 0x0db8 // documentation
            }
        },
    }
}

impl Dialer for SystemDialer {
    fn dial<'a>(&'a self, host: &'a str, port: u16) -> BoxFuture<'a, io::Result<TcpStream>> {
        Box::pin(async move {
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host, port)).await?.collect();
            let allowed: Vec<SocketAddr> = addrs.iter().copied().filter(|a| !is_forbidden_destination(a.ip(), self.guest_subnet)).collect();
            if allowed.is_empty() {
                return Err(io::Error::new(io::ErrorKind::PermissionDenied, format!("{host} resolves only to internal addresses")));
            }
            let mut last = io::Error::new(io::ErrorKind::NotFound, format!("{host}: no addresses"));
            for addr in allowed {
                match tokio::time::timeout(self.connect_timeout, TcpStream::connect(addr)).await {
                    Ok(Ok(stream)) => return Ok(stream),
                    Ok(Err(e)) => last = e,
                    Err(_) => last = io::Error::new(io::ErrorKind::TimedOut, format!("connect {addr}: timed out")),
                }
            }
            Err(last)
        })
    }
}

/// A parsed `CONNECT host:port` target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    /// Lowercased, without a trailing dot, brackets stripped from IPv6 literals.
    pub host: String,
    pub port: u16,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HeadError {
    /// Anything other than `CONNECT`.
    Method(String),
    Malformed,
}

/// Parses the request line of a proxy request head.
pub fn parse_connect(head: &[u8]) -> Result<Target, HeadError> {
    let text = std::str::from_utf8(head).map_err(|_| HeadError::Malformed)?;
    let line = text.split("\r\n").next().unwrap_or_default();
    let mut parts = line.split(' ');
    let (Some(method), Some(target), Some(version), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else {
        return Err(HeadError::Malformed);
    };
    if !version.starts_with("HTTP/1.") {
        return Err(HeadError::Malformed);
    }
    if method != "CONNECT" {
        return Err(HeadError::Method(method.to_owned()));
    }
    let (host, port) = target.rsplit_once(':').ok_or(HeadError::Malformed)?;
    let port: u16 = port.parse().map_err(|_| HeadError::Malformed)?;
    if port == 0 {
        return Err(HeadError::Malformed);
    }
    let host = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')).unwrap_or(host);
    let host = host.strip_suffix('.').unwrap_or(host).to_ascii_lowercase();
    if host.is_empty() {
        return Err(HeadError::Malformed);
    }
    Ok(Target { host, port })
}

/// Reads until the blank line ending the head; returns the head and any bytes after it.
async fn read_head(stream: &mut TcpStream) -> io::Result<(Vec<u8>, Vec<u8>)> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    loop {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "closed before request head"));
        }
        let scan_from = buf.len().saturating_sub(3);
        buf.extend_from_slice(&chunk[..n]);
        let found = buf[scan_from..].windows(4).position(|w| w == b"\r\n\r\n").map(|pos| scan_from + pos + 4);
        // The limit applies to the head itself, however the bytes were split across reads.
        if found.unwrap_or(buf.len()) > MAX_HEAD {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "request head too large"));
        }
        if let Some(end) = found {
            let rest = buf.split_off(end);
            return Ok((buf, rest));
        }
    }
}

/// Answers and closes. Unread input is drained briefly first: closing a socket with unread
/// data sends a reset, which can destroy the response before the client reads it.
async fn respond(stream: &mut TcpStream, status: &str, body: &str) -> io::Result<()> {
    let msg = format!("HTTP/1.1 {status}\r\ncontent-type: text/plain\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len());
    stream.write_all(msg.as_bytes()).await?;
    stream.shutdown().await?;
    let drain = async {
        let mut sink = [0u8; 4096];
        let mut left = 64 * 1024usize;
        while left > 0 {
            match stream.read(&mut sink).await {
                Ok(0) | Err(_) => break,
                Ok(n) => left = left.saturating_sub(n),
            }
        }
    };
    let _ = tokio::time::timeout(Duration::from_secs(1), drain).await;
    Ok(())
}

/// The proxy. Cheap to share: `Arc<EgressProxy<_>>`.
pub struct EgressProxy<D> {
    policies: EgressPolicies,
    dialer: D,
    head_timeout: Duration,
}

impl<D: Dialer> EgressProxy<D> {
    pub fn new(policies: EgressPolicies, dialer: D) -> Self {
        EgressProxy { policies, dialer, head_timeout: Duration::from_secs(10) }
    }

    /// Accepts connections forever, one task per tunnel.
    pub async fn serve(self: Arc<Self>, listener: TcpListener) -> io::Result<()> {
        loop {
            let (stream, peer) = listener.accept().await?;
            let proxy = self.clone();
            tokio::spawn(async move {
                if let Err(e) = proxy.handle(stream, peer.ip()).await {
                    tracing::debug!(%peer, error = %e, "egress connection ended");
                }
            });
        }
    }

    async fn handle(&self, mut client: TcpStream, peer: IpAddr) -> io::Result<()> {
        let (head, rest) = match tokio::time::timeout(self.head_timeout, read_head(&mut client)).await {
            Ok(Ok(parts)) => parts,
            Ok(Err(e)) if e.kind() == io::ErrorKind::InvalidData => {
                return respond(&mut client, "431 Request Header Fields Too Large", "request head too large\n").await;
            }
            Ok(Err(e)) => return Err(e),
            Err(_) => return respond(&mut client, "408 Request Timeout", "request head timed out\n").await,
        };
        let target = match parse_connect(&head) {
            Ok(t) => t,
            Err(HeadError::Method(m)) => {
                return respond(&mut client, "405 Method Not Allowed", &format!("only CONNECT is proxied, not {m}\n")).await;
            }
            Err(HeadError::Malformed) => return respond(&mut client, "400 Bad Request", "malformed request\n").await,
        };
        let Some(policy) = self.policies.get(peer) else {
            tracing::warn!(%peer, host = %target.host, "egress from unknown address refused");
            return respond(&mut client, "403 Forbidden", "unknown guest\n").await;
        };
        if !is_hostname(&target.host, false) || !policy.allows(&target.host) {
            tracing::info!(%peer, project = %policy.project, host = %target.host, "egress refused");
            let body = format!("egress to {} is not in the deployment's allowlist\n", target.host);
            return respond(&mut client, "403 Forbidden", &body).await;
        }
        let mut upstream = match self.dialer.dial(&target.host, target.port).await {
            Ok(s) => s,
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                return respond(&mut client, "403 Forbidden", &format!("{e}\n")).await;
            }
            Err(e) => return respond(&mut client, "502 Bad Gateway", &format!("{e}\n")).await,
        };
        tracing::info!(%peer, project = %policy.project, host = %target.host, port = target.port, "egress tunnel");
        client.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n").await?;
        if !rest.is_empty() {
            upstream.write_all(&rest).await?;
        }
        tokio::io::copy_bidirectional(&mut client, &mut upstream).await.map(drop)
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv6Addr;

    use super::*;

    #[test]
    fn connect_heads_parse_and_normalize() {
        let t = parse_connect(b"CONNECT API.Stripe.com.:443 HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
        assert_eq!(t, Target { host: "api.stripe.com".into(), port: 443 });
        let v6 = parse_connect(b"CONNECT [::1]:8443 HTTP/1.1\r\n\r\n").unwrap();
        assert_eq!(v6, Target { host: "::1".into(), port: 8443 });
        assert_eq!(parse_connect(b"GET http://x.com/ HTTP/1.1\r\n\r\n"), Err(HeadError::Method("GET".into())));
        for bad in [
            &b"CONNECT x.com HTTP/1.1\r\n\r\n"[..],
            b"CONNECT x.com:0 HTTP/1.1\r\n\r\n",
            b"CONNECT x.com:99999 HTTP/1.1\r\n\r\n",
            b"CONNECT :443 HTTP/1.1\r\n\r\n",
            b"CONNECT x.com:443\r\n\r\n",
            b"CONNECT x.com:443 SMTP\r\n\r\n",
            b"CONNECT  x.com:443 HTTP/1.1\r\n\r\n",
        ] {
            assert_eq!(parse_connect(bad), Err(HeadError::Malformed), "{}", String::from_utf8_lossy(bad));
        }
    }

    #[test]
    fn wildcards_cover_subdomains_only() {
        let p = GuestPolicy { project: "shop".into(), stage: Stage::Live, allow: vec!["api.example.com".into(), "*.stripe.com".into()] };
        assert!(p.allows("api.example.com"));
        assert!(p.allows("files.stripe.com"));
        assert!(p.allows("a.b.stripe.com"));
        assert!(!p.allows("stripe.com"));
        assert!(!p.allows("evilstripe.com"));
        assert!(!p.allows("example.com"));
        assert!(!p.allows("api.example.com.evil.net"));
    }

    #[test]
    fn policies_key_by_guest_including_mapped_v6_peers() {
        let policies = EgressPolicies::default();
        let guest = IpAddr::V4(Ipv4Addr::new(10, 77, 0, 2));
        policies.set(guest, GuestPolicy { project: "p".into(), stage: Stage::Preview, allow: vec![] });
        let mapped = IpAddr::V6(Ipv4Addr::new(10, 77, 0, 2).to_ipv6_mapped());
        assert_eq!(policies.get(mapped).unwrap().project, "p");
        policies.remove(mapped);
        assert!(policies.get(guest).is_none());
    }

    #[test]
    fn internal_destinations_are_forbidden() {
        let subnet: Ipv4Cidr = "10.77.0.0/24".parse().unwrap();
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "10.77.0.1",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "224.0.0.1",
            "255.255.255.255",
            "::1",
            "::",
            "fe80::1",
            "fd00::1",
            "::ffff:127.0.0.1",
        ] {
            assert!(is_forbidden_destination(ip.parse().unwrap(), subnet), "{ip}");
        }
        for ip in ["8.8.8.8", "1.1.1.1", "2606:4700::1111", "::ffff:8.8.4.4"] {
            assert!(!is_forbidden_destination(ip.parse().unwrap(), subnet), "{ip}");
        }
        let custom: Ipv4Cidr = "203.1.113.0/24".parse().unwrap();
        assert!(is_forbidden_destination(IpAddr::V4(Ipv4Addr::new(203, 1, 113, 9)), custom));
        assert!(!is_forbidden_destination(IpAddr::V6(Ipv6Addr::new(0x2a00, 0, 0, 0, 0, 0, 0, 1)), custom));
    }
}
