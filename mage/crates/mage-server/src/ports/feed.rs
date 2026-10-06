//! `ChangeFeed` (server.md "`ChangeFeed`"): the per-org change feed behind the UI's org SSE stream.
//!
//! Store methods write feed rows in the same transaction as the state change they announce
//! (`adapters::driven::pg::feed::append_in`); [`ChangeFeed::append`] is for writers that have no
//! store transaction (e.g. the gateway's `Settings` notice). Ids are per-org, start at 1, and
//! commit order equals id order (storage.md `org_feed`), so a reader resuming after an id never
//! skips one. Notices are wakeups only: readers fetch entries with [`ChangeFeed::after`].

use std::pin::Pin;

use async_trait::async_trait;
use futures::Stream;
use mage_core::domain::OrgId;
pub use mage_core::wire::api::FeedKind;
use time::OffsetDateTime;

use crate::error::AppError;

/// Per-org monotonic feed id; `FeedId(0)` = before the first entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FeedId(pub i64);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedEntry {
    pub id: FeedId,
    pub org: OrgId,
    pub kind: FeedKind,
    /// Prefixed id of the changed subject (`tkt_…`, `run_…`, …).
    pub subject: String,
    pub at: OffsetDateTime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedNotice {
    /// A committed entry of the subscribed org.
    Entry { org: OrgId, id: FeedId },
    /// Notices may have been missed (slow subscriber, listener reconnect): resync with `after`.
    Lagged,
}

pub type NoticeStream = Pin<Box<dyn Stream<Item = FeedNotice> + Send>>;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FeedError {
    #[error("not found")]
    NotFound,
    #[error("conflict")]
    Conflict,
    #[error("invalid: {0}")]
    Invalid(String),
    #[error("unavailable: {0}")]
    Unavailable(String),
}

/// Maximum entries per `after` call (`limit` is clamped to `1..=FEED_PAGE_MAX`).
pub const FEED_PAGE_MAX: u32 = 500;

#[async_trait]
pub trait ChangeFeed: Send + Sync {
    /// Notices for `org` committed after the call returns.
    async fn subscribe(&self, org: OrgId) -> Result<NoticeStream, FeedError>;
    /// Entries with id > `after`, ascending, at most `limit` (clamped).
    async fn after(&self, org: OrgId, after: FeedId, limit: u32) -> Result<Vec<FeedEntry>, FeedError>;
    /// Id of the newest committed entry (`FeedId(0)` if none).
    async fn head(&self, org: OrgId) -> Result<FeedId, FeedError>;
    /// Appends one entry in its own transaction (time from the adapter's `Clock`).
    async fn append(&self, org: OrgId, kind: FeedKind, subject: &str) -> Result<FeedId, FeedError>;
}

impl From<FeedError> for AppError {
    fn from(err: FeedError) -> AppError {
        match err {
            FeedError::NotFound => AppError::NotFound("feed".into()),
            FeedError::Conflict => AppError::Conflict("feed".into()),
            FeedError::Invalid(msg) => AppError::Invalid(msg),
            FeedError::Unavailable(msg) => AppError::Unavailable(msg),
        }
    }
}
