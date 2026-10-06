//! Event pump (harness.md "Event pump"): the stdout reader emits raw events into an unbounded
//! queue (it never blocks), a coalescer task merges `AssistantText` deltas (flush every 250 ms, at
//! 4 KiB, or before any non-text event), redacts every string, truncates previews, and forwards
//! into a bounded channel (1024) that the consumer drains. Non-text events are never dropped: the
//! coalescer awaits capacity.

use std::time::Duration;

use async_trait::async_trait;
use parking_lot::Mutex;
use tokio::sync::mpsc;
use tokio::time::MissedTickBehavior;

use super::redact::{PREVIEW_MAX, Redactor, StreamRedactor, chunks, preview};
use crate::ports::harness::{HarnessEvent, HarnessEventStream, TurnOutcome};

pub const CHANNEL_CAPACITY: usize = 1024;
pub const FLUSH_EVERY: Duration = Duration::from_millis(250);

/// Producer side; cloned freely, closed exactly once when the session has exited.
#[derive(Debug, Default)]
pub struct EventSink {
    tx: Mutex<Option<mpsc::UnboundedSender<HarnessEvent>>>,
}

impl EventSink {
    /// `false` once the sink is closed (the event is dropped: the stream already ended).
    pub fn emit(&self, event: HarnessEvent) -> bool {
        match self.tx.lock().as_ref() {
            Some(tx) => tx.send(event).is_ok(),
            None => false,
        }
    }

    /// Ends the stream after everything emitted so far has been delivered.
    pub fn close(&self) {
        self.tx.lock().take();
    }
}

/// Consumer side (`HarnessSession::events`).
pub struct EventStream {
    rx: mpsc::Receiver<HarnessEvent>,
}

#[async_trait]
impl HarnessEventStream for EventStream {
    async fn next(&mut self) -> Option<HarnessEvent> {
        self.rx.recv().await
    }
}

/// Starts the coalescer task.
pub fn pump(redactor: Redactor) -> (EventSink, EventStream) {
    let (in_tx, in_rx) = mpsc::unbounded_channel();
    let (out_tx, out_rx) = mpsc::channel(CHANNEL_CAPACITY);
    tokio::spawn(coalesce(in_rx, out_tx, redactor));
    (EventSink { tx: Mutex::new(Some(in_tx)) }, EventStream { rx: out_rx })
}

struct TextBuffer {
    turn: u32,
    text: String,
    stream: StreamRedactor,
}

async fn coalesce(mut rx: mpsc::UnboundedReceiver<HarnessEvent>, tx: mpsc::Sender<HarnessEvent>, redactor: Redactor) {
    let mut tick = tokio::time::interval(FLUSH_EVERY);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut text: Option<TextBuffer> = None;
    loop {
        tokio::select! {
            event = rx.recv() => {
                let Some(event) = event else { break };
                match event {
                    HarnessEvent::AssistantText { turn, text: delta } => {
                        if text.as_ref().is_some_and(|b| b.turn != turn) && !flush(&mut text, &tx, true).await {
                            return;
                        }
                        if text.is_none() {
                            // The 250 ms window starts at the first buffered delta; a tick missed
                            // while nothing was buffered must not flush it immediately.
                            tick.reset();
                        }
                        let buf = text.get_or_insert_with(|| TextBuffer { turn, text: String::new(), stream: StreamRedactor::new(redactor.clone()) });
                        buf.text.push_str(&delta);
                        if buf.text.len() >= PREVIEW_MAX && !flush(&mut text, &tx, false).await {
                            return;
                        }
                    }
                    other => {
                        if !flush(&mut text, &tx, true).await || tx.send(scrub(other, &redactor)).await.is_err() {
                            return;
                        }
                    }
                }
            }
            _ = tick.tick(), if text.is_some() => {
                if !flush(&mut text, &tx, false).await {
                    return;
                }
            }
        }
    }
    flush(&mut text, &tx, true).await;
}

/// Sends the buffered text (redacted, ≤ 4 KiB per event). A non-final flush holds back a
/// possible secret prefix at the end; a final flush empties the buffer. `false` when the consumer
/// is gone.
async fn flush(text: &mut Option<TextBuffer>, tx: &mpsc::Sender<HarnessEvent>, last: bool) -> bool {
    let Some(buf) = text.as_mut() else { return true };
    let mut out = buf.stream.push(std::mem::take(&mut buf.text).as_bytes());
    let turn = buf.turn;
    if last {
        out.extend(buf.stream.finish());
        *text = None;
    }
    let out = String::from_utf8_lossy(&out).into_owned();
    for piece in chunks(&out, PREVIEW_MAX) {
        if tx.send(HarnessEvent::AssistantText { turn, text: piece }).await.is_err() {
            return false;
        }
    }
    true
}

