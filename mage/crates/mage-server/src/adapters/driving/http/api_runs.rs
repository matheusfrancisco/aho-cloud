//! UI routes of the runs area (server.md catalog; DTOs in `mage_core::wire::api`): run list and
//! detail, event history, steer, cancel, artifacts and their bytes (with `Range`), "Run
//! diagnostic" on a cause, Learn and the onboarding Boot verify. Session, CSRF and `{org}`
//! resolution come from the platform layers (`OrgCtx`).

use std::collections::HashMap;
use std::ops::Range;

use axum::Router;
use axum::body::Body;
use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Json;

use mage_core::domain::{ArtifactId, ArtifactKind, CauseId, MemberRole, RepoId, RunId, TicketId};
use mage_core::wire::api::{
    ArtifactList, ArtifactView, CommandQueued, Paged, RunDetail, RunEventPage, RunEventsQuery, RunListQuery, RunStarted, RunStatusView, RunSummary,
    StartHarnessRunRequest, StartLearnRequest, SteerRequest, VerifyRecipeRequest, routes,
};

use crate::adapters::driving::http::auth::OrgCtx;
use crate::app::runs::query::artifact_view;
use crate::app::runs::{CancelRun, RunManager, SteerRun};
use crate::error::AppError;
use crate::ports::runs::ArtifactRecord;

/// Bytes per read when streaming an artifact without `Range`.
pub const CHUNK: u64 = 4 * 1024 * 1024;
/// Largest single `Range` answer (bigger ranges are shortened; clients continue).
pub const MAX_RANGE: u64 = 16 * 1024 * 1024;

#[derive(Clone)]
pub struct RunsApiState {
    runs: RunManager,
}

impl RunsApiState {
    pub fn new(runs: RunManager) -> RunsApiState {
        RunsApiState { runs }
    }
}

pub fn router(state: RunsApiState) -> Router {
    Router::new()
        .route(routes::RUNS, get(list_runs))
        .route(routes::RUN, get(run_detail))
        .route(routes::RUN_EVENTS, get(run_events))
        .route(routes::RUN_STEER, post(steer))
        .route(routes::RUN_CANCEL, post(cancel))
        .route(routes::RUN_ARTIFACTS, get(run_artifacts))
        .route(routes::ARTIFACT, get(artifact))
        .route(routes::ARTIFACT_CONTENT, get(artifact_content))
        .route(routes::CAUSE_DIAGNOSE, post(start_diagnose))
        .route(routes::REPO_LEARN, post(start_learn))
        .route(routes::REPO_RECIPE_VERIFY, post(verify_recipe))
        .with_state(state)
}

type Params = Path<HashMap<String, String>>;

fn param<T: std::str::FromStr>(p: &HashMap<String, String>, name: &str) -> Result<T, AppError> {
    p.get(name).and_then(|v| v.parse().ok()).ok_or_else(|| AppError::NotFound(name.to_owned()))
}

fn body<T>(b: Result<Json<T>, JsonRejection>) -> Result<T, AppError> {
    b.map(|Json(v)| v).map_err(|e| AppError::Invalid(e.body_text()))
}

fn query<T>(q: Result<Query<T>, QueryRejection>) -> Result<T, AppError> {
    q.map(|Query(v)| v).map_err(|e| AppError::Invalid(e.body_text()))
}

async fn list_runs(State(s): State<RunsApiState>, ctx: OrgCtx, q: Result<Query<RunListQuery>, QueryRejection>) -> Result<Json<Paged<RunSummary>>, AppError> {
    Ok(Json(s.runs.list_runs(ctx.org_id, &query(q)?).await?))
}

async fn run_detail(State(s): State<RunsApiState>, ctx: OrgCtx, Path(p): Params) -> Result<Json<RunDetail>, AppError> {
    let run: RunId = param(&p, "run")?;
    Ok(Json(s.runs.run_detail(ctx.org_id, run).await?))
}

async fn run_events(
    State(s): State<RunsApiState>,
    ctx: OrgCtx,
    Path(p): Params,
    q: Result<Query<RunEventsQuery>, QueryRejection>,
) -> Result<Json<RunEventPage>, AppError> {
    let run: RunId = param(&p, "run")?;
    let q = query(q)?;
    Ok(Json(s.runs.events_page(ctx.org_id, run, q.after, q.limit).await?))
}

