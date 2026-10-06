//! StreamEvents (runs.md, server.md "Live updates"): replay `events_after` in pages of 500, then
//! follow `RunSignals` (re-checking every 2 s anyway), ping every 15 s, end when the run is
//! terminal with its lease released, and ask the client to reconnect after `sse_max` or on
//! shutdown. Subscribes before the first read, so no event committed meanwhile is missed.

use std::time::Duration;

use futures::Stream;

use mage_core::domain::{OrgId, RunEvent, RunId};

use crate::app::runs::RunManager;
use crate::app::runs::lease::RECHECK;
use crate::error::AppError;
use crate::ports::runs::SignalTopic;

/// Events per store read.
pub const PAGE: u32 = 500;
/// Keep-alive comment interval.
pub const PING_EVERY: Duration = Duration::from_secs(15);

/// One item of a run event stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamItem {
    /// `event: run`, `id: seq`.
    Event(RunEvent),
    /// `: ping` comment.
    Ping,
    /// `event: end`: terminal, lease released, every event sent.
    End,
    /// `event: reconnect` (`sse_max` reached or shutdown).
    Reconnect,
}

impl RunManager {
    /// Events of `run` with `seq > after`, then live. The caller checked org membership.
    pub fn event_stream(&self, org: OrgId, run: RunId, after: u64) -> impl Stream<Item = Result<StreamItem, AppError>> + Send + use<> {
        let this = self.clone();
        async_stream::try_stream! {
            let mut sub = this.d.signals.subscribe(SignalTopic::RunEvents(run)).await.map_err(|e| AppError::Unavailable(e.to_string()))?;
            this.d.runs.get(org, run).await?;
            let started = tokio::time::Instant::now();
            let max = Duration::from_secs(this.d.policy.sse_max_secs);
            let mut cursor = after;
            let mut last_write = tokio::time::Instant::now();
            loop {
                let page = this.d.runs.events_after(org, run, cursor, PAGE).await?;
                let full = page.len() >= PAGE as usize;
                for event in page {
                    cursor = event.seq;
                    last_write = tokio::time::Instant::now();
                    yield StreamItem::Event(event);
                }
                if full {
                    continue;
                }
                let rec = this.d.runs.get(org, run).await?;
                if rec.run.status.is_terminal() && rec.lease.is_none() && rec.last_seq <= cursor {
                    yield StreamItem::End;
                    return;
                }
                let remaining = max.saturating_sub(started.elapsed());
                if remaining.is_zero() || this.d.shutdown.is_cancelled() {
                    yield StreamItem::Reconnect;
                    return;
                }
                tokio::select! {
                    () = this.d.shutdown.cancelled() => {}
                    _ = sub.wait(remaining.min(RECHECK)) => {}
                }
                if last_write.elapsed() >= PING_EVERY {
                    last_write = tokio::time::Instant::now();
                    yield StreamItem::Ping;
                }
            }
        }
    }
}
