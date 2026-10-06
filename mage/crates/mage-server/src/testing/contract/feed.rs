//! `ChangeFeed` contract (server.md "Testing": entries appear in commit order; `after` pagination;
//! a subscriber receives notices for its org only; a forced listener drop yields `Lagged`).

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use futures::future::BoxFuture;
use mage_core::domain::OrgId;
use mage_core::ports::clock::Clock;
use mage_core::testing::{FakeClock, join_all};
use uuid::Uuid;

use crate::ports::feed::{ChangeFeed, FEED_PAGE_MAX, FeedEntry, FeedError, FeedId, FeedKind, FeedNotice, NoticeStream};
use crate::testing::seed::Seed;

/// How long a scenario waits for a notice that must arrive.
const NOTICE_WAIT: Duration = Duration::from_secs(10);

pub struct FeedFixture {
    pub feed: Arc<dyn ChangeFeed>,
    pub seed: Arc<dyn Seed>,
    /// Breaks the notification source (pg: kills the LISTEN connection) and returns once notices
    /// flow again.
    pub drop_listener: Arc<dyn Fn() -> BoxFuture<'static, ()> + Send + Sync>,
    /// Keeps background listeners alive for the scenario (dropped with the fixture).
    pub keep_alive: Option<Box<dyn std::any::Any + Send + Sync>>,
}

/// Runs every `ChangeFeed` scenario, each against a fresh fixture from `make`.
pub async fn contract_change_feed<F, Fut>(make: F)
where
    F: Fn(Arc<FakeClock>) -> Fut,
    Fut: Future<Output = FeedFixture>,
{
    macro_rules! scenario {
        ($name:ident) => {{
            let clock = Arc::new(FakeClock::new());
            let fixture = make(clock.clone()).await;
            $name(&clock, &fixture).await;
        }};
    }
    scenario!(ids_are_per_org_and_after_pages_in_order);
    scenario!(concurrent_appends_leave_no_gaps);
    scenario!(subscribers_see_only_their_org);
    scenario!(a_dropped_listener_reports_lagged_then_recovers);
}

fn org(n: u128) -> OrgId {
    OrgId::from_uuid(Uuid::from_u128(0x0c00 + n))
}

async fn ids_are_per_org_and_after_pages_in_order(clock: &FakeClock, f: &FeedFixture) {
    let feed = &*f.feed;
    f.seed.org(org(1)).await;
    f.seed.org(org(2)).await;
    assert_eq!(feed.head(org(1)).await, Ok(FeedId(0)), "an empty feed's head is 0");
    assert_eq!(feed.after(org(1), FeedId(0), 10).await, Ok(vec![]));

    let t0 = clock.now();
    assert_eq!(feed.append(org(1), FeedKind::Ticket, "tkt_a").await, Ok(FeedId(1)));
    assert_eq!(feed.append(org(2), FeedKind::Run, "run_b").await, Ok(FeedId(1)), "ids are per org");
    clock.advance(Duration::from_secs(1));
    assert_eq!(feed.append(org(1), FeedKind::Settings, "org_c").await, Ok(FeedId(2)));
    assert_eq!(feed.append(org(1), FeedKind::Member, "usr_d").await, Ok(FeedId(3)));
    assert_eq!(feed.append(org(1), FeedKind::Ticket, "").await, Err(FeedError::Invalid("feed subject is empty".into())));

    let t1 = t0 + Duration::from_secs(1);
    let entry = |id, kind, subject: &str, at| FeedEntry { id: FeedId(id), org: org(1), kind, subject: subject.into(), at };
    let all = vec![entry(1, FeedKind::Ticket, "tkt_a", t0), entry(2, FeedKind::Settings, "org_c", t1), entry(3, FeedKind::Member, "usr_d", t1)];
    assert_eq!(feed.after(org(1), FeedId(0), 100).await, Ok(all.clone()));
    assert_eq!(feed.after(org(1), FeedId(0), 2).await, Ok(all[..2].to_vec()), "limit");
    assert_eq!(feed.after(org(1), FeedId(2), 2).await, Ok(all[2..].to_vec()), "resume after an id");
    assert_eq!(feed.after(org(1), FeedId(3), 2).await, Ok(vec![]));
    assert_eq!(feed.after(org(1), FeedId(0), 0).await.map(|e| e.len()), Ok(1), "limit 0 is clamped to 1");
    assert_eq!(feed.head(org(1)).await, Ok(FeedId(3)));
    assert_eq!(feed.head(org(2)).await, Ok(FeedId(1)));
    assert_eq!(feed.after(org(3), FeedId(0), 10).await, Ok(vec![]), "another org sees nothing");
}

