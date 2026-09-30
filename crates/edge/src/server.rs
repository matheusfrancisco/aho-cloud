//! Listeners and the per-request pipeline: host → site → state gate → `/.edge/*` or route
//! mode → header hygiene → principal → upstream.

use std::collections::HashMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use cloud_api::internal::{Site, SiteState};
use cloud_contract::edge::{Mode, Routes};
use cloud_contract::principal;
use http::header::{self, HeaderValue};
use http::{Request, Response, StatusCode, Uri};
use http_body_util::{BodyExt, Limited};
use hyper::body::{Body as _, Bytes, Incoming};
use hyper::service::service_fn;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto;
use parking_lot::Mutex;
use subtle::ConstantTimeEq;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

use crate::directory::Directory;
use crate::headers::{self, Host, SESSION_COOKIE};
use crate::keys::KeyStore;
use crate::login;
use crate::mail::Mailer;
use crate::oidc::{OidcClient, OidcSettings};
use crate::pages::{self, Body};
use crate::proxy::{Outbound, Upstreams};
use crate::session::{self, Identity, MagicToken, PendingLogin, Session, TtlMap};
use crate::webhook;

const TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(30);
const ACME_PREFIX: &str = "/.well-known/acme-challenge/";

/// Answers ACME HTTP-01 challenges on port 80.
pub trait Http01: Send + Sync {
    /// The key authorization for `token`, if a challenge for it is pending.
    fn key_authorization(&self, token: &str) -> Option<String>;
}

/// Edge tunables.
#[derive(Clone, Debug)]
pub struct Settings {
    /// Lifetime of an edge login session.
    pub session_ttl_secs: u64,
    /// Upstream TCP connect timeout.
    pub connect_timeout: Duration,
    /// Cap on concurrently live sessions (and, separately, pending logins and magic links).
    pub max_sessions: usize,
    pub oidc: OidcSettings,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            session_ttl_secs: 12 * 3600,
            connect_timeout: Duration::from_secs(5),
            max_sessions: 1_000_000,
            oidc: OidcSettings::default(),
        }
    }
}

/// What the edge is built from.
pub struct Components {
    pub directory: Arc<dyn Directory>,
    pub keys: Arc<KeyStore>,
    /// Without a mailer, magic-link sites answer 503 on login.
    pub mailer: Option<Arc<dyn Mailer>>,
    /// Without one, ACME challenges answer 404.
    pub http01: Option<Arc<dyn Http01>>,
    pub settings: Settings,
}

#[derive(Debug, thiserror::Error)]
pub enum EdgeError {
    #[error("http client: {0}")]
    Http(#[from] reqwest::Error),
}

/// How a listener speaks.
pub enum Listener {
    /// The public https listener.
    Tls(Arc<rustls::ServerConfig>),
    /// The same app pipeline over plain HTTP (tests, local development); the app sees
    /// `X-Forwarded-Proto: http`.
    Plain,
    /// Port 80: ACME HTTP-01 answers, redirects to https.
    Redirect,
}

/// rustls config for the https listener: certificates chosen by `resolver` (SNI), ALPN h2 and
/// http/1.1, ring provider, TLS 1.2+.
pub fn tls_server_config(resolver: Arc<dyn rustls::server::ResolvesServerCert>) -> Result<Arc<rustls::ServerConfig>, rustls::Error> {
    let mut cfg = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_cert_resolver(resolver);
    cfg.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(Arc::new(cfg))
}

pub(crate) struct Inner {
    pub directory: Arc<dyn Directory>,
    pub keys: Arc<KeyStore>,
    pub mailer: Option<Arc<dyn Mailer>>,
    pub http01: Option<Arc<dyn Http01>>,
    pub settings: Settings,
    pub sessions: TtlMap<Session>,
    pub pending: TtlMap<PendingLogin>,
    pub magic: TtlMap<MagicToken>,
    pub oidc: OidcClient,
    upstreams: Upstreams,
    routes: Mutex<HashMap<String, CachedRoutes>>,
}

/// A site's compiled `edge.toml`, valid while the directory hands out the same `Arc<Site>`.
type CachedRoutes = (Arc<Site>, Result<Arc<Routes>, String>);

impl Inner {
    /// The live session behind `secret`, if it belongs to this site and host.
    pub fn session(&self, secret: &str, site: &Site, host: &Host) -> Option<Session> {
        self.sessions.get(secret, now()).filter(|s| s.belongs_to(site, &host.name))
    }

