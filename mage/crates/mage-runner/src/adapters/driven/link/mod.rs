//! `ServerLink` over `reqwest` (runs.md "`mage_runner::adapters::driven::link`").
//!
//! - Every `/runner/v1` call sends `Mage-Protocol: 1`; all but `register` carry the runner token.
//! - Token lifecycle: the token lives in memory; the adapter re-registers with an Assertion (same
//!   `instance_id`) at 2/3 of its TTL or on a 401 (once per call), single-flight.
//! - Retries (runs.md "Retry rules"): network errors, 5xx and 429 (`retry_after_ms`) with
//!   full-jitter exponential backoff (250 ms base, ×2, 10 s cap) until the call's deadline.
//! - Uploads and downloads go straight to the signed URL without the runner token.

mod credentials;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures::StreamExt;
use mage_core::domain::{ArtifactId, RunId, RunStatus};
use mage_core::ports::blob::HttpMethod;
use mage_core::ports::{Clock, IdGen};
use mage_core::wire::runner::{
    ArtifactSpec, Capabilities, CommandBatch, CompleteUploadAck, CompleteUploadRequest, Download, DownloadRef, EventsAck, FinishAck,
    FinishReport, HeartbeatAck, HeartbeatRequest, HeldRun, LeaseRequest, LeaseResponse, OutboundEvent, PROTOCOL_HEADER, PROTOCOL_VERSION,
    RefreshDownloadsResponse, RegisterGrant,
    RegistrationAck, Rejection, RunAssignment, RunnerPhase, SubmitAck, UploadGrant, WireError, WireErrorCode, WireRunResult,
    routes,
};
use mage_core::wire::token::{RunnerAssertion, TokenCodec};
use parking_lot::Mutex;
use rand::Rng;
use reqwest::{Method, StatusCode};
use serde::Serialize;
use serde::de::DeserializeOwned;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tokio::time::Instant;
use uuid::Uuid;

pub use credentials::Credentials;

use crate::ports::link::{LinkConflict, LinkError, ServerLink};

/// `[runner.link]`.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinkConfig {
    /// The server's public URL (`public_url`): token issuer and base of `/runner/v1`.
    pub server_url: String,
    /// Runner name shown in the UI (bootstrap grant).
    pub name: String,
    /// One-time `mgb_` token; used only while the credential file holds no runner id.
    #[serde(default)]
    pub bootstrap_token: Option<String>,
    /// Ed25519 key + runner id, mode 0600, in the runner state dir.
    pub credential_file: PathBuf,
}

/// Backoff of transient failures: full jitter over `base × 2^n`, capped.
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    pub base: Duration,
    pub cap: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        RetryPolicy { base: Duration::from_millis(250), cap: Duration::from_secs(10) }
    }
}

impl RetryPolicy {
    fn pause(&self, failures: u32) -> Duration {
        let ceiling = self.base.saturating_mul(1u32 << failures.min(16)).min(self.cap);
        let ms = u64::try_from(ceiling.as_millis()).unwrap_or(u64::MAX);
        Duration::from_millis(rand::thread_rng().gen_range(0..=ms))
    }
}

/// Per-call deadlines (runs.md "Retry rules"); `None` = retry forever.
mod deadline {
    use std::time::Duration;

    pub const HEARTBEAT: Duration = Duration::from_secs(60);
    pub const ARTIFACT: Duration = Duration::from_secs(10 * 60);
    pub const SUBMIT: Duration = Duration::from_secs(5 * 60);
    pub const FINISH: Duration = Duration::from_secs(10 * 60);
    pub const DOWNLOADS: Duration = Duration::from_secs(5 * 60);
    /// A token refresh while the current token is still valid.
    pub const REFRESH: Duration = Duration::from_secs(30);
}

/// Per-request HTTP timeout outside long-polls.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Long-poll HTTP timeout is the wait plus this.
const LONG_POLL_SLACK: Duration = Duration::from_secs(10);
/// Assertion lifetime (`exp - iat`, at most 60 s).
const ASSERTION_LIFETIME: time::Duration = time::Duration::seconds(60);
/// Integrity failures (size or sha256) tolerated per download.
const DOWNLOAD_INTEGRITY_TRIES: u32 = 3;

