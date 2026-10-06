//! The public HTTP surface assembled from every area's router (server.md "Router map",
//! "Request pipeline", "Credential isolation").
//!
//! Layers, outermost first: host routing (trace host vs public host) → request id + span
//! `http.request` → client IP → per group: security headers, handler timeout, body limit,
//! credential (session for `/api`, `/events`, `/integrations/*/callback`; token groups bring
//! their own) → rate limit → handler. Errors in session groups render as `ApiErrorBody` with the
//! request id. Anything not routed is 404.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use axum::extract::{ConnectInfo, DefaultBodyLimit, Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, header};
use axum::middleware::{Next, from_fn, from_fn_with_state};
use axum::response::{IntoResponse, Response};
use futures::StreamExt;
use parking_lot::Mutex;
use tower::ServiceExt;
use tracing::Instrument;
use url::Url;

use mage_core::wire::api::ApiErrorCode;

use crate::adapters::driving::http::auth::{AuthState, Authed, ClientIp, RequestId, SessionGuard, keys_router, require_session};
use crate::adapters::driving::http::error::{api_error, fill_request_id};
use crate::adapters::driving::http::{api_orgs, assets, auth, feed_sse, health};
use crate::app::auth::SessionId;
use crate::error::AppError;
use crate::ports::auth::TokenSigner;

/// `Content-Security-Policy` of UI, API and event responses (server.md "Untrusted content").
pub const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data: https://avatars.githubusercontent.com https://lh3.googleusercontent.com; media-src 'self'; connect-src 'self'; font-src 'self'; base-uri 'none'; form-action 'self' https://github.com https://accounts.google.com; frame-ancestors 'none'";

const API_BODY_LIMIT: usize = 1024 * 1024;
const AUTH_BODY_LIMIT: usize = 16 * 1024;
const API_TIMEOUT: Duration = Duration::from_secs(30);
const AUTH_TIMEOUT: Duration = Duration::from_secs(10);

/// Routers contributed by the other areas (`pub fn router(state) -> Router`, state applied,
/// absolute paths, no fallback unless nested).
#[derive(Default)]
pub struct AreaRouters {
    /// `/api/v1/**`: session + CSRF + `OrgCtx`, 1 MiB default body limit (routes may lower it
    /// with `DefaultBodyLimit`), 30 s, 20 req/s per session.
    pub api: Vec<Router>,
    /// `/events/**` (SSE): session + `OrgCtx`, no timeout (streams close themselves), at most
    /// 6 open streams per session.
    pub events: Vec<Router>,
    /// `/integrations/*/callback`-style browser redirects: session, 16 KiB, 10 s.
    pub callbacks: Vec<Router>,
    /// Token/signature groups (`/hooks`, `/runner/v1`, `/mcp/knowledge`, `/share`, dev blob):
    /// merged as-is; the area applies its credential, limits and timeouts. They never see
    /// session cookies' effects (no session layer).
    pub token: Vec<Router>,
    /// Routers nested under a prefix so their own fallback applies there
    /// (e.g. `("/llm/anthropic", llm::router(..))`).
    pub nested: Vec<(&'static str, Router)>,
}

/// The trace-viewer origin: only this router answers on `host` (server.md "Trace viewer origin").
pub struct TraceHost {
    /// Host header value, e.g. `mage-trace.example.com` (port included when not default).
    pub host: String,
    pub router: Router,
}

/// Per-instance rate limits (approximate by design: no shared state across instances).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RateLimits {
    pub api_per_sec: f64,
    pub api_burst: f64,
    pub auth_per_min: f64,
    pub event_streams_per_session: usize,
}

impl Default for RateLimits {
    fn default() -> Self {
        RateLimits { api_per_sec: 20.0, api_burst: 60.0, auth_per_min: 10.0, event_streams_per_session: 6 }
    }
}

/// Everything the platform's own routes need.
pub struct PlatformHttp {
    pub public_url: Url,
    pub guard: SessionGuard,
    pub auth: AuthState,
    pub orgs_api: api_orgs::OrgsApiState,
    pub feed: feed_sse::FeedState,
    pub health: health::HealthState,
    pub signer: Arc<dyn TokenSigner>,
    /// The embedded `mage/web-app` bundle; `None` serves the API only (UI paths answer 404).
    pub ui: Option<assets::Assets>,
    pub trace: Option<TraceHost>,
    pub limits: RateLimits,
}

