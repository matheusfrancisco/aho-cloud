//! Webhook endpoints `/hooks/{github,github-fix,linear,slack}` (intake.md "Endpoints", "Signature
//! verification", "Handler algorithm"; server.md webhook rows). Unauthenticated by design: the
//! provider HMAC is the credential. Every request is verified on the exact raw bytes before any
//! parsing, then handed to [`AcceptDelivery`], which stores it with its job in one transaction.
//!
//! Status mapping:
//!
//! | Situation | Status |
//! |---|---|
//! | new or duplicate delivery, GitHub `ping`, Slack `url_verification` | 200 |
//! | missing required header, malformed header (not ASCII, bad prefix, invalid hex, bad timestamp) | 400 |
//! | signature mismatch, stale signed timestamp | 401 |
//! | `/hooks/github-fix` without a configured secret (F3 off) | 404 |
//! | body above the route limit | 413 |
//! | more than [`RATE_PER_SEC`] requests per second from one client IP | 429 |
//! | inbox unavailable, accept deadline or route timeout passed, secret missing after refresh | 503 |
//!
//! Bodies are never logged.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use axum::Router;
use axum::body::Bytes;
use axum::error_handling::HandleErrorLayer;
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use parking_lot::Mutex;
use serde::Deserialize;
use time::OffsetDateTime;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tower::{BoxError, ServiceBuilder};

use mage_core::domain::WebhookSource;
use mage_core::ports::Clock;

use crate::adapters::driving::http::auth::ClientIp;
use crate::app::intake::accept::{AcceptDelivery, AcceptOutcome, InboundDelivery};
use crate::domain::intake::secret_names;
use crate::error::AppError;
use crate::ports::vault::{SecretBytes, SecretName, SecretScope, SecretState, SecretVault, VaultError};

use self::verify::VerifyError;

/// GitHub caps payloads at 25 MB; both GitHub routes take that much.
pub const GITHUB_BODY_LIMIT: usize = 25 * 1024 * 1024;
/// Linear and Slack routes.
pub const SMALL_BODY_LIMIT: usize = 1024 * 1024;
/// Whole-route budget, inside Slack's 3 s.
pub const ROUTE_TIMEOUT: Duration = Duration::from_millis(2_500);
/// Budget of the `AcceptDelivery` round trip.
pub const ACCEPT_DEADLINE: Duration = Duration::from_secs(2);
/// Default accepted age of Linear's signed `webhookTimestamp`.
pub const LINEAR_DEFAULT_MAX_SKEW: Duration = Duration::from_secs(60);
/// Accepted age of `X-Slack-Request-Timestamp`.
pub const SLACK_MAX_SKEW: Duration = Duration::from_secs(300);
/// Per-client-IP request rate (and burst) that bounds signature-check CPU under abuse.
pub const RATE_PER_SEC: u32 = 50;
/// How often [`HookSecrets::spawn_refresh`] reloads the secrets.
pub const SECRET_REFRESH_EVERY: Duration = Duration::from_secs(5 * 60);

/// Longest accepted delivery id or event name header (GitHub GUIDs and Linear UUIDs are 36 chars).
const MAX_ID_LEN: usize = 256;
/// Above this many tracked client IPs, idle buckets are dropped before a new one is added.
const MAX_TRACKED_IPS: usize = 10_000;

mod header {
    pub const GITHUB_EVENT: &str = "x-github-event";
    pub const GITHUB_DELIVERY: &str = "x-github-delivery";
    pub const GITHUB_SIGNATURE: &str = "x-hub-signature-256";
    pub const LINEAR_DELIVERY: &str = "linear-delivery";
    pub const LINEAR_EVENT: &str = "linear-event";
    pub const LINEAR_SIGNATURE: &str = "linear-signature";
    pub const SLACK_SIGNATURE: &str = "x-slack-signature";
    pub const SLACK_TIMESTAMP: &str = "x-slack-request-timestamp";
    pub const SLACK_RETRY_NUM: &str = "x-slack-retry-num";
    pub const SLACK_RETRY_REASON: &str = "x-slack-retry-reason";
}

/// Pure signature and freshness checks on the exact raw bytes (HMAC-SHA256, constant-time
/// compare via `Mac::verify_slice`). Each function accepts any of the given secrets
/// (`[active, previous]` during a rotation). Header hex is decoded first.
pub mod verify {
    use std::time::Duration;

    use axum::http::StatusCode;
    use hmac::{Hmac, Mac};
    use serde::Deserialize;
    use sha2::Sha256;
    use time::OffsetDateTime;

    use super::header;

    type HmacSha256 = Hmac<Sha256>;

