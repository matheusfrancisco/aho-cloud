//! `OutboundLog` contract (intake.md "`OutboundLog`", "Port contract tests"): one row per
//! (ticket, target, notice), concurrent `begin` agree on one client id, the pending → sent /
//! skipped / failed state machine, ordering and tenancy.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use futures::future::join_all;
use mage_core::domain::{OrgId, TicketId};
use mage_core::ports::clock::Clock;
use mage_core::testing::FakeClock;
use uuid::Uuid;

use crate::ports::outbound::{OutboundError, OutboundKey, OutboundLog, OutboundRecord, OutboundState, OutboundTarget};
use crate::testing::seed::Seed;

/// A fresh, empty outbound log.
pub struct OutboundFixture {
    pub log: Arc<dyn OutboundLog>,
    pub seed: Arc<dyn Seed>,
}

/// Runs every `OutboundLog` scenario, each against a fresh fixture from `make`.
pub async fn contract_outbound_log<F, Fut>(make: F)
where
    F: Fn(Arc<FakeClock>) -> Fut,
    Fut: Future<Output = OutboundFixture>,
{
    macro_rules! scenario {
        ($name:ident) => {{
            let clock = Arc::new(FakeClock::new());
            let fixture = make(clock.clone()).await;
            $name(&clock, &fixture).await;
        }};
    }
    scenario!(begin_inserts_once_and_keeps_the_first_client_id);
    scenario!(concurrent_begins_agree_on_one_client_id);
    scenario!(attempts_count_only_while_pending);
    scenario!(sent_is_final_and_found_by_external_id);
    scenario!(skipped_and_failed_keep_their_reason);
    scenario!(list_for_ticket_is_oldest_first);
    scenario!(rows_are_tenant_scoped);
}

fn org(n: u128) -> OrgId {
    OrgId::from_uuid(Uuid::from_u128(0x0b00 + n))
}

fn ticket(n: u128) -> TicketId {
    TicketId::from_uuid(Uuid::from_u128(0x0b10 + n))
}

fn client(n: u128) -> Uuid {
    Uuid::from_u128(0x0b20 + n)
}

fn key(ticket: TicketId, target: OutboundTarget, notice: &str) -> OutboundKey {
    OutboundKey { ticket_id: ticket, target, notice_key: notice.into() }
}

async fn seed(f: &OutboundFixture, org: OrgId, tickets: &[TicketId]) {
    f.seed.org(org).await;
    for t in tickets {
        f.seed.ticket(org, *t).await;
    }
}

/// The stored row of `key` (read through `list_for_ticket`).
async fn record(f: &OutboundFixture, org: OrgId, key: &OutboundKey) -> Option<OutboundRecord> {
    f.log.list_for_ticket(org, key.ticket_id).await.expect("list_for_ticket").into_iter().find(|r| r.key == *key)
}

async fn begin_inserts_once_and_keeps_the_first_client_id(clock: &FakeClock, f: &OutboundFixture) {
    seed(f, org(1), &[ticket(1)]).await;
    let k = key(ticket(1), OutboundTarget::LinearComment, "triaged:v1");
    let t0 = clock.now();
    let first = f.log.begin(org(1), &k, client(1), [1; 32]).await.unwrap();
    let pending = OutboundRecord {
        key: k.clone(),
        client_id: client(1),
        state: OutboundState::Pending { uncertain: false },
        attempts: 0,
        body_sha256: [1; 32],
        external_id: None,
        last_error: None,
        created_at: t0,
        sent_at: None,
    };
    assert_eq!(first, pending);

    clock.advance(Duration::from_secs(30));
    assert_eq!(f.log.begin(org(1), &k, client(2), [2; 32]).await, Ok(pending.clone()), "a retry gets the stored row, not its own values");
    assert_eq!(f.log.list_for_ticket(org(1), ticket(1)).await, Ok(vec![pending]), "one row per key");
}

async fn concurrent_begins_agree_on_one_client_id(_clock: &FakeClock, f: &OutboundFixture) {
    seed(f, org(1), &[ticket(1)]).await;
    let k = key(ticket(1), OutboundTarget::SlackReply, "fix_ready");
    let candidates: BTreeSet<Uuid> = (1..=8).map(client).collect();
    let results = join_all(candidates.iter().map(|c| f.log.begin(org(1), &k, *c, [7; 32]))).await;
    let ids: BTreeSet<Uuid> = results.into_iter().map(|r| r.expect("begin").client_id).collect();
    assert_eq!(ids.len(), 1, "every concurrent begin returns the same client id: {ids:?}");
    let winner = *ids.first().unwrap();
    assert!(candidates.contains(&winner), "the stored client id is one of the offered ones");
    let rows = f.log.list_for_ticket(org(1), ticket(1)).await.unwrap();
    assert_eq!(rows.iter().map(|r| r.client_id).collect::<Vec<_>>(), vec![winner], "exactly one row was stored");
}