struct Token {
    value: String,
    refresh_at: Instant,
    expires_at: Instant,
}

/// The HTTP `ServerLink`.
pub struct HttpServerLink {
    http: reqwest::Client,
    base: String,
    codec: TokenCodec,
    name: String,
    bootstrap_token: Option<String>,
    version: String,
    instance_id: Uuid,
    credentials: Mutex<Credentials>,
    capabilities: Mutex<Option<Capabilities>>,
    token: Mutex<Option<Token>>,
    refresh: tokio::sync::Mutex<()>,
    retry: RetryPolicy,
    clock: Arc<dyn Clock>,
    ids: Arc<dyn IdGen>,
}

/// What one request attempt produced.
enum Attempt {
    Ok(Bytes),
    /// Retry after a pause (`Some` = server-provided `retry_after_ms`).
    Retry { reason: String, after: Option<Duration> },
    /// 401: re-register once, then retry.
    Reauth,
    Fail(LinkError),
}

/// Borrowed bodies with the exact JSON shape of the `mage_core::wire::runner` request DTOs
/// (`Registration`, `HeartbeatRequest`, `EventsRequest`, …), so large payloads are not copied.
#[derive(Serialize)]
struct RegisterBody<'a> {
    grant: RegisterGrant,
    instance_id: Uuid,
    version: &'a str,
    capabilities: &'a Capabilities,
    held: &'a [HeldRun],
}

#[derive(Serialize)]
struct EventsBody<'a> {
    attempt: u16,
    events: &'a [OutboundEvent],
}

#[derive(Serialize)]
struct UploadBody<'a> {
    attempt: u16,
    #[serde(flatten)]
    spec: &'a ArtifactSpec,
}

#[derive(Serialize)]
struct RefreshBody<'a> {
    attempt: u16,
    refs: &'a [DownloadRef],
}

#[derive(Serialize)]
struct SubmitBody<'a> {
    attempt: u16,
    submission_id: Uuid,
    result: &'a WireRunResult,
}

#[derive(Serialize)]
struct FinishBody<'a> {
    attempt: u16,
    #[serde(flatten)]
    report: &'a FinishReport,
}