    #[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
    pub enum VerifyError {
        /// A required header is absent. 400.
        #[error("missing header {0}")]
        MissingHeader(&'static str),
        /// A header (or the signed Linear timestamp) is present but unusable: not ASCII, wrong
        /// prefix, invalid hex, non-numeric timestamp. 400.
        #[error("malformed {0}")]
        Malformed(&'static str),
        /// No secret produces the given signature. 401.
        #[error("signature mismatch")]
        BadSignature,
        /// The signed timestamp is outside the accepted window. 401.
        #[error("stale timestamp")]
        Stale,
    }

    impl VerifyError {
        pub fn status(self) -> StatusCode {
            match self {
                VerifyError::MissingHeader(_) | VerifyError::Malformed(_) => StatusCode::BAD_REQUEST,
                VerifyError::BadSignature | VerifyError::Stale => StatusCode::UNAUTHORIZED,
            }
        }
    }

    /// `X-Hub-Signature-256: sha256=<hex>` over the raw body.
    pub fn github(secrets: &[&[u8]], header: Option<&str>, body: &[u8]) -> Result<(), VerifyError> {
        let value = header.ok_or(VerifyError::MissingHeader(header::GITHUB_SIGNATURE))?;
        let hex = value.strip_prefix("sha256=").ok_or(VerifyError::Malformed(header::GITHUB_SIGNATURE))?;
        let signature = decode_hex(hex, header::GITHUB_SIGNATURE)?;
        check(secrets, &signature, &[body])
    }

    /// `Linear-Signature: <hex>` over the raw body, then the signed body field `webhookTimestamp`
    /// (ms since the epoch) within `max_skew` of `now`. The unsigned `Linear-Timestamp` header
    /// is ignored.
    pub fn linear(
        secrets: &[&[u8]],
        header: Option<&str>,
        body: &[u8],
        now: OffsetDateTime,
        max_skew: Duration,
    ) -> Result<(), VerifyError> {
        let value = header.ok_or(VerifyError::MissingHeader(header::LINEAR_SIGNATURE))?;
        let signature = decode_hex(value, header::LINEAR_SIGNATURE)?;
        check(secrets, &signature, &[body])?;

        #[derive(Deserialize)]
        struct Signed {
            #[serde(rename = "webhookTimestamp")]
            webhook_timestamp: serde_json::Number,
        }
        let signed: Signed = serde_json::from_slice(body).map_err(|_| VerifyError::Malformed("webhookTimestamp"))?;
        let ts_ms = signed
            .webhook_timestamp
            .as_i64()
            .or_else(|| signed.webhook_timestamp.as_f64().filter(|f| f.is_finite()).map(|f| f as i64))
            .ok_or(VerifyError::Malformed("webhookTimestamp"))?;
        let now_ms = now.unix_timestamp_nanos() / 1_000_000;
        fresh(now_ms.abs_diff(i128::from(ts_ms)), max_skew.as_millis())
    }

    /// `X-Slack-Signature: v0=<hex>` over `v0:{X-Slack-Request-Timestamp}:{raw body}`, then the
    /// timestamp (seconds) within `max_skew` (Slack: 300 s) of `now`.
    pub fn slack(
        secrets: &[&[u8]],
        signature_header: Option<&str>,
        timestamp_header: Option<&str>,
        body: &[u8],
        now: OffsetDateTime,
        max_skew: Duration,
    ) -> Result<(), VerifyError> {
        let value = signature_header.ok_or(VerifyError::MissingHeader(header::SLACK_SIGNATURE))?;
        let ts_text = timestamp_header.ok_or(VerifyError::MissingHeader(header::SLACK_TIMESTAMP))?;
        let hex = value.strip_prefix("v0=").ok_or(VerifyError::Malformed(header::SLACK_SIGNATURE))?;
        let signature = decode_hex(hex, header::SLACK_SIGNATURE)?;
        let ts: i64 = ts_text.parse().map_err(|_| VerifyError::Malformed(header::SLACK_TIMESTAMP))?;
        check(secrets, &signature, &[b"v0:", ts_text.as_bytes(), b":", body])?;
        fresh(i128::from(now.unix_timestamp()).abs_diff(i128::from(ts)), u128::from(max_skew.as_secs()))
    }

    fn decode_hex(text: &str, name: &'static str) -> Result<Vec<u8>, VerifyError> {
        hex::decode(text).map_err(|_| VerifyError::Malformed(name))
    }

    fn check(secrets: &[&[u8]], signature: &[u8], parts: &[&[u8]]) -> Result<(), VerifyError> {
        let matched = secrets.iter().any(|secret| {
            // HMAC accepts keys of any length; the error branch cannot happen.
            let Ok(mut mac) = HmacSha256::new_from_slice(secret) else { return false };
            for part in parts {
                mac.update(part);
            }
            mac.verify_slice(signature).is_ok()
        });
        if matched { Ok(()) } else { Err(VerifyError::BadSignature) }
    }

    fn fresh(skew: u128, max: u128) -> Result<(), VerifyError> {
        if skew <= max { Ok(()) } else { Err(VerifyError::Stale) }
    }
}

/// Platform-scope SecretVault name of a route's webhook secret.
pub fn secret_name(source: WebhookSource) -> &'static str {
    match source {
        WebhookSource::GitHub => secret_names::GITHUB_WEBHOOK_SECRET,
        WebhookSource::GitHubFix => secret_names::GITHUB_FIX_WEBHOOK_SECRET,
        WebhookSource::Linear => secret_names::LINEAR_WEBHOOK_SECRET,
        WebhookSource::Slack => secret_names::SLACK_SIGNING_SECRET,
    }
}

const SOURCES: [WebhookSource; 4] = [WebhookSource::GitHub, WebhookSource::GitHubFix, WebhookSource::Linear, WebhookSource::Slack];

fn slot(source: WebhookSource) -> usize {
    match source {
        WebhookSource::GitHub => 0,
        WebhookSource::GitHubFix => 1,
        WebhookSource::Linear => 2,
        WebhookSource::Slack => 3,
    }
}

#[derive(Debug, thiserror::Error)]
pub enum HookSecretsError {
    /// A required webhook secret has no active version: the role refuses to start.
    #[error("webhook secret {0} is not configured (platform scope)")]
    Missing(&'static str),
    /// An empty HMAC key would let anyone sign.
    #[error("webhook secret {0} is empty")]
    Empty(&'static str),
    #[error("loading webhook secret {name}: {source}")]
    Vault { name: &'static str, source: VaultError },
}

/// Active secret plus the newest retired one, accepted during a provider-side rotation.
#[derive(Clone)]
struct SecretPair {
    active: SecretBytes,
    previous: Option<SecretBytes>,
}

impl SecretPair {
    fn with_keys<R>(&self, f: impl FnOnce(&[&[u8]]) -> R) -> R {
        match &self.previous {
            Some(previous) => f(&[self.active.expose(), previous.expose()]),
            None => f(&[self.active.expose()]),
        }
    }
}

/// `[active, previous]` webhook secrets per route, loaded from SecretVault (Platform scope) at
/// start and refreshed by [`HookSecrets::spawn_refresh`].
pub struct HookSecrets {
    vault: Arc<dyn SecretVault>,
    slots: ArcSwap<[Option<SecretPair>; 4]>,
}

impl HookSecrets {
    /// Loads every route's secrets. GitHub, Linear and Slack are required; `github_fix` is
    /// optional (absent when F3 is not configured, and `/hooks/github-fix` then answers 404).
    pub async fn load(vault: Arc<dyn SecretVault>) -> Result<HookSecrets, HookSecretsError> {
        let mut slots: [Option<SecretPair>; 4] = [None, None, None, None];
        for source in SOURCES {
            let name = secret_name(source);
            let pair = load_pair(vault.as_ref(), name).await?;
            if pair.is_none() && source != WebhookSource::GitHubFix {
                return Err(HookSecretsError::Missing(name));
            }
            slots[slot(source)] = pair;
        }
        Ok(HookSecrets { vault, slots: ArcSwap::from_pointee(slots) })
    }

    /// Reloads every route. A source whose active version disappeared (destroyed) is cleared;
    /// a source that failed to load keeps its previous secrets. Returns the first failure.
    pub async fn refresh(&self) -> Result<(), HookSecretsError> {
        let mut slots = (**self.slots.load()).clone();
        let mut first_error = None;
        for source in SOURCES {
            match load_pair(self.vault.as_ref(), secret_name(source)).await {
                Ok(pair) => slots[slot(source)] = pair,
                Err(err) => {
                    first_error.get_or_insert(err);
                }
            }
        }
        self.slots.store(Arc::new(slots));
        first_error.map_or(Ok(()), Err)
    }

    /// Calls [`HookSecrets::refresh`] every `every` until `shutdown` is cancelled.
    pub fn spawn_refresh(self: &Arc<Self>, every: Duration, shutdown: CancellationToken) -> JoinHandle<()> {
        let this = Arc::clone(self);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(every);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            tick.tick().await;
            loop {
                tokio::select! {
                    () = shutdown.cancelled() => break,
                    _ = tick.tick() => {
                        if let Err(err) = this.refresh().await {
                            tracing::warn!(error = %err, "webhook secret refresh failed; keeping the loaded secrets");
                        }
                    }
                }
            }
        })
    }

    fn get(&self, source: WebhookSource) -> Option<SecretPair> {
        self.slots.load()[slot(source)].clone()
    }
}

async fn load_pair(vault: &dyn SecretVault, name: &'static str) -> Result<Option<SecretPair>, HookSecretsError> {
    let vault_err = |source| HookSecretsError::Vault { name, source };
    let secret_name = SecretName::new(name).map_err(vault_err)?;
    let scope = SecretScope::Platform;
    let active = match vault.load(&scope, &secret_name).await {
        Ok((_, active)) => active,
        Err(VaultError::NotFound) => return Ok(None),
        Err(err) => return Err(vault_err(err)),
    };
    if active.is_empty() {
        return Err(HookSecretsError::Empty(name));
    }
    let newest_retired = vault
        .versions(&scope, &secret_name)
        .await
        .map_err(vault_err)?
        .into_iter()
        .filter(|meta| meta.state == SecretState::Retired)
        .max_by_key(|meta| meta.version);
    let previous = match newest_retired {
        Some(meta) => match vault.load_version(&scope, &secret_name, meta.version).await {
            Ok(bytes) if !bytes.is_empty() => Some(bytes),
            Ok(_) | Err(VaultError::NotFound) => None,
            Err(err) => return Err(vault_err(err)),
        },
        None => None,
    };
    Ok(Some(SecretPair { active, previous }))
}

/// Token bucket per client IP (`RATE_PER_SEC` refill, same burst), approximate per instance.
struct RateLimiter {
    per_sec: f64,
    buckets: Mutex<HashMap<IpAddr, Bucket>>,
}

struct Bucket {
    tokens: f64,
    at: OffsetDateTime,
}

impl RateLimiter {
    fn new(per_sec: u32) -> Self {
        Self { per_sec: f64::from(per_sec.max(1)), buckets: Mutex::new(HashMap::new()) }
    }

