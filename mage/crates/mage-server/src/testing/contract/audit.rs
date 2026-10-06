//! `AuditLog` contract (server.md "Testing": append-only; `list` order and filters; org isolation).

use std::borrow::Cow;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use mage_core::domain::{OrgId, RunnerId, UserId};
use mage_core::ports::clock::Clock;
use mage_core::testing::FakeClock;
use uuid::Uuid;

use crate::ports::audit::{Actor, AuditAction, AuditEvent, AuditFilter, AuditLog, AuditOutcome, AuditRecord, AuditTarget};

pub struct AuditFixture {
    pub audit: Arc<dyn AuditLog>,
}

/// Runs every `AuditLog` scenario, each against a fresh fixture from `make`.
pub async fn contract_audit_log<F, Fut>(make: F)
where
    F: Fn(Arc<FakeClock>) -> Fut,
    Fut: Future<Output = AuditFixture>,
{
    macro_rules! scenario {
        ($name:ident) => {{
            let clock = Arc::new(FakeClock::new());
            let fixture = make(clock.clone()).await;
            $name(&clock, &*fixture.audit).await;
        }};
    }
    scenario!(records_round_trip_newest_first);
    scenario!(filters_page_and_limit);
    scenario!(orgs_and_platform_rows_are_isolated);
}

fn org(n: u128) -> OrgId {
    OrgId::from_uuid(Uuid::from_u128(0x0a00 + n))
}

fn user(n: u128) -> UserId {
    UserId::from_uuid(Uuid::from_u128(0x0b00 + n))
}

fn event(org: Option<OrgId>, actor: Actor, action: AuditAction) -> AuditEvent {
    AuditEvent {
        org,
        actor,
        action,
        target: AuditTarget::new("member", "usr_1"),
        outcome: AuditOutcome::Success,
        ip: None,
        request_id: None,
        details: Vec::new(),
    }
}

async fn records_round_trip_newest_first(clock: &FakeClock, audit: &dyn AuditLog) {
    let first = AuditEvent {
        org: Some(org(1)),
        actor: Actor::User(user(1)),
        action: AuditAction::ByokKeySet,
        target: AuditTarget::new("secret", "model/anthropic_api_key"),
        outcome: AuditOutcome::Success,
        ip: Some("203.0.113.7".parse::<IpAddr>().unwrap()),
        request_id: Some("req-1".into()),
        details: vec![(Cow::Borrowed("fingerprint"), "sk-ant-…a1b2".into()), (Cow::Borrowed("version"), "3".into())],
    };
    let t1 = clock.now();
    let id1 = audit.record(first.clone()).await.expect("record");
    clock.advance(Duration::from_secs(5));
    let second = AuditEvent {
        actor: Actor::Runner(RunnerId::from_uuid(Uuid::from_u128(77))),
        action: AuditAction::RunnerRegistered,
        target: AuditTarget::new("runner", "rnr_x"),
        outcome: AuditOutcome::Denied,
        ip: Some("2001:db8::1".parse::<IpAddr>().unwrap()),
        request_id: None,
        details: vec![(Cow::Borrowed("z"), "last".into()), (Cow::Borrowed("a"), "first".into())],
        ..first.clone()
    };
    let id2 = audit.record(second.clone()).await.expect("record");
    let third = AuditEvent { actor: Actor::Operator { os_user: "ops".into() }, outcome: AuditOutcome::Failed, ..event(Some(org(1)), Actor::System, AuditAction::KeysRotated) };
    let id3 = audit.record(third.clone()).await.expect("record");
    assert!(id1 < id2 && id2 < id3, "ids increase: {id1} {id2} {id3}");

    let listed = audit.list(org(1), AuditFilter::default(), 10).await.unwrap();
    let t2 = t1 + Duration::from_secs(5);
    assert_eq!(listed, vec![
        AuditRecord { id: id3, at: t2, event: third },
        AuditRecord { id: id2, at: t2, event: second },
        AuditRecord { id: id1, at: t1, event: first },
    ], "every field round-trips (detail order kept), newest first");
}

