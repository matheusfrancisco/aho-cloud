//! `MemChangeFeed`: in-memory `ChangeFeed`. The fake stores append through [`MemChangeFeed::push`]
//! inside their own write (one lock scope), mirroring the pg stores' same-transaction feed rows.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use async_trait::async_trait;
use futures::StreamExt;
use mage_core::domain::OrgId;
use mage_core::ports::clock::Clock;
use parking_lot::Mutex;
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;

use crate::ports::feed::{ChangeFeed, FEED_PAGE_MAX, FeedEntry, FeedError, FeedId, FeedKind, FeedNotice, NoticeStream};

/// Notices buffered per subscriber before it lags.
const CAPACITY: usize = 1024;

pub struct MemChangeFeed {
    clock: Arc<dyn Clock>,
    state: Mutex<State>,
    notices: broadcast::Sender<FeedNotice>,
}

#[derive(Default)]
struct State {
    entries: BTreeMap<(OrgId, i64), FeedEntry>,
    heads: HashMap<OrgId, i64>,
}

impl MemChangeFeed {
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        let (notices, _) = broadcast::channel(CAPACITY);
        Self { clock, state: Mutex::new(State::default()), notices }
    }

    /// Appends an entry now (fake stores call this inside their write) and notifies subscribers.
    pub fn push(&self, org: OrgId, kind: FeedKind, subject: impl Into<String>) -> FeedId {
        let id = {
            let mut state = self.state.lock();
            let head = state.heads.entry(org).or_insert(0);
            *head += 1;
            let id = *head;
            state.entries.insert((org, id), FeedEntry { id: FeedId(id), org, kind, subject: subject.into(), at: self.clock.now() });
            FeedId(id)
        };
        // No subscriber is not an error.
        let _ = self.notices.send(FeedNotice::Entry { org, id });
        id
    }

    /// Every entry of `org`, oldest first (test inspection).
    pub fn entries(&self, org: OrgId) -> Vec<FeedEntry> {
        self.state.lock().entries.range((org, i64::MIN)..=(org, i64::MAX)).map(|(_, e)| e.clone()).collect()
    }

    /// Simulates a lost listener connection: every subscriber gets `Lagged`.
    pub fn drop_listener(&self) {
        let _ = self.notices.send(FeedNotice::Lagged);
    }
}

#[async_trait]
impl ChangeFeed for MemChangeFeed {
    async fn subscribe(&self, org: OrgId) -> Result<NoticeStream, FeedError> {
        let stream = BroadcastStream::new(self.notices.subscribe()).filter_map(move |item| async move {
            match item {
                Ok(FeedNotice::Entry { org: o, id }) if o == org => Some(FeedNotice::Entry { org: o, id }),
                Ok(FeedNotice::Entry { .. }) => None,
                Ok(FeedNotice::Lagged) | Err(BroadcastStreamRecvError::Lagged(_)) => Some(FeedNotice::Lagged),
            }
        });
        Ok(Box::pin(stream))
    }

    async fn after(&self, org: OrgId, after: FeedId, limit: u32) -> Result<Vec<FeedEntry>, FeedError> {
        let limit = limit.clamp(1, FEED_PAGE_MAX) as usize;
        let state = self.state.lock();
        let from = after.0.saturating_add(1);
        Ok(state.entries.range((org, from)..=(org, i64::MAX)).take(limit).map(|(_, e)| e.clone()).collect())
    }

    async fn head(&self, org: OrgId) -> Result<FeedId, FeedError> {
        Ok(FeedId(self.state.lock().heads.get(&org).copied().unwrap_or(0)))
    }

    async fn append(&self, org: OrgId, kind: FeedKind, subject: &str) -> Result<FeedId, FeedError> {
        if subject.is_empty() {
            return Err(FeedError::Invalid("feed subject is empty".into()));
        }
        Ok(self.push(org, kind, subject))
    }
}
