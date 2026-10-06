//! `FakeBrowserRunner`: scripted `BrowserRunner` outcomes (executor.md "Testing": "fake returns
//! scripted outcomes"). Outcomes are served FIFO, then the default; every call is recorded.
//! Artifacts of a scripted outcome are materialized in `RecordOpts.outbox` as
//! `<label>.<name>` (bytes = the name), so uploads of them work like the real adapter's exports.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use mage_core::domain::ArtifactKind;
use sha2::{Digest, Sha256};

use crate::ports::browser::{
    BrowserError, BrowserOutcome, BrowserRunner, BrowserScript, BrowserStatus, LocalArtifact, RecordOpts, TestFailure,
};
use crate::ports::executor::{Env, EnvId};

/// One recorded `run` call.
#[derive(Debug, Clone)]
pub struct BrowserCall {
    pub env: EnvId,
    pub script: BrowserScript,
    pub opts: RecordOpts,
}

/// A scripted answer. `artifacts` are `(kind, file)` pairs materialized as `<label>.<file>`.
#[derive(Debug, Clone)]
pub struct Scripted {
    pub outcome: Result<BrowserOutcome, BrowserError>,
    pub artifacts: Vec<(ArtifactKind, String)>,
}

pub struct FakeBrowserRunner {
    queue: parking_lot::Mutex<VecDeque<Scripted>>,
    default: parking_lot::Mutex<Scripted>,
    calls: parking_lot::Mutex<Vec<BrowserCall>>,
    delay: parking_lot::Mutex<Duration>,
    cancelled: Arc<AtomicUsize>,
}

/// Counts a `run` future dropped before it finished (a cancelled tool call).
struct CancelProbe {
    cancelled: Arc<AtomicUsize>,
    finished: bool,
}

impl Drop for CancelProbe {
    fn drop(&mut self) {
        if !self.finished {
            self.cancelled.fetch_add(1, Ordering::SeqCst);
        }
    }
}

impl Default for FakeBrowserRunner {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeBrowserRunner {
    /// Every run passes with no artifacts unless scripted otherwise.
    pub fn new() -> Self {
        Self {
            queue: parking_lot::Mutex::new(VecDeque::new()),
            default: parking_lot::Mutex::new(Scripted { outcome: Ok(passed()), artifacts: Vec::new() }),
            calls: parking_lot::Mutex::new(Vec::new()),
            delay: parking_lot::Mutex::new(Duration::ZERO),
            cancelled: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Every run takes `delay` before answering (to exercise cancellation).
    pub fn set_delay(&self, delay: Duration) {
        *self.delay.lock() = delay;
    }

    /// Runs dropped before they answered.
    pub fn cancelled(&self) -> usize {
        self.cancelled.load(Ordering::SeqCst)
    }

    /// Next answer (FIFO).
    pub fn push(&self, outcome: Result<BrowserOutcome, BrowserError>) {
        self.queue.lock().push_back(Scripted { outcome, artifacts: Vec::new() });
    }

    /// Next answer with recordings: video (`video.webm`), trace (`trace.zip`), HAR, and report.
    pub fn push_recorded(&self, outcome: BrowserOutcome) {
        self.queue.lock().push_back(Scripted { outcome: Ok(outcome), artifacts: recordings() });
    }

    pub fn set_default(&self, outcome: Result<BrowserOutcome, BrowserError>, recorded: bool) {
        *self.default.lock() = Scripted { outcome, artifacts: if recorded { recordings() } else { Vec::new() } };
    }

    pub fn calls(&self) -> Vec<BrowserCall> {
        self.calls.lock().clone()
    }
}

fn recordings() -> Vec<(ArtifactKind, String)> {
    vec![
        (ArtifactKind::Video, "video.webm".to_owned()),
        (ArtifactKind::Trace, "trace.zip".to_owned()),
        (ArtifactKind::Log, "network.har".to_owned()),
        (ArtifactKind::Log, "report.json".to_owned()),
    ]
}

/// A passing outcome.
pub fn passed() -> BrowserOutcome {
    BrowserOutcome {
        passed: true,
        status: BrowserStatus::Passed,
        error_text: None,
        failures: Vec::new(),
        video: None,
        trace: None,
        screenshots: Vec::new(),
        har: None,
        report: None,
        http_statuses: Vec::new(),
        stdout_tail: "  1 passed".to_owned(),
        duration: Duration::from_millis(1200),
    }
}

/// A failing outcome with one assertion failure and the given HTTP error statuses.
pub fn failed(message: &str, http_statuses: &[u16]) -> BrowserOutcome {
    BrowserOutcome {
        passed: false,
        status: BrowserStatus::Failed,
        error_text: Some(message.to_owned()),
        failures: vec![TestFailure { title: "repro.spec.ts › repro".to_owned(), message: message.to_owned(), location: None }],
        http_statuses: http_statuses.to_vec(),
        stdout_tail: "  1 failed".to_owned(),
        ..passed()
    }
}

fn content_type(file: &str) -> &'static str {
    match file.rsplit('.').next() {
        Some("webm") => "video/webm",
        Some("zip") => "application/zip",
        Some("png") => "image/png",
        _ => "application/json",
    }
}

#[async_trait]
impl BrowserRunner for FakeBrowserRunner {
    async fn run(&self, env: &dyn Env, script: &BrowserScript, opts: &RecordOpts) -> Result<BrowserOutcome, BrowserError> {
        self.calls.lock().push(BrowserCall { env: env.id(), script: script.clone(), opts: opts.clone() });
        let mut probe = CancelProbe { cancelled: self.cancelled.clone(), finished: false };
        let delay = *self.delay.lock();
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        probe.finished = true;
        let scripted = self.queue.lock().pop_front().unwrap_or_else(|| self.default.lock().clone());
        let mut outcome = scripted.outcome?;
        for (kind, file) in scripted.artifacts {
            let recorded = match kind {
                ArtifactKind::Video => opts.video,
                ArtifactKind::Trace => opts.trace,
                ArtifactKind::Log if file == "network.har" => opts.har,
                _ => true,
            };
            if !recorded {
                continue;
            }
            let name = format!("{}.{file}", opts.label);
            let path: PathBuf = opts.outbox.join(&name);
            std::fs::create_dir_all(&opts.outbox).map_err(|e| BrowserError::Unavailable(e.to_string()))?;
            std::fs::write(&path, name.as_bytes()).map_err(|e| BrowserError::Unavailable(e.to_string()))?;
            let local = LocalArtifact {
                kind,
                name: name.clone(),
                path,
                bytes: name.len() as u64,
                sha256: Sha256::digest(name.as_bytes()).into(),
                content_type: content_type(&file).to_owned(),
            };
            match (kind, file.as_str()) {
                (ArtifactKind::Video, _) => outcome.video = Some(local),
                (ArtifactKind::Trace, _) => outcome.trace = Some(local),
                (ArtifactKind::Screenshot, _) => outcome.screenshots.push(local),
                (_, "network.har") => outcome.har = Some(local),
                _ => outcome.report = Some(local),
            }
        }
        Ok(outcome)
    }
}
