//! `RunSignals` (runs.md "Adapter internals"): one `PgListener` per process on `mage_run_events`,
//! `mage_run_commands` and `mage_queue`, fanned out to in-process subscribers through a
//! [`SignalHub`]. Every (re)connect wakes every subscriber so it re-reads the store; the 2 s
//! re-check of the waiters keeps correctness when a notification is lost.
//!
//! `PgRunStore::commit` sends the notifications inside its transaction (Postgres delivers them at
//! commit and drops them on rollback); payloads carry ids only.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use sqlx::PgPool;
use sqlx::postgres::PgListener;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use mage_core::domain::{OrgId, RunId};

use crate::ports::runs::{RunSignals, SignalError, SignalTopic, Subscription, Woke};

pub const CHANNEL_RUN_EVENTS: &str = "mage_run_events";
pub const CHANNEL_RUN_COMMANDS: &str = "mage_run_commands";
pub const CHANNEL_QUEUE: &str = "mage_queue";

/// `org:run:seq` (canonical uuids).
pub fn events_payload(org: OrgId, run: RunId, seq: u64) -> String {
    format!("{}:{}:{seq}", org.as_uuid(), run.as_uuid())
}

/// `org:run` (canonical uuids).
pub fn commands_payload(org: OrgId, run: RunId) -> String {
    format!("{}:{}", org.as_uuid(), run.as_uuid())
}

