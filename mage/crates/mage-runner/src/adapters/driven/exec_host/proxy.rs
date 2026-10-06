//! Per-env loopback egress proxy (executor.md "Egress proxy").
//!
//! Env processes reach the network only through this HTTP CONNECT proxy (`HTTPS_PROXY`). It runs
//! inside the daemon on a leased loopback port and tunnels only `CONNECT <allowlisted host>:<port>`.
//! It resolves DNS itself and refuses private, loopback, link-local, CGNAT, and metadata
//! destinations, except for the server origin host. Every request head it receives is recorded as
//! one `allow`/`deny` line in the env's `egress.log`.

use std::borrow::Cow;
use std::collections::HashSet;
use std::fmt;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinSet;
use tokio::time::{Instant, timeout, timeout_at};
use tokio_util::sync::CancellationToken;

/// Maximum request head: request line, headers, and the terminating empty line.
const HEAD_MAX: usize = 8 * 1024;
/// Deadline for the client to send its whole request head.
const HEAD_TIMEOUT: Duration = Duration::from_secs(10);
/// Deadline for DNS resolution, and separately for connecting upstream.
const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(10);
/// Pause after a failed `accept` (e.g. fd exhaustion) so the loop does not spin.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);
/// Longest DNS name in text form, without the trailing dot.
const DNS_NAME_MAX: usize = 253;
const ESTABLISHED: &[u8] = b"HTTP/1.1 200 Connection Established\r\n\r\n";

#[derive(Debug, Clone)]
pub(crate) struct EgressPolicy {
    /// Exact lowercase hostnames allowed (registries ∪ server origin host, or server host only).
    pub allow_hosts: Vec<String>,
    /// The server origin host: the only host whose resolved addresses may be private/loopback.
    pub server_host: Option<String>,
    /// Allowed CONNECT port; 443 in production (a field so tests can use an ephemeral port).
    pub port: u16,
}

#[derive(Debug)]
pub(crate) struct EgressProxy {
    addr: SocketAddr,
    /// Cancelled by `shutdown`/`Drop`: stops the accept loop, which then aborts every tunnel.
    cancel: CancellationToken,
    /// Cancelled by the accept task itself (through a drop guard) once it has ended.
    accept_done: CancellationToken,
}

impl EgressProxy {
    /// Serves on `listener` (already bound to 127.0.0.1:<leased port>). Appends one line per
    /// request head (allowed or denied) to `log_path` (created on demand, append-only; format
    /// `<RFC3339 UTC> allow|deny <host>:<port> <reason>`, `-` when no target could be parsed).
    /// Nothing else from the request is logged. Must be called within a tokio runtime.
    ///
    /// Fails only if the listener's local address cannot be read.
    pub(crate) fn start(listener: TcpListener, policy: EgressPolicy, log_path: PathBuf) -> io::Result<EgressProxy> {
        let addr = listener.local_addr()?;
        let shared = Arc::new(Shared::new(policy, log_path));
        let cancel = CancellationToken::new();
        let accept_done = CancellationToken::new();
        tokio::spawn(accept_loop(listener, shared, cancel.clone(), accept_done.clone()));
        Ok(EgressProxy { addr, cancel, accept_done })
    }

    pub(crate) fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Stops accepting and aborts open tunnels; idempotent; returns when the accept task ended.
    pub(crate) async fn shutdown(&self) {
        self.cancel.cancel();
        self.accept_done.cancelled().await;
    }
}

