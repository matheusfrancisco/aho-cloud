//! UI API of the fix path (server.md endpoint catalog; fix.md F1–F3):
//!
//! | Route | Use case | Role |
//! |---|---|---|
//! | `GET CAUSE_FIX_BRIEF?version=` | `GetFixBrief` (markdown for view / Copy, signed `.md` download) | Member |
//! | `POST CAUSE_FIX` `{harness?}` | `StartFixRun` ("Let Mage fix") | Member |
//! | `POST CAUSE_PULL_REQUEST` | `RequestPullRequest` (enqueue `fix.open_pr`) | Member |
//!
//! The gated patch is downloaded through the generic `ARTIFACT_CONTENT` route (Patch artifacts
//! are served as `text/x-diff` attachments). Session, `{org}` → `OrgCtx` and CSRF are Platform
//! middleware.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Path, Query, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use mage_core::domain::{CauseId, MemberRole, TicketId};
use mage_core::wire::api::{FixBriefQuery, FixBriefView, JobQueued, RunStarted, StartHarnessRunRequest, routes};

use crate::adapters::driving::http::auth::OrgCtx;
use crate::app::fix::{FixDeps, GetFixBrief, RequestPullRequest, StartFix, StartFixRun};
use crate::error::AppError;

#[derive(Clone)]
pub struct FixApiState {
    pub brief: GetFixBrief,
    pub start: StartFixRun,
    pub request_pr: RequestPullRequest,
}

impl FixApiState {
    pub fn new(d: Arc<FixDeps>) -> Self {
        Self { brief: GetFixBrief::new(d.clone()), start: StartFixRun::new(d.clone()), request_pr: RequestPullRequest::new(d) }
    }
}

pub fn router(state: FixApiState) -> Router {
    Router::new()
        .route(routes::CAUSE_FIX_BRIEF, get(fix_brief))
        .route(routes::CAUSE_FIX, post(start_fix))
        .route(routes::CAUSE_PULL_REQUEST, post(open_pull_request))
        .with_state(state)
}

type CausePath = Path<(String, TicketId, CauseId)>;

async fn fix_brief(
    State(s): State<FixApiState>,
    ctx: OrgCtx,
    Path((_, ticket, cause)): CausePath,
    q: Result<Query<FixBriefQuery>, QueryRejection>,
) -> Result<Json<FixBriefView>, AppError> {
    ctx.require(MemberRole::Member)?;
    let Query(q) = q.map_err(|e| AppError::Invalid(e.body_text()))?;
    Ok(Json(s.brief.run(ctx.org_id, ticket, cause, q.version).await?))
}

/// The body is optional (`{}` or empty = org default harness).
async fn start_fix(State(s): State<FixApiState>, ctx: OrgCtx, Path((_, ticket, cause)): CausePath, body: Bytes) -> Result<Json<RunStarted>, AppError> {
    ctx.require(MemberRole::Member)?;
    let req: StartHarnessRunRequest =
        if body.iter().all(u8::is_ascii_whitespace) { StartHarnessRunRequest::default() } else { serde_json::from_slice(&body).map_err(|e| AppError::Invalid(e.to_string()))? };
    let run = s
        .start
        .run(StartFix { org: ctx.org_id, ticket, cause, requested_by: ctx.user_id, role: ctx.role, harness: req.harness })
        .await?;
    Ok(Json(RunStarted { run_id: run.id() }))
}

async fn open_pull_request(State(s): State<FixApiState>, ctx: OrgCtx, Path((_, ticket, cause)): CausePath) -> Result<Json<JobQueued>, AppError> {
    ctx.require(MemberRole::Member)?;
    Ok(Json(s.request_pr.run(ctx.org_id, ticket, cause, ctx.role).await?))
}

#[cfg(test)]
mod tests;