/// Redaction + preview truncation for every non-text event.
fn scrub(event: HarnessEvent, redactor: &Redactor) -> HarnessEvent {
    match event {
        HarnessEvent::ToolCall { turn, call_id, tool, args_preview } => {
            HarnessEvent::ToolCall { turn, call_id, tool, args_preview: preview(&redactor.redact(&args_preview)) }
        }
        HarnessEvent::ToolResult { turn, call_id, tool, ok, preview: p } => {
            HarnessEvent::ToolResult { turn, call_id, tool, ok, preview: preview(&redactor.redact(&p)) }
        }
        HarnessEvent::Notice { level, text } => HarnessEvent::Notice { level, text: preview(&redactor.redact(&text)) },
        HarnessEvent::TurnEnded { turn, outcome: TurnOutcome::Failed { message, retryable, http_status } } => HarnessEvent::TurnEnded {
            turn,
            outcome: TurnOutcome::Failed { message: preview(&redactor.redact(&message)), retryable, http_status },
        },
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ports::harness::{NoticeLevel, ToolRef};

    const SECRET: &str = "mgg_abcdefghijklmnop";

    async fn drain(mut stream: EventStream) -> Vec<HarnessEvent> {
        let mut out = Vec::new();
        while let Some(e) = stream.next().await {
            out.push(e);
        }
        out
    }

    #[tokio::test]
    async fn deltas_coalesce_and_flush_before_other_events() {
        let (sink, stream) = pump(Redactor::default());
        sink.emit(HarnessEvent::TurnStarted { turn: 1 });
        for d in ["Hel", "lo ", "world"] {
            sink.emit(HarnessEvent::AssistantText { turn: 1, text: d.into() });
        }
        sink.emit(HarnessEvent::TurnEnded { turn: 1, outcome: TurnOutcome::Completed });
        sink.close();
        assert_eq!(
            drain(stream).await,
            vec![
                HarnessEvent::TurnStarted { turn: 1 },
                HarnessEvent::AssistantText { turn: 1, text: "Hello world".into() },
                HarnessEvent::TurnEnded { turn: 1, outcome: TurnOutcome::Completed },
            ]
        );
        assert!(!sink.emit(HarnessEvent::Idle));
    }

    #[tokio::test(start_paused = true)]
    async fn first_delta_after_a_quiet_period_waits_for_the_flush_window() {
        let (sink, stream) = pump(Redactor::default());
        // Ticks elapse while nothing is buffered.
        tokio::time::sleep(FLUSH_EVERY * 3).await;
        sink.emit(HarnessEvent::AssistantText { turn: 1, text: "Hel".into() });
        tokio::time::sleep(FLUSH_EVERY / 5).await;
        sink.emit(HarnessEvent::AssistantText { turn: 1, text: "lo".into() });
        sink.emit(HarnessEvent::Idle);
        sink.close();
        assert_eq!(drain(stream).await, vec![HarnessEvent::AssistantText { turn: 1, text: "Hello".into() }, HarnessEvent::Idle]);
    }

    #[tokio::test]
    async fn long_text_splits_into_4k_events() {
        let (sink, stream) = pump(Redactor::default());
        sink.emit(HarnessEvent::AssistantText { turn: 1, text: "x".repeat(10_000) });
        sink.close();
        let events = drain(stream).await;
        let texts: Vec<usize> = events
            .iter()
            .map(|e| match e {
                HarnessEvent::AssistantText { text, .. } => text.len(),
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        assert!(texts.iter().all(|l| *l <= PREVIEW_MAX));
        assert_eq!(texts.iter().sum::<usize>(), 10_000);
    }

    #[tokio::test]
    async fn secret_split_across_timed_flushes_is_redacted() {
        let (sink, stream) = pump(Redactor::new([SECRET]));
        sink.emit(HarnessEvent::AssistantText { turn: 1, text: format!("token {}", &SECRET[..7]) });
        // Let the 250 ms flush fire between the two halves.
        tokio::time::sleep(FLUSH_EVERY * 3).await;
        sink.emit(HarnessEvent::AssistantText { turn: 1, text: format!("{} done", &SECRET[7..]) });
        sink.close();
        let text: String = drain(stream)
            .await
            .into_iter()
            .map(|e| match e {
                HarnessEvent::AssistantText { text, .. } => text,
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        assert_eq!(text, "token [redacted] done");
    }

    #[tokio::test]
    async fn previews_and_notices_are_redacted_and_bounded() {
        let (sink, stream) = pump(Redactor::new([SECRET]));
        let tool = ToolRef::Builtin("bash".into());
        sink.emit(HarnessEvent::ToolCall { turn: 1, call_id: "c".into(), tool: tool.clone(), args_preview: format!("cat {SECRET}") });
        sink.emit(HarnessEvent::ToolResult { turn: 1, call_id: "c".into(), tool, ok: true, preview: format!("{SECRET}{}", "y".repeat(9000)) });
        sink.emit(HarnessEvent::Notice { level: NoticeLevel::Error, text: format!("stderr: {SECRET}") });
        sink.emit(HarnessEvent::TurnEnded {
            turn: 1,
            outcome: TurnOutcome::Failed { message: format!("401 {SECRET}"), retryable: false, http_status: Some(401) },
        });
        sink.close();
        let events = drain(stream).await;
        let debug = format!("{events:?}");
        assert!(!debug.contains(SECRET));
        match &events[1] {
            HarnessEvent::ToolResult { preview, .. } => {
                assert!(preview.starts_with("[redacted]"));
                assert!(preview.len() <= PREVIEW_MAX);
            }
            other => panic!("unexpected {other:?}"),
        }
    }
}
