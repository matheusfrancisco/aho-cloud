//! Fix routes through the router: JSON shapes, optional body, role and error statuses.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use mage_core::domain::{MemberRole, TicketStatus};
use mage_core::wire::api::{ApiErrorBody, FixBriefView, RunStarted};
use tower::ServiceExt;

use super::*;
use crate::app::fix::tests::{W, world};

fn app(w: &W, role: MemberRole) -> Router {
    let ctx = OrgCtx { org_id: w.org, user_id: w.user, role };
    router(FixApiState::new(w.d.clone())).layer(axum::Extension(ctx))
}

fn cause_path(w: &W, tail: &str) -> String {
    format!("/api/v1/orgs/acme/tickets/{}/causes/{}/{tail}", w.ticket, w.cause)
}

async fn json<T: serde::de::DeserializeOwned>(res: axum::response::Response) -> T {
    serde_json::from_slice(&axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap()).unwrap()
}

#[tokio::test]
async fn get_brief_latest_and_by_version() {
    let w = world().await;
    w.brief().await;
    w.brief().await;
    let res = app(&w, MemberRole::Member).oneshot(Request::get(cause_path(&w, "fix-brief")).body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let view: FixBriefView = json(res).await;
    assert_eq!((view.version, view.latest_version), (2, 2));
    assert!(view.markdown.starts_with("# Fix brief: SUP-1"));
    assert!(!view.artifact_url.is_empty());

    let res = app(&w, MemberRole::Member).oneshot(Request::get(cause_path(&w, "fix-brief?version=1")).body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(json::<FixBriefView>(res).await.version, 1);
    let res = app(&w, MemberRole::Member).oneshot(Request::get(cause_path(&w, "fix-brief?version=7")).body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    let res = app(&w, MemberRole::Member).oneshot(Request::get(cause_path(&w, "fix-brief?version=x")).body(Body::empty()).unwrap()).await.unwrap();
    assert!(res.status().is_client_error(), "{}", res.status());
}

#[tokio::test]
async fn start_fix_accepts_an_empty_body_and_conflicts_on_a_second_start() {
    let w = world().await;
    w.brief().await;
    w.settings(|_| {}).await;
    let post = || Request::post(cause_path(&w, "fix")).body(Body::empty()).unwrap();
    let res = app(&w, MemberRole::Member).oneshot(post()).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let started: RunStarted = json(res).await;
    assert_eq!(w.d.runs.get(w.org, started.run_id).await.unwrap().run.kind, mage_core::domain::RunKind::Fix);
    assert_eq!(w.ticket_status().await, TicketStatus::Fixing);
    let res = app(&w, MemberRole::Member).oneshot(post()).await.unwrap();
    assert_eq!(res.status(), StatusCode::CONFLICT);
    let err: ApiErrorBody = json(res).await;
    assert_eq!(err.error.code, mage_core::wire::api::ApiErrorCode::Conflict);
}

#[tokio::test]
async fn pull_request_needs_a_fix_ready_ticket_and_the_setting() {
    let w = world().await;
    let post = || Request::post(cause_path(&w, "pull-request")).body(Body::empty()).unwrap();
    let res = app(&w, MemberRole::Member).oneshot(post()).await.unwrap();
    assert_eq!(json::<ApiErrorBody>(res).await.error.code, mage_core::wire::api::ApiErrorCode::Invalid, "fix.pull_requests off");
    w.settings(|_| {}).await;
    let res = app(&w, MemberRole::Member).oneshot(post()).await.unwrap();
    assert_eq!(res.status(), StatusCode::CONFLICT, "ticket is diagnosed, not fix ready");
}