/// `scheme://host[:port]` of a URL (the browser's `Origin` serialization).
pub fn origin_of(url: &Url) -> String {
    url.origin().ascii_serialization()
}

/// The whole public surface.
pub fn router(platform: PlatformHttp, areas: AreaRouters) -> Router {
    let PlatformHttp { public_url, guard, auth, orgs_api, feed, health, signer, ui, trace, limits } = platform;
    let hsts = public_url.scheme() == "https";
    let headers = SecurityHeaders { hsts };

    // /api/v1 — session + CSRF + OrgCtx.
    let api_limiter = Arc::new(Buckets::new(limits.api_per_sec, limits.api_burst));
    let mut api = api_orgs::router(orgs_api.clone());
    for r in areas.api {
        api = api.merge(r);
    }
    let api = api
        .layer(from_fn_with_state(api_limiter, limit_per_session))
        .layer(from_fn_with_state(guard.clone(), require_session))
        .layer(DefaultBodyLimit::max(API_BODY_LIMIT))
        .layer(from_fn_with_state(API_TIMEOUT, timeout))
        .layer(from_fn_with_state(headers, security_headers));

    // /events — session + OrgCtx, stream cap per session, no timeout.
    let streams = Arc::new(StreamCounter::new(limits.event_streams_per_session));
    let mut events = feed_sse::router(feed);
    for r in areas.events {
        events = events.merge(r);
    }
    let events = events
        .layer(from_fn_with_state(streams, limit_streams))
        .layer(from_fn_with_state(guard.clone(), require_session))
        .layer(from_fn_with_state(headers, security_headers));

    // /auth and integration callbacks — per-IP limit, small bodies, 10 s.
    let auth_limiter = Arc::new(Buckets::new(limits.auth_per_min / 60.0, limits.auth_per_min));
    let mut callbacks = api_orgs::callbacks_router(orgs_api);
    for r in areas.callbacks {
        callbacks = callbacks.merge(r);
    }
    let callbacks = callbacks.layer(from_fn_with_state(guard, require_session));
    let auth_group = auth::router(auth)
        .merge(callbacks)
        .layer(from_fn_with_state(auth_limiter, limit_per_ip))
        .layer(DefaultBodyLimit::max(AUTH_BODY_LIMIT))
        .layer(from_fn_with_state(AUTH_TIMEOUT, timeout))
        .layer(from_fn_with_state(headers, security_headers));

    let mut public = Router::new().merge(api).merge(events).merge(auth_group).merge(health::router(health)).merge(keys_router(signer));
    if let Some(ui) = ui {
        public = public.merge(assets::router(ui).layer(from_fn_with_state(headers, security_headers)));
    }
    for r in areas.token {
        public = public.merge(r);
    }
    for (prefix, r) in areas.nested {
        public = public.nest(prefix, r);
    }
    let public = public.fallback(not_found);

    let app = match trace {
        Some(trace) => {
            let dispatch = Arc::new(trace);
            public.layer(from_fn_with_state(dispatch, route_host))
        }
        None => public,
    };
    app.layer(from_fn(client_ip)).layer(from_fn(request_id))
}

// ---------------------------------------------------------------------------------------------
// Layers
// ---------------------------------------------------------------------------------------------

/// Outermost: the trace host serves only its own router.
async fn route_host(State(trace): State<Arc<TraceHost>>, req: Request, next: Next) -> Response {
    let host = req.headers().get(header::HOST).and_then(|h| h.to_str().ok()).unwrap_or("");
    if host.eq_ignore_ascii_case(&trace.host) {
        return match trace.router.clone().oneshot(req).await {
            Ok(response) => response,
            Err(never) => match never {},
        };
    }
    next.run(req).await
}