impl Drop for EgressProxy {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

/// Normalized policy and the log, shared by the accept loop and every connection task.
struct Shared {
    allow_hosts: HashSet<String>,
    server_host: Option<String>,
    port: u16,
    log: EgressLog,
}

impl Shared {
    fn new(policy: EgressPolicy, log_path: PathBuf) -> Self {
        let allow_hosts = policy
            .allow_hosts
            .iter()
            .filter_map(|entry| {
                let host = normalize_host(entry);
                if host.is_none() {
                    tracing::warn!(entry = ?entry, "egress allowlist entry is neither a hostname nor an IP; ignored");
                }
                host
            })
            .collect();
        Self {
            allow_hosts,
            server_host: policy.server_host.as_deref().and_then(normalize_host),
            port: policy.port,
            log: EgressLog { path: log_path, lock: tokio::sync::Mutex::new(()) },
        }
    }
}

async fn accept_loop(listener: TcpListener, shared: Arc<Shared>, cancel: CancellationToken, accept_done: CancellationToken) {
    // Signals `shutdown` even if this task panics or is dropped with the runtime.
    let _done = accept_done.drop_guard();
    let mut conns = JoinSet::new();
    loop {
        tokio::select! {
            biased;
            () = cancel.cancelled() => break,
            Some(joined) = conns.join_next(), if !conns.is_empty() => {
                if let Err(err) = joined {
                    if err.is_panic() {
                        tracing::error!(error = %err, "egress proxy connection task panicked");
                    }
                }
            }
            accepted = listener.accept() => match accepted {
                Ok((stream, _peer)) => {
                    conns.spawn(serve(stream, Arc::clone(&shared)));
                }
                Err(err) => {
                    tracing::warn!(error = %err, "egress proxy accept failed");
                    tokio::select! {
                        () = cancel.cancelled() => break,
                        () = tokio::time::sleep(ACCEPT_BACKOFF) => {}
                    }
                }
            },
        }
    }
    drop(listener);
    conns.shutdown().await;
}

/// Handles one client connection: read the head, enforce the policy, then tunnel.
async fn serve(mut client: TcpStream, shared: Arc<Shared>) {
    let head = match timeout(HEAD_TIMEOUT, read_head(&mut client)).await {
        Ok(Ok(head)) => head,
        // Nothing complete was requested: a probe, or a client that went away.
        Ok(Err(HeadError::Closed | HeadError::Io)) => return,
        Ok(Err(HeadError::Incomplete)) => {
            return refuse(&mut client, &shared, Denial::bad_request()).await;
        }
        Ok(Err(HeadError::TooLarge)) => {
            let denial = Denial::new(Status::BadRequest, None, "head_too_large");
            return refuse(&mut client, &shared, denial).await;
        }
        Err(_elapsed) => {
            shared.log.record(Verdict::Deny, None, "head_timeout").await;
            return;
        }
    };
    let (target, mut upstream, peer) = match open_upstream(&head, &shared).await {
        Ok(opened) => opened,
        Err(denial) => return refuse(&mut client, &shared, denial).await,
    };
    shared.log.record(Verdict::Allow, Some(&target), &format!("connected {peer}")).await;
    if client.write_all(ESTABLISHED).await.is_err() {
        return;
    }
    // Clients may pipeline the first tunnel bytes (e.g. a TLS ClientHello) behind the head.
    let early = head.leftover();
    if !early.is_empty() && upstream.write_all(early).await.is_err() {
        return;
    }
    drop(head);
    // Runs until both directions are closed or either side fails; shutdown aborts this task.
    let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
}

/// Applies the policy to the parsed request and connects upstream.
async fn open_upstream(head: &Head, shared: &Shared) -> Result<(Target, TcpStream, SocketAddr), Denial> {
    let target = parse_request(head.head())?;
    if target.port != shared.port {
        return Err(Denial::new(Status::Forbidden, Some(target), "port_not_allowed"));
    }
    if !shared.allow_hosts.contains(&target.host) {
        let reason = if target.ip.is_some() { "ip_literal_not_allowed" } else { "host_not_allowed" };
        return Err(Denial::new(Status::Forbidden, Some(target), reason));
    }
    let Some(addrs) = resolve(&target).await else {
        return Err(Denial::new(Status::BadGateway, Some(target), "dns_failed"));
    };
    // Any blocked answer refuses the whole host, so mixed answers cannot be used for rebinding.
    if shared.server_host.as_ref() != Some(&target.host) {
        if let Some(blocked) = addrs.iter().find(|addr| is_blocked(addr.ip())) {
            let reason = format!("blocked_address {}", blocked.ip());
            return Err(Denial::new(Status::Forbidden, Some(target), reason));
        }
    }
    // First address that accepts within the deadline (e.g. `localhost` → ::1 refused, 127.0.0.1).
    let deadline = Instant::now() + UPSTREAM_TIMEOUT;
    for addr in addrs {
        match timeout_at(deadline, TcpStream::connect(addr)).await {
            Ok(Ok(stream)) => return Ok((target, stream, addr)),
            Ok(Err(_)) => {}
            Err(_elapsed) => break,
        }
    }
    Err(Denial::new(Status::BadGateway, Some(target), "connect_failed"))
}

/// Resolves the target in the proxy (env processes have no DNS). `None` on failure, timeout, or
/// an empty answer.
async fn resolve(target: &Target) -> Option<Vec<SocketAddr>> {
    if let Some(ip) = target.ip {
        return Some(vec![SocketAddr::new(ip, target.port)]);
    }
    let lookup = tokio::net::lookup_host((target.host.as_str(), target.port));
    let addrs: Vec<SocketAddr> = timeout(UPSTREAM_TIMEOUT, lookup).await.ok()?.ok()?.collect();
    (!addrs.is_empty()).then_some(addrs)
}

/// Logs the denial, then answers with its status and closes.
async fn refuse(client: &mut TcpStream, shared: &Shared, denial: Denial) {
    shared.log.record(Verdict::Deny, denial.target.as_ref(), &denial.reason).await;
    // Best effort: the client may already be gone.
    if client.write_all(denial.status.response()).await.is_ok() {
        let _ = client.shutdown().await;
    }
}

/// Destinations env traffic must never reach (unless the host is the server origin): loopback,
/// RFC 1918 / fc00::/7, link-local (incl. the 169.254.169.254 metadata address) / fe80::/10,
/// unspecified, broadcast, and CGNAT 100.64.0.0/10. IPv4-mapped IPv6 is judged as its IPv4.
fn is_blocked(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_blocked_v4(v4),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => is_blocked_v4(v4),
            None => is_blocked_v6(v6),
        },
    }
}