    fn routes(&self, host: &str, site: &Arc<Site>) -> Result<Arc<Routes>, String> {
        let mut cache = self.routes.lock();
        if let Some((cached, routes)) = cache.get(host)
            && Arc::ptr_eq(cached, site)
        {
            return routes.clone();
        }
        let compiled = site.edge.compile().map(Arc::new).map_err(|e| e.to_string());
        cache.insert(host.to_owned(), (site.clone(), compiled.clone()));
        compiled
    }

    /// Mints the principal header value, or the 500 to return instead.
    pub fn principal(&self, site: &Site, identity: &Identity, sid: &str) -> Result<HeaderValue, Box<Response<Body>>> {
        let claims = session::claims(site, identity, sid, now());
        let token = self.keys.mint(&site.project, site.stage, &claims).map_err(|e| {
            tracing::error!(project = %site.project, error = %e, "cannot mint principal");
            Box::new(internal_error())
        })?;
        HeaderValue::from_str(&token).map_err(|_| Box::new(internal_error()))
    }
}

pub(crate) fn internal_error() -> Response<Body> {
    pages::message(StatusCode::INTERNAL_SERVER_ERROR, "Edge error", "The edge could not serve this request.")
}

/// Unix seconds.
pub(crate) fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or_default()
}

/// The multi-tenant edge. Cheap to clone; clones share all state.
#[derive(Clone)]
pub struct Edge {
    inner: Arc<Inner>,
}

#[derive(Clone)]
struct Conn {
    peer: SocketAddr,
    sni: Option<String>,
    scheme: &'static str,
    redirect: bool,
}

impl Edge {
    pub fn new(c: Components) -> Result<Self, EdgeError> {
        let cap = c.settings.max_sessions;
        let inner = Inner {
            upstreams: Upstreams::new(c.settings.connect_timeout),
            oidc: OidcClient::new()?,
            directory: c.directory,
            keys: c.keys,
            mailer: c.mailer,
            http01: c.http01,
            settings: c.settings,
            sessions: TtlMap::new(cap),
            pending: TtlMap::new(cap),
            magic: TtlMap::new(cap),
            routes: Mutex::new(HashMap::new()),
        };
        Ok(Self { inner: Arc::new(inner) })
    }

    /// Accepts connections on `listener` forever.
    pub async fn serve(&self, listener: TcpListener, kind: Listener) {
        let (acceptor, scheme, redirect) = match kind {
            Listener::Tls(cfg) => (Some(TlsAcceptor::from(cfg)), "https", false),
            Listener::Plain => (None, "http", false),
            Listener::Redirect => (None, "http", true),
        };
        loop {
            let (tcp, peer) = match listener.accept().await {
                Ok(x) => x,
                Err(e) => {
                    tracing::warn!(error = %e, "accept failed");
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                }
            };
            let _ = tcp.set_nodelay(true);
            let inner = self.inner.clone();
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                match acceptor {
                    Some(acceptor) => {
                        let tls = match tokio::time::timeout(TLS_HANDSHAKE_TIMEOUT, acceptor.accept(tcp)).await {
                            Ok(Ok(tls)) => tls,
                            Ok(Err(e)) => return tracing::debug!(%peer, error = %e, "tls handshake failed"),
                            Err(_) => return tracing::debug!(%peer, "tls handshake timed out"),
                        };
                        let sni = tls.get_ref().1.server_name().map(str::to_ascii_lowercase);
                        serve_conn(inner, tls, Conn { peer, sni, scheme, redirect }).await;
                    }
                    None => serve_conn(inner, tcp, Conn { peer, sni: None, scheme, redirect }).await,
                }
            });
        }
    }
}

