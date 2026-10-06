//! `/runner/v1` (runs.md "Runner protocol", "Adapter internals"): routes exactly as
//! `mage_core::wire::runner::routes`, `Mage-Protocol: 1`, runner-token auth (except `register`),
//! per-runner token bucket (50 req/s, burst 200), body limits (events 4 MiB after gzip
//! decompression, result 1 MiB, everything else 64 KiB), 30 s handler timeout, `WireError` bodies.

use std::collections::HashMap;
use std::io::Read as _;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use parking_lot::Mutex;
use serde::Serialize;
use serde::de::DeserializeOwned;

use mage_core::domain::{ArtifactId, RunId, RunnerId};
use mage_core::wire::runner::{
    CommandsQuery, CompleteUploadRequest, EventsRequest, FinishRequest, HeartbeatRequest, LeaseRequest, LeaseResponse, MAX_EVENTS_BATCH_BYTES,
    PROTOCOL_HEADER, PROTOCOL_VERSION, RefreshDownloadsRequest, RefreshDownloadsResponse, Registration, SubmitResultRequest, UploadRequest,
    WireError, WireErrorCode, routes,
};

use crate::app::runs::{ProtocolError, RunManager, RunnerCaller};
use crate::error::AppError;

/// Body limits per route group.
pub const EVENTS_LIMIT: usize = MAX_EVENTS_BATCH_BYTES;
pub const RESULT_LIMIT: usize = 1024 * 1024;
pub const DEFAULT_LIMIT: usize = 64 * 1024;
/// Per-call handler timeout (long-polls return within 25 s).
pub const HANDLER_TIMEOUT: Duration = Duration::from_secs(30);
/// Token bucket per runner.
pub const RATE_PER_SEC: f64 = 50.0;
pub const RATE_BURST: f64 = 200.0;

#[derive(Clone)]
pub struct RunnerApiState {
    runs: RunManager,
    buckets: Arc<Mutex<HashMap<RunnerId, (f64, Instant)>>>,
}

impl RunnerApiState {
    pub fn new(runs: RunManager) -> RunnerApiState {
        RunnerApiState { runs, buckets: Arc::new(Mutex::new(HashMap::new())) }
    }

    /// Takes one token from the runner's bucket; `Some(retry_after)` when empty.
    fn take(&self, runner: RunnerId) -> Option<Duration> {
        let now = Instant::now();
        let mut buckets = self.buckets.lock();
        let (tokens, at) = buckets.entry(runner).or_insert((RATE_BURST, now));
        *tokens = (*tokens + now.duration_since(*at).as_secs_f64() * RATE_PER_SEC).min(RATE_BURST);
        *at = now;
        if *tokens >= 1.0 {
            *tokens -= 1.0;
            None
        } else {
            Some(Duration::from_secs_f64((1.0 - *tokens) / RATE_PER_SEC))
        }
    }
}

pub fn router(state: RunnerApiState) -> Router {
    Router::new()
        .route(routes::REGISTER, post(register))
        .route(routes::LEASE, post(lease))
        .route(routes::HEARTBEAT, post(heartbeat))
        .route(routes::EVENTS, post(events))
        .route(routes::COMMANDS, get(commands))
        .route(routes::ARTIFACTS, post(request_upload))
        .route(routes::ARTIFACT_COMPLETE, post(complete_upload))
        .route(routes::DOWNLOADS, post(downloads))
        .route(routes::RESULT, post(result))
        .route(routes::FINISH, post(finish))
        .with_state(state)
}

/// A `/runner/v1` error response.
pub struct WireReply(pub WireError);