impl HttpServerLink {
    /// Loads (or creates) the credential file and builds the HTTP client. `version` is the
    /// runner version sent with `register`; `instance_id` is fresh per process.
    pub fn new(config: LinkConfig, version: String, clock: Arc<dyn Clock>, ids: Arc<dyn IdGen>) -> Result<Self, LinkError> {
        let credentials = Credentials::load_or_create(&config.credential_file)
            .map_err(|e| LinkError::Invalid(format!("credential file {}: {e}", config.credential_file.display())))?;
        let http = reqwest::Client::builder()
            .pool_idle_timeout(Duration::from_secs(90))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| LinkError::Invalid(format!("http client: {e}")))?;
        let base = config.server_url.trim_end_matches('/').to_owned();
        Ok(HttpServerLink {
            http,
            codec: TokenCodec::new(base.clone()),
            base,
            name: config.name,
            bootstrap_token: config.bootstrap_token,
            version,
            instance_id: ids.uuid_v4(),
            credentials: Mutex::new(credentials),
            capabilities: Mutex::new(None),
            token: Mutex::new(None),
            refresh: tokio::sync::Mutex::new(()),
            retry: RetryPolicy::default(),
            clock,
            ids,
        })
    }

    /// Replaces the backoff policy (tests use millisecond pauses).
    pub fn with_retry(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    /// The runner's Ed25519 public key (shown when registering a runner by hand).
    pub fn public_key(&self) -> [u8; 32] {
        self.credentials.lock().public_key()
    }

    /// This process's instance id (the runner token's `inst` claim).
    pub fn instance_id(&self) -> Uuid {
        self.instance_id
    }

    fn url(&self, route: &str, run: Option<RunId>, artifact: Option<ArtifactId>) -> String {
        let mut path = route.to_owned();
        if let Some(run) = run {
            path = path.replace("{run}", &run.to_string());
        }
        if let Some(artifact) = artifact {
            path = path.replace("{artifact}", &artifact.to_string());
        }
        format!("{}{path}", self.base)
    }

    fn grant(&self) -> Result<RegisterGrant, LinkError> {
        let credentials = self.credentials.lock();
        match credentials.runner_id() {
            Some(runner) => {
                let iat = self.clock.now();
                let assertion = RunnerAssertion { runner, iat, exp: iat + ASSERTION_LIFETIME, jti: self.ids.uuid_v4() };
                let signed = self
                    .codec
                    .sign_assertion(credentials.key(), &assertion)
                    .map_err(|e| LinkError::Invalid(format!("signing the runner assertion: {e}")))?;
                Ok(RegisterGrant::Assertion { runner_id: runner, assertion: signed })
            }
            None => {
                let Some(token) = &self.bootstrap_token else {
                    return Err(LinkError::Invalid("no runner id in the credential file and no bootstrap token configured".into()));
                };
                Ok(RegisterGrant::Bootstrap { bootstrap_token: token.clone(), name: self.name.clone(), public_key: credentials.public_key() })
            }
        }
    }

    /// `POST /register` with a fresh grant per try (assertion `jti` is single use). Records the
    /// runner id (first start) and the token.
    async fn register_until(&self, capabilities: &Capabilities, held: &[HeldRun], until: Option<Instant>) -> Result<RegistrationAck, LinkError> {
        let url = self.url(routes::REGISTER, None, None);
        let mut failures = 0u32;
        let ack: RegistrationAck = loop {
            let body = RegisterBody { grant: self.grant()?, instance_id: self.instance_id, version: &self.version, capabilities, held };
            let body = Self::json(&body)?;
            let (reason, after) = match self.send(Method::POST, &url, None, body, REQUEST_TIMEOUT).await {
                Attempt::Ok(bytes) => break decode(&bytes)?,
                Attempt::Reauth => return Err(LinkError::Unauthorized),
                Attempt::Fail(e) => return Err(e),
                Attempt::Retry { reason, after } => (reason, after),
            };
            failures += 1;
            self.pause(failures, after, until, &reason).await?;
        };
        if let Err(e) = self.credentials.lock().set_runner_id(ack.runner_id) {
            return Err(LinkError::Invalid(format!("recording the runner id: {e}")));
        }
        let now = Instant::now();
        let ttl = Duration::try_from(ack.expires_at - self.clock.now()).unwrap_or(Duration::ZERO);
        *self.token.lock() = Some(Token { value: ack.runner_token.clone(), refresh_at: now + ttl * 2 / 3, expires_at: now + ttl });
        Ok(ack)
    }

    async fn pause(&self, failures: u32, after: Option<Duration>, until: Option<Instant>, reason: &str) -> Result<(), LinkError> {
        let pause = after.unwrap_or_else(|| self.retry.pause(failures));
        if let Some(until) = until {
            if Instant::now() + pause >= until {
                return Err(LinkError::Unavailable(reason.to_owned()));
            }
        }
        tokio::time::sleep(pause).await;
        Ok(())
    }

    /// The current runner token, refreshed at 2/3 of its TTL.
    async fn bearer(&self) -> Result<String, LinkError> {
        if let Some(token) = self.token.lock().as_ref() {
            if Instant::now() < token.refresh_at {
                return Ok(token.value.clone());
            }
        }
        self.refresh_token(None).await
    }

    /// Single-flight re-registration with an Assertion. `rejected` = the token a 401 refused
    /// (forced refresh); otherwise a scheduled refresh that falls back to the still-valid token
    /// when the server is unreachable.
    async fn refresh_token(&self, rejected: Option<&str>) -> Result<String, LinkError> {
        let _flight = self.refresh.lock().await;
        let current = self.token.lock().as_ref().map(|t| (t.value.clone(), t.refresh_at, t.expires_at));
        let now = Instant::now();
        if let Some((value, refresh_at, _)) = &current {
            let fresh = match rejected {
                Some(rejected) => value != rejected,
                None => now < *refresh_at,
            };
            if fresh {
                return Ok(value.clone());
            }
        }
        let Some(capabilities) = self.capabilities.lock().clone() else {
            return Err(LinkError::Unauthorized);
        };
        let valid = current.as_ref().filter(|(_, _, expires_at)| rejected.is_none() && now < *expires_at);
        let until = valid.map(|(_, _, expires_at)| (*expires_at).min(now + deadline::REFRESH));
        match self.register_until(&capabilities, &[], until).await {
            Ok(ack) => {
                if !ack.released.is_empty() {
                    tracing::warn!(released = ack.released.len(), "token refresh released runs; the instance id changed");
                }
                Ok(ack.runner_token)
            }
            Err(e) => match valid {
                Some((value, _, expires_at)) if !matches!(e, LinkError::Unauthorized | LinkError::Forbidden) => {
                    tracing::warn!(error = %e, "runner token refresh failed; using the current token until it expires");
                    if let Some(token) = self.token.lock().as_mut() {
                        token.refresh_at = (now + deadline::REFRESH).min(*expires_at);
                    }
                    Ok(value.clone())
                }
                _ => Err(e),
            },
        }
    }

    /// One request; maps the answer per runs.md "Errors".
    async fn send(&self, method: Method, url: &str, bearer: Option<&str>, body: Option<Bytes>, timeout: Duration) -> Attempt {
        let mut request = self.http.request(method, url).timeout(timeout).header(PROTOCOL_HEADER, PROTOCOL_VERSION.to_string());
        if let Some(bearer) = bearer {
            request = request.bearer_auth(bearer);
        }
        if let Some(body) = body {
            request = request.header(reqwest::header::CONTENT_TYPE, "application/json").body(body);
        }
        let response = match request.send().await {
            Ok(response) => response,
            Err(e) => return Attempt::Retry { reason: format!("network: {e}"), after: None },
        };
        let status = response.status();
        let bytes = match response.bytes().await {
            Ok(bytes) => bytes,
            Err(e) => return Attempt::Retry { reason: format!("network: {e}"), after: None },
        };
        if status.is_success() {
            return Attempt::Ok(bytes);
        }
        classify(status, serde_json::from_slice::<WireError>(&bytes).ok())
    }

    /// An authenticated `/runner/v1` call with retries until `until`.
    async fn call<T: DeserializeOwned>(
        &self,
        method: Method,
        url: String,
        body: Option<Bytes>,
        timeout: Duration,
        until: Option<Instant>,
    ) -> Result<T, LinkError> {
        let mut failures = 0u32;
        let mut reauthed = false;
        loop {
            let bearer = self.bearer().await?;
            let (reason, after) = match self.send(method.clone(), &url, Some(&bearer), body.clone(), timeout).await {
                Attempt::Ok(bytes) => return decode(&bytes),
                Attempt::Fail(e) => return Err(e),
                Attempt::Reauth if !reauthed => {
                    reauthed = true;
                    self.refresh_token(Some(&bearer)).await?;
                    continue;
                }
                Attempt::Reauth => return Err(LinkError::Unauthorized),
                Attempt::Retry { reason, after } => (reason, after),
            };
            failures += 1;
            tracing::debug!(url, failures, reason, "retrying runner call");
            self.pause(failures, after, until, &reason).await?;
        }
    }

    fn json<B: Serialize>(body: &B) -> Result<Option<Bytes>, LinkError> {
        serde_json::to_vec(body).map(|v| Some(Bytes::from(v))).map_err(|e| LinkError::Invalid(format!("encode: {e}")))
    }

    /// One GET of a signed URL into `dest.partial`, checked, then renamed.
    async fn fetch_once(&self, dl: &Download, dest: &Path, timeout: Duration) -> Result<(), Fetch> {
        let response = match self.http.get(&dl.url).timeout(timeout).send().await {
            Ok(response) => response,
            Err(e) => return Err(Fetch::Transient(format!("network: {e}"))),
        };
        match response.status() {
            s if s.is_success() => {}
            StatusCode::BAD_REQUEST | StatusCode::FORBIDDEN => return Err(Fetch::Expired),
            StatusCode::NOT_FOUND => return Err(Fetch::Fatal(LinkError::NotFound)),
            s if s.is_server_error() || s == StatusCode::TOO_MANY_REQUESTS => return Err(Fetch::Transient(format!("blob host {s}"))),
            s => return Err(Fetch::Fatal(LinkError::Invalid(format!("blob host answered {s}")))),
        }
        let partial = partial_path(dest);
        if let Some(dir) = dest.parent().filter(|d| !d.as_os_str().is_empty()) {
            tokio::fs::create_dir_all(dir).await.map_err(|e| Fetch::Fatal(LinkError::Invalid(format!("create {}: {e}", dir.display()))))?;
        }
        let mut file = tokio::fs::File::create(&partial)
            .await
            .map_err(|e| Fetch::Fatal(LinkError::Invalid(format!("create {}: {e}", partial.display()))))?;
        let mut hasher = Sha256::new();
        let mut total = 0u64;
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| Fetch::Transient(format!("network: {e}")))?;
            total += chunk.len() as u64;
            if total > dl.bytes {
                let _ = tokio::fs::remove_file(&partial).await;
                return Err(Fetch::Integrity(format!("more than the declared {} bytes", dl.bytes)));
            }
            hasher.update(&chunk);
            file.write_all(&chunk).await.map_err(|e| Fetch::Fatal(LinkError::Invalid(format!("write {}: {e}", partial.display()))))?;
        }
        file.sync_all().await.map_err(|e| Fetch::Fatal(LinkError::Invalid(format!("sync {}: {e}", partial.display()))))?;
        drop(file);
        let digest: [u8; 32] = hasher.finalize().into();
        if total != dl.bytes || digest != dl.sha256 {
            let _ = tokio::fs::remove_file(&partial).await;
            return Err(Fetch::Integrity(format!("got {total} bytes, sha256 {}; expected {} bytes, sha256 {}", hex::encode(digest), dl.bytes, hex::encode(dl.sha256))));
        }
        tokio::fs::rename(&partial, dest).await.map_err(|e| Fetch::Fatal(LinkError::Invalid(format!("rename to {}: {e}", dest.display()))))
    }
}