/// Request id + root span; renders the id into API error bodies and `x-request-id`.
async fn request_id(mut req: Request, next: Next) -> Response {
    let id = format!("req_{}", uuid::Uuid::new_v4().simple());
    req.extensions_mut().insert(RequestId(id.clone()));
    let span = tracing::info_span!(
        "http.request",
        method = %req.method(),
        path = %req.uri().path(),
        request_id = %id,
        traceparent = tracing::field::Empty,
        status = tracing::field::Empty,
        org_id = tracing::field::Empty,
        user_id = tracing::field::Empty,
    );
    if let Some(tp) = req.headers().get("traceparent").and_then(|v| v.to_str().ok()) {
        span.record("traceparent", tp);
    }
    let response = next.run(req).instrument(span.clone()).await;
    span.record("status", response.status().as_u16());
    let mut response = fill_request_id(response, &id);
    if let Ok(value) = HeaderValue::from_str(&id) {
        response.headers_mut().insert(HeaderName::from_static("x-request-id"), value);
    }
    response
}

/// The rightmost `X-Forwarded-For` entry is the address Google's front end saw (earlier
/// entries are client-controlled); without the header, the TCP peer.
pub fn client_ip_of(headers: &HeaderMap, peer: Option<SocketAddr>) -> Option<IpAddr> {
    let forwarded = headers
        .get_all("x-forwarded-for")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .filter_map(|hop| hop.trim().parse::<IpAddr>().ok())
        .last();
    forwarded.or(peer.map(|p| p.ip()))
}

async fn client_ip(mut req: Request, next: Next) -> Response {
    let peer = req.extensions().get::<ConnectInfo<SocketAddr>>().map(|c| c.0);
    let ip = client_ip_of(req.headers(), peer);
    req.extensions_mut().insert(ClientIp(ip));
    next.run(req).await
}

#[derive(Debug, Clone, Copy)]
struct SecurityHeaders {
    hsts: bool,
}

/// server.md "Untrusted content": CSP, HSTS, no referrer, nosniff (handlers may set stricter
/// values; existing headers are kept).
async fn security_headers(State(cfg): State<SecurityHeaders>, req: Request, next: Next) -> Response {
    let mut response = next.run(req).await;
    let h = response.headers_mut();
    h.entry(header::CONTENT_SECURITY_POLICY).or_insert(HeaderValue::from_static(CSP));
    h.entry(header::REFERRER_POLICY).or_insert(HeaderValue::from_static("no-referrer"));
    h.entry(header::X_CONTENT_TYPE_OPTIONS).or_insert(HeaderValue::from_static("nosniff"));
    if cfg.hsts {
        h.entry(header::STRICT_TRANSPORT_SECURITY).or_insert(HeaderValue::from_static("max-age=63072000; includeSubDomains"));
    }
    response
}

async fn timeout(State(limit): State<Duration>, req: Request, next: Next) -> Response {
    match tokio::time::timeout(limit, next.run(req)).await {
        Ok(response) => response,
        Err(_) => AppError::Unavailable(format!("handler exceeded {} s", limit.as_secs())).into_response(),
    }
}

async fn not_found(req: Request) -> Response {
    let path = req.uri().path();
    if ["/api/", "/events/", "/auth/"].iter().any(|p| path.starts_with(p)) {
        api_error(ApiErrorCode::NotFound, "not found")
    } else {
        (axum::http::StatusCode::NOT_FOUND, "not found").into_response()
    }
}

// ---------------------------------------------------------------------------------------------
// Rate limits
// ---------------------------------------------------------------------------------------------

/// Token buckets by key, refilled continuously.
pub struct Buckets {
    rate: f64,
    burst: f64,
    buckets: Mutex<HashMap<String, (f64, Instant)>>,
}

const BUCKETS_MAX: usize = 50_000;

impl Buckets {
    pub fn new(rate_per_sec: f64, burst: f64) -> Buckets {
        Buckets { rate: rate_per_sec.max(f64::MIN_POSITIVE), burst: burst.max(1.0), buckets: Mutex::new(HashMap::new()) }
    }

    /// Takes one token; `Err(seconds)` until one is available.
    pub fn take(&self, key: &str, now: Instant) -> Result<(), u64> {
        let mut buckets = self.buckets.lock();
        if buckets.len() >= BUCKETS_MAX {
            let (rate, burst) = (self.rate, self.burst);
            buckets.retain(|_, (tokens, at)| *tokens + now.saturating_duration_since(*at).as_secs_f64() * rate < burst);
        }
        let (tokens, at) = buckets.entry(key.to_owned()).or_insert((self.burst, now));
        *tokens = (*tokens + now.saturating_duration_since(*at).as_secs_f64() * self.rate).min(self.burst);
        *at = now;
        if *tokens >= 1.0 {
            *tokens -= 1.0;
            Ok(())
        } else {
            Err(((1.0 - *tokens) / self.rate).ceil().max(1.0) as u64)
        }
    }
}

