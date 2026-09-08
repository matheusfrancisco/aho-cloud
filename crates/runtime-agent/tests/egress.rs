//! The egress proxy with real CONNECT tunnels to a local target.
// Helpers outside `#[test]` fns are test code too.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use cloud_contract::ledger::Stage;
use cloud_runtime::egress::{Dialer, EgressPolicies, EgressProxy, GuestPolicy};
use futures::future::BoxFuture;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Resolves test hostnames to local listeners; unknown names fail like NXDOMAIN, and
/// `internal.example.net` is refused like an internal address.
struct LocalDns(HashMap<String, SocketAddr>);

impl Dialer for LocalDns {
    fn dial<'a>(&'a self, host: &'a str, _port: u16) -> BoxFuture<'a, io::Result<TcpStream>> {
        Box::pin(async move {
            if host == "internal.example.net" {
                return Err(io::Error::new(io::ErrorKind::PermissionDenied, "internal address"));
            }
            let addr = self.0.get(host).ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such host"))?;
            TcpStream::connect(addr).await
        })
    }
}

/// Echo server that prefixes replies, so a tunnel is observable end to end.
async fn echo() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (mut s, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let mut buf = [0u8; 256];
                loop {
                    let n = s.read(&mut buf).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    let mut reply = b"echo:".to_vec();
                    reply.extend_from_slice(&buf[..n]);
                    if s.write_all(&reply).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    addr
}

async fn proxy(policies: EgressPolicies, target: SocketAddr) -> SocketAddr {
    let dns = LocalDns(
        [("api.stripe.com", target), ("o7.ingest.sentry.io", target), ("sentry.io", target), ("evil.com", target)]
            .into_iter()
            .map(|(h, a)| (h.to_owned(), a))
            .collect(),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let proxy = Arc::new(EgressProxy::new(policies, dns));
    tokio::spawn(proxy.serve(listener));
    addr
}

/// Sends `head`, returns the status line and the stream.
async fn request(proxy: SocketAddr, head: &str) -> (String, TcpStream) {
    let mut s = TcpStream::connect(proxy).await.unwrap();
    s.write_all(head.as_bytes()).await.unwrap();
    let mut got = Vec::new();
    let mut byte = [0u8; 1];
    while !got.ends_with(b"\r\n\r\n") {
        let n = s.read(&mut byte).await.unwrap();
        assert_eq!(n, 1, "proxy closed before a response head: {:?}", String::from_utf8_lossy(&got));
        got.push(byte[0]);
    }
    let text = String::from_utf8(got).unwrap();
    (text.lines().next().unwrap().to_owned(), s)
}

async fn connect(proxy: SocketAddr, authority: &str) -> (String, TcpStream) {
    request(proxy, &format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n\r\n")).await
}

fn policies() -> EgressPolicies {
    let p = EgressPolicies::default();
    p.set(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        GuestPolicy { project: "acme-shop".into(), stage: Stage::Live, allow: vec!["api.stripe.com".into(), "*.sentry.io".into()] },
    );
    p
}

#[tokio::test]
async fn allowed_hosts_are_tunnelled_byte_for_byte() {
    let target = echo().await;
    let addr = proxy(policies(), target).await;
    for host in ["api.stripe.com:443", "o7.ingest.sentry.io:443", "API.Stripe.COM.:8443"] {
        let (status, mut s) = connect(addr, host).await;
        assert_eq!(status, "HTTP/1.1 200 Connection Established", "{host}");
        s.write_all(b"hello").await.unwrap();
        let mut buf = [0u8; 10];
        s.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"echo:hello");
    }
}

#[tokio::test]
async fn bytes_sent_with_the_head_reach_the_target() {
    let target = echo().await;
    let addr = proxy(policies(), target).await;
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(b"CONNECT api.stripe.com:443 HTTP/1.1\r\n\r\nearly").await.unwrap();
    let mut buf = vec![0u8; "HTTP/1.1 200 Connection Established\r\n\r\n".len() + 10];
    s.read_exact(&mut buf).await.unwrap();
    assert!(buf.ends_with(b"echo:early"), "{:?}", String::from_utf8_lossy(&buf));
}

#[tokio::test]
async fn everything_else_is_refused() {
    let target = echo().await;
    let addr = proxy(policies(), target).await;
    for (host, why) in [
        ("evil.com:443", "not allowlisted"),
        ("sentry.io:443", "wildcard does not cover the apex"),
        ("api.stripe.com.evil.com:443", "suffix trick"),
        ("127.0.0.1:443", "IP literal"),
        ("[::1]:443", "IPv6 literal"),
        ("internal.example.net:443", "internal destination"),
    ] {
        let (status, mut s) = connect(addr, host).await;
        assert_eq!(status, "HTTP/1.1 403 Forbidden", "{host}: {why}");
        // Nothing is tunnelled after a refusal.
        let _ = s.write_all(b"x").await;
        let mut rest = Vec::new();
        let _ = s.read_to_end(&mut rest).await;
        assert!(!rest.windows(5).any(|w| w == b"echo:"), "{host} leaked");
    }

    let (status, _) = request(addr, "GET http://api.stripe.com/ HTTP/1.1\r\nHost: api.stripe.com\r\n\r\n").await;
    assert_eq!(status, "HTTP/1.1 405 Method Not Allowed");
    let (status, _) = request(addr, "CONNECT api.stripe.com HTTP/1.1\r\n\r\n").await;
    assert_eq!(status, "HTTP/1.1 400 Bad Request");
    let huge = format!("CONNECT api.stripe.com:443 HTTP/1.1\r\nX: {}\r\n\r\n", "a".repeat(9000));
    let (status, _) = request(addr, &huge).await;
    assert_eq!(status, "HTTP/1.1 431 Request Header Fields Too Large");
}

#[tokio::test]
async fn policy_follows_the_guest_address() {
    let target = echo().await;
    let p = policies();
    let addr = proxy(p.clone(), target).await;
    let (status, _) = connect(addr, "api.stripe.com:443").await;
    assert_eq!(status, "HTTP/1.1 200 Connection Established");

    // The replica stopped: its address has no policy, so nothing is allowed.
    p.remove(IpAddr::V4(Ipv4Addr::LOCALHOST));
    let (status, _) = connect(addr, "api.stripe.com:443").await;
    assert_eq!(status, "HTTP/1.1 403 Forbidden");

    // A new replica at the same address gets exactly its own deployment's allowlist.
    p.set(IpAddr::V4(Ipv4Addr::LOCALHOST), GuestPolicy { project: "other".into(), stage: Stage::Preview, allow: vec!["evil.com".into()] });
    assert_eq!(connect(addr, "evil.com:443").await.0, "HTTP/1.1 200 Connection Established");
    assert_eq!(connect(addr, "api.stripe.com:443").await.0, "HTTP/1.1 403 Forbidden");
}
