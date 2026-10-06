//! `POST …/tickets/{ticket}/retriage` through the router.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use mage_core::domain::{JobKind, JobPayload, MemberRole, TicketStatus, TriageTrigger, UserId};
use mage_core::wire::api::JobQueued;

use super::*;
use crate::adapters::driving::http::auth::OrgCtx;
use crate::app::triage::testkit::*;

fn app(w: &World, role: MemberRole) -> Router {
    let ctx = OrgCtx { org_id: org(), user_id: UserId::from_uuid(uuid(5)), role };
    router(TriageHttpState { retriage: Arc::new(w.retriage()) }).layer(axum::Extension(ctx))
}

fn post(ticket: &str) -> Request<Body> {
    Request::post(format!("/api/v1/orgs/acme/tickets/{ticket}/retriage")).header("content-type", "application/json").body(Body::empty()).unwrap()
}

#[tokio::test]
async fn retriage_answers_202_with_the_queued_job() {
    let w = World::new().await;
    w.put_ticket(&ticket(TicketStatus::Triaged, hash(2), Some(hash(1)), None)).await;
    let res = app(&w, MemberRole::Member).oneshot(post(&ticket_id().to_string())).await.unwrap();
    assert_eq!(res.status(), StatusCode::ACCEPTED);
    let body = axum::body::to_bytes(res.into_body(), 1 << 16).await.unwrap();
    let queued: JobQueued = serde_json::from_slice(&body).unwrap();
    let jobs = w.queued(JobKind::TriageTicket);
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].id, queued.job_id);
    assert!(matches!(jobs[0].payload, JobPayload::TriageTicket { trigger: TriageTrigger::Manual { .. }, .. }));
}

#[tokio::test]
async fn retriage_maps_refusals_to_http_errors() {
    let w = World::new().await;
    w.put_ticket(&ticket(TicketStatus::Diagnosing, hash(1), Some(hash(1)), None)).await;
    let res = app(&w, MemberRole::Member).oneshot(post(&ticket_id().to_string())).await.unwrap();
    assert_eq!(res.status(), StatusCode::CONFLICT);

    let res = app(&w, MemberRole::Member).oneshot(post("tkt_not-an-id")).await.unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);

    let missing = mage_core::domain::TicketId::from_uuid(uuid(0x999));
    let res = app(&w, MemberRole::Member).oneshot(post(&missing.to_string())).await.unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}
