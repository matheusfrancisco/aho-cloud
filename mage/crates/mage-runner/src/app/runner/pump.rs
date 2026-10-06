//! Per-run plumbing between a run task and the server (runs.md "Harness run task": event buffer,
//! command poller, heartbeat ticker; "Retry rules": the local lease deadline).
//!
//! [`RunWatch`] is the shared per-run state: the self-fence (lease lost, local deadline passed,
//! run unknown or out of scope), the newest server status, and the local phase heartbeats carry.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use mage_core::domain::{RunId, RunStatus};
use mage_core::ports::Clock;
use mage_core::wire::runner::{
    CommandEnvelope, HeartbeatAck, LONG_POLL_MAX, LeaseTerms, MAX_PREVIEW_BYTES, OutboundEvent, OutboundEventKind, RunnerPhase,
};
use parking_lot::{Mutex, RwLock};
use tokio::sync::{Notify, mpsc, watch};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::ports::link::{LinkConflict, LinkError, ServerLink};

/// The runner self-fences this long before the lease expires on the server
/// (`send_time_of_last_acked_heartbeat + ttl - 5 s`).
pub const FENCE_MARGIN: Duration = Duration::from_secs(5);
/// Pause after a failed call before the pump tries again.
const RETRY_PAUSE: Duration = Duration::from_secs(1);

/// Flush when the oldest unsent event is this old, …
pub const FLUSH_EVERY: Duration = Duration::from_millis(250);
/// … or this many events are unsent, …
pub const FLUSH_EVENTS: usize = 100;
/// … or this many bytes are unsent.
pub const FLUSH_BYTES: usize = 256 * 1024;
/// Unacked caps; above them the oldest `AssistantText` is dropped first.
pub const MAX_UNACKED_EVENTS: usize = 10_000;
pub const MAX_UNACKED_BYTES: usize = 8 * 1024 * 1024;
/// Cap for text events (`AssistantText`, `Notice`, `Error`); previews use [`MAX_PREVIEW_BYTES`].
pub const MAX_TEXT_BYTES: usize = 16 * 1024;

/// Producers of run events (harness pump, `RunTools`, the run task) send through this.
pub type EventTx = mpsc::UnboundedSender<OutboundEventKind>;

// ---------------------------------------------------------------------------------------------
// RunWatch
// ---------------------------------------------------------------------------------------------

/// Shared per-run state between the run task and its pumps.
pub struct RunWatch {
    fence: CancellationToken,
    fence_reason: Mutex<Option<String>>,
    status: watch::Sender<Option<RunStatus>>,
    phase: watch::Sender<RunnerPhase>,
    beat_now: Notify,
}

impl RunWatch {
    pub fn new(phase: RunnerPhase) -> Arc<Self> {
        Arc::new(RunWatch {
            fence: CancellationToken::new(),
            fence_reason: Mutex::new(None),
            status: watch::Sender::new(None),
            phase: watch::Sender::new(phase),
            beat_now: Notify::new(),
        })
    }

    /// Self-fence: the lease is presumed lost; every pump stops and the task tears down without
    /// uploads. The first reason wins.
    pub fn trip(&self, reason: impl Into<String>) {
        {
            let mut current = self.fence_reason.lock();
            if current.is_none() {
                *current = Some(reason.into());
            }
        }
        self.fence.cancel();
    }

    pub fn is_fenced(&self) -> bool {
        self.fence.is_cancelled()
    }

    pub fn fence_reason(&self) -> Option<String> {
        self.fence_reason.lock().clone()
    }

    pub fn fence(&self) -> &CancellationToken {
        &self.fence
    }

    /// Records a status reported by the server. Terminal statuses are absorbing: a stale
    /// non-terminal answer that arrives later never replaces one.
    pub fn observe_status(&self, status: RunStatus) {
        self.status.send_if_modified(|current| match *current {
            Some(old) if old == status || old.is_terminal() => false,
            _ => {
                *current = Some(status);
                true
            }
        });
    }

    pub fn status(&self) -> Option<RunStatus> {
        *self.status.borrow()
    }

    pub fn status_rx(&self) -> watch::Receiver<Option<RunStatus>> {
        self.status.subscribe()
    }

    /// Sets the local phase; a change triggers an immediate heartbeat.
    pub fn set_phase(&self, phase: RunnerPhase) {
        if self.phase.send_replace(phase) != phase {
            self.beat_now.notify_one();
        }
    }