enum Fetch {
    /// 400/403 from the blob host: the signed URL expired.
    Expired,
    Integrity(String),
    Transient(String),
    Fatal(LinkError),
}

fn partial_path(dest: &Path) -> PathBuf {
    let mut name = dest.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(".partial");
    dest.with_file_name(name)
}

fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, LinkError> {
    serde_json::from_slice(bytes).map_err(|e| LinkError::Invalid(format!("response body: {e}")))
}

/// runs.md "Errors": HTTP status + `WireError.code` → retry, re-auth, or a final `LinkError`.
fn classify(status: StatusCode, wire: Option<WireError>) -> Attempt {
    let message = wire.as_ref().map(|w| w.message.clone()).unwrap_or_else(|| status.to_string());
    let code = wire.as_ref().map(|w| w.code);
    match status.as_u16() {
        400 if code == Some(WireErrorCode::UnsupportedProtocol) => Attempt::Fail(LinkError::Invalid(format!("unsupported_protocol: {message}"))),
        400 => Attempt::Fail(LinkError::Invalid(message)),
        401 => Attempt::Reauth,
        403 => Attempt::Fail(LinkError::Forbidden),
        404 => Attempt::Fail(LinkError::NotFound),
        409 => {
            let status = wire.as_ref().and_then(|w| w.status);
            let conflict = match (code, status) {
                (Some(WireErrorCode::LeaseLost), _) => LinkConflict::LeaseLost,
                (Some(WireErrorCode::RunTerminal), Some(s)) => LinkConflict::RunTerminal(s),
                (Some(WireErrorCode::InvalidState), Some(s)) => LinkConflict::InvalidState(s),
                (Some(WireErrorCode::IdempotencyMismatch), _) => LinkConflict::IdempotencyMismatch,
                _ => return Attempt::Fail(LinkError::Invalid(format!("409 without a usable code/status: {message}"))),
            };
            Attempt::Fail(LinkError::Conflict(conflict))
        }
        413 => Attempt::Fail(LinkError::TooLarge(message)),
        422 => {
            let rejection = wire.as_ref().and_then(WireError::rejection).unwrap_or(Rejection { gate: None, message, items: Vec::new() });
            Attempt::Fail(LinkError::Rejected(rejection))
        }
        429 => Attempt::Retry {
            reason: format!("rate limited: {message}"),
            after: wire.as_ref().and_then(|w| w.retry_after_ms).map(Duration::from_millis),
        },
        s if s >= 500 => Attempt::Retry { reason: format!("{status}: {message}"), after: None },
        _ => Attempt::Fail(LinkError::Invalid(format!("{status}: {message}"))),
    }
}