async fn serve_conn<I>(inner: Arc<Inner>, io: I, conn: Conn)
where
    I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let peer = conn.peer;
    let svc = service_fn(move |req| {
        let inner = inner.clone();
        let conn = conn.clone();
        async move {
            let resp = if conn.redirect { redirect(&inner, req) } else { app(&inner, &conn, req).await };
            Ok::<_, Infallible>(resp)
        }
    });
    let mut builder = auto::Builder::new(TokioExecutor::new());
    builder.http1().timer(TokioTimer::new()).header_read_timeout(HEADER_READ_TIMEOUT);
    if let Err(e) = builder.serve_connection_with_upgrades(TokioIo::new(io), svc).await {
        tracing::debug!(%peer, error = %e, "connection ended with error");
    }
}

fn redirect(inner: &Inner, req: Request<Incoming>) -> Response<Body> {
    let path = req.uri().path();
    if let Some(token) = path.strip_prefix(ACME_PREFIX) {
        let valid = !token.is_empty() && token.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
        return match inner.http01.as_ref().filter(|_| valid).and_then(|h| h.key_authorization(token)) {
            Some(key_auth) => pages::text(StatusCode::OK, key_auth),
            None => pages::text(StatusCode::NOT_FOUND, "no such challenge\n"),
        };
    }
    let Some(host) = headers::host_of(req.uri(), req.headers(), "http") else {
        return pages::text(StatusCode::BAD_REQUEST, "missing or invalid Host\n");
    };
    let Some(site) = inner.directory.site(&host.name) else { return pages::misdirected() };
    if site.state == SiteState::Tombstoned {
        return pages::tombstone(&host.name);
    }
    let target = req.uri().path_and_query().map_or("/", |p| p.as_str());
    pages::redirect(StatusCode::PERMANENT_REDIRECT, &format!("https://{}{target}", host.name))
}