fn is_blocked_v4(ip: Ipv4Addr) -> bool {
    let [a, b, _, _] = ip.octets();
    let cgnat = a == 100 && (b & 0b1100_0000) == 64;
    ip.is_loopback() || ip.is_private() || ip.is_link_local() || ip.is_unspecified() || ip.is_broadcast() || cgnat
}

fn is_blocked_v6(ip: Ipv6Addr) -> bool {
    ip.is_loopback() || ip.is_unspecified() || ip.is_unique_local() || ip.is_unicast_link_local()
}

/// A CONNECT destination (or, for refused plain-HTTP requests, the absolute-form authority).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Target {
    /// Output of [`normalize_host`]: safe to log verbatim.
    host: String,
    /// Set when `host` is an IP literal.
    ip: Option<IpAddr>,
    port: u16,
}

impl Target {
    fn new(raw_host: &str, port: u16) -> Option<Self> {
        let host = normalize_host(raw_host)?;
        let ip = host.parse().ok();
        Some(Self { host, ip, port })
    }
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.ip {
            Some(IpAddr::V6(_)) => write!(f, "[{}]:{}", self.host, self.port),
            _ => write!(f, "{}:{}", self.host, self.port),
        }
    }
}

/// Canonical form used for allowlist comparison and logging: one trailing dot and IPv6 brackets
/// removed, IP literals in canonical text form, names lowercased. `None` for anything that is not
/// a plausible DNS name or IP literal, which also keeps log lines free of control characters.
fn normalize_host(raw: &str) -> Option<String> {
    let host = raw.strip_suffix('.').unwrap_or(raw);
    let unbracketed = host.strip_prefix('[').and_then(|inner| inner.strip_suffix(']')).unwrap_or(host);
    if let Ok(ip) = unbracketed.parse::<IpAddr>() {
        return Some(ip.to_string());
    }
    let valid = !host.is_empty()
        && host.len() <= DNS_NAME_MAX
        && host.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_'));
    valid.then(|| host.to_ascii_lowercase())
}