fn within(d: Duration) -> Option<Instant> {
    Some(Instant::now() + d)
}

#[async_trait::async_trait]
impl ServerLink for HttpServerLink {
    async fn register(&self, capabilities: &Capabilities, held: &[HeldRun]) -> Result<RegistrationAck, LinkError> {
        *self.capabilities.lock() = Some(capabilities.clone());
        let _flight = self.refresh.lock().await;
        self.register_until(capabilities, held, None).await
    }

    async fn lease(&self, req: &LeaseRequest) -> Result<Option<RunAssignment>, LinkError> {
        let url = self.url(routes::LEASE, None, None);
        let response: LeaseResponse = self.call(Method::POST, url, Self::json(req)?, req.wait + LONG_POLL_SLACK, None).await?;
        Ok(response.assignment)
    }

    async fn heartbeat(&self, run: RunId, attempt: u16, phase: RunnerPhase) -> Result<HeartbeatAck, LinkError> {
        let url = self.url(routes::HEARTBEAT, Some(run), None);
        self.call(Method::POST, url, Self::json(&HeartbeatRequest { attempt, phase })?, REQUEST_TIMEOUT, within(deadline::HEARTBEAT)).await
    }

    async fn push_events(&self, run: RunId, attempt: u16, events: &[OutboundEvent]) -> Result<EventsAck, LinkError> {
        let url = self.url(routes::EVENTS, Some(run), None);
        self.call(Method::POST, url, Self::json(&EventsBody { attempt, events })?, REQUEST_TIMEOUT, None).await
    }