async fn steer(State(s): State<RunsApiState>, ctx: OrgCtx, Path(p): Params, b: Result<Json<SteerRequest>, JsonRejection>) -> Result<Json<CommandQueued>, AppError> {
    ctx.require(MemberRole::Member)?;
    let run: RunId = param(&p, "run")?;
    let req = body(b)?;
    let seq = s.runs.steer_run(SteerRun { org: ctx.org_id, run, text: req.text, by: ctx.user_id }).await?;
    Ok(Json(CommandQueued { seq }))
}

async fn cancel(State(s): State<RunsApiState>, ctx: OrgCtx, Path(p): Params) -> Result<Json<RunStatusView>, AppError> {
    ctx.require(MemberRole::Member)?;
    let run: RunId = param(&p, "run")?;
    let status = s.runs.cancel_run(CancelRun { org: ctx.org_id, run, by: Some(ctx.user_id), notice: None }).await?;
    Ok(Json(RunStatusView { status }))
}

async fn run_artifacts(State(s): State<RunsApiState>, ctx: OrgCtx, Path(p): Params) -> Result<Json<ArtifactList>, AppError> {
    let run: RunId = param(&p, "run")?;
    Ok(Json(ArtifactList { artifacts: s.runs.run_artifacts(ctx.org_id, run).await? }))
}

async fn artifact(State(s): State<RunsApiState>, ctx: OrgCtx, Path(p): Params) -> Result<Json<ArtifactView>, AppError> {
    let id: ArtifactId = param(&p, "artifact")?;
    Ok(Json(artifact_view(&s.runs.artifact_meta(ctx.org_id, id).await?)))
}

/// Fixed content type per kind (never the runner's declaration) and whether it renders inline.
pub fn served_type(a: &ArtifactRecord) -> (&'static str, bool) {
    match a.artifact.kind {
        ArtifactKind::Video if a.name.ends_with(".mp4") => ("video/mp4", true),
        ArtifactKind::Video => ("video/webm", true),
        ArtifactKind::Gif => ("image/gif", true),
        ArtifactKind::Screenshot => ("image/png", true),
        ArtifactKind::Trace => ("application/zip", false),
        ArtifactKind::Log | ArtifactKind::Script => ("text/plain; charset=utf-8", false),
        ArtifactKind::Patch => ("text/x-diff; charset=utf-8", false),
        ArtifactKind::Transcript => ("application/x-ndjson", false),
        ArtifactKind::FixBrief => ("text/markdown; charset=utf-8", false),
        ArtifactKind::ScipIndex | ArtifactKind::SourceBundle => ("application/octet-stream", false),
    }
}

/// `bytes=a-b`, `bytes=a-`, `bytes=-n` against an object of `size` bytes. `Ok(None)` = no or
/// unparsable range (serve the whole object); `Err` = unsatisfiable.
pub fn parse_range(value: &str, size: u64) -> Result<Option<Range<u64>>, ()> {
    let Some(spec) = value.trim().strip_prefix("bytes=") else { return Ok(None) };
    if spec.contains(',') {
        return Ok(None);
    }
    let Some((start, end)) = spec.split_once('-') else { return Ok(None) };
    let (start, end) = (start.trim(), end.trim());
    let range = match (start.is_empty(), end.is_empty()) {
        (true, false) => {
            let Ok(n) = end.parse::<u64>() else { return Ok(None) };
            if n == 0 {
                return Err(());
            }
            size.saturating_sub(n)..size
        }
        (false, _) => {
            let Ok(a) = start.parse::<u64>() else { return Ok(None) };
            let b = if end.is_empty() { size.saturating_sub(1) } else { end.parse::<u64>().map_err(|_| ())?.min(size.saturating_sub(1)) };
            if a >= size || b < a {
                return Err(());
            }
            a..b + 1
        }
        (true, true) => return Ok(None),
    };
    if range.is_empty() { Err(()) } else { Ok(Some(range)) }
}