/// Parses the request line: only `CONNECT <host>:<port> HTTP/1.x` yields a target.
fn parse_request(head: &[u8]) -> Result<Target, Denial> {
    let line = head.split(|&b| b == b'\n').next().unwrap_or_default();
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    let line = std::str::from_utf8(line).map_err(|_| Denial::bad_request())?;
    let mut parts = line.split(' ');
    let (Some(method), Some(target), Some(version), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else {
        return Err(Denial::bad_request());
    };
    if !matches!(version, "HTTP/1.1" | "HTTP/1.0") {
        return Err(Denial::bad_request());
    }
    if method != "CONNECT" {
        return Err(Denial::new(Status::Forbidden, absolute_form_target(target), "not_connect"));
    }
    parse_authority(target).ok_or_else(Denial::bad_request)
}

/// `host:port` or `[v6]:port` (RFC 9110 authority-form).
fn parse_authority(authority: &str) -> Option<Target> {
    let (host, port) = if authority.starts_with('[') {
        let close = authority.find(']')?;
        let (host, rest) = authority.split_at(close + 1);
        (host, rest.strip_prefix(':')?)
    } else {
        let (host, port) = authority.rsplit_once(':')?;
        if host.contains(':') {
            return None;
        }
        (host, port)
    };
    let port_valid = !port.is_empty() && port.len() <= 5 && port.bytes().all(|b| b.is_ascii_digit());
    if !port_valid {
        return None;
    }
    Target::new(host, port.parse().ok()?)
}

/// Best-effort `host:port` of an absolute-form target (`GET http://host/…`), for the log only.
fn absolute_form_target(target: &str) -> Option<Target> {
    let url = url::Url::parse(target).ok()?;
    Target::new(url.host_str()?, url.port_or_known_default()?)
}

/// The request head plus any bytes the client sent after it in the same reads.
struct Head {
    buf: Vec<u8>,
    /// Index just past the empty line ending the head.
    end: usize,
    /// Bytes read into `buf`.
    len: usize,
}

impl Head {
    fn head(&self) -> &[u8] {
        &self.buf[..self.end]
    }

    fn leftover(&self) -> &[u8] {
        &self.buf[self.end..self.len]
    }
}

enum HeadError {
    /// EOF before any byte.
    Closed,
    /// EOF inside the head.
    Incomplete,
    /// No end of head within [`HEAD_MAX`] bytes.
    TooLarge,
    Io,
}

async fn read_head(stream: &mut TcpStream) -> Result<Head, HeadError> {
    let mut buf = vec![0; HEAD_MAX];
    let mut len = 0;
    loop {
        if len == HEAD_MAX {
            return Err(HeadError::TooLarge);
        }
        let read = stream.read(&mut buf[len..]).await.map_err(|_| HeadError::Io)?;
        if read == 0 {
            return Err(if len == 0 { HeadError::Closed } else { HeadError::Incomplete });
        }
        // The terminator may straddle reads.
        let scan_from = len.saturating_sub(3);
        len += read;
        if let Some(end) = find_head_end(&buf[..len], scan_from) {
            return Ok(Head { buf, end, len });
        }
    }
}

/// Index just past the first empty line (`\r\n\r\n`, or bare-LF `\n\n`) at or after `from`.
fn find_head_end(buf: &[u8], from: usize) -> Option<usize> {
    (from..buf.len()).find_map(|i| {
        if buf[i] != b'\n' {
            return None;
        }
        let ends = match i.checked_sub(1).map(|j| buf[j]) {
            Some(b'\n') => true,
            Some(b'\r') => i >= 2 && buf[i - 2] == b'\n',
            _ => false,
        };
        ends.then_some(i + 1)
    })
}

#[derive(Debug, Clone, Copy)]
enum Status {
    BadRequest,
    Forbidden,
    BadGateway,
}

impl Status {
    fn response(self) -> &'static [u8] {
        match self {
            Status::BadRequest => b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            Status::Forbidden => b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            Status::BadGateway => b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        }
    }
}

struct Denial {
    status: Status,
    target: Option<Target>,
    reason: Cow<'static, str>,
}

impl Denial {
    fn new(status: Status, target: Option<Target>, reason: impl Into<Cow<'static, str>>) -> Self {
        Self { status, target, reason: reason.into() }
    }

    fn bad_request() -> Self {
        Self::new(Status::BadRequest, None, "bad_request")
    }
}

#[derive(Debug, Clone, Copy)]
enum Verdict {
    Allow,
    Deny,
}

impl Verdict {
    fn as_str(self) -> &'static str {
        match self {
            Verdict::Allow => "allow",
            Verdict::Deny => "deny",
        }
    }
}