/// Maps one notification to its topic; `None` for unknown channels or malformed payloads.
pub fn topic_of(channel: &str, payload: &str) -> Option<SignalTopic> {
    let run = || -> Option<RunId> { payload.split(':').nth(1).and_then(|s| Uuid::parse_str(s).ok()).map(RunId::from_uuid) };
    match channel {
        CHANNEL_RUN_EVENTS => run().map(SignalTopic::RunEvents),
        CHANNEL_RUN_COMMANDS => run().map(SignalTopic::RunCommands),
        CHANNEL_QUEUE => Some(SignalTopic::Queue),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Signal {
    Topic(SignalTopic),
    /// Listener (re)connected: every subscriber re-reads.
    All,
}

/// In-process fan-out of run signals (one broadcast channel; subscribers filter by topic).
/// Used by [`PgRunSignals`] and, directly, as the in-memory `RunSignals` of the fakes.
#[derive(Debug, Clone)]
pub struct SignalHub {
    tx: broadcast::Sender<Signal>,
}

impl Default for SignalHub {
    fn default() -> Self {
        Self::new()
    }
}

impl SignalHub {
    pub fn new() -> SignalHub {
        let (tx, _) = broadcast::channel(4096);
        SignalHub { tx }
    }

    pub fn notify(&self, topic: SignalTopic) {
        // No subscriber is not an error: signals are hints.
        let _ = self.tx.send(Signal::Topic(topic));
    }

    pub fn wake_all(&self) {
        let _ = self.tx.send(Signal::All);
    }
}

#[async_trait]
impl RunSignals for SignalHub {
    async fn subscribe(&self, topic: SignalTopic) -> Result<Box<dyn Subscription>, SignalError> {
        Ok(Box::new(HubSubscription { rx: self.tx.subscribe(), topic }))
    }
}

struct HubSubscription {
    rx: broadcast::Receiver<Signal>,
    topic: SignalTopic,
}

impl HubSubscription {
    fn wants(&self, s: Signal) -> bool {
        match s {
            Signal::All => true,
            Signal::Topic(t) => t == self.topic,
        }
    }

    /// Consumes everything already buffered; `true` if any of it was for this topic.
    fn drain(&mut self) -> bool {
        let mut hit = false;
        loop {
            match self.rx.try_recv() {
                Ok(s) => hit |= self.wants(s),
                Err(broadcast::error::TryRecvError::Lagged(_)) => hit = true,
                Err(broadcast::error::TryRecvError::Empty | broadcast::error::TryRecvError::Closed) => return hit,
            }
        }
    }
}

#[async_trait]
impl Subscription for HubSubscription {
    async fn wait(&mut self, timeout: Duration) -> Woke {
        if self.drain() {
            return Woke::Signal;
        }
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            match tokio::time::timeout_at(deadline, self.rx.recv()).await {
                Err(_) => return Woke::Timeout,
                Ok(Ok(s)) if self.wants(s) => {
                    self.drain();
                    return Woke::Signal;
                }
                Ok(Ok(_)) => {}
                Ok(Err(broadcast::error::RecvError::Lagged(_))) => {
                    self.drain();
                    return Woke::Signal;
                }
                Ok(Err(broadcast::error::RecvError::Closed)) => {
                    tokio::time::sleep_until(deadline).await;
                    return Woke::Timeout;
                }
            }
        }
    }
}

/// `RunSignals` over Postgres `LISTEN`.
#[derive(Debug, Clone)]
pub struct PgRunSignals {
    hub: Arc<SignalHub>,
}

impl PgRunSignals {
    /// Spawns the listener task on the current runtime; it stops when `shutdown` is cancelled.
    pub fn start(pool: PgPool, shutdown: CancellationToken) -> PgRunSignals {
        let hub = Arc::new(SignalHub::new());
        tokio::spawn(listen(pool, hub.clone(), shutdown));
        PgRunSignals { hub }
    }

    pub fn hub(&self) -> &Arc<SignalHub> {
        &self.hub
    }
}

#[async_trait]
impl RunSignals for PgRunSignals {
    async fn subscribe(&self, topic: SignalTopic) -> Result<Box<dyn Subscription>, SignalError> {
        self.hub.subscribe(topic).await
    }
}

const RECONNECT_DELAY: Duration = Duration::from_secs(1);

async fn listen(pool: PgPool, hub: Arc<SignalHub>, shutdown: CancellationToken) {
    while !shutdown.is_cancelled() {
        let mut listener = match PgListener::connect_with(&pool).await {
            Ok(l) => l,
            Err(err) => {
                tracing::warn!(error = %err, "run signals: listener connect failed");
                pause(&shutdown).await;
                continue;
            }
        };
        if let Err(err) = listener.listen_all([CHANNEL_RUN_EVENTS, CHANNEL_RUN_COMMANDS, CHANNEL_QUEUE]).await {
            tracing::warn!(error = %err, "run signals: LISTEN failed");
            pause(&shutdown).await;
            continue;
        }
        hub.wake_all();
        loop {
            tokio::select! {
                () = shutdown.cancelled() => return,
                next = listener.try_recv() => match next {
                    Ok(Some(n)) => {
                        if let Some(topic) = topic_of(n.channel(), n.payload()) {
                            hub.notify(topic);
                        }
                    }
                    // Connection lost and re-established (eager reconnect): notifications may be lost.
                    Ok(None) => hub.wake_all(),
                    Err(err) => {
                        tracing::warn!(error = %err, "run signals: listener failed");
                        hub.wake_all();
                        break;
                    }
                },
            }
        }
        pause(&shutdown).await;
    }
}

async fn pause(shutdown: &CancellationToken) {
    tokio::select! {
        () = shutdown.cancelled() => {}
        () = tokio::time::sleep(RECONNECT_DELAY) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(n: u128) -> RunId {
        RunId::from_uuid(Uuid::from_u128(n))
    }

    #[test]
    fn payloads_round_trip_to_topics() {
        let org = OrgId::from_uuid(Uuid::from_u128(7));
        assert_eq!(topic_of(CHANNEL_RUN_EVENTS, &events_payload(org, run(1), 42)), Some(SignalTopic::RunEvents(run(1))));
        assert_eq!(topic_of(CHANNEL_RUN_COMMANDS, &commands_payload(org, run(2))), Some(SignalTopic::RunCommands(run(2))));
        assert_eq!(topic_of(CHANNEL_QUEUE, ""), Some(SignalTopic::Queue));
        assert_eq!(topic_of(CHANNEL_RUN_EVENTS, "garbage"), None);
        assert_eq!(topic_of("other", ""), None);
    }

    #[tokio::test]
    async fn subscription_wakes_on_its_topic_and_coalesces() {
        let hub = SignalHub::new();
        let mut sub = hub.subscribe(SignalTopic::RunEvents(run(1))).await.unwrap();
        hub.notify(SignalTopic::RunEvents(run(2)));
        assert_eq!(sub.wait(Duration::from_millis(20)).await, Woke::Timeout);
        hub.notify(SignalTopic::RunEvents(run(1)));
        hub.notify(SignalTopic::RunEvents(run(1)));
        assert_eq!(sub.wait(Duration::from_millis(20)).await, Woke::Signal);
        // both notifications were coalesced into the first wake
        assert_eq!(sub.wait(Duration::from_millis(20)).await, Woke::Timeout);
        hub.wake_all();
        assert_eq!(sub.wait(Duration::from_millis(20)).await, Woke::Signal);
    }

    #[tokio::test]
    async fn waiter_is_woken_while_blocked() {
        let hub = SignalHub::new();
        let mut sub = hub.subscribe(SignalTopic::Queue).await.unwrap();
        let h2 = hub.clone();
        let notifier = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(30)).await;
            h2.notify(SignalTopic::Queue);
        });
        let started = std::time::Instant::now();
        assert_eq!(sub.wait(Duration::from_secs(5)).await, Woke::Signal);
        assert!(started.elapsed() < Duration::from_secs(2));
        notifier.await.unwrap();
    }
}