async fn artifact_content(State(s): State<RunsApiState>, ctx: OrgCtx, Path(p): Params, headers: HeaderMap) -> Result<Response, AppError> {
    let id: ArtifactId = param(&p, "artifact")?;
    let a = s.runs.artifact_meta(ctx.org_id, id).await?;
    RunManager::viewable(ctx.org_id, &a)?;
    let size = a.artifact.bytes;
    let (ctype, inline) = served_type(&a);
    let range = match headers.get(header::RANGE).and_then(|v| v.to_str().ok()) {
        Some(v) => match parse_range(v, size) {
            Ok(r) => r,
            Err(()) => {
                let mut resp = StatusCode::RANGE_NOT_SATISFIABLE.into_response();
                if let Ok(v) = HeaderValue::from_str(&format!("bytes */{size}")) {
                    resp.headers_mut().insert(header::CONTENT_RANGE, v);
                }
                return Ok(resp);
            }
        },
        None => None,
    };
    let mut resp = match range {
        Some(r) => {
            let r = r.start..r.end.min(r.start + MAX_RANGE);
            let bytes = s.runs.artifact_bytes(ctx.org_id, &a, r.clone()).await?;
            let mut resp = (StatusCode::PARTIAL_CONTENT, bytes).into_response();
            if let Ok(v) = HeaderValue::from_str(&format!("bytes {}-{}/{size}", r.start, r.end - 1)) {
                resp.headers_mut().insert(header::CONTENT_RANGE, v);
            }
            resp
        }
        None if size == 0 => StatusCode::OK.into_response(),
        None => {
            let first_end = size.min(CHUNK);
            let first = s.runs.artifact_bytes(ctx.org_id, &a, 0..first_end).await?;
            let runs = s.runs.clone();
            let org = ctx.org_id;
            let rest = a.clone();
            let stream = async_stream::stream! {
                yield Ok::<_, std::io::Error>(axum::body::Bytes::from(first));
                let mut at = first_end;
                while at < size {
                    let end = size.min(at + CHUNK);
                    match runs.artifact_bytes(org, &rest, at..end).await {
                        Ok(b) => yield Ok(axum::body::Bytes::from(b)),
                        Err(e) => {
                            yield Err(std::io::Error::other(e.to_string()));
                            return;
                        }
                    }
                    at = end;
                }
            };
            let mut resp = Response::new(Body::from_stream(stream));
            if let Ok(v) = HeaderValue::from_str(&size.to_string()) {
                resp.headers_mut().insert(header::CONTENT_LENGTH, v);
            }
            resp
        }
    };
    let h = resp.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(ctype));
    h.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("private, max-age=300"));
    if !inline && let Ok(v) = HeaderValue::from_str(&format!("attachment; filename=\"{}\"", a.name)) {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    Ok(resp)
}

async fn start_diagnose(
    State(s): State<RunsApiState>,
    ctx: OrgCtx,
    Path(p): Params,
    b: Result<Json<StartHarnessRunRequest>, JsonRejection>,
) -> Result<Json<RunStarted>, AppError> {
    ctx.require(MemberRole::Member)?;
    let ticket: TicketId = param(&p, "ticket")?;
    let cause: CauseId = param(&p, "cause")?;
    let req = body(b)?;
    let rec = s.runs.start_diagnose(ctx.org_id, ticket, cause, req.harness, ctx.user_id).await?;
    Ok(Json(RunStarted { run_id: rec.id() }))
}

async fn start_learn(
    State(s): State<RunsApiState>,
    ctx: OrgCtx,
    Path(p): Params,
    b: Result<Json<StartLearnRequest>, JsonRejection>,
) -> Result<Json<RunStarted>, AppError> {
    ctx.require(MemberRole::Admin)?;
    let repo: RepoId = param(&p, "repo")?;
    let req = body(b)?;
    let rec = s.runs.start_learn(ctx.org_id, repo, req.sha, req.harness, ctx.user_id).await?;
    Ok(Json(RunStarted { run_id: rec.id() }))
}

async fn verify_recipe(
    State(s): State<RunsApiState>,
    ctx: OrgCtx,
    Path(p): Params,
    b: Result<Json<VerifyRecipeRequest>, JsonRejection>,
) -> Result<Json<RunStarted>, AppError> {
    ctx.require(MemberRole::Admin)?;
    let repo: RepoId = param(&p, "repo")?;
    let req = body(b)?;
    let rec = s.runs.verify_recipe(ctx.org_id, repo, req.hash, req.sha, ctx.user_id).await?;
    Ok(Json(RunStarted { run_id: rec.id() }))
}

#[cfg(test)]
mod tests {
    use super::parse_range;

    #[test]
    fn ranges_follow_rfc_9110() {
        assert_eq!(parse_range("bytes=0-9", 100), Ok(Some(0..10)));
        assert_eq!(parse_range("bytes=90-", 100), Ok(Some(90..100)));
        assert_eq!(parse_range("bytes=-10", 100), Ok(Some(90..100)));
        assert_eq!(parse_range("bytes=95-200", 100), Ok(Some(95..100)));
        assert_eq!(parse_range("bytes=100-", 100), Err(()));
        assert_eq!(parse_range("bytes=5-1", 100), Err(()));
        assert_eq!(parse_range("items=0-1", 100), Ok(None));
        assert_eq!(parse_range("bytes=0-1,5-6", 100), Ok(None));
    }
}
