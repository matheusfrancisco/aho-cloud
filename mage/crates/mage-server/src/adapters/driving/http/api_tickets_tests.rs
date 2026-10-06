//! Tickets routes through the router: JSON shapes, path/body parsing, error statuses.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use mage_core::domain::{MemberRole, TargetOrigin, UserId};
use mage_core::wire::api::{Paged, TicketDetail, TicketSummary};
use tower::ServiceExt;
use uuid::Uuid;

use super::*;
use crate::adapters::driving::http::auth::OrgCtx;
use crate::app::intake::testkit::{Kit, OTHER, sha};
use crate::ports::ticket::TicketStore;

fn app(kit: &Kit) -> Router {
    let ctx = OrgCtx { org_id: kit.org, user_id: UserId::from_uuid(Uuid::from_u128(0xa1)), role: MemberRole::Member };
    router(TicketsHttpState { tickets: Arc::new(kit.tickets_api()) }).layer(axum::Extension(ctx))
}

async fn body<T: serde::de::DeserializeOwned>(res: axum::response::Response) -> T {
    serde_json::from_slice(&axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap()).unwrap()
}

#[tokio::test]
async fn list_detail_target_resolve_round_trip() {
    let kit = Kit::new().await;
    kit.tracker.put_issue(kit.org, kit.issue("h1"));
    kit.ingest().run(kit.org, "h1").await.unwrap();
    let id = kit.tickets.get_by_linear_issue(kit.org, "h1").await.unwrap().unwrap().value.id;

    let res = app(&kit).oneshot(Request::get("/api/v1/orgs/acme/tickets?status=new&limit=10").body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let page: Paged<TicketSummary> = body(res).await;
    assert_eq!(page.items.iter().map(|s| s.id).collect::<Vec<_>>(), vec![id]);

    let res = app(&kit).oneshot(Request::get(format!("/api/v1/orgs/acme/tickets/{id}")).body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let detail: TicketDetail = body(res).await;
    assert!(detail.assumed);

    let put = serde_json::json!({ "repo_id": kit.repo.id, "sha": sha(OTHER) });
    let req = Request::put(format!("/api/v1/orgs/acme/tickets/{id}/target")).header("content-type", "application/json").body(Body::from(put.to_string())).unwrap();
    let res = app(&kit).oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let target: TicketTarget = body(res).await;
    assert_eq!(target.origin, TargetOrigin::Override);

    // Unknown fields are refused (deny_unknown_fields) before the use case runs.
    let req = Request::post(format!("/api/v1/orgs/acme/tickets/{id}/resolve"))
        .header("content-type", "application/json")
        .body(Body::from(r#"{"root_cause":"x","extra":1}"#))
        .unwrap();
    assert!(app(&kit).oneshot(req).await.unwrap().status().is_client_error());
    let req = Request::post(format!("/api/v1/orgs/acme/tickets/{id}/resolve"))
        .header("content-type", "application/json")
        .body(Body::from(r#"{"root_cause":"Token TTL"}"#))
        .unwrap();
    let res = app(&kit).oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let resolution: Resolution = body(res).await;
    assert_eq!(resolution.ticket_id, id);
}

#[tokio::test]
async fn errors_map_to_statuses() {
    let kit = Kit::new().await;
    let missing = mage_core::domain::TicketId::from_uuid(Uuid::from_u128(0x404));
    let res = app(&kit).oneshot(Request::get(format!("/api/v1/orgs/acme/tickets/{missing}")).body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    let res = app(&kit).oneshot(Request::get("/api/v1/orgs/acme/tickets/not-a-ticket").body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    // Without the OrgCtx the platform middleware would have set, the org is unknown.
    let bare = router(TicketsHttpState { tickets: Arc::new(kit.tickets_api()) });
    let res = bare.oneshot(Request::get("/api/v1/orgs/acme/tickets").body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}
