//! UI API: manual retriage (`POST /api/v1/orgs/{org}/tickets/{ticket}/retriage`, role `Member`;
//! triage.md UC5). Session, `{org}` → `OrgCtx` and CSRF are Platform middleware; the cost-cap
//! bypass for Admin/Owner is checked by `RequestRetriage`. Eval commands run from the CLI against
//! the use cases directly and have no HTTP route.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};

use mage_core::domain::{MemberRole, TicketId};
use mage_core::wire::api::{JobQueued, routes};

use crate::adapters::driving::http::auth::OrgCtx;
use crate::app::triage::RequestRetriage;
use crate::error::AppError;

#[derive(Clone)]
pub struct TriageHttpState {
    pub retriage: Arc<RequestRetriage>,
}

pub fn router(state: TriageHttpState) -> Router {
    Router::new().route(routes::TICKET_RETRIAGE, post(retriage)).with_state(state)
}

async fn retriage(
    State(s): State<TriageHttpState>,
    ctx: OrgCtx,
    Path((_, ticket)): Path<(String, TicketId)>,
) -> Result<(StatusCode, Json<JobQueued>), AppError> {
    ctx.require(MemberRole::Member)?;
    let enqueued = s.retriage.run(ctx.org_id, ticket, ctx.user_id).await?;
    Ok((StatusCode::ACCEPTED, Json(JobQueued { job_id: enqueued.job_id() })))
}

#[cfg(test)]
#[path = "api_triage_tests.rs"]
mod tests;