async fn app(inner: &Inner, conn: &Conn, mut req: Request<Incoming>) -> Response<Body> {
    let Some(host) = headers::host_of(req.uri(), req.headers(), conn.scheme) else {
        return pages::text(StatusCode::BAD_REQUEST, "missing or invalid Host\n");
    };
    if conn.sni.as_deref().is_some_and(|sni| sni != host.name) {
        return pages::misdirected();
    }
    let Some(site) = inner.directory.site(&host.name) else { return pages::misdirected() };
    match site.state {
        SiteState::Active => {}
        SiteState::Tombstoned => return pages::tombstone(&host.name),
        SiteState::Sleeping => return pages::paused(&host.name),
        SiteState::Maintenance => return pages::maintenance(&host.name),
    }

    let path = headers::normalize_path(req.uri().path());
    if path == "/.edge" || path.starts_with("/.edge/") {
        return login::handle(inner, &site, &host, req, &path).await;
    }
    let routes = match inner.routes(&host.name, &site) {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(site = %host.name, error = %e, "edge.toml does not compile; refusing to guess");
            return internal_error();
        }
    };
    let matched = routes.resolve(&path);
    let query = req.uri().query().map(str::to_owned);
    let session = headers::cookie(req.headers(), SESSION_COOKIE).and_then(|sid| inner.session(&sid, &site, &host));
    let upgrade = headers::upgrade_protocol(req.method(), req.version(), req.headers()).map(|proto| (proto, hyper::upgrade::on(&mut req)));

    let (mut parts, body) = req.into_parts();
    let mut outbound = if body.is_end_stream() { Outbound::Empty } else { Outbound::Stream(body) };
    let mut principal = None;
    match matched.mode {
        Mode::Edge => {
            let Some(s) = session else {
                return login::redirect_to_login(&path, query.as_deref());
            };
            if !session::allowed(&site, &s.identity) {
                return login::not_invited();
            }
            principal = Some(match inner.principal(&site, &s.identity, &s.id) {
                Ok(v) => v,
                Err(resp) => return *resp,
            });
        }
        Mode::Optional => {
            if let Some(s) = session.filter(|s| session::allowed(&site, &s.identity)) {
                principal = Some(match inner.principal(&site, &s.identity, &s.id) {
                    Ok(v) => v,
                    Err(resp) => return *resp,
                });
            }
        }
        Mode::Bearer => {
            let Some(hash) = bearer(&parts.headers, &site) else {
                let mut r = pages::json(StatusCode::UNAUTHORIZED, &serde_json::json!({ "error": "invalid_token" }));
                r.headers_mut().insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
                return r;
            };
            let short = &hash[..12];
            let identity =
                Identity { sub: format!("svc:{short}"), email: None, email_verified: false, groups: Vec::new(), amr: vec!["svc".into()] };
            principal = Some(match inner.principal(&site, &identity, &format!("svc:{short}")) {
                Ok(v) => v,
                Err(resp) => return *resp,
            });
            // The edge consumed the credential; the app gets the principal instead.
            parts.headers.remove(header::AUTHORIZATION);
        }
        Mode::Public => {
            if let Some(verifier) = matched.verify {
                let Some(secret) = site.verifier_secrets.get(verifier.secret()) else {
                    tracing::error!(site = %host.name, secret = verifier.secret(), "verifier secret missing");
                    return internal_error();
                };
                let bytes = match buffer(outbound, &parts.headers).await {
                    Ok(b) => b,
                    Err(resp) => return *resp,
                };
                if let Err(e) = webhook::verify(verifier, secret, &parts.headers, &bytes, now()) {
                    tracing::info!(site = %host.name, path = %path, error = %e, "webhook signature refused");
                    return pages::json(
                        StatusCode::UNAUTHORIZED,
                        &serde_json::json!({ "error": "invalid_signature", "detail": e.to_string() }),
                    );
                }
                outbound = Outbound::Buffered(bytes);
            }
        }
        Mode::Passthrough => {}
    }

    headers::strip_inbound(&mut parts.headers);
    headers::set_forwarded(&mut parts.headers, conn.peer.ip(), conn.scheme, &host);
    if let Some(p) = principal {
        parts.headers.insert(principal::HEADER, p);
    }
    let upgrade = upgrade.map(|(proto, on)| {
        parts.headers.insert(header::CONNECTION, HeaderValue::from_static("upgrade"));
        parts.headers.insert(header::UPGRADE, proto);
        on
    });
    let target = match &query {
        Some(q) => format!("{path}?{q}"),
        None => path.clone(),
    };
    parts.uri = match Uri::try_from(target) {
        Ok(u) => u,
        Err(_) => return pages::text(StatusCode::BAD_REQUEST, "invalid request target\n"),
    };
    inner.upstreams.forward(&site, parts, outbound, upgrade).await
}

/// BLAKE3 hex of a valid bearer token, compared in constant time against every known hash.
fn bearer(headers: &http::HeaderMap, site: &Site) -> Option<String> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    let token = token.trim();
    if !scheme.eq_ignore_ascii_case("bearer") || token.is_empty() {
        return None;
    }
    let hash = blake3::hash(token.as_bytes()).to_hex().to_string();
    let mut found = subtle::Choice::from(0);
    for known in &site.service_token_hashes {
        found |= hash.as_bytes().ct_eq(known.trim().to_ascii_lowercase().as_bytes());
    }
    bool::from(found).then_some(hash)
}

/// Reads the whole body, refusing anything over [`webhook::MAX_BODY`].
async fn buffer(body: Outbound, headers: &http::HeaderMap) -> Result<Bytes, Box<Response<Body>>> {
    let too_large = || Box::new(pages::text(StatusCode::PAYLOAD_TOO_LARGE, "body exceeds 1 MiB\n"));
    let declared = headers.get(header::CONTENT_LENGTH).and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<u64>().ok());
    if declared.is_some_and(|n| n > webhook::MAX_BODY as u64) {
        return Err(too_large());
    }
    match body {
        Outbound::Empty => Ok(Bytes::new()),
        Outbound::Buffered(b) => Ok(b),
        Outbound::Stream(s) => match Limited::new(s, webhook::MAX_BODY).collect().await {
            Ok(c) => Ok(c.to_bytes()),
            Err(e) if e.downcast_ref::<http_body_util::LengthLimitError>().is_some() => Err(too_large()),
            Err(_) => Err(Box::new(pages::text(StatusCode::BAD_REQUEST, "could not read body\n"))),
        },
    }
}