async fn attempts_count_only_while_pending(_clock: &FakeClock, f: &OutboundFixture) {
    seed(f, org(1), &[ticket(1)]).await;
    let k = key(ticket(1), OutboundTarget::LinearComment, "triaged:v1");
    f.log.begin(org(1), &k, client(1), [1; 32]).await.unwrap();

    assert_eq!(f.log.mark_attempt(org(1), &k, true, Some("timeout after 20s")).await, Ok(()));
    let r = record(f, org(1), &k).await.unwrap();
    assert_eq!((r.state, r.attempts, r.last_error.as_deref()), (OutboundState::Pending { uncertain: true }, 1, Some("timeout after 20s")));

    assert_eq!(f.log.mark_attempt(org(1), &k, false, None).await, Ok(()));
    let r = record(f, org(1), &k).await.unwrap();
    assert_eq!((r.state, r.attempts, r.last_error), (OutboundState::Pending { uncertain: false }, 2, None), "each attempt replaces the error");

    let missing = key(ticket(1), OutboundTarget::LinearComment, "never-begun");
    assert_eq!(f.log.mark_attempt(org(1), &missing, false, None).await, Err(OutboundError::NotFound));

    f.log.mark_sent(org(1), &k, "comment-1", FakeClock::DEFAULT_START).await.unwrap();
    assert_eq!(f.log.mark_attempt(org(1), &k, true, None).await, Err(OutboundError::Conflict), "attempts after a final state");
    assert_eq!(record(f, org(1), &k).await.unwrap().attempts, 2);
}

async fn sent_is_final_and_found_by_external_id(clock: &FakeClock, f: &OutboundFixture) {
    seed(f, org(1), &[ticket(1)]).await;
    let k = key(ticket(1), OutboundTarget::LinearComment, "triaged:v1");
    let begun = f.log.begin(org(1), &k, client(1), [1; 32]).await.unwrap();
    f.log.mark_attempt(org(1), &k, true, Some("connection reset")).await.unwrap();

    clock.advance(Duration::from_secs(10));
    let at = clock.now() - Duration::from_secs(3);
    assert_eq!(f.log.mark_sent(org(1), &k, "comment-1", at).await, Ok(()));
    let sent = OutboundRecord {
        state: OutboundState::Sent,
        attempts: 1,
        external_id: Some("comment-1".into()),
        last_error: Some("connection reset".into()),
        sent_at: Some(at),
        ..begun
    };
    assert_eq!(record(f, org(1), &k).await, Some(sent.clone()), "sent_at is the caller's time");

    assert_eq!(f.log.mark_sent(org(1), &k, "comment-1", clock.now()).await, Ok(()), "the same send again is a no-op");
    assert_eq!(record(f, org(1), &k).await, Some(sent.clone()));
    assert_eq!(f.log.mark_sent(org(1), &k, "comment-2", at).await, Err(OutboundError::Conflict));
    assert_eq!(f.log.mark_skipped(org(1), &k, "too late").await, Err(OutboundError::Conflict));
    assert_eq!(f.log.mark_failed(org(1), &k, "too late").await, Err(OutboundError::Conflict));
    assert_eq!(record(f, org(1), &k).await, Some(sent.clone()), "rejected transitions change nothing");

    assert_eq!(f.log.find_by_external(org(1), OutboundTarget::LinearComment, "comment-1").await, Ok(Some(sent)));
    assert_eq!(f.log.find_by_external(org(1), OutboundTarget::SlackReply, "comment-1").await, Ok(None), "external ids are per target");
    assert_eq!(f.log.find_by_external(org(1), OutboundTarget::LinearComment, "comment-2").await, Ok(None));

    let missing = key(ticket(1), OutboundTarget::SlackReply, "triaged:v1");
    assert_eq!(f.log.mark_sent(org(1), &missing, "1700000000.000100", at).await, Err(OutboundError::NotFound));
}

