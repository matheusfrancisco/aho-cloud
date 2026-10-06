//! Tickets API use cases: filters and summaries, detail, target override, resolve.

use mage_core::domain::{CommitSha, JobKind, RepoId, TicketStatus};
use mage_core::wire::api::{ResolveTicketRequest, SetTargetRequest, TicketListQuery};
use uuid::Uuid;

use super::*;
use crate::app::intake::testkit::{Kit, OTHER, sha};

async fn two_tickets(kit: &Kit) -> (Ticket, Ticket) {
    let mut a = kit.issue("x1");
    a.title = "Invoice export fails".into();
    kit.tracker.put_issue(kit.org, a);
    let mut b = kit.issue("x22");
    b.title = "Login loop after SSO".into();
    kit.tracker.put_issue(kit.org, b);
    kit.ingest().run(kit.org, "x1").await.unwrap();
    kit.ingest().run(kit.org, "x22").await.unwrap();
    let get = |id: &'static str| async move { kit.tickets.get_by_linear_issue(kit.org, id).await.unwrap().unwrap().value };
    (get("x1").await, get("x22").await)
}

#[tokio::test]
async fn list_filters_and_summarizes() {
    let kit = Kit::new().await;
    let (a, b) = two_tickets(&kit).await;
    let api = kit.tickets_api();
    let all = api.list(kit.org, &TicketListQuery::default()).await.unwrap();
    assert_eq!(all.items.len(), 2);
    let sa = all.items.iter().find(|s| s.id == a.id).unwrap();
    assert_eq!((sa.identifier.as_str(), sa.assumed, sa.top_cause.is_none()), ("ENG-2", true, true));
    kit.tickets.transition(kit.org, b.id, TicketStatus::New, TicketStatus::Triaged, &[]).await.unwrap();
    let triaged = api.list(kit.org, &TicketListQuery { status: Some(TicketStatus::Triaged), ..Default::default() }).await.unwrap();
    assert_eq!(triaged.items.iter().map(|s| s.id).collect::<Vec<_>>(), vec![b.id]);
    let other_repo = api.list(kit.org, &TicketListQuery { repo: Some(RepoId::from_uuid(Uuid::from_u128(0xfff))), ..Default::default() }).await.unwrap();
    assert!(other_repo.items.is_empty());
    let paged = api.list(kit.org, &TicketListQuery { limit: Some(1), ..Default::default() }).await.unwrap();
    assert_eq!(paged.items.len(), 1);
    let next = api.list(kit.org, &TicketListQuery { limit: Some(1), cursor: paged.next_cursor.clone(), ..Default::default() }).await.unwrap();
    assert_eq!(next.items.len(), 1);
    assert_ne!(next.items[0].id, paged.items[0].id);
}

#[tokio::test]
async fn detail_carries_repo_and_assumed() {
    let kit = Kit::new().await;
    let (a, _) = two_tickets(&kit).await;
    let d = kit.tickets_api().detail(kit.org, a.id).await.unwrap();
    assert_eq!((d.ticket.id, d.assumed, d.repo.map(|r| r.id)), (a.id, true, Some(kit.repo.id)));
    assert!(d.causes.is_empty() && d.runs.is_empty() && d.pull_requests.is_empty() && d.resolution.is_none());
    // Another org's id is NotFound.
    let other = mage_core::domain::OrgId::from_uuid(Uuid::from_u128(0x999));
    assert!(matches!(kit.tickets_api().detail(other, a.id).await, Err(AppError::NotFound(_))));
}

#[tokio::test]
async fn target_override_is_validated_pinned_and_reingested() {
    let kit = Kit::new().await;
    let (a, _) = two_tickets(&kit).await;
    kit.drain().await;
    let api = kit.tickets_api();
    // Unknown commit in the repo.
    let bad = SetTargetRequest { repo_id: kit.repo.id, sha: CommitSha::from_bytes([0x99; 20]) };
    assert!(matches!(api.set_target(kit.org, a.id, &bad).await, Err(AppError::Invalid(_))));
    // Unknown repo.
    let missing = SetTargetRequest { repo_id: RepoId::from_uuid(Uuid::from_u128(0x404)), sha: sha(OTHER) };
    assert!(matches!(api.set_target(kit.org, a.id, &missing).await, Err(AppError::NotFound(_))));

    let t = api.set_target(kit.org, a.id, &SetTargetRequest { repo_id: kit.repo.id, sha: sha(OTHER) }).await.unwrap();
    assert_eq!((t.sha, t.origin.clone()), (sha(OTHER), TargetOrigin::Override));
    assert_eq!(kit.tickets.get_ticket(kit.org, a.id).await.unwrap().value.target, Some(t));
    assert_eq!(kit.queued(JobKind::IndexVersion).len(), 1);
    assert_eq!(kit.queued(JobKind::IntakeLinearIssue).len(), 1);
    // The re-ingest keeps the override and updates the hash.
    kit.ingest().run(kit.org, "x1").await.unwrap();
    let after = kit.tickets.get_ticket(kit.org, a.id).await.unwrap().value;
    assert_eq!(after.target.clone().unwrap().sha, sha(OTHER));
    assert_ne!(after.content_hash, a.content_hash);
}

#[tokio::test]
async fn resolve_records_and_closes() {
    let kit = Kit::new().await;
    let (a, _) = two_tickets(&kit).await;
    let api = kit.tickets_api();
    let by = mage_core::domain::UserId::from_uuid(Uuid::from_u128(0xa1));
    let empty = ResolveTicketRequest { confirmed_cause: None, root_cause: "  ".into(), fix_pr_url: None, repro_script: None };
    assert!(matches!(api.resolve(kit.org, a.id, &empty, by).await, Err(AppError::Domain(_))));
    let req = ResolveTicketRequest { root_cause: "Token TTL misconfigured".into(), fix_pr_url: Some("https://github.com/acme/shop/pull/3".into()), ..empty };
    let r = api.resolve(kit.org, a.id, &req, by).await.unwrap();
    assert_eq!((r.resolved_by, r.root_cause.as_str()), (by, "Token TTL misconfigured"));
    assert_eq!(kit.tickets.get_ticket(kit.org, a.id).await.unwrap().value.status, TicketStatus::Resolved);
    assert!(matches!(api.resolve(kit.org, a.id, &req, by).await, Err(AppError::Conflict(_))));
}