    async fn poll_commands(&self, run: RunId, attempt: u16, after_seq: u64, wait: Duration) -> Result<CommandBatch, LinkError> {
        let url = format!(
            "{}?attempt={attempt}&after={after_seq}&wait_ms={}",
            self.url(routes::COMMANDS, Some(run), None),
            wait.as_millis()
        );
        self.call(Method::GET, url, None, wait + LONG_POLL_SLACK, None).await
    }

    async fn request_upload(&self, run: RunId, attempt: u16, spec: &ArtifactSpec) -> Result<UploadGrant, LinkError> {
        let url = self.url(routes::ARTIFACTS, Some(run), None);
        self.call(Method::POST, url, Self::json(&UploadBody { attempt, spec })?, REQUEST_TIMEOUT, within(deadline::ARTIFACT)).await
    }

    async fn upload(&self, grant: &UploadGrant, file: &Path) -> Result<(), LinkError> {
        let until = Instant::now() + deadline::ARTIFACT;
        let method = match grant.method {
            HttpMethod::Put => Method::PUT,
            HttpMethod::Get => return Err(LinkError::Invalid("upload grant with method GET".into())),
        };
        let mut failures = 0u32;
        loop {
            let handle = tokio::fs::File::open(file).await.map_err(|e| LinkError::Invalid(format!("open {}: {e}", file.display())))?;
            let len = handle.metadata().await.map_err(|e| LinkError::Invalid(format!("stat {}: {e}", file.display())))?.len();
            let body = reqwest::Body::wrap_stream(tokio_util::io::ReaderStream::new(handle));
            let mut request = self
                .http
                .request(method.clone(), &grant.url)
                .timeout(until.saturating_duration_since(Instant::now()))
                .header(reqwest::header::CONTENT_LENGTH, len)
                .body(body);
            for (name, value) in &grant.headers {
                request = request.header(name.as_str(), value.as_str());
            }
            let reason = match request.send().await {
                Ok(response) => match response.status() {
                    s if s.is_success() => return Ok(()),
                    StatusCode::BAD_REQUEST | StatusCode::FORBIDDEN => return Err(LinkError::Forbidden),
                    s if s.is_server_error() || s == StatusCode::TOO_MANY_REQUESTS => format!("blob host {s}"),
                    s => return Err(LinkError::Invalid(format!("blob host answered {s}"))),
                },
                Err(e) => format!("network: {e}"),
            };
            failures += 1;
            self.pause(failures, None, Some(until), &reason).await?;
        }
    }