/// `<fork>/logs/egress.log`, opened per line so it may be created after the proxy starts.
struct EgressLog {
    path: PathBuf,
    /// Serializes appends from concurrent connections.
    lock: tokio::sync::Mutex<()>,
}

impl EgressLog {
    async fn record(&self, verdict: Verdict, target: Option<&Target>, reason: &str) {
        if let Err(err) = self.append(verdict, target, reason).await {
            tracing::warn!(path = %self.path.display(), error = %err, "egress log append failed");
        }
    }

    async fn append(&self, verdict: Verdict, target: Option<&Target>, reason: &str) -> io::Result<()> {
        let ts = OffsetDateTime::now_utc().format(&Rfc3339).map_err(io::Error::other)?;
        let verdict = verdict.as_str();
        let line = match target {
            Some(target) => format!("{ts} {verdict} {target} {reason}\n"),
            None => format!("{ts} {verdict} - {reason}\n"),
        };
        let _serialized = self.lock.lock().await;
        let mut file = tokio::fs::OpenOptions::new().create(true).append(true).open(&self.path).await?;
        file.write_all(line.as_bytes()).await?;
        file.flush().await
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::path::Path;

    use tempfile::TempDir;

    use super::*;

    const WAIT: Duration = Duration::from_secs(5);

    struct Fixture {
        proxy: EgressProxy,
        log: PathBuf,
        _dir: TempDir,
    }

    async fn start_proxy(allow: &[&str], server_host: Option<&str>, port: u16) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("egress.log");
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let policy = EgressPolicy {
            allow_hosts: allow.iter().map(|h| (*h).to_owned()).collect(),
            server_host: server_host.map(str::to_owned),
            port,
        };
        let proxy = EgressProxy::start(listener, policy, log.clone()).unwrap();
        Fixture { proxy, log, _dir: dir }
    }