    /// Sets the local phase without waking the ticker (the caller sends the heartbeat itself).
    pub fn set_phase_quiet(&self, phase: RunnerPhase) {
        self.phase.send_replace(phase);
    }

    pub fn phase(&self) -> RunnerPhase {
        *self.phase.borrow()
    }

    /// Asks the heartbeat ticker for an immediate heartbeat.
    pub fn beat_now(&self) {
        self.beat_now.notify_one();
    }

    /// Run-level meaning of an error from a per-run call (runs.md "Errors"): 409 `lease_lost`
    /// self-fences; 404 and 403 drop the run locally (also a fence: stop calling for it); 409
    /// `run_terminal` / `invalid_state` carry the status to follow.
    pub fn observe_error(&self, e: &LinkError) {
        match e {
            LinkError::Conflict(LinkConflict::LeaseLost) => self.trip("lease lost (409 lease_lost)"),
            LinkError::NotFound => self.trip("run unknown to the server (404)"),
            LinkError::Forbidden => self.trip("run out of the runner's scope (403)"),
            LinkError::Conflict(LinkConflict::RunTerminal(s) | LinkConflict::InvalidState(s)) => self.observe_status(*s),
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Heartbeats
// ---------------------------------------------------------------------------------------------

struct BeatState {
    deadline: Instant,
    every: Duration,
}

/// Heartbeat ticker with the local lease deadline (runs.md "Retry rules": the runner's deadline
/// is `send_time_of_last_acked_heartbeat + ttl - 5 s`; past it the runner self-fences).
pub struct Heartbeats {
    link: Arc<dyn ServerLink>,
    watch: Arc<RunWatch>,
    run: RunId,
    attempt: u16,
    state: Mutex<BeatState>,
}

impl Heartbeats {
    /// `lease_sent_at`: when the lease request that returned the assignment was sent (the
    /// server's lease runs from a moment at or after it).
    pub fn new(
        link: Arc<dyn ServerLink>,
        watch: Arc<RunWatch>,
        run: RunId,
        attempt: u16,
        terms: LeaseTerms,
        lease_sent_at: Instant,
    ) -> Arc<Self> {
        Arc::new(Heartbeats {
            link,
            watch,
            run,
            attempt,
            state: Mutex::new(BeatState {
                deadline: lease_sent_at + terms.ttl.saturating_sub(FENCE_MARGIN),
                every: terms.heartbeat_every,
            }),
        })
    }

    /// The current local lease deadline.
    pub fn deadline(&self) -> Instant {
        self.state.lock().deadline
    }

    /// One heartbeat with the current phase. Moves the local deadline on success; self-fences
    /// when the deadline passes before the server answers, or on `lease_lost`.
    pub async fn beat(&self) -> Result<HeartbeatAck, LinkError> {
        let phase = self.watch.phase();
        let sent = Instant::now();
        let deadline = self.deadline();
        match tokio::time::timeout_at(deadline, self.link.heartbeat(self.run, self.attempt, phase)).await {
            Err(_) => {
                self.watch.trip("local lease deadline passed");
                Err(LinkError::Unavailable("local lease deadline passed".into()))
            }
            Ok(Ok(ack)) => {
                {
                    let mut state = self.state.lock();
                    state.deadline = sent + ack.lease.ttl.saturating_sub(FENCE_MARGIN);
                    state.every = ack.lease.heartbeat_every;
                }
                self.watch.observe_status(ack.status);
                Ok(ack)
            }
            Ok(Err(e)) => {
                self.watch.observe_error(&e);
                Err(e)
            }
        }
    }

    /// Ticks every `heartbeat_every` (or at once on a phase change / [`RunWatch::beat_now`])
    /// until `stop` or the fence. The fence trips exactly at the local deadline when no
    /// heartbeat got through before it.
    pub async fn run(self: Arc<Self>, stop: CancellationToken) {
        let mut next = Instant::now() + self.state.lock().every;
        loop {
            let deadline = self.deadline();
            tokio::select! {
                _ = stop.cancelled() => return,
                _ = self.watch.fence().cancelled() => return,
                _ = tokio::time::sleep_until(next.min(deadline)) => {}
                _ = self.watch.beat_now.notified() => {}
            }
            if Instant::now() >= self.deadline() {
                self.watch.trip("local lease deadline passed");
                return;
            }
            let sent = Instant::now();
            match self.beat().await {
                Ok(_) => next = sent + self.state.lock().every,
                Err(_) if self.watch.is_fenced() => return,
                Err(e) => {
                    tracing::warn!(run = %self.run, attempt = self.attempt, error = %e, "heartbeat failed");
                    next = Instant::now() + RETRY_PAUSE;
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Command poller
// ---------------------------------------------------------------------------------------------

/// One long-poll at a time with `after` = the last delivered seq (memory only: commands are
/// attempt-scoped). Commands go to the task in seq order, before the batch status is recorded.
pub async fn poll_commands(
    link: Arc<dyn ServerLink>,
    watch: Arc<RunWatch>,
    run: RunId,
    attempt: u16,
    tx: mpsc::UnboundedSender<CommandEnvelope>,
    stop: CancellationToken,
) {
    let mut after = 0u64;
    loop {
        let polled = tokio::select! {
            _ = stop.cancelled() => return,
            _ = watch.fence().cancelled() => return,
            r = link.poll_commands(run, attempt, after, LONG_POLL_MAX) => r,
        };
        match polled {
            Ok(mut batch) => {
                batch.commands.sort_by_key(|c| c.seq);
                for command in batch.commands {
                    if command.seq <= after {
                        continue;
                    }
                    after = command.seq;
                    if command.attempt == attempt && tx.send(command).is_err() {
                        return;
                    }
                }
                watch.observe_status(batch.status);
            }
            Err(e) => {
                watch.observe_error(&e);
                if watch.is_fenced() {
                    return;
                }
                tracing::warn!(run = %run, attempt, error = %e, "command poll failed");
                tokio::select! {
                    _ = stop.cancelled() => return,
                    _ = tokio::time::sleep(RETRY_PAUSE) => {}
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Scrubber
// ---------------------------------------------------------------------------------------------

/// Removes known token strings (knowledge, gateway, Run MCP bearer) from event text and caps
/// sizes (previews 4 KiB, other text [`MAX_TEXT_BYTES`]).
#[derive(Default)]
pub struct Scrubber {
    secrets: RwLock<Vec<String>>,
}

impl Scrubber {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a secret; strings shorter than 8 bytes are ignored (they would match noise).
    pub fn add(&self, secret: &str) {
        if secret.len() >= 8 {
            let mut secrets = self.secrets.write();
            if !secrets.iter().any(|s| s == secret) {
                secrets.push(secret.to_owned());
            }
        }
    }

    pub fn text(&self, mut text: String, max: usize) -> String {
        for secret in self.secrets.read().iter() {
            if text.contains(secret.as_str()) {
                text = text.replace(secret.as_str(), "[redacted]");
            }
        }
        cap_text(text, max)
    }

    pub fn clean(&self, kind: OutboundEventKind) -> OutboundEventKind {
        match kind {
            OutboundEventKind::AssistantText(t) => OutboundEventKind::AssistantText(self.text(t, MAX_TEXT_BYTES)),
            OutboundEventKind::ToolCall { name, args_preview } => {
                OutboundEventKind::ToolCall { name, args_preview: self.text(args_preview, MAX_PREVIEW_BYTES) }
            }
            OutboundEventKind::ToolResult { name, ok, preview } => {
                OutboundEventKind::ToolResult { name, ok, preview: self.text(preview, MAX_PREVIEW_BYTES) }
            }
            OutboundEventKind::Usage(u) => OutboundEventKind::Usage(u),
            OutboundEventKind::Notice(t) => OutboundEventKind::Notice(self.text(t, MAX_TEXT_BYTES)),
            OutboundEventKind::Error(t) => OutboundEventKind::Error(self.text(t, MAX_TEXT_BYTES)),
        }
    }
}

/// UTF-8-safe cut to at most `max` bytes, ending with `…[+N bytes]` when cut.
pub fn cap_text(mut text: String, max: usize) -> String {
    if text.len() <= max {
        return text;
    }
    let total = text.len();
    let mut end = max.saturating_sub(24);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text.push_str(&format!("…[+{} bytes]", total - end));
    text
}

// ---------------------------------------------------------------------------------------------
// Event buffer
// ---------------------------------------------------------------------------------------------

/// Unacked outbound events of one attempt with their `local_seq` (monotonic per attempt).
/// The front `in_flight` events are the batch being sent; only events behind them are
/// coalesced or dropped.
#[derive(Debug, Default)]
pub struct EventBuffer {
    next_seq: u64,
    queue: VecDeque<(OutboundEvent, usize)>,
    bytes: usize,
    in_flight: usize,
    dropped: u64,
}

fn event_size(kind: &OutboundEventKind) -> usize {
    64 + match kind {
        OutboundEventKind::AssistantText(t) | OutboundEventKind::Notice(t) | OutboundEventKind::Error(t) => t.len(),
        OutboundEventKind::ToolCall { name, args_preview } => name.len() + args_preview.len(),
        OutboundEventKind::ToolResult { name, preview, .. } => name.len() + preview.len(),
        OutboundEventKind::Usage(_) => 96,
    }
}

impl EventBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends an event. `Usage` (cumulative) replaces unsent older `Usage`. Above the unacked
    /// caps the oldest unsent `AssistantText` is dropped first, then the oldest unsent other
    /// event (never the newest `Usage`); drops are reported by one `Notice` in the next batch.
    pub fn push(&mut self, at: time::OffsetDateTime, kind: OutboundEventKind) {
        if matches!(kind, OutboundEventKind::Usage(_)) {
            self.remove_unsent(|k| matches!(k, OutboundEventKind::Usage(_)), usize::MAX);
        }
        self.next_seq += 1;
        let size = event_size(&kind);
        self.queue.push_back((OutboundEvent { local_seq: self.next_seq, at, kind }, size));
        self.bytes += size;
        while self.queue.len() > MAX_UNACKED_EVENTS || self.bytes > MAX_UNACKED_BYTES {
            let dropped = self.remove_unsent(|k| matches!(k, OutboundEventKind::AssistantText(_)), 1) == 1
                || self.remove_unsent(|k| !matches!(k, OutboundEventKind::Usage(_)), 1) == 1;
            if !dropped {
                break;
            }
            self.dropped += 1;
        }
    }

    /// Removes up to `limit` unsent events matching `pred`, oldest first.
    fn remove_unsent(&mut self, pred: impl Fn(&OutboundEventKind) -> bool, limit: usize) -> usize {
        let mut removed = 0;
        let mut i = self.in_flight;
        while i < self.queue.len() && removed < limit {
            if pred(&self.queue[i].0.kind) {
                if let Some((_, size)) = self.queue.remove(i) {
                    self.bytes -= size;
                }
                removed += 1;
            } else {
                i += 1;
            }
        }
        removed
    }

    pub fn unsent(&self) -> usize {
        self.queue.len() - self.in_flight
    }

    pub fn is_empty(&self) -> bool {
        self.queue.is_empty() && self.dropped == 0
    }

    /// Unsent events reached the count or byte flush threshold.
    pub fn batch_ready(&self) -> bool {
        self.unsent() >= FLUSH_EVENTS || self.queue.iter().skip(self.in_flight).map(|(_, s)| s).sum::<usize>() >= FLUSH_BYTES
    }

    /// The next batch (at most `max_events` and [`FLUSH_BYTES`], at least one event), marked in
    /// flight until [`EventBuffer::ack`] or [`EventBuffer::failed`]. Includes the
    /// "n events dropped" notice when events were dropped.
    pub fn next_batch(&mut self, at: time::OffsetDateTime, max_events: usize) -> Option<Vec<OutboundEvent>> {
        if self.dropped > 0 {
            let notice = OutboundEventKind::Notice(format!("{} events dropped", self.dropped));
            self.dropped = 0;
            self.next_seq += 1;
            let size = event_size(&notice);
            self.queue.push_back((OutboundEvent { local_seq: self.next_seq, at, kind: notice }, size));
            self.bytes += size;
        }
        self.in_flight = 0;
        let mut bytes = 0;
        let mut batch = Vec::new();
        for (event, size) in &self.queue {
            if batch.len() >= max_events.max(1) || (!batch.is_empty() && bytes + size > FLUSH_BYTES) {
                break;
            }
            bytes += size;
            batch.push(event.clone());
        }
        self.in_flight = batch.len();
        (!batch.is_empty()).then_some(batch)
    }

    /// The server ingested everything up to `acked_local_seq`.
    pub fn ack(&mut self, acked_local_seq: u64) {
        while let Some((event, size)) = self.queue.front() {
            if event.local_seq > acked_local_seq {
                break;
            }
            self.bytes -= size;
            self.queue.pop_front();
        }
        self.in_flight = 0;
    }

    /// The in-flight batch was not ingested; it is sent again.
    pub fn failed(&mut self) {
        self.in_flight = 0;
    }

    /// Drops the oldest event (one the server refuses even alone).
    pub fn drop_front(&mut self) {
        if let Some((_, size)) = self.queue.pop_front() {
            self.bytes -= size;
        }
        self.in_flight = 0;
    }

    pub fn len(&self) -> usize {
        self.queue.len()
    }
}

// ---------------------------------------------------------------------------------------------
// Event pump
// ---------------------------------------------------------------------------------------------

enum Flush {
    Sent,
    Retry,
    Fenced,
}

/// Drains [`EventTx`] into an [`EventBuffer`] and sends one batch at a time to
/// `POST /runs/{id}/events`.
pub struct EventPump {
    link: Arc<dyn ServerLink>,
    watch: Arc<RunWatch>,
    scrubber: Arc<Scrubber>,
    clock: Arc<dyn Clock>,
    run: RunId,
    attempt: u16,
    rx: mpsc::UnboundedReceiver<OutboundEventKind>,
    buffer: EventBuffer,
    max_batch: usize,
}

impl EventPump {
    pub fn new(
        link: Arc<dyn ServerLink>,
        watch: Arc<RunWatch>,
        scrubber: Arc<Scrubber>,
        clock: Arc<dyn Clock>,
        run: RunId,
        attempt: u16,
    ) -> (EventTx, EventPump) {
        let (tx, rx) = mpsc::unbounded_channel();
        let pump = EventPump { link, watch, scrubber, clock, run, attempt, rx, buffer: EventBuffer::new(), max_batch: FLUSH_EVENTS };
        (tx, pump)
    }

    fn push(&mut self, kind: OutboundEventKind) {
        let kind = self.scrubber.clean(kind);
        self.buffer.push(self.clock.now(), kind);
    }

    async fn flush(&mut self) -> Flush {
        let Some(batch) = self.buffer.next_batch(self.clock.now(), self.max_batch) else {
            return Flush::Sent;
        };
        let pushed = tokio::select! {
            _ = self.watch.fence().cancelled() => return Flush::Fenced,
            r = self.link.push_events(self.run, self.attempt, &batch) => r,
        };
        match pushed {
            Ok(ack) => {
                self.buffer.ack(ack.acked_local_seq);
                self.watch.observe_status(ack.status);
                Flush::Sent
            }
            Err(LinkError::TooLarge(m)) => {
                if self.max_batch > 1 {
                    self.max_batch = (batch.len() / 2).max(1);
                    self.buffer.failed();
                } else {
                    tracing::warn!(run = %self.run, reason = %m, "dropping one event the server refuses as too large");
                    self.buffer.drop_front();
                }
                Flush::Retry
            }
            Err(LinkError::Invalid(m)) => {
                tracing::error!(run = %self.run, reason = %m, events = batch.len(), "server rejected an event batch; dropped");
                self.buffer.ack(batch.last().map_or(0, |e| e.local_seq));
                Flush::Retry
            }
            Err(e) => {
                self.watch.observe_error(&e);
                if self.watch.is_fenced() {
                    return Flush::Fenced;
                }
                if let LinkError::Conflict(LinkConflict::RunTerminal(_)) = e {
                    self.buffer.ack(batch.last().map_or(0, |e| e.local_seq));
                } else {
                    self.buffer.failed();
                }
                tracing::warn!(run = %self.run, error = %e, "event batch not sent");
                Flush::Retry
            }
        }
    }

    /// Runs until `close` (final flush for at most `final_flush`, then the rest is dropped with
    /// a log line), the fence (events dropped), or every producer gone.
    pub async fn run(mut self, close: CancellationToken, final_flush: Duration) {
        let mut flush_at: Option<Instant> = None;
        let mut paused_until: Option<Instant> = None;
        loop {
            let wake = flush_at.map(|f| paused_until.map_or(f, |p| p.max(f)));
            tokio::select! {
                biased;
                _ = self.watch.fence().cancelled() => {
                    self.log_dropped("run fenced");
                    return;
                }
                _ = close.cancelled() => break,
                msg = self.rx.recv() => match msg {
                    Some(kind) => {
                        self.push(kind);
                        if flush_at.is_none() {
                            flush_at = Some(Instant::now() + FLUSH_EVERY);
                        }
                        if self.buffer.batch_ready() && paused_until.is_none_or(|p| Instant::now() >= p) {
                            flush_at = Some(Instant::now());
                        }
                    }
                    None => break,
                },
                _ = tokio::time::sleep_until(wake.unwrap_or_else(Instant::now)), if wake.is_some() => {
                    match self.flush().await {
                        Flush::Fenced => {
                            self.log_dropped("run fenced");
                            return;
                        }
                        Flush::Retry => paused_until = Some(Instant::now() + RETRY_PAUSE),
                        Flush::Sent => paused_until = None,
                    }
                    flush_at = if self.buffer.is_empty() {
                        None
                    } else if self.buffer.batch_ready() {
                        Some(Instant::now())
                    } else {
                        Some(Instant::now() + FLUSH_EVERY)
                    };
                }
            }
        }
        while let Ok(kind) = self.rx.try_recv() {
            self.push(kind);
        }
        let deadline = Instant::now() + final_flush;
        while !self.buffer.is_empty() && Instant::now() < deadline {
            match tokio::time::timeout_at(deadline, self.flush()).await {
                Ok(Flush::Sent) => {}
                Ok(Flush::Retry) => tokio::time::sleep_until(deadline.min(Instant::now() + RETRY_PAUSE)).await,
                Ok(Flush::Fenced) | Err(_) => break,
            }
        }
        self.log_dropped("teardown flush deadline");
    }

    fn log_dropped(&self, why: &str) {
        if self.buffer.len() > 0 {
            tracing::warn!(run = %self.run, attempt = self.attempt, events = self.buffer.len(), why, "unsent run events dropped");
        }
    }
}

#[cfg(test)]
mod tests {
    use mage_core::domain::Usage;
    use time::macros::datetime;

    use super::*;

    const AT: time::OffsetDateTime = datetime!(2026-01-01 00:00 UTC);

    fn text(s: &str) -> OutboundEventKind {
        OutboundEventKind::AssistantText(s.to_owned())
    }

    fn usage(turns: u32) -> OutboundEventKind {
        OutboundEventKind::Usage(Usage { turns, ..Usage::default() })
    }

    #[test]
    fn local_seq_is_monotonic_and_ack_removes_the_acked_prefix() {
        let mut b = EventBuffer::new();
        b.push(AT, text("a"));
        b.push(AT, text("b"));
        let batch = b.next_batch(AT, 100).unwrap();
        assert_eq!(batch.iter().map(|e| e.local_seq).collect::<Vec<_>>(), vec![1, 2]);
        b.push(AT, text("c"));
        b.ack(2);
        let next = b.next_batch(AT, 100).unwrap();
        assert_eq!(next.iter().map(|e| e.local_seq).collect::<Vec<_>>(), vec![3]);
    }

    #[test]
    fn failed_batch_is_resent_unchanged() {
        let mut b = EventBuffer::new();
        b.push(AT, text("a"));
        let first = b.next_batch(AT, 100).unwrap();
        b.failed();
        assert_eq!(b.next_batch(AT, 100).unwrap(), first);
    }

    #[test]
    fn usage_coalesces_to_the_latest_unsent_value_but_never_touches_the_batch_in_flight() {
        let mut b = EventBuffer::new();
        b.push(AT, usage(1));
        let in_flight = b.next_batch(AT, 100).unwrap();
        b.push(AT, usage(2));
        b.push(AT, text("x"));
        b.push(AT, usage(3));
        b.ack(in_flight[0].local_seq);
        let batch = b.next_batch(AT, 100).unwrap();
        let kinds: Vec<_> = batch.into_iter().map(|e| e.kind).collect();
        assert_eq!(kinds, vec![text("x"), usage(3)]);
    }

    #[test]
    fn batches_respect_the_count_limit() {
        let mut b = EventBuffer::new();
        for i in 0..250 {
            b.push(AT, text(&i.to_string()));
        }
        assert!(b.batch_ready());
        assert_eq!(b.next_batch(AT, FLUSH_EVENTS).unwrap().len(), 100);
    }

    #[test]
    fn overflow_drops_oldest_assistant_text_first_and_reports_it() {
        let mut b = EventBuffer::new();
        b.push(AT, OutboundEventKind::Notice("keep".into()));
        for _ in 0..MAX_UNACKED_EVENTS {
            b.push(AT, text("t"));
        }
        assert_eq!(b.len(), MAX_UNACKED_EVENTS);
        let mut sent = Vec::new();
        while let Some(batch) = b.next_batch(AT, FLUSH_EVENTS) {
            assert!(batch.len() <= FLUSH_EVENTS);
            b.ack(batch.last().unwrap().local_seq);
            sent.extend(batch);
        }
        assert_eq!(sent[0].kind, OutboundEventKind::Notice("keep".into()), "non-text kept while text remains");
        assert_eq!(sent.last().unwrap().kind, OutboundEventKind::Notice("1 events dropped".into()));
        assert_eq!(sent.len(), MAX_UNACKED_EVENTS + 1);
        assert!(sent.windows(2).all(|w| w[0].local_seq < w[1].local_seq));
        assert!(b.is_empty());
    }

    #[test]
    fn scrubber_redacts_secrets_and_caps_previews() {
        let s = Scrubber::new();
        s.add("mgk_secret-token-123");
        let cleaned =
            s.clean(OutboundEventKind::ToolResult { name: "run.exec".into(), ok: true, preview: format!("x mgk_secret-token-123 {}", "é".repeat(4000)) });
        let OutboundEventKind::ToolResult { preview, .. } = cleaned else { unreachable!() };
        assert!(!preview.contains("mgk_secret-token-123"));
        assert!(preview.starts_with("x [redacted] "));
        assert!(preview.len() <= MAX_PREVIEW_BYTES);
        assert!(preview.ends_with(" bytes]"));
    }

    fn terms() -> LeaseTerms {
        LeaseTerms { ttl: Duration::from_secs(60), heartbeat_every: Duration::from_secs(15) }
    }

    fn run_id() -> RunId {
        uuid::Uuid::now_v7().into()
    }

    #[tokio::test(start_paused = true)]
    async fn missed_heartbeats_self_fence_exactly_at_the_local_deadline() {
        let link = crate::testing::link::FakeServerLink::new();
        link.script_heartbeat(crate::testing::link::HeartbeatScript::Hang);
        let watch = RunWatch::new(RunnerPhase::Running);
        let start = Instant::now();
        let beats = Heartbeats::new(link.clone(), watch.clone(), run_id(), 1, terms(), start);
        let ticker = tokio::spawn(beats.run(CancellationToken::new()));
        watch.fence().cancelled().await;
        assert_eq!(start.elapsed(), Duration::from_secs(55), "lease sent + ttl - 5 s");
        assert_eq!(watch.fence_reason().as_deref(), Some("local lease deadline passed"));
        ticker.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn an_acked_heartbeat_moves_the_deadline_to_its_send_time_plus_ttl() {
        let link = crate::testing::link::FakeServerLink::new();
        let watch = RunWatch::new(RunnerPhase::Running);
        let start = Instant::now();
        let beats = Heartbeats::new(link.clone(), watch.clone(), run_id(), 1, terms(), start);
        tokio::spawn(beats.clone().run(CancellationToken::new()));
        link.wait_for(Duration::from_secs(20), |calls| !calls.is_empty()).await;
        assert_eq!(start.elapsed(), Duration::from_secs(15), "first tick after heartbeat_every");
        link.script_heartbeat(crate::testing::link::HeartbeatScript::Hang);
        watch.fence().cancelled().await;
        assert_eq!(start.elapsed(), Duration::from_secs(15 + 55));
    }

    #[tokio::test(start_paused = true)]
    async fn lease_lost_on_a_heartbeat_fences_at_once() {
        let link = crate::testing::link::FakeServerLink::new();
        link.script_heartbeat(crate::testing::link::HeartbeatScript::Fail(LinkError::Conflict(LinkConflict::LeaseLost)));
        let watch = RunWatch::new(RunnerPhase::Preparing);
        let beats = Heartbeats::new(link, watch.clone(), run_id(), 1, terms(), Instant::now());
        assert!(beats.beat().await.is_err());
        assert!(watch.is_fenced());
    }

    #[test]
    fn terminal_status_is_absorbing() {
        let watch = RunWatch::new(RunnerPhase::Running);
        watch.observe_status(RunStatus::Running);
        watch.observe_status(RunStatus::Cancelled);
        watch.observe_status(RunStatus::Running);
        assert_eq!(watch.status(), Some(RunStatus::Cancelled));
    }
}
