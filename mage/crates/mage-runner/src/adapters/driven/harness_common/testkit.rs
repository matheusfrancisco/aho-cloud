//! Test support for the harness adapters: a scripted child process on a `FakeEnv`, event
//! summaries for golden comparisons, and a fake `--version` binary.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, Lines};

use super::config::HarnessConfig;
use crate::ports::executor::Exit;
use crate::ports::harness::{HarnessEvent, NoticeLevel, TurnOutcome};
use crate::testing::executor::{FakeChildControl, FakeSignal};
use crate::testing::harness_contract::EVENT_TIMEOUT;

/// Peer side of a scripted harness process.
pub struct Peer {
    pub lines: Lines<BufReader<DuplexStream>>,
    pub stdout: Option<DuplexStream>,
    pub stderr: Option<DuplexStream>,
    /// Every stdin line read so far (including those `recv_until` skipped).
    pub received: Vec<serde_json::Value>,
    ctl: Arc<FakeChildControl>,
}

impl Peer {
    /// Takes the child's pipes; the child exits with signal 15 as soon as the adapter terminates it.
    pub fn new(mut ctl: FakeChildControl) -> Self {
        let stdin = std::mem::replace(&mut ctl.stdin, tokio::io::duplex(1).0);
        let stdout = std::mem::replace(&mut ctl.stdout, tokio::io::duplex(1).0);
        let stderr = std::mem::replace(&mut ctl.stderr, tokio::io::duplex(1).0);
        let ctl = Arc::new(ctl);
        let watch = ctl.clone();
        tokio::spawn(async move {
            loop {
                if watch.signals().contains(&FakeSignal::Terminate) {
                    watch.exit(Exit::Signal(15));
                    return;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        });
        Self { lines: BufReader::new(stdin).lines(), stdout: Some(stdout), stderr: Some(stderr), received: Vec::new(), ctl }
    }

    pub async fn send(&mut self, line: &str) {
        if let Some(out) = self.stdout.as_mut() {
            let _ = out.write_all(line.as_bytes()).await;
            let _ = out.write_all(b"\n").await;
            let _ = out.flush().await;
        }
    }

    pub async fn send_json(&mut self, v: &serde_json::Value) {
        self.send(&v.to_string()).await;
    }

    pub async fn stderr(&mut self, text: &str) {
        if let Some(err) = self.stderr.as_mut() {
            let _ = err.write_all(text.as_bytes()).await;
            let _ = err.flush().await;
        }
    }

    /// Next stdin line as JSON; `None` at EOF. Panics when the adapter writes nothing for
    /// [`EVENT_TIMEOUT`], so a missing command fails the test instead of hanging it.
    pub async fn recv(&mut self) -> Option<serde_json::Value> {
        loop {
            let next = tokio::time::timeout(EVENT_TIMEOUT, self.lines.next_line())
                .await
                .unwrap_or_else(|_| panic!("timed out waiting for stdin; received: {:#?}", self.received));
            let line = next.ok()??;
            if line.trim().is_empty() {
                continue;
            }
            let value: serde_json::Value = serde_json::from_str(&line).unwrap_or_else(|e| panic!("adapter wrote non-JSON {line:?}: {e}"));
            self.received.push(value.clone());
            return Some(value);
        }
    }

    /// Reads stdin until a command matching `pred`; returns it.
    pub async fn recv_until(&mut self, pred: impl Fn(&serde_json::Value) -> bool) -> serde_json::Value {
        loop {
            match self.recv().await {
                Some(v) if pred(&v) => return v,
                Some(_) => {}
                None => panic!("stdin closed while waiting for a command"),
            }
        }
    }

    /// Waits for stdin EOF (the adapter closed stdin), then exits with `exit`.
    pub async fn exit_on_eof(mut self, exit: Exit) {
        while self.recv().await.is_some() {}
        self.stdout = None;
        self.stderr = None;
        self.ctl.exit(exit);
    }

    /// Ends stdout and exits now (crash).
    pub fn crash(mut self, exit: Exit) {
        self.stdout = None;
        self.stderr = None;
        self.ctl.exit(exit);
    }

    pub fn signals(&self) -> Vec<FakeSignal> {
        self.ctl.signals()
    }

    pub fn control(&self) -> Arc<FakeChildControl> {
        self.ctl.clone()
    }
}

/// Awaits `fut`, panicking after [`EVENT_TIMEOUT`] so a lost wake-up fails the test instead of
/// hanging the suite.
pub async fn within<F: Future>(what: &str, fut: F) -> F::Output {
    tokio::time::timeout(EVENT_TIMEOUT, fut).await.unwrap_or_else(|_| panic!("timed out waiting for {what}"))
}

/// A shell script printing `version_line` for `--version`.
pub fn fake_binary(dir: &Path, name: &str, version_line: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\necho '{version_line}'\n")).expect("write fake binary");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    path
}

/// Config with a temp log dir and fake binaries.
pub fn config(dir: &Path) -> HarnessConfig {
    let mut cfg = HarnessConfig { log_dir: dir.join("logs"), ..HarnessConfig::default() };
    cfg.omp.binary = fake_binary(dir, "omp", "omp/18.3.1").to_string_lossy().into_owned();
    cfg.claude.binary = fake_binary(dir, "claude", "2.1.269 (Claude Code)").to_string_lossy().into_owned();
    cfg
}

/// Compact text form of an event for golden comparisons.
pub fn summary(e: &HarnessEvent) -> String {
    match e {
        HarnessEvent::Ready(info) => format!("ready {} tools={}", info.model, info.tools.len()),
        HarnessEvent::TurnStarted { turn } => format!("turn_started {turn}"),
        HarnessEvent::AssistantText { turn, text } => format!("text {turn} {text}"),
        HarnessEvent::ToolCall { turn, tool, args_preview, .. } => format!("call {turn} {} {args_preview}", tool.event_name()),
        HarnessEvent::ToolResult { turn, tool, ok, .. } => format!("result {turn} {} {}", tool.event_name(), if *ok { "ok" } else { "err" }),
        HarnessEvent::Usage(u) => format!(
            "usage in={} out={} cr={} cw={} cost={:?} calls={}",
            u.input_tokens, u.output_tokens, u.cache_read_tokens, u.cache_write_tokens, u.cost_micros, u.model_calls
        ),
        HarnessEvent::Notice { level, text } => {
            let level = match level {
                NoticeLevel::Info => "info",
                NoticeLevel::Warning => "warning",
                NoticeLevel::Error => "error",
            };
            format!("notice {level} {text}")
        }
        HarnessEvent::TurnEnded { turn, outcome } => match outcome {
            TurnOutcome::Completed => format!("turn_ended {turn} completed"),
            TurnOutcome::Aborted => format!("turn_ended {turn} aborted"),
            TurnOutcome::Failed { message, retryable, http_status } => {
                format!("turn_ended {turn} failed {message} retryable={retryable} status={http_status:?}")
            }
        },
        HarnessEvent::Idle => "idle".into(),
    }
}

/// Fixture lines (`testdata/*.jsonl`).
pub fn fixture_lines(text: &str) -> Vec<serde_json::Value> {
    text.lines().filter(|l| !l.trim().is_empty()).map(|l| serde_json::from_str(l).expect("fixture line")).collect()
}