async fn filters_page_and_limit(_clock: &FakeClock, audit: &dyn AuditLog) {
    let alice = Actor::User(user(1));
    let bob = Actor::User(user(2));
    let mut ids = Vec::new();
    for (actor, action) in [
        (&alice, AuditAction::AuthLogin),
        (&bob, AuditAction::AuthLogin),
        (&alice, AuditAction::SettingsChanged),
        (&alice, AuditAction::AuthLogin),
        (&Actor::System, AuditAction::AuthLogin),
    ] {
        ids.push(audit.record(event(Some(org(1)), actor.clone(), action)).await.unwrap());
    }
    let listed_ids = |records: Vec<AuditRecord>| records.into_iter().map(|r| r.id).collect::<Vec<_>>();

    let logins = audit.list(org(1), AuditFilter { action: Some(AuditAction::AuthLogin), ..AuditFilter::default() }, 50).await.unwrap();
    assert_eq!(listed_ids(logins), vec![ids[4], ids[3], ids[1], ids[0]]);
    let by_alice = audit.list(org(1), AuditFilter { actor: Some(alice.clone()), ..AuditFilter::default() }, 50).await.unwrap();
    assert_eq!(listed_ids(by_alice), vec![ids[3], ids[2], ids[0]]);
    let by_system = audit.list(org(1), AuditFilter { actor: Some(Actor::System), ..AuditFilter::default() }, 50).await.unwrap();
    assert_eq!(listed_ids(by_system), vec![ids[4]], "an id-less actor matches only itself");
    let both = AuditFilter { action: Some(AuditAction::AuthLogin), actor: Some(alice.clone()), before: None };
    assert_eq!(listed_ids(audit.list(org(1), both, 50).await.unwrap()), vec![ids[3], ids[0]]);

    // `before` pages by id, newest first.
    let page1 = audit.list(org(1), AuditFilter::default(), 2).await.unwrap();
    assert_eq!(listed_ids(page1.clone()), vec![ids[4], ids[3]]);
    let page2 = audit.list(org(1), AuditFilter { before: page1.last().map(|r| r.id), ..AuditFilter::default() }, 2).await.unwrap();
    assert_eq!(listed_ids(page2.clone()), vec![ids[2], ids[1]]);
    let page3 = audit.list(org(1), AuditFilter { before: page2.last().map(|r| r.id), ..AuditFilter::default() }, 2).await.unwrap();
    assert_eq!(listed_ids(page3), vec![ids[0]]);

    assert_eq!(audit.list(org(1), AuditFilter::default(), 0).await.unwrap().len(), 1, "limit 0 is clamped to 1");
}

async fn orgs_and_platform_rows_are_isolated(_clock: &FakeClock, audit: &dyn AuditLog) {
    let a = audit.record(event(Some(org(1)), Actor::System, AuditAction::OrgCreated)).await.unwrap();
    audit.record(event(Some(org(2)), Actor::System, AuditAction::OrgCreated)).await.unwrap();
    audit.record(event(None, Actor::Operator { os_user: "root".into() }, AuditAction::TokenRevoked)).await.unwrap();
    let only_a = audit.list(org(1), AuditFilter::default(), 50).await.unwrap();
    assert_eq!(only_a.iter().map(|r| r.id).collect::<Vec<_>>(), vec![a], "other orgs' and platform rows are not listed");
    assert!(audit.list(org(3), AuditFilter::default(), 50).await.unwrap().is_empty());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::MemAuditLog;

    #[tokio::test]
    async fn mem_audit_log_passes_the_contract() {
        contract_audit_log(|clock| async move { AuditFixture { audit: Arc::new(MemAuditLog::new(clock)) } }).await;
    }
}