async fn limit_per_session(State(buckets): State<Arc<Buckets>>, req: Request, next: Next) -> Response {
    let key = req.extensions().get::<Authed>().map(|a| hex::encode(a.session.0));
    if let Some(key) = key {
        if let Err(secs) = buckets.take(&key, Instant::now()) {
            return AppError::RateLimited { retry_after_secs: Some(secs) }.into_response();
        }
    }
    next.run(req).await
}

async fn limit_per_ip(State(buckets): State<Arc<Buckets>>, req: Request, next: Next) -> Response {
    let key = req.extensions().get::<ClientIp>().and_then(|c| c.0).map_or_else(|| "unknown".to_owned(), |ip| ip.to_string());
    if let Err(secs) = buckets.take(&key, Instant::now()) {
        return AppError::RateLimited { retry_after_secs: Some(secs) }.into_response();
    }
    next.run(req).await
}

/// Open SSE streams per session.
pub struct StreamCounter {
    max: usize,
    open: Arc<Mutex<HashMap<SessionId, usize>>>,
}

impl StreamCounter {
    pub fn new(max: usize) -> StreamCounter {
        StreamCounter { max: max.max(1), open: Arc::new(Mutex::new(HashMap::new())) }
    }

    fn acquire(&self, session: SessionId) -> Option<StreamSlot> {
        let mut open = self.open.lock();
        let n = open.entry(session).or_insert(0);
        if *n >= self.max {
            return None;
        }
        *n += 1;
        Some(StreamSlot { session, open: self.open.clone() })
    }
}

/// Released when the response body is dropped (stream closed by either side).
struct StreamSlot {
    session: SessionId,
    open: Arc<Mutex<HashMap<SessionId, usize>>>,
}

impl Drop for StreamSlot {
    fn drop(&mut self) {
        let mut open = self.open.lock();
        if let Some(n) = open.get_mut(&self.session) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                open.remove(&self.session);
            }
        }
    }
}

async fn limit_streams(State(counter): State<Arc<StreamCounter>>, req: Request, next: Next) -> Response {
    let Some(session) = req.extensions().get::<Authed>().map(|a| a.session) else {
        return next.run(req).await;
    };
    let Some(slot) = counter.acquire(session) else {
        return AppError::RateLimited { retry_after_secs: Some(5) }.into_response();
    };
    let response = next.run(req).await;
    let (parts, body) = response.into_parts();
    let guarded = body.into_data_stream().map(move |chunk| {
        let _held = &slot;
        chunk
    });
    Response::from_parts(parts, Body::from_stream(guarded))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_ip_is_the_rightmost_forwarded_hop() {
        let mut h = HeaderMap::new();
        h.append("x-forwarded-for", HeaderValue::from_static("6.6.6.6, 10.0.0.1"));
        h.append("x-forwarded-for", HeaderValue::from_static("203.0.113.9"));
        let peer: SocketAddr = "127.0.0.1:9".parse().expect("addr");
        assert_eq!(client_ip_of(&h, Some(peer)), Some("203.0.113.9".parse().expect("ip")));
        assert_eq!(client_ip_of(&HeaderMap::new(), Some(peer)), Some(peer.ip()));
        assert_eq!(client_ip_of(&HeaderMap::new(), None), None);
    }

    #[test]
    fn token_buckets_allow_bursts_then_refill() {
        let b = Buckets::new(1.0, 3.0);
        let t0 = Instant::now();
        assert!(b.take("k", t0).is_ok() && b.take("k", t0).is_ok() && b.take("k", t0).is_ok());
        assert_eq!(b.take("k", t0), Err(1));
        assert!(b.take("other", t0).is_ok(), "keys are independent");
        assert!(b.take("k", t0 + Duration::from_millis(1100)).is_ok());
    }

    #[test]
    fn stream_slots_are_released_on_drop() {
        let c = StreamCounter::new(2);
        let s = SessionId([1; 32]);
        let a = c.acquire(s).expect("1");
        let _b = c.acquire(s).expect("2");
        assert!(c.acquire(s).is_none());
        drop(a);
        assert!(c.acquire(s).is_some());
    }
}