    /// Echo server on 127.0.0.1 (IPv4 only, so `localhost` → ::1 is refused and must fall back).
    async fn echo_server() -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let (mut reader, mut writer) = stream.split();
                    let _ = tokio::io::copy(&mut reader, &mut writer).await;
                });
            }
        });
        port
    }

    async fn read_response_head(stream: &mut TcpStream) -> String {
        let mut out = Vec::new();
        let mut byte = [0u8; 1];
        while !out.ends_with(b"\r\n\r\n") {
            let read = timeout(WAIT, stream.read(&mut byte)).await.unwrap().unwrap();
            assert_eq!(read, 1, "closed before response head: {:?}", String::from_utf8_lossy(&out));
            out.push(byte[0]);
        }
        String::from_utf8(out).unwrap()
    }

    async fn send(addr: SocketAddr, request: &[u8]) -> (String, TcpStream) {
        let mut stream = TcpStream::connect(addr).await.unwrap();
        stream.write_all(request).await.unwrap();
        let head = read_response_head(&mut stream).await;
        (head, stream)
    }

    async fn assert_closed(stream: &mut TcpStream) {
        let mut buf = [0u8; 64];
        match timeout(WAIT, stream.read(&mut buf)).await.unwrap() {
            Ok(read) => assert_eq!(read, 0, "connection still open"),
            Err(err) => assert!(matches!(err.kind(), io::ErrorKind::ConnectionReset | io::ErrorKind::BrokenPipe), "{err}"),
        }
    }

    /// Asserts the log holds exactly one line `<RFC3339 UTC> <verdict> <target> <reason…>`.
    fn assert_one_line(log: &Path, verdict: &str, target: &str, reason_prefix: &str) -> String {
        let text = std::fs::read_to_string(log).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 1, "{lines:?}");
        let mut fields = lines[0].splitn(4, ' ');
        let ts = fields.next().unwrap();
        assert!(ts.ends_with('Z'), "{ts}");
        OffsetDateTime::parse(ts, &Rfc3339).unwrap();
        assert_eq!(fields.next(), Some(verdict), "{}", lines[0]);
        assert_eq!(fields.next(), Some(target), "{}", lines[0]);
        let reason = fields.next().unwrap_or_default();
        assert!(reason.starts_with(reason_prefix), "{}", lines[0]);
        lines[0].to_owned()
    }

    async fn assert_denied(fx: &Fixture, request: &str, status: &str) {
        let (head, mut stream) = send(fx.proxy.addr(), request.as_bytes()).await;
        assert!(head.starts_with(&format!("HTTP/1.1 {status} ")), "{head}");
        assert_closed(&mut stream).await;
    }

    #[tokio::test]
    async fn allowed_connect_tunnels_both_ways_and_logs_allow() {
        let port = echo_server().await;
        let fx = start_proxy(&["localhost"], Some("localhost"), port).await;
        // Early tunnel bytes ride behind the head; header values must never reach the log.
        let request = format!(
            "CONNECT localhost:{port} HTTP/1.1\r\nHost: localhost:{port}\r\n\
             Proxy-Authorization: Basic c2VjcmV0\r\n\r\nhello"
        );
        let (head, mut stream) = send(fx.proxy.addr(), request.as_bytes()).await;
        assert_eq!(head.as_bytes(), ESTABLISHED);

        let mut buf = [0u8; 5];
        timeout(WAIT, stream.read_exact(&mut buf)).await.unwrap().unwrap();
        assert_eq!(&buf, b"hello");
        stream.write_all(b"world").await.unwrap();
        timeout(WAIT, stream.read_exact(&mut buf)).await.unwrap().unwrap();
        assert_eq!(&buf, b"world");

        let line = assert_one_line(&fx.log, "allow", &format!("localhost:{port}"), &format!("connected 127.0.0.1:{port}"));
        assert!(!line.contains("c2VjcmV0") && !line.contains("Authorization"), "{line}");
    }

    #[tokio::test]
    async fn host_match_ignores_case_and_trailing_dot() {
        let port = echo_server().await;
        let fx = start_proxy(&["LocalHost."], Some("LOCALHOST"), port).await;
        let request = format!("CONNECT LOCALhost.:{port} HTTP/1.0\r\n\r\n");
        let (head, _stream) = send(fx.proxy.addr(), request.as_bytes()).await;
        assert_eq!(head.as_bytes(), ESTABLISHED);
        assert_one_line(&fx.log, "allow", &format!("localhost:{port}"), "connected");
    }

    #[tokio::test]
    async fn host_not_in_allowlist_is_forbidden() {
        let fx = start_proxy(&["registry.npmjs.org"], None, 443).await;
        assert_denied(&fx, "CONNECT evil.example.com:443 HTTP/1.1\r\n\r\n", "403").await;
        assert_one_line(&fx.log, "deny", "evil.example.com:443", "host_not_allowed");
    }

    #[tokio::test]
    async fn wrong_port_is_forbidden() {
        let fx = start_proxy(&["registry.npmjs.org"], None, 443).await;
        assert_denied(&fx, "CONNECT registry.npmjs.org:22 HTTP/1.1\r\n\r\n", "403").await;
        assert_one_line(&fx.log, "deny", "registry.npmjs.org:22", "port_not_allowed");
    }

    #[tokio::test]
    async fn plain_http_get_is_forbidden() {
        let fx = start_proxy(&["x"], None, 443).await;
        assert_denied(&fx, "GET http://x/ HTTP/1.1\r\nHost: x\r\n\r\n", "403").await;
        assert_one_line(&fx.log, "deny", "x:80", "not_connect");
    }

    #[tokio::test]
    async fn malformed_connect_is_bad_request() {
        let fx = start_proxy(&["localhost"], None, 443).await;
        assert_denied(&fx, "CONNECT localhost HTTP/1.1\r\n\r\n", "400").await;
        assert_one_line(&fx.log, "deny", "-", "bad_request");
    }

    #[tokio::test]
    async fn allowlisted_host_resolving_to_loopback_is_forbidden() {
        let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = upstream.local_addr().unwrap().port();
        let fx = start_proxy(&["localhost"], None, port).await;
        let request = format!("CONNECT localhost:{port} HTTP/1.1\r\n\r\n");
        assert_denied(&fx, &request, "403").await;
        assert_one_line(&fx.log, "deny", &format!("localhost:{port}"), "blocked_address");
        let accepted = timeout(Duration::from_millis(200), upstream.accept()).await;
        assert!(accepted.is_err(), "proxy must not connect to a blocked address");
    }

    #[tokio::test]
    async fn unlisted_ip_literal_is_forbidden() {
        let port = echo_server().await;
        let fx = start_proxy(&["localhost"], Some("localhost"), port).await;
        let request = format!("CONNECT 127.0.0.1:{port} HTTP/1.1\r\n\r\n");
        assert_denied(&fx, &request, "403").await;
        assert_one_line(&fx.log, "deny", &format!("127.0.0.1:{port}"), "ip_literal_not_allowed");
    }

    #[tokio::test]
    async fn head_limit_is_8_kib() {
        let port = echo_server().await;
        let prefix = format!("CONNECT localhost:{port} HTTP/1.1\r\nX-Pad: ");

        let fx = start_proxy(&["localhost"], Some("localhost"), port).await;
        let mut exact = prefix.clone().into_bytes();
        exact.resize(HEAD_MAX - 4, b'a');
        exact.extend_from_slice(b"\r\n\r\n");
        assert_eq!(exact.len(), HEAD_MAX);
        let (head, _stream) = send(fx.proxy.addr(), &exact).await;
        assert_eq!(head.as_bytes(), ESTABLISHED);

        let fx = start_proxy(&["localhost"], Some("localhost"), port).await;
        let mut unterminated = prefix.into_bytes();
        unterminated.resize(HEAD_MAX, b'a');
        let (head, mut stream) = send(fx.proxy.addr(), &unterminated).await;
        assert!(head.starts_with("HTTP/1.1 400 "), "{head}");
        assert_closed(&mut stream).await;
        assert_one_line(&fx.log, "deny", "-", "head_too_large");
    }

    #[tokio::test]
    async fn shutdown_aborts_tunnels_and_stops_accepting() {
        let port = echo_server().await;
        let fx = start_proxy(&["localhost"], Some("localhost"), port).await;
        let request = format!("CONNECT localhost:{port} HTTP/1.1\r\n\r\nping");
        let (head, mut stream) = send(fx.proxy.addr(), request.as_bytes()).await;
        assert_eq!(head.as_bytes(), ESTABLISHED);
        let mut buf = [0u8; 4];
        timeout(WAIT, stream.read_exact(&mut buf)).await.unwrap().unwrap();
        assert_eq!(&buf, b"ping");

        timeout(WAIT, fx.proxy.shutdown()).await.unwrap();
        assert_closed(&mut stream).await;
        assert!(TcpStream::connect(fx.proxy.addr()).await.is_err());
        // Idempotent.
        timeout(WAIT, fx.proxy.shutdown()).await.unwrap();
    }

    #[tokio::test]
    async fn drop_stops_accepting() {
        let fx = start_proxy(&["localhost"], None, 443).await;
        let addr = fx.proxy.addr();
        drop(fx.proxy);
        let deadline = Instant::now() + WAIT;
        while TcpStream::connect(addr).await.is_ok() {
            assert!(Instant::now() < deadline, "listener still accepting after drop");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[test]
    fn address_classification() {
        let cases: &[(&str, bool)] = &[
            ("169.254.169.254", true),
            ("169.254.0.1", true),
            ("10.1.2.3", true),
            ("172.16.0.1", true),
            ("172.31.255.255", true),
            ("172.32.0.1", false),
            ("172.15.255.255", false),
            ("192.168.1.1", true),
            ("100.64.0.1", true),
            ("100.127.255.255", true),
            ("100.63.255.255", false),
            ("100.128.0.0", false),
            ("127.0.0.1", true),
            ("0.0.0.0", true),
            ("255.255.255.255", true),
            ("::1", true),
            ("::", true),
            ("fe80::1", true),
            ("fc00::1", true),
            ("fd00:ec2::254", true),
            ("::ffff:127.0.0.1", true),
            ("::ffff:169.254.169.254", true),
            ("8.8.8.8", false),
            ("104.16.0.1", false),
            ("2001:4860:4860::8888", false),
            ("::ffff:8.8.8.8", false),
        ];
        for &(ip, blocked) in cases {
            let parsed: IpAddr = ip.parse().unwrap();
            assert_eq!(is_blocked(parsed), blocked, "{ip}");
        }
    }
}