async fn skipped_and_failed_keep_their_reason(_clock: &FakeClock, f: &OutboundFixture) {
    seed(f, org(1), &[ticket(1)]).await;
    let skipped = key(ticket(1), OutboundTarget::SlackReply, "triaged:v1");
    let failed = key(ticket(1), OutboundTarget::LinearComment, "triaged:v1");
    f.log.begin(org(1), &skipped, client(1), [1; 32]).await.unwrap();
    f.log.begin(org(1), &failed, client(2), [2; 32]).await.unwrap();

    assert_eq!(f.log.mark_skipped(org(1), &skipped, "no slack thread").await, Ok(()));
    let r = record(f, org(1), &skipped).await.unwrap();
    assert_eq!((r.state, r.last_error.as_deref()), (OutboundState::Skipped { reason: "no slack thread".into() }, Some("no slack thread")));
    assert_eq!(f.log.mark_skipped(org(1), &skipped, "no slack thread").await, Ok(()), "the same skip again is a no-op");
    assert_eq!(f.log.mark_skipped(org(1), &skipped, "another reason").await, Err(OutboundError::Conflict));
    assert_eq!(f.log.mark_failed(org(1), &skipped, "no slack thread").await, Err(OutboundError::Conflict));
    assert_eq!(f.log.mark_sent(org(1), &skipped, "1700000000.000100", FakeClock::DEFAULT_START).await, Err(OutboundError::Conflict));
    assert_eq!(f.log.mark_attempt(org(1), &skipped, false, None).await, Err(OutboundError::Conflict));

    f.log.mark_attempt(org(1), &failed, true, Some("502 from Linear")).await.unwrap();
    assert_eq!(f.log.mark_failed(org(1), &failed, "gave up after 5 attempts").await, Ok(()));
    let r = record(f, org(1), &failed).await.unwrap();
    assert_eq!(
        (r.state, r.attempts, r.last_error.as_deref()),
        (OutboundState::Failed { reason: "gave up after 5 attempts".into() }, 1, Some("gave up after 5 attempts"))
    );
    assert_eq!(f.log.mark_failed(org(1), &failed, "gave up after 5 attempts").await, Ok(()), "the same failure again is a no-op");
    assert_eq!(f.log.mark_failed(org(1), &failed, "other").await, Err(OutboundError::Conflict));
    assert_eq!(f.log.mark_skipped(org(1), &failed, "gave up after 5 attempts").await, Err(OutboundError::Conflict));

    let missing = key(ticket(1), OutboundTarget::LinearComment, "fix_ready");
    assert_eq!(f.log.mark_skipped(org(1), &missing, "x").await, Err(OutboundError::NotFound));
    assert_eq!(f.log.mark_failed(org(1), &missing, "x").await, Err(OutboundError::NotFound));
}

async fn list_for_ticket_is_oldest_first(clock: &FakeClock, f: &OutboundFixture) {
    seed(f, org(1), &[ticket(1), ticket(2)]).await;
    let first = key(ticket(1), OutboundTarget::LinearComment, "triaged:v1");
    let second = key(ticket(1), OutboundTarget::SlackReply, "triaged:v1");
    let third = key(ticket(1), OutboundTarget::LinearComment, "fix_ready");
    let other = key(ticket(2), OutboundTarget::LinearComment, "triaged:v1");
    clock.advance(Duration::from_secs(60));
    f.log.begin(org(1), &third, client(3), [3; 32]).await.unwrap();
    f.log.begin(org(1), &other, client(4), [4; 32]).await.unwrap();
    clock.set(FakeClock::DEFAULT_START);
    f.log.begin(org(1), &first, client(1), [1; 32]).await.unwrap();
    f.log.begin(org(1), &second, client(2), [2; 32]).await.unwrap();

    let keys: Vec<OutboundKey> = f.log.list_for_ticket(org(1), ticket(1)).await.unwrap().into_iter().map(|r| r.key).collect();
    assert_eq!(keys, vec![first, second, third], "by created_at, then creation order within one instant");
    assert_eq!(f.log.list_for_ticket(org(1), ticket(3)).await, Ok(vec![]));
}

async fn rows_are_tenant_scoped(_clock: &FakeClock, f: &OutboundFixture) {
    seed(f, org(1), &[ticket(1)]).await;
    seed(f, org(2), &[ticket(1)]).await;
    let k = key(ticket(1), OutboundTarget::LinearComment, "triaged:v1");
    f.log.begin(org(1), &k, client(1), [1; 32]).await.unwrap();
    f.log.mark_sent(org(1), &k, "comment-1", FakeClock::DEFAULT_START).await.unwrap();

    assert_eq!(f.log.list_for_ticket(org(2), ticket(1)).await, Ok(vec![]));
    assert_eq!(f.log.find_by_external(org(2), OutboundTarget::LinearComment, "comment-1").await, Ok(None));
    assert_eq!(f.log.mark_attempt(org(2), &k, false, None).await, Err(OutboundError::NotFound));
    assert_eq!(f.log.mark_sent(org(2), &k, "comment-1", FakeClock::DEFAULT_START).await, Err(OutboundError::NotFound));
    assert_eq!(f.log.mark_skipped(org(2), &k, "x").await, Err(OutboundError::NotFound));
    assert_eq!(f.log.mark_failed(org(2), &k, "x").await, Err(OutboundError::NotFound));

    let theirs = f.log.begin(org(2), &k, client(2), [2; 32]).await.unwrap();
    assert_eq!((theirs.client_id, theirs.state), (client(2), OutboundState::Pending { uncertain: false }), "the same key in another org is another row");
    assert_eq!(record(f, org(1), &k).await.map(|r| (r.client_id, r.state)), Some((client(1), OutboundState::Sent)));
}

#[cfg(test)]
mod tests {
    use mage_core::testing::SeqIdGen;

    use super::*;
    use crate::testing::{MemOutboundLog, NoSeed};

    #[tokio::test]
    async fn mem_outbound_log_passes_the_contract() {
        contract_outbound_log(|clock| async move {
            OutboundFixture { log: Arc::new(MemOutboundLog::new(clock, Arc::new(SeqIdGen::new()))), seed: Arc::new(NoSeed) }
        })
        .await;
    }
}