async fn concurrent_appends_leave_no_gaps(_clock: &FakeClock, f: &FeedFixture) {
    let feed = &*f.feed;
    f.seed.org(org(1)).await;
    f.seed.org(org(2)).await;
    let results = join_all((0..24).map(|i| {
        let target = if i % 3 == 0 { org(2) } else { org(1) };
        async move { (target, feed.append(target, FeedKind::Run, &format!("run_{i}")).await) }
    }))
    .await;
    for (target, expected) in [(org(1), 16usize), (org(2), 8)] {
        let mut ids: Vec<i64> = results.iter().filter(|(o, _)| *o == target).map(|(_, r)| r.clone().expect("append").0).collect();
        ids.sort_unstable();
        assert_eq!(ids, (1..=expected as i64).collect::<Vec<_>>(), "ids are dense per org");
        let entries = feed.after(target, FeedId(0), FEED_PAGE_MAX).await.unwrap();
        assert_eq!(entries.iter().map(|e| e.id.0).collect::<Vec<_>>(), ids, "after() returns every committed id in order");
    }
}

/// The next notice, or a panic after [`NOTICE_WAIT`].
async fn next_notice(stream: &mut NoticeStream) -> FeedNotice {
    tokio::time::timeout(NOTICE_WAIT, stream.next()).await.expect("a notice within the wait").expect("the notice stream ended")
}

async fn subscribers_see_only_their_org(_clock: &FakeClock, f: &FeedFixture) {
    let feed = &*f.feed;
    f.seed.org(org(1)).await;
    f.seed.org(org(2)).await;
    let mut a = feed.subscribe(org(1)).await.unwrap();
    let mut b = feed.subscribe(org(2)).await.unwrap();
    feed.append(org(2), FeedKind::Repo, "repo_x").await.unwrap();
    feed.append(org(1), FeedKind::Ticket, "tkt_y").await.unwrap();
    feed.append(org(1), FeedKind::Cause, "cause_z").await.unwrap();
    assert_eq!(next_notice(&mut a).await, FeedNotice::Entry { org: org(1), id: FeedId(1) }, "org 2's entry is filtered out");
    assert_eq!(next_notice(&mut a).await, FeedNotice::Entry { org: org(1), id: FeedId(2) });
    assert_eq!(next_notice(&mut b).await, FeedNotice::Entry { org: org(2), id: FeedId(1) });
}

async fn a_dropped_listener_reports_lagged_then_recovers(_clock: &FakeClock, f: &FeedFixture) {
    let feed = &*f.feed;
    f.seed.org(org(1)).await;
    let mut a = feed.subscribe(org(1)).await.unwrap();
    (f.drop_listener)().await;
    assert_eq!(next_notice(&mut a).await, FeedNotice::Lagged, "subscribers learn that notices may be lost");
    let id = feed.append(org(1), FeedKind::Ticket, "tkt_after").await.unwrap();
    assert_eq!(next_notice(&mut a).await, FeedNotice::Entry { org: org(1), id }, "notices flow again after the reconnect");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{MemChangeFeed, NoSeed};

    #[tokio::test]
    async fn mem_change_feed_passes_the_contract() {
        contract_change_feed(|clock| async move {
            let feed = Arc::new(MemChangeFeed::new(clock));
            let dropper = feed.clone();
            FeedFixture {
                feed,
                seed: Arc::new(NoSeed),
                drop_listener: Arc::new(move || {
                    dropper.drop_listener();
                    Box::pin(async {})
                }),
                keep_alive: None,
            }
        })
        .await;
    }
}