    async fn complete_upload(&self, run: RunId, attempt: u16, artifact: ArtifactId) -> Result<(), LinkError> {
        let url = self.url(routes::ARTIFACT_COMPLETE, Some(run), Some(artifact));
        let _: CompleteUploadAck =
            self.call(Method::POST, url, Self::json(&CompleteUploadRequest { attempt })?, REQUEST_TIMEOUT, within(deadline::ARTIFACT)).await?;
        Ok(())
    }

    async fn refresh_downloads(&self, run: RunId, attempt: u16, refs: &[DownloadRef]) -> Result<Vec<Download>, LinkError> {
        let url = self.url(routes::DOWNLOADS, Some(run), None);
        let response: RefreshDownloadsResponse =
            self.call(Method::POST, url, Self::json(&RefreshBody { attempt, refs })?, REQUEST_TIMEOUT, within(deadline::DOWNLOADS)).await?;
        Ok(response.downloads)
    }

    async fn download(&self, run: RunId, attempt: u16, dl: &Download, dest: &Path) -> Result<(), LinkError> {
        let until = Instant::now() + deadline::ARTIFACT;
        let mut current = dl.clone();
        let mut refreshed = false;
        let mut integrity = 0u32;
        let mut failures = 0u32;
        loop {
            let expired = self.clock.now() >= current.expires_at;
            let fetched = if expired { Err(Fetch::Expired) } else { self.fetch_once(&current, dest, until.saturating_duration_since(Instant::now())).await };
            let reason = match fetched {
                Ok(()) => return Ok(()),
                Err(Fetch::Expired) if !refreshed => {
                    refreshed = true;
                    let fresh = self.refresh_downloads(run, attempt, &[dl.what]).await?;
                    current = fresh
                        .into_iter()
                        .find(|d| d.what == dl.what)
                        .ok_or_else(|| LinkError::Invalid("refresh_downloads omitted the requested download".into()))?;
                    continue;
                }
                Err(Fetch::Expired) => return Err(LinkError::Forbidden),
                Err(Fetch::Fatal(e)) => return Err(e),
                Err(Fetch::Integrity(reason)) => {
                    integrity += 1;
                    if integrity >= DOWNLOAD_INTEGRITY_TRIES {
                        return Err(LinkError::Invalid(format!("download integrity check failed: {reason}")));
                    }
                    reason
                }
                Err(Fetch::Transient(reason)) => reason,
            };
            failures += 1;
            self.pause(failures, None, Some(until), &reason).await?;
        }
    }

    async fn submit_result(&self, run: RunId, attempt: u16, submission_id: Uuid, result: &WireRunResult) -> Result<SubmitAck, LinkError> {
        let url = self.url(routes::RESULT, Some(run), None);
        let body = Self::json(&SubmitBody { attempt, submission_id, result })?;
        self.call(Method::POST, url, body, REQUEST_TIMEOUT, within(deadline::SUBMIT)).await
    }

    async fn finish(&self, run: RunId, attempt: u16, report: &FinishReport) -> Result<RunStatus, LinkError> {
        let url = self.url(routes::FINISH, Some(run), None);
        let ack: FinishAck =
            self.call(Method::POST, url, Self::json(&FinishBody { attempt, report })?, REQUEST_TIMEOUT, within(deadline::FINISH)).await?;
        Ok(ack.status)
    }
}

#[cfg(test)]
mod tests;
