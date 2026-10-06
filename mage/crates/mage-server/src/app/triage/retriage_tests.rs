//! RequestRetriage: allowed statuses, target requirement, cost-cap role bypass.

use mage_core::domain::{JobKind, JobPayload, MemberRole, Membership, TicketStatus, TriageTrigger, UserId};
use mage_core::ports::jobs::Enqueued;

use super::*;
use crate::app::triage::testkit::*;
use crate::ports::org::OrgStore;

fn user(n: u128) -> UserId {
    UserId::from_uuid(uuid(n))
}

#[tokio::test]
async fn enqueues_a_manual_triage_for_the_current_content() {
    for status in RETRIAGE_FROM {
        let w = World::new().await;
        w.put_ticket(&ticket(status, hash(4), Some(hash(3)), None)).await;
        let enqueued = w.retriage().run(org(), ticket_id(), user(5)).await.unwrap();
        assert!(matches!(enqueued, Enqueued::New(_)), "{status:?}");
        let jobs = w.queued(JobKind::TriageTicket);
        assert_eq!(
            jobs[0].payload,
            JobPayload::TriageTicket { org_id: org(), ticket_id: ticket_id(), content_hash: hash(4), trigger: TriageTrigger::Manual { by: user(5) } }
        );
        // A second click while the job is pending returns the same job.
        assert_eq!(w.retriage().run(org(), ticket_id(), user(6)).await.unwrap(), Enqueued::Duplicate(enqueued.job_id()));
    }
}

#[tokio::test]
async fn statuses_after_triage_are_refused() {
    for status in [TicketStatus::Diagnosing, TicketStatus::Diagnosed, TicketStatus::Fixing, TicketStatus::Resolved, TicketStatus::Closed] {
        let w = World::new().await;
        w.put_ticket(&ticket(status, hash(1), None, None)).await;
        let err = w.retriage().run(org(), ticket_id(), user(5)).await.unwrap_err();
        assert!(matches!(err, AppError::Conflict(_)), "{status:?}: {err:?}");
        assert!(w.queued(JobKind::TriageTicket).is_empty());
    }
}

#[tokio::test]
async fn a_ticket_without_target_cannot_be_retriaged() {
    let w = World::new().await;
    let mut t = ticket(TicketStatus::Triaged, hash(1), None, None).into_data();
    t.target = None;
    w.put_ticket(&mage_core::domain::Ticket::rehydrate(t)).await;
    assert!(matches!(w.retriage().run(org(), ticket_id(), user(5)).await, Err(AppError::Invalid(_))));
}

#[tokio::test]
async fn over_the_cost_cap_only_admins_and_owners_may_retriage() {
    let w = World::new().await;
    w.put_ticket(&ticket(TicketStatus::Triaged, hash(2), Some(hash(1)), None)).await;
    w.set_settings(|s| s.ticket_cost_cap_micros = 500).await;
    let run = w.seed_run(hash(1), TriageTrigger::Auto).await.run.id;
    w.set_cost(run, 500).await;
    for (n, role) in [(5, MemberRole::Member), (6, MemberRole::Admin), (7, MemberRole::Owner)] {
        w.h.orgs.put_membership(&Membership { org_id: org(), user_id: user(n), role }).await.unwrap();
    }
    assert!(matches!(w.retriage().run(org(), ticket_id(), user(5)).await, Err(AppError::Forbidden(_))));
    assert!(w.retriage().run(org(), ticket_id(), user(6)).await.is_ok());
    assert!(w.retriage().run(org(), ticket_id(), user(7)).await.is_ok());
}