    fn allow(&self, ip: IpAddr, now: OffsetDateTime) -> bool {
        let burst = self.per_sec;
        let mut buckets = self.buckets.lock();
        if buckets.len() >= MAX_TRACKED_IPS && !buckets.contains_key(&ip) {
            // A bucket idle for a second is full again: forgetting it changes nothing.
            buckets.retain(|_, b| (now - b.at).as_seconds_f64() < 1.0);
        }
        let bucket = buckets.entry(ip).or_insert(Bucket { tokens: burst, at: now });
        let elapsed = (now - bucket.at).as_seconds_f64().max(0.0);
        bucket.tokens = (bucket.tokens + elapsed * self.per_sec).min(burst);
        bucket.at = now;
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// State of the `/hooks` routes.
#[derive(Clone)]
pub struct HooksState {
    accept: Arc<AcceptDelivery>,
    secrets: Arc<HookSecrets>,
    clock: Arc<dyn Clock>,
    linear_max_skew: Duration,
    limiter: Arc<RateLimiter>,
}

impl HooksState {
    /// `linear_max_skew`: `IntakeSection::linear_max_skew()` (default [`LINEAR_DEFAULT_MAX_SKEW`]).
    pub fn new(accept: Arc<AcceptDelivery>, secrets: Arc<HookSecrets>, clock: Arc<dyn Clock>, linear_max_skew: Duration) -> Self {
        Self { accept, secrets, clock, linear_max_skew, limiter: Arc::new(RateLimiter::new(RATE_PER_SEC)) }
    }
}

/// `POST /hooks/{github,github-fix,linear,slack}` with body limits, the route timeout and the
/// per-IP rate limit applied. No session, CSRF or UI auth middleware.
pub fn router(state: HooksState) -> Router {
    let timeout = ServiceBuilder::new().layer(HandleErrorLayer::new(on_route_error)).timeout(ROUTE_TIMEOUT);
    Router::new()
        .route("/hooks/github", post(github).layer(DefaultBodyLimit::max(GITHUB_BODY_LIMIT)))
        .route("/hooks/github-fix", post(github_fix).layer(DefaultBodyLimit::max(GITHUB_BODY_LIMIT)))
        .route("/hooks/linear", post(linear).layer(DefaultBodyLimit::max(SMALL_BODY_LIMIT)))
        .route("/hooks/slack", post(slack).layer(DefaultBodyLimit::max(SMALL_BODY_LIMIT)))
        .route_layer(timeout)
        .route_layer(middleware::from_fn_with_state(state.clone(), rate_limit))
        .with_state(state)
}

async fn on_route_error(err: BoxError) -> StatusCode {
    if err.is::<tower::timeout::error::Elapsed>() {
        tracing::warn!(timeout_ms = ROUTE_TIMEOUT.as_millis() as u64, "webhook request timed out");
    } else {
        tracing::error!(error = %err, "webhook route failed");
    }
    StatusCode::SERVICE_UNAVAILABLE
}

/// Keyed by the platform shell's [`ClientIp`] (rightmost `X-Forwarded-For` hop, else the socket
/// peer); requests without a known IP are not limited.
async fn rate_limit(State(state): State<HooksState>, req: Request, next: Next) -> Response {
    if let Some(ClientIp(Some(ip))) = req.extensions().get::<ClientIp>().copied() {
        if !state.limiter.allow(ip, state.clock.now()) {
            tracing::debug!(%ip, path = req.uri().path(), "webhook rate limit exceeded");
            return StatusCode::TOO_MANY_REQUESTS.into_response();
        }
    }
    next.run(req).await
}

async fn github(State(s): State<HooksState>, ip: ClientIp, headers: HeaderMap, body: Bytes) -> Response {
    handle(&s, WebhookSource::GitHub, ip, &headers, body).await
}

async fn github_fix(State(s): State<HooksState>, ip: ClientIp, headers: HeaderMap, body: Bytes) -> Response {
    handle(&s, WebhookSource::GitHubFix, ip, &headers, body).await
}

async fn linear(State(s): State<HooksState>, ip: ClientIp, headers: HeaderMap, body: Bytes) -> Response {
    handle(&s, WebhookSource::Linear, ip, &headers, body).await
}

async fn slack(State(s): State<HooksState>, ip: ClientIp, headers: HeaderMap, body: Bytes) -> Response {
    handle(&s, WebhookSource::Slack, ip, &headers, body).await
}

/// What a verified request turned out to be.
enum Verified {
    /// Answered inline, not recorded (GitHub `ping`, Slack `url_verification`, Slack envelopes
    /// without an event id).
    Inline { event: String, response: Response },
    Delivery { delivery_id: String, event: String },
}

/// Slack Events API envelope: only the fields the handler reads.
#[derive(Deserialize)]
struct SlackEnvelope {
    #[serde(rename = "type")]
    kind: String,
    challenge: Option<String>,
    event_id: Option<String>,
    event: Option<SlackEvent>,
}

#[derive(Deserialize)]
struct SlackEvent {
    #[serde(rename = "type")]
    kind: String,
}

async fn handle(state: &HooksState, source: WebhookSource, client: ClientIp, headers: &HeaderMap, body: Bytes) -> Response {
    let started = tokio::time::Instant::now();
    let latency_ms = || started.elapsed().as_millis() as u64;
    let src = source.as_str();

    let Some(secrets) = state.secrets.get(source) else {
        if source == WebhookSource::GitHubFix {
            return StatusCode::NOT_FOUND.into_response();
        }
        tracing::error!(source = src, "webhook secret not configured");
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };

    if source == WebhookSource::Slack {
        if let Some(retry) = text_header(headers, header::SLACK_RETRY_NUM).ok().flatten() {
            let reason = text_header(headers, header::SLACK_RETRY_REASON).ok().flatten().unwrap_or("");
            tracing::info!(source = src, retry_num = retry, retry_reason = reason, "slack retry");
        }
    }

    let verified = secrets.with_keys(|keys| verify_request(state, source, keys, headers, &body));
    let (delivery_id, event) = match verified {
        Err(err) => {
            let status = err.status();
            if status == StatusCode::UNAUTHORIZED {
                let ip = client.0.map(|ip| ip.to_string());
                tracing::warn!(source = src, ip = ip.as_deref().unwrap_or("unknown"), error = %err, latency_ms = latency_ms(), "webhook rejected");
            } else {
                tracing::info!(source = src, error = %err, latency_ms = latency_ms(), "webhook rejected");
            }
            return status.into_response();
        }
        Ok(Verified::Inline { event, response }) => {
            tracing::info!(source = src, event = %event, outcome = "inline", latency_ms = latency_ms(), "webhook answered");
            return response;
        }
        Ok(Verified::Delivery { delivery_id, event }) => (delivery_id, event),
    };

    let inbound = InboundDelivery { source, delivery_id: delivery_id.clone(), event: event.clone(), body: body.to_vec() };
    let (status, outcome) = match tokio::time::timeout(ACCEPT_DEADLINE, state.accept.accept(inbound)).await {
        Ok(Ok(AcceptOutcome::Accepted)) => (StatusCode::OK, "accepted"),
        Ok(Ok(AcceptOutcome::Duplicate)) => (StatusCode::OK, "duplicate"),
        Ok(Err(AppError::Unavailable(msg))) => {
            tracing::warn!(source = src, delivery_id = %delivery_id, error = %msg, "webhook inbox unavailable");
            (StatusCode::SERVICE_UNAVAILABLE, "unavailable")
        }
        Ok(Err(AppError::Invalid(msg))) => {
            tracing::warn!(source = src, delivery_id = %delivery_id, error = %msg, "webhook delivery invalid");
            (StatusCode::BAD_REQUEST, "invalid")
        }
        Ok(Err(err)) => {
            tracing::error!(source = src, delivery_id = %delivery_id, error = %err, "webhook accept failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "error")
        }
        Err(_) => (StatusCode::SERVICE_UNAVAILABLE, "deadline"),
    };
    tracing::info!(source = src, delivery_id = %delivery_id, event = %event, outcome, latency_ms = latency_ms(), "webhook delivery");
    status.into_response()
}

/// Steps 2–6 of the handler algorithm; no parsing before the signature passes.
fn verify_request(
    state: &HooksState,
    source: WebhookSource,
    keys: &[&[u8]],
    headers: &HeaderMap,
    body: &[u8],
) -> Result<Verified, VerifyError> {
    match source {
        WebhookSource::GitHub | WebhookSource::GitHubFix => {
            let event = required_id(headers, header::GITHUB_EVENT)?;
            let delivery_id = required_id(headers, header::GITHUB_DELIVERY)?;
            verify::github(keys, text_header(headers, header::GITHUB_SIGNATURE)?, body)?;
            if event == "ping" {
                return Ok(Verified::Inline { event: event.to_owned(), response: StatusCode::OK.into_response() });
            }
            Ok(Verified::Delivery { delivery_id: delivery_id.to_owned(), event: event.to_owned() })
        }
        WebhookSource::Linear => {
            let delivery_id = required_id(headers, header::LINEAR_DELIVERY)?;
            let event = required_id(headers, header::LINEAR_EVENT)?;
            verify::linear(keys, text_header(headers, header::LINEAR_SIGNATURE)?, body, state.clock.now(), state.linear_max_skew)?;
            Ok(Verified::Delivery { delivery_id: delivery_id.to_owned(), event: event.to_owned() })
        }
        WebhookSource::Slack => {
            verify::slack(
                keys,
                text_header(headers, header::SLACK_SIGNATURE)?,
                text_header(headers, header::SLACK_TIMESTAMP)?,
                body,
                state.clock.now(),
                SLACK_MAX_SKEW,
            )?;
            let envelope: SlackEnvelope = serde_json::from_slice(body).map_err(|_| VerifyError::Malformed("slack body"))?;
            if envelope.kind == "url_verification" {
                let challenge = envelope.challenge.ok_or(VerifyError::Malformed("slack challenge"))?;
                let response = axum::Json(serde_json::json!({ "challenge": challenge })).into_response();
                return Ok(Verified::Inline { event: envelope.kind, response });
            }
            match (envelope.event_id, envelope.event) {
                (Some(delivery_id), Some(event)) if valid_id(&delivery_id) && valid_id(&event.kind) => {
                    Ok(Verified::Delivery { delivery_id, event: event.kind })
                }
                (Some(_), Some(_)) => Err(VerifyError::Malformed("slack event_id")),
                // e.g. `app_rate_limited`: nothing to record without an event id.
                _ => Ok(Verified::Inline { event: envelope.kind, response: StatusCode::OK.into_response() }),
            }
        }
    }
}

/// Header as text; present but not visible ASCII → `Malformed`.
fn text_header<'h>(headers: &'h HeaderMap, name: &'static str) -> Result<Option<&'h str>, VerifyError> {
    headers.get(name).map(|v| v.to_str().map_err(|_| VerifyError::Malformed(name))).transpose()
}

fn required_id<'h>(headers: &'h HeaderMap, name: &'static str) -> Result<&'h str, VerifyError> {
    let value = text_header(headers, name)?.ok_or(VerifyError::MissingHeader(name))?;
    if valid_id(value) { Ok(value) } else { Err(VerifyError::Malformed(name)) }
}

fn valid_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_ID_LEN
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use axum::body::Body;
    use axum::http::Request as HttpRequest;
    use hmac::{Hmac, Mac};
    use mage_core::testing::{FakeClock, MemJobQueue, SeqIdGen};
    use sha2::Sha256;
    use tower::ServiceExt;

    use super::*;
    use crate::ports::audit::Actor;
    use crate::ports::vault::Activation;
    use crate::testing::{MemSecretVault, MemWebhookInbox};

    const GITHUB_DOC_SECRET: &[u8] = b"It's a Secret to Everybody";
    const GITHUB_DOC_SIGNATURE: &str = "sha256=757107ea0eb2509fc211221cce984b8a37570b6d7586c22c46f4379c8b043e17";
    const SLACK_DOC_SECRET: &[u8] = b"8f742231b10e8888abcd99yyyzzz85a5";
    const SLACK_DOC_TS: i64 = 1_531_420_618;
    const SLACK_DOC_BODY: &str = "token=xyzz0WbapA4vBCDEFasx0q6G&team_id=T1DC2JH3J&team_domain=testteamnow&channel_id=G8PSS9T3V&channel_name=foobar&user_id=U2CERLKJA&user_name=roadrunner&command=%2Fwebhook-collect&text=&response_url=https%3A%2F%2Fhooks.slack.com%2Fcommands%2FT1DC2JH3J%2F397700885554%2F96rGlfmibIGlgcZRskXaIFfN&trigger_id=398738663015.47445629121.803a0bc887a14d10d2c447fce8b6703c";
    const SLACK_DOC_SIGNATURE: &str = "v0=a2114d57b48eac39b9ad189dd8316235a7b4a8d21a10bd27519666489c69b503";

    fn sign(secret: &[u8], parts: &[&[u8]]) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(secret).unwrap();
        for part in parts {
            mac.update(part);
        }
        hex::encode(mac.finalize().into_bytes())
    }

    fn at_secs(secs: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(secs).unwrap()
    }

    // ---- verify ------------------------------------------------------------------------------

    #[test]
    fn github_doc_vector_verifies_and_any_tampering_fails() {
        let body = b"Hello, World!";
        assert_eq!(verify::github(&[GITHUB_DOC_SECRET], Some(GITHUB_DOC_SIGNATURE), body), Ok(()));
        assert_eq!(verify::github(&[GITHUB_DOC_SECRET], Some(GITHUB_DOC_SIGNATURE), b"Hello, World?"), Err(VerifyError::BadSignature));
        assert_eq!(verify::github(&[b"other"], Some(GITHUB_DOC_SIGNATURE), body), Err(VerifyError::BadSignature));
        assert_eq!(verify::github(&[GITHUB_DOC_SECRET], None, body), Err(VerifyError::MissingHeader("x-hub-signature-256")));
        let no_prefix = GITHUB_DOC_SIGNATURE.trim_start_matches("sha256=");
        assert_eq!(verify::github(&[GITHUB_DOC_SECRET], Some(no_prefix), body), Err(VerifyError::Malformed("x-hub-signature-256")));
        let bad_hex = GITHUB_DOC_SIGNATURE.replace('7', "z");
        assert_eq!(verify::github(&[GITHUB_DOC_SECRET], Some(&bad_hex), body), Err(VerifyError::Malformed("x-hub-signature-256")));
        // Valid hex of the wrong length is a mismatch, not a malformed header.
        assert_eq!(verify::github(&[GITHUB_DOC_SECRET], Some("sha256=abcd"), body), Err(VerifyError::BadSignature));
        assert_eq!(verify::github(&[], Some(GITHUB_DOC_SIGNATURE), body), Err(VerifyError::BadSignature));
    }

    #[test]
    fn previous_secret_is_accepted_next_to_the_active_one() {
        let body = b"Hello, World!";
        assert_eq!(verify::github(&[b"rotated-in", GITHUB_DOC_SECRET], Some(GITHUB_DOC_SIGNATURE), body), Ok(()));
        assert_eq!(verify::github(&[GITHUB_DOC_SECRET, b"rotated-out"], Some(GITHUB_DOC_SIGNATURE), body), Ok(()));
    }

    #[test]
    fn slack_doc_vector_verifies_within_the_window_only() {
        let ts = SLACK_DOC_TS.to_string();
        let body = SLACK_DOC_BODY.as_bytes();
        let ok = |now: i64| verify::slack(&[SLACK_DOC_SECRET], Some(SLACK_DOC_SIGNATURE), Some(&ts), body, at_secs(now), SLACK_MAX_SKEW);
        assert_eq!(ok(SLACK_DOC_TS + 10), Ok(()));
        assert_eq!(ok(SLACK_DOC_TS + 300), Ok(()));
        assert_eq!(ok(SLACK_DOC_TS - 300), Ok(()));
        assert_eq!(ok(SLACK_DOC_TS + 301), Err(VerifyError::Stale));
        assert_eq!(ok(SLACK_DOC_TS - 301), Err(VerifyError::Stale));

        let now = at_secs(SLACK_DOC_TS);
        let mut tampered = body.to_vec();
        tampered[0] ^= 1;
        assert_eq!(
            verify::slack(&[SLACK_DOC_SECRET], Some(SLACK_DOC_SIGNATURE), Some(&ts), &tampered, now, SLACK_MAX_SKEW),
            Err(VerifyError::BadSignature)
        );
        // The timestamp is signed: moving it breaks the signature.
        let moved = (SLACK_DOC_TS + 1).to_string();
        assert_eq!(
            verify::slack(&[SLACK_DOC_SECRET], Some(SLACK_DOC_SIGNATURE), Some(&moved), body, now, SLACK_MAX_SKEW),
            Err(VerifyError::BadSignature)
        );
        assert_eq!(
            verify::slack(&[SLACK_DOC_SECRET], Some(SLACK_DOC_SIGNATURE), None, body, now, SLACK_MAX_SKEW),
            Err(VerifyError::MissingHeader("x-slack-request-timestamp"))
        );
        assert_eq!(
            verify::slack(&[SLACK_DOC_SECRET], Some(SLACK_DOC_SIGNATURE), Some("soon"), body, now, SLACK_MAX_SKEW),
            Err(VerifyError::Malformed("x-slack-request-timestamp"))
        );
        assert_eq!(
            verify::slack(&[SLACK_DOC_SECRET], Some("v1=00"), Some(&ts), body, now, SLACK_MAX_SKEW),
            Err(VerifyError::Malformed("x-slack-signature"))
        );
    }

    #[test]
    fn linear_checks_the_signed_body_timestamp_not_a_header() {
        let secret = b"linear-secret";
        let ts_ms: i64 = 1_767_225_600_000; // 2026-01-01T00:00:00Z
        let body = format!(r#"{{"action":"create","type":"Issue","webhookTimestamp":{ts_ms}}}"#);
        let sig = sign(secret, &[body.as_bytes()]);
        let check = |now_ms: i64| {
            let now = OffsetDateTime::from_unix_timestamp_nanos(i128::from(now_ms) * 1_000_000).unwrap();
            verify::linear(&[secret], Some(&sig), body.as_bytes(), now, LINEAR_DEFAULT_MAX_SKEW)
        };
        assert_eq!(check(ts_ms + 60_000), Ok(()));
        assert_eq!(check(ts_ms - 60_000), Ok(()));
        assert_eq!(check(ts_ms + 60_001), Err(VerifyError::Stale));
        assert_eq!(check(ts_ms - 60_001), Err(VerifyError::Stale));

        let now = OffsetDateTime::from_unix_timestamp_nanos(i128::from(ts_ms) * 1_000_000).unwrap();
        let forged = body.replace(&ts_ms.to_string(), &(ts_ms + 1).to_string());
        assert_eq!(verify::linear(&[secret], Some(&sig), forged.as_bytes(), now, LINEAR_DEFAULT_MAX_SKEW), Err(VerifyError::BadSignature));
        assert_eq!(
            verify::linear(&[secret], Some(&format!("sha256={sig}")), body.as_bytes(), now, LINEAR_DEFAULT_MAX_SKEW),
            Err(VerifyError::Malformed("linear-signature"))
        );
        let unstamped = r#"{"action":"create"}"#;
        let unstamped_sig = sign(secret, &[unstamped.as_bytes()]);
        assert_eq!(
            verify::linear(&[secret], Some(&unstamped_sig), unstamped.as_bytes(), now, LINEAR_DEFAULT_MAX_SKEW),
            Err(VerifyError::Malformed("webhookTimestamp"))
        );
    }

    #[test]
    fn rate_limiter_allows_a_burst_then_refills_over_time() {
        let limiter = RateLimiter::new(50);
        let ip: IpAddr = "203.0.113.7".parse().unwrap();
        let other: IpAddr = "203.0.113.8".parse().unwrap();
        let t0 = FakeClock::DEFAULT_START;
        assert!((0..50).all(|_| limiter.allow(ip, t0)));
        assert!(!limiter.allow(ip, t0));
        assert!(limiter.allow(other, t0), "buckets are per IP");
        let later = t0 + time::Duration::milliseconds(100);
        assert_eq!((0..10).filter(|_| limiter.allow(ip, later)).count(), 5);
    }

    // ---- router ------------------------------------------------------------------------------

    const GITHUB_SECRET: &str = "gh-secret";
    const LINEAR_SECRET: &str = "linear-secret";
    const SLACK_SECRET: &str = "slack-secret";

    struct World {
        clock: Arc<FakeClock>,
        vault: Arc<MemSecretVault>,
        jobs: Arc<MemJobQueue>,
        inbox: Arc<MemWebhookInbox>,
        secrets: Arc<HookSecrets>,
        app: Router,
    }

    async fn put_secret(vault: &MemSecretVault, name: &str, value: &str) {
        vault
            .put(&SecretScope::Platform, &SecretName::new(name).unwrap(), SecretBytes::from(value.to_owned()), Activation::Now, &Actor::System)
            .await
            .unwrap();
    }

    fn new_vault(clock: &Arc<FakeClock>) -> Arc<MemSecretVault> {
        Arc::new(MemSecretVault::new(clock.clone()))
    }

    async fn seed_required(vault: &MemSecretVault) {
        put_secret(vault, secret_names::GITHUB_WEBHOOK_SECRET, GITHUB_SECRET).await;
        put_secret(vault, secret_names::LINEAR_WEBHOOK_SECRET, LINEAR_SECRET).await;
        put_secret(vault, secret_names::SLACK_SIGNING_SECRET, SLACK_SECRET).await;
    }

    async fn world() -> World {
        let clock = Arc::new(FakeClock::new());
        let vault = new_vault(&clock);
        seed_required(&vault).await;
        world_with(clock, vault).await
    }

    async fn world_with(clock: Arc<FakeClock>, vault: Arc<MemSecretVault>) -> World {
        let jobs = Arc::new(MemJobQueue::new(clock.clone(), Arc::new(SeqIdGen::new())));
        let inbox = Arc::new(MemWebhookInbox::new(clock.clone(), jobs.clone()));
        let secrets = Arc::new(HookSecrets::load(vault.clone()).await.unwrap());
        let accept = Arc::new(AcceptDelivery::new(inbox.clone()));
        let state = HooksState::new(accept, secrets.clone(), clock.clone(), LINEAR_DEFAULT_MAX_SKEW);
        World { clock, vault, jobs, inbox, secrets, app: router(state) }
    }

    impl World {
        async fn send(&self, path: &str, headers: &[(&str, String)], body: impl Into<Vec<u8>>) -> (StatusCode, Vec<u8>) {
            let mut req = HttpRequest::post(path);
            for (name, value) in headers {
                req = req.header(*name, value);
            }
            let resp = self.app.clone().oneshot(req.body(Body::from(body.into())).unwrap()).await.unwrap();
            let status = resp.status();
            let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
            (status, bytes.to_vec())
        }

        async fn github(&self, secret: &str, event: &str, delivery: &str, body: &str) -> StatusCode {
            let sig = format!("sha256={}", sign(secret.as_bytes(), &[body.as_bytes()]));
            let headers = [
                ("X-GitHub-Event", event.to_owned()),
                ("X-GitHub-Delivery", delivery.to_owned()),
                ("X-Hub-Signature-256", sig),
            ];
            self.send("/hooks/github", &headers, body).await.0
        }

        fn now_secs(&self) -> i64 {
            self.clock.now().unix_timestamp()
        }

        fn slack_headers(&self, secret: &str, ts: i64, body: &str) -> Vec<(&'static str, String)> {
            let ts = ts.to_string();
            let sig = format!("v0={}", sign(secret.as_bytes(), &[b"v0:", ts.as_bytes(), b":", body.as_bytes()]));
            vec![("X-Slack-Signature", sig), ("X-Slack-Request-Timestamp", ts)]
        }

        fn linear_body(&self, at: OffsetDateTime) -> String {
            let ms = at.unix_timestamp_nanos() / 1_000_000;
            format!(r#"{{"action":"create","type":"Issue","data":{{"id":"i1"}},"webhookTimestamp":{ms}}}"#)
        }

        fn linear_headers(&self, secret: &str, delivery: &str, body: &str) -> Vec<(&'static str, String)> {
            vec![
                ("Linear-Delivery", delivery.to_owned()),
                ("Linear-Event", "Issue".to_owned()),
                ("Linear-Signature", sign(secret.as_bytes(), &[body.as_bytes()])),
            ]
        }
    }

    const PUSH: &str = r#"{"ref":"refs/heads/main","installation":{"id":1}}"#;

    #[tokio::test]
    async fn verified_github_delivery_is_recorded_once_per_delivery_and_per_body() {
        let w = world().await;
        assert_eq!(w.github(GITHUB_SECRET, "push", "d-1", PUSH).await, StatusCode::OK);
        assert_eq!(w.github(GITHUB_SECRET, "push", "d-1", PUSH).await, StatusCode::OK, "redelivery is a 200 duplicate");
        assert_eq!(w.github(GITHUB_SECRET, "push", "d-forged", PUSH).await, StatusCode::OK, "replayed body is a 200 duplicate");
        assert_eq!(w.jobs.jobs().len(), 1);
        let deliveries = w.inbox.deliveries();
        assert_eq!(deliveries.len(), 1);
        assert_eq!((deliveries[0].source, deliveries[0].delivery_id.as_str(), deliveries[0].event.as_str()), (WebhookSource::GitHub, "d-1", "push"));
        assert_eq!(deliveries[0].body.as_deref(), Some(PUSH.as_bytes()));
    }

    #[tokio::test]
    async fn github_rejections_record_nothing() {
        let w = world().await;
        assert_eq!(w.github("wrong-secret", "push", "d-1", PUSH).await, StatusCode::UNAUTHORIZED);

        let sig = format!("sha256={}", sign(GITHUB_SECRET.as_bytes(), &[PUSH.as_bytes()]));
        let mut tampered = PUSH.as_bytes().to_vec();
        tampered[3] ^= 0x20;
        let full = [("X-GitHub-Event", "push".to_owned()), ("X-GitHub-Delivery", "d-2".to_owned()), ("X-Hub-Signature-256", sig.clone())];
        assert_eq!(w.send("/hooks/github", &full, tampered).await.0, StatusCode::UNAUTHORIZED);
        assert_eq!(w.send("/hooks/github", &full[..2], PUSH).await.0, StatusCode::BAD_REQUEST, "missing signature");
        assert_eq!(w.send("/hooks/github", &[full[0].clone(), full[2].clone()], PUSH).await.0, StatusCode::BAD_REQUEST, "missing delivery id");
        let bad_hex = [full[0].clone(), full[1].clone(), ("X-Hub-Signature-256", format!("sha256={}", "zz".repeat(32)))];
        assert_eq!(w.send("/hooks/github", &bad_hex, PUSH).await.0, StatusCode::BAD_REQUEST, "invalid hex");
        assert!(w.jobs.jobs().is_empty());
        assert!(w.inbox.deliveries().is_empty());
    }

    #[tokio::test]
    async fn github_ping_is_answered_without_recording() {
        let w = world().await;
        assert_eq!(w.github(GITHUB_SECRET, "ping", "d-ping", r#"{"zen":"Keep it logically awesome."}"#).await, StatusCode::OK);
        assert!(w.inbox.deliveries().is_empty());
        assert_eq!(w.github("wrong-secret", "ping", "d-ping", "{}").await, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn github_fix_route_is_404_without_its_secret_and_live_with_it() {
        let w = world().await;
        let sig = format!("sha256={}", sign(b"fix-secret", &[PUSH.as_bytes()]));
        let headers = [("X-GitHub-Event", "pull_request".to_owned()), ("X-GitHub-Delivery", "f-1".to_owned()), ("X-Hub-Signature-256", sig)];
        assert_eq!(w.send("/hooks/github-fix", &headers, PUSH).await.0, StatusCode::NOT_FOUND);

        put_secret(&w.vault, secret_names::GITHUB_FIX_WEBHOOK_SECRET, "fix-secret").await;
        w.secrets.refresh().await.unwrap();
        assert_eq!(w.send("/hooks/github-fix", &headers, PUSH).await.0, StatusCode::OK);
        let deliveries = w.inbox.deliveries();
        assert_eq!(deliveries.len(), 1);
        assert_eq!(deliveries[0].source, WebhookSource::GitHubFix);
    }

    #[tokio::test]
    async fn rotation_accepts_active_and_newest_retired_secret_only() {
        let w = world().await;
        put_secret(&w.vault, secret_names::GITHUB_WEBHOOK_SECRET, "gh-secret-2").await;
        w.secrets.refresh().await.unwrap();
        assert_eq!(w.github(GITHUB_SECRET, "push", "r-1", r#"{"n":1}"#).await, StatusCode::OK, "previous accepted during rotation");
        assert_eq!(w.github("gh-secret-2", "push", "r-2", r#"{"n":2}"#).await, StatusCode::OK);

        put_secret(&w.vault, secret_names::GITHUB_WEBHOOK_SECRET, "gh-secret-3").await;
        w.secrets.refresh().await.unwrap();
        assert_eq!(w.github(GITHUB_SECRET, "push", "r-3", r#"{"n":3}"#).await, StatusCode::UNAUTHORIZED, "two rotations back is gone");
        assert_eq!(w.github("gh-secret-2", "push", "r-4", r#"{"n":4}"#).await, StatusCode::OK);
        assert_eq!(w.github("gh-secret-3", "push", "r-5", r#"{"n":5}"#).await, StatusCode::OK);
        assert_eq!(w.jobs.jobs().len(), 4);
    }

    #[tokio::test]
    async fn failed_refresh_keeps_the_loaded_secrets() {
        let w = world().await;
        w.vault.set_failure(Some(VaultError::Unavailable("kms down".into())));
        assert!(matches!(w.secrets.refresh().await, Err(HookSecretsError::Vault { source: VaultError::Unavailable(_), .. })));
        w.vault.set_failure(None);
        assert_eq!(w.github(GITHUB_SECRET, "push", "k-1", PUSH).await, StatusCode::OK);
    }

    #[tokio::test]
    async fn startup_refuses_without_a_required_secret() {
        let clock = Arc::new(FakeClock::new());
        let vault = new_vault(&clock);
        put_secret(&vault, secret_names::GITHUB_WEBHOOK_SECRET, GITHUB_SECRET).await;
        put_secret(&vault, secret_names::SLACK_SIGNING_SECRET, SLACK_SECRET).await;
        let err = HookSecrets::load(vault).await.err().expect("linear secret missing");
        assert!(matches!(err, HookSecretsError::Missing(name) if name == secret_names::LINEAR_WEBHOOK_SECRET), "{err}");
    }

    #[tokio::test]
    async fn linear_delivery_needs_a_fresh_signed_timestamp() {
        let w = world().await;
        let body = w.linear_body(w.clock.now());
        assert_eq!(w.send("/hooks/linear", &w.linear_headers(LINEAR_SECRET, "l-1", &body), body.clone()).await.0, StatusCode::OK);
        assert_eq!(w.inbox.deliveries()[0].event, "Issue");

        let stale = w.linear_body(w.clock.now() - time::Duration::seconds(61));
        assert_eq!(w.send("/hooks/linear", &w.linear_headers(LINEAR_SECRET, "l-2", &stale), stale).await.0, StatusCode::UNAUTHORIZED);
        let mut headers = w.linear_headers(LINEAR_SECRET, "l-3", &body);
        headers.retain(|(name, _)| *name != "Linear-Delivery");
        assert_eq!(w.send("/hooks/linear", &headers, body).await.0, StatusCode::BAD_REQUEST);
        assert_eq!(w.jobs.jobs().len(), 1);
    }

    #[tokio::test]
    async fn oversized_body_is_413() {
        let w = world().await;
        let body = "x".repeat(SMALL_BODY_LIMIT + 1);
        let (status, _) = w.send("/hooks/linear", &w.linear_headers(LINEAR_SECRET, "big", &body), body).await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
        assert!(w.inbox.deliveries().is_empty());
    }

    #[tokio::test]
    async fn slack_url_verification_is_answered_only_with_a_valid_signature() {
        let w = world().await;
        let body = r#"{"token":"t","challenge":"3eZbrw1aBm2rZgRNFdxV2595E9CY3gmdALWMmHkvFXO7tYXAYM8P","type":"url_verification"}"#;
        let (status, resp) = w.send("/hooks/slack", &w.slack_headers(SLACK_SECRET, w.now_secs(), body), body).await;
        assert_eq!(status, StatusCode::OK);
        let json: serde_json::Value = serde_json::from_slice(&resp).unwrap();
        assert_eq!(json, serde_json::json!({ "challenge": "3eZbrw1aBm2rZgRNFdxV2595E9CY3gmdALWMmHkvFXO7tYXAYM8P" }));

        let (status, resp) = w.send("/hooks/slack", &w.slack_headers("wrong", w.now_secs(), body), body).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert!(resp.is_empty(), "no challenge leaks without a valid signature");
        assert!(w.inbox.deliveries().is_empty(), "url_verification is not recorded");
    }

    #[tokio::test]
    async fn slack_event_is_keyed_by_body_event_id_and_needs_a_fresh_timestamp() {
        let w = world().await;
        let body = r#"{"type":"event_callback","team_id":"T1","event_id":"Ev123","event":{"type":"app_uninstalled"}}"#;
        let mut headers = w.slack_headers(SLACK_SECRET, w.now_secs(), body);
        headers.push(("X-Slack-Retry-Num", "1".to_owned()));
        headers.push(("X-Slack-Retry-Reason", "http_timeout".to_owned()));
        assert_eq!(w.send("/hooks/slack", &headers, body).await.0, StatusCode::OK);
        let deliveries = w.inbox.deliveries();
        assert_eq!((deliveries[0].delivery_id.as_str(), deliveries[0].event.as_str()), ("Ev123", "app_uninstalled"));

        let other = r#"{"type":"event_callback","team_id":"T1","event_id":"Ev456","event":{"type":"tokens_revoked"}}"#;
        let stale = w.slack_headers(SLACK_SECRET, w.now_secs() - 301, other);
        assert_eq!(w.send("/hooks/slack", &stale, other).await.0, StatusCode::UNAUTHORIZED);
        assert_eq!(w.jobs.jobs().len(), 1);
    }

    #[tokio::test]
    async fn client_ip_over_the_rate_is_429_before_verification() {
        let w = world().await;
        let send = |ip: &'static str| {
            let req = HttpRequest::post("/hooks/github")
                .extension(ClientIp(Some(ip.parse().unwrap())))
                .body(Body::from(PUSH))
                .unwrap();
            let app = w.app.clone();
            async move { app.oneshot(req).await.unwrap().status() }
        };
        for _ in 0..RATE_PER_SEC {
            assert_eq!(send("198.51.100.1").await, StatusCode::BAD_REQUEST);
        }
        assert_eq!(send("198.51.100.1").await, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(send("198.51.100.2").await, StatusCode::BAD_REQUEST, "other clients unaffected");
        w.clock.advance(Duration::from_secs(1));
        assert_eq!(send("198.51.100.1").await, StatusCode::BAD_REQUEST, "bucket refilled");
    }

    #[tokio::test]
    async fn inbox_unavailable_is_503() {
        let w = world().await;
        w.inbox.set_unavailable(true);
        assert_eq!(w.github(GITHUB_SECRET, "push", "u-1", PUSH).await, StatusCode::SERVICE_UNAVAILABLE);
        w.inbox.set_unavailable(false);
        assert_eq!(w.github(GITHUB_SECRET, "push", "u-1", PUSH).await, StatusCode::OK, "provider retry succeeds");
        assert_eq!(w.jobs.jobs().len(), 1);
    }
}