impl IntoResponse for WireReply {
    fn into_response(self) -> Response {
        let status = StatusCode::from_u16(self.0.code.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let retry = self.0.retry_after_ms.map(|ms| ms.div_ceil(1000).max(1));
        let mut resp = (status, axum::Json(self.0)).into_response();
        if let Some(secs) = retry
            && let Ok(v) = header::HeaderValue::from_str(&secs.to_string())
        {
            resp.headers_mut().insert(header::RETRY_AFTER, v);
        }
        resp
    }
}

impl From<ProtocolError> for WireReply {
    fn from(err: ProtocolError) -> Self {
        if let ProtocolError::App(AppError::Internal(msg)) = &err {
            tracing::error!(error = %msg, "runner api internal error");
        }
        WireReply(err.to_wire())
    }
}

fn wire(code: WireErrorCode, message: impl Into<String>) -> WireReply {
    WireReply(WireError::new(code, message))
}

type Reply<T> = Result<axum::Json<T>, WireReply>;

fn check_protocol(headers: &HeaderMap) -> Result<(), WireReply> {
    let version = headers.get(PROTOCOL_HEADER).and_then(|v| v.to_str().ok()).and_then(|v| v.trim().parse::<u32>().ok());
    match version {
        Some(PROTOCOL_VERSION) => Ok(()),
        _ => Err(wire(WireErrorCode::UnsupportedProtocol, format!("send {PROTOCOL_HEADER}: {PROTOCOL_VERSION}"))),
    }
}

async fn authenticate(state: &RunnerApiState, headers: &HeaderMap) -> Result<RunnerCaller, WireReply> {
    check_protocol(headers)?;
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or_else(|| wire(WireErrorCode::Unauthorized, "runner token missing"))?;
    let caller = state.runs.authenticate_runner(token.trim()).await?;
    if let Some(wait) = state.take(caller.id()) {
        let ms = u64::try_from(wait.as_millis()).unwrap_or(u64::MAX).max(1);
        return Err(WireReply(WireError { retry_after_ms: Some(ms), ..WireError::new(WireErrorCode::RateLimited, "per-runner rate limit") }));
    }
    Ok(caller)
}

/// Reads and parses a JSON body under `limit` (after gzip decompression).
async fn json_body<T: DeserializeOwned>(headers: &HeaderMap, body: Body, limit: usize) -> Result<T, WireReply> {
    let too_large = || wire(WireErrorCode::TooLarge, format!("body over {limit} bytes"));
    let raw: Bytes = axum::body::to_bytes(body, limit).await.map_err(|_| too_large())?;
    let encoding = headers.get(header::CONTENT_ENCODING).and_then(|v| v.to_str().ok()).map(str::trim);
    let bytes = match encoding {
        None | Some("identity") => raw.to_vec(),
        Some("gzip") => {
            let mut out = Vec::new();
            let mut dec = flate2::read::GzDecoder::new(raw.as_ref()).take(limit as u64 + 1);
            dec.read_to_end(&mut out).map_err(|e| wire(WireErrorCode::Invalid, format!("bad gzip body: {e}")))?;
            if out.len() > limit {
                return Err(too_large());
            }
            out
        }
        Some(other) => return Err(wire(WireErrorCode::Invalid, format!("unsupported content-encoding {other}"))),
    };
    serde_json::from_slice(&bytes).map_err(|e| wire(WireErrorCode::Invalid, format!("malformed body: {e}")))
}

fn run_id(raw: &str) -> Result<RunId, WireReply> {
    raw.parse().map_err(|_| wire(WireErrorCode::Invalid, "bad run id"))
}

async fn timed<T: Serialize>(fut: impl Future<Output = Result<T, ProtocolError>>) -> Reply<T> {
    match tokio::time::timeout(HANDLER_TIMEOUT, fut).await {
        Ok(Ok(v)) => Ok(axum::Json(v)),
        Ok(Err(e)) => Err(e.into()),
        Err(_) => Err(wire(WireErrorCode::Unavailable, "handler timeout")),
    }
}

async fn register(State(s): State<RunnerApiState>, headers: HeaderMap, body: Body) -> Reply<mage_core::wire::runner::RegistrationAck> {
    check_protocol(&headers)?;
    let reg: Registration = json_body(&headers, body, DEFAULT_LIMIT).await?;
    timed(s.runs.register_runner(reg)).await
}

async fn lease(State(s): State<RunnerApiState>, headers: HeaderMap, body: Body) -> Reply<LeaseResponse> {
    let caller = authenticate(&s, &headers).await?;
    let req: LeaseRequest = json_body(&headers, body, DEFAULT_LIMIT).await?;
    timed(async { Ok(LeaseResponse { assignment: s.runs.lease(&caller, req).await? }) }).await
}

async fn heartbeat(State(s): State<RunnerApiState>, Path(run): Path<String>, headers: HeaderMap, body: Body) -> Reply<mage_core::wire::runner::HeartbeatAck> {
    let caller = authenticate(&s, &headers).await?;
    let run = run_id(&run)?;
    let req: HeartbeatRequest = json_body(&headers, body, DEFAULT_LIMIT).await?;
    timed(s.runs.heartbeat(&caller, run, req)).await
}

async fn events(State(s): State<RunnerApiState>, Path(run): Path<String>, headers: HeaderMap, body: Body) -> Reply<mage_core::wire::runner::EventsAck> {
    let caller = authenticate(&s, &headers).await?;
    let run = run_id(&run)?;
    let req: EventsRequest = json_body(&headers, body, EVENTS_LIMIT).await?;
    timed(s.runs.ingest_events(&caller, run, req)).await
}

async fn commands(
    State(s): State<RunnerApiState>,
    Path(run): Path<String>,
    q: Result<Query<CommandsQuery>, axum::extract::rejection::QueryRejection>,
    headers: HeaderMap,
) -> Reply<mage_core::wire::runner::CommandBatch> {
    let caller = authenticate(&s, &headers).await?;
    let run = run_id(&run)?;
    let Query(q) = q.map_err(|e| wire(WireErrorCode::Invalid, format!("bad query: {e}")))?;
    timed(s.runs.poll_commands(&caller, run, q)).await
}

async fn request_upload(State(s): State<RunnerApiState>, Path(run): Path<String>, headers: HeaderMap, body: Body) -> Reply<mage_core::wire::runner::UploadGrant> {
    let caller = authenticate(&s, &headers).await?;
    let run = run_id(&run)?;
    let req: UploadRequest = json_body(&headers, body, DEFAULT_LIMIT).await?;
    timed(s.runs.request_upload(&caller, run, req)).await
}

async fn complete_upload(
    State(s): State<RunnerApiState>,
    Path((run, artifact)): Path<(String, String)>,
    headers: HeaderMap,
    body: Body,
) -> Reply<mage_core::wire::runner::CompleteUploadAck> {
    let caller = authenticate(&s, &headers).await?;
    let run = run_id(&run)?;
    let artifact: ArtifactId = artifact.parse().map_err(|_| wire(WireErrorCode::Invalid, "bad artifact id"))?;
    let req: CompleteUploadRequest = json_body(&headers, body, DEFAULT_LIMIT).await?;
    timed(s.runs.complete_upload(&caller, run, artifact, req.attempt)).await
}

async fn downloads(State(s): State<RunnerApiState>, Path(run): Path<String>, headers: HeaderMap, body: Body) -> Reply<RefreshDownloadsResponse> {
    let caller = authenticate(&s, &headers).await?;
    let run = run_id(&run)?;
    let req: RefreshDownloadsRequest = json_body(&headers, body, DEFAULT_LIMIT).await?;
    timed(async { Ok(RefreshDownloadsResponse { downloads: s.runs.refresh_downloads(&caller, run, req).await? }) }).await
}

async fn result(State(s): State<RunnerApiState>, Path(run): Path<String>, headers: HeaderMap, body: Body) -> Reply<mage_core::wire::runner::SubmitAck> {
    let caller = authenticate(&s, &headers).await?;
    let run = run_id(&run)?;
    let req: SubmitResultRequest = json_body(&headers, body, RESULT_LIMIT).await?;
    timed(s.runs.submit_result(&caller, run, req)).await
}

async fn finish(State(s): State<RunnerApiState>, Path(run): Path<String>, headers: HeaderMap, body: Body) -> Reply<mage_core::wire::runner::FinishAck> {
    let caller = authenticate(&s, &headers).await?;
    let run = run_id(&run)?;
    let req: FinishRequest = json_body(&headers, body, DEFAULT_LIMIT).await?;
    timed(s.runs.finish_run(&caller, run, req)).await
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;
    use uuid::Uuid;

    use mage_core::domain::{RunId, RunKind, UserId};
    use mage_core::wire::fill;
    use mage_core::wire::runner::{HeartbeatAck, HeartbeatRequest, LeaseRequest, LeaseResponse, RunnerPhase, SlotCounts, WireError, WireErrorCode, routes};

    use super::{RunnerApiState, router};
    use crate::ports::tokens::RunnerScope;
    use crate::testing::RunsHarness;

    fn post(path: &str, token: Option<&str>, body: serde_json::Value) -> Request<Body> {
        let mut req = Request::post(path).header("content-type", "application/json").header("Mage-Protocol", "1");
        if let Some(t) = token {
            req = req.header("authorization", format!("Bearer {t}"));
        }
        req.body(Body::from(body.to_string())).unwrap()
    }

    async fn json<T: serde::de::DeserializeOwned>(resp: axum::response::Response) -> T {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        serde_json::from_slice(&bytes).unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&bytes)))
    }

    #[tokio::test]
    async fn auth_protocol_fencing_and_idempotent_finish_over_http() {
        let h = RunsHarness::new();
        let org = h.org();
        let repo = h.repo(org).await;
        let ticket = h.ticket(org, &repo, 1).await;
        let run = h.manager.start_diagnose(org, ticket.ticket, ticket.causes[0], None, UserId::from_uuid(Uuid::nil())).await.unwrap();
        let (_, ack) = h.runner("r1", RunnerScope::Shared).await;
        let app = router(RunnerApiState::new(h.manager.clone()));
        let token = ack.runner_token.as_str();

        // missing token → 401 unauthorized
        let resp = app.clone().oneshot(post(routes::LEASE, None, serde_json::json!({}))).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(json::<WireError>(resp).await.code, WireErrorCode::Unauthorized);
        // forged token → 401
        let resp = app.clone().oneshot(post(routes::LEASE, Some("mgr_bogus"), serde_json::json!({}))).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        // unknown protocol version → 400 unsupported_protocol
        let req = Request::post(routes::LEASE).header("Mage-Protocol", "2").header("authorization", format!("Bearer {token}")).body(Body::empty()).unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert_eq!(json::<WireError>(resp).await.code, WireErrorCode::UnsupportedProtocol);
        // malformed body → 400 invalid
        let resp = app.clone().oneshot(post(routes::LEASE, Some(token), serde_json::json!({"nope": 1}))).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert_eq!(json::<WireError>(resp).await.code, WireErrorCode::Invalid);

        // lease
        let lease = LeaseRequest { request_id: Uuid::now_v7(), wait: std::time::Duration::ZERO, free: SlotCounts { light: 1, work: 1, verify: 1 }, prepared: Vec::new() };
        let resp = app.clone().oneshot(post(routes::LEASE, Some(token), serde_json::to_value(&lease).unwrap())).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let got: LeaseResponse = json(resp).await;
        let a = got.assignment.unwrap();
        assert_eq!((a.run_id, a.kind), (run.id(), RunKind::Diagnose));

        // heartbeat with a wrong attempt → 409 lease_lost
        let hb = |attempt| serde_json::to_value(HeartbeatRequest { attempt, phase: RunnerPhase::Preparing }).unwrap();
        let path = fill(routes::HEARTBEAT, &[("run", &run.id().to_string())]);
        let resp = app.clone().oneshot(post(&path, Some(token), hb(2))).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        assert_eq!(json::<WireError>(resp).await.code, WireErrorCode::LeaseLost);
        let resp = app.clone().oneshot(post(&path, Some(token), hb(1))).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(json::<HeartbeatAck>(resp).await.status, mage_core::domain::RunStatus::Preparing);
        // unknown run → 404
        let other = fill(routes::HEARTBEAT, &[("run", &RunId::from_uuid(Uuid::now_v7()).to_string())]);
        let resp = app.clone().oneshot(post(&other, Some(token), hb(1))).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        // finish Done while not terminal → 409 invalid_state with status
        let finish = fill(routes::FINISH, &[("run", &run.id().to_string())]);
        let body = serde_json::json!({"attempt": 1, "outcome": {"type": "done"}, "message": null});
        let resp = app.clone().oneshot(post(&finish, Some(token), body)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        let err: WireError = json(resp).await;
        assert_eq!((err.code, err.status), (WireErrorCode::InvalidState, Some(mage_core::domain::RunStatus::Preparing)));
        // a retryable failure hands the lease back; the retried finish replays the stored status
        let body = serde_json::json!({"attempt": 1, "outcome": {"type": "failed", "stage": "prepare", "reason": "disk", "retryable": true}, "message": null});
        let first = app.clone().oneshot(post(&finish, Some(token), body.clone())).await.unwrap();
        assert_eq!(first.status(), StatusCode::OK);
        let first: serde_json::Value = json(first).await;
        assert_eq!(first["status"], "queued");
        let again = app.clone().oneshot(post(&finish, Some(token), body)).await.unwrap();
        assert_eq!(again.status(), StatusCode::OK);
        assert_eq!(json::<serde_json::Value>(again).await, first);
        // the old attempt is fenced off afterwards
        let resp = app.clone().oneshot(post(&path, Some(token), hb(1))).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn gzip_event_batches_are_accepted() {
        use std::io::Write;
        let h = RunsHarness::new();
        let org = h.org();
        let repo = h.repo(org).await;
        let ticket = h.ticket(org, &repo, 1).await;
        let run = h.manager.start_diagnose(org, ticket.ticket, ticket.causes[0], None, UserId::from_uuid(Uuid::nil())).await.unwrap();
        let (caller, ack) = h.runner("r1", RunnerScope::Shared).await;
        let lease = LeaseRequest { request_id: Uuid::now_v7(), wait: std::time::Duration::ZERO, free: SlotCounts { light: 1, work: 1, verify: 1 }, prepared: Vec::new() };
        h.manager.lease(&caller, lease).await.unwrap().unwrap();
        let app = router(RunnerApiState::new(h.manager.clone()));
        let body = serde_json::json!({"attempt": 1, "events": [{"local_seq": 1, "at": "2026-01-01T00:00:00Z", "kind": {"type": "notice", "data": "hello"}}]});
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(body.to_string().as_bytes()).unwrap();
        let gz = enc.finish().unwrap();
        let path = fill(routes::EVENTS, &[("run", &run.id().to_string())]);
        let req = Request::post(&path)
            .header("Mage-Protocol", "1")
            .header("authorization", format!("Bearer {}", ack.runner_token))
            .header("content-encoding", "gzip")
            .body(Body::from(gz))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let ack: mage_core::wire::runner::EventsAck = json(resp).await;
        assert_eq!(ack.acked_local_seq, 1);
    }
}
