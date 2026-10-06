//! UI API: tickets (server.md endpoint catalog; DTOs in `mage_core::wire::api`): list with
//! filters, detail, target override, resolve. All Member routes; session, `{org}` → `OrgCtx` and
//! CSRF are Platform middleware. `POST …/retriage` is served by `api_triage` (triage's
//! `RequestRetriage`).

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use mage_core::domain::{MemberRole, Resolution, TicketId, TicketTarget};
use mage_core::wire::api::{Paged, ResolveTicketRequest, SetTargetRequest, TicketDetail, TicketListQuery, TicketSummary, routes};

use crate::adapters::driving::http::auth::OrgCtx;
use crate::app::intake::tickets::TicketsApi;
use crate::error::AppError;

#[derive(Clone)]
pub struct TicketsHttpState {
    pub tickets: Arc<TicketsApi>,
}

pub fn router(state: TicketsHttpState) -> Router {
    Router::new()
        .route(routes::TICKETS, get(list))
        .route(routes::TICKET, get(detail))
        .route(routes::TICKET_TARGET, put(set_target))
        .route(routes::TICKET_RESOLVE, post(resolve))
        .with_state(state)
}

async fn list(State(s): State<TicketsHttpState>, ctx: OrgCtx, Query(q): Query<TicketListQuery>) -> Result<Json<Paged<TicketSummary>>, AppError> {
    ctx.require(MemberRole::Member)?;
    Ok(Json(s.tickets.list(ctx.org_id, &q).await?))
}

async fn detail(State(s): State<TicketsHttpState>, ctx: OrgCtx, Path((_, ticket)): Path<(String, TicketId)>) -> Result<Json<TicketDetail>, AppError> {
    ctx.require(MemberRole::Member)?;
    Ok(Json(s.tickets.detail(ctx.org_id, ticket).await?))
}

async fn set_target(
    State(s): State<TicketsHttpState>,
    ctx: OrgCtx,
    Path((_, ticket)): Path<(String, TicketId)>,
    Json(req): Json<SetTargetRequest>,
) -> Result<Json<TicketTarget>, AppError> {
    ctx.require(MemberRole::Member)?;
    Ok(Json(s.tickets.set_target(ctx.org_id, ticket, &req).await?))
}

async fn resolve(
    State(s): State<TicketsHttpState>,
    ctx: OrgCtx,
    Path((_, ticket)): Path<(String, TicketId)>,
    Json(req): Json<ResolveTicketRequest>,
) -> Result<Json<Resolution>, AppError> {
    ctx.require(MemberRole::Member)?;
    Ok(Json(s.tickets.resolve(ctx.org_id, ticket, &req, ctx.user_id).await?))
}

#[cfg(test)]
#[path = "api_tickets_tests.rs"]
mod tests;
