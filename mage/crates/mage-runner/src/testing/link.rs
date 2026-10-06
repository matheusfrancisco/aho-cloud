//! `FakeServerLink`: an in-memory server side of `/runner/v1` for runner use-case tests.
//!
//! It records every call in order, keeps per-run status, events, commands and artifacts,
//! applies the heartbeat phase rows (`EnvPreparing`, `Started`) and submit acks to the run
//! status, long-polls `lease` / `poll_commands` on a notifier, and answers `submit_result` and
//! `heartbeat` from scripts.

use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use mage_core::domain::{ArtifactId, ArtifactKind, RunCommand, RunId, RunStatus, RunnerId};
use mage_core::ports::blob::HttpMethod;
use mage_core::wire::runner::{
    ArtifactSpec, Capabilities, CommandBatch, CommandEnvelope, Download, DownloadRef, EventsAck, FinishReport, HeartbeatAck, HeldRun,
    LeaseRequest, LeaseTerms, OutboundEvent, OutboundEventKind, RegistrationAck, ReleasedRun, RunAssignment, RunnerPhase, SubmitAck,
    UploadGrant, WireRunResult,
};
use parking_lot::Mutex;
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use time::macros::datetime;
use tokio::sync::Notify;
use uuid::Uuid;

use crate::ports::link::{LinkConflict, LinkError, ServerLink};

/// Fixed times of the fake: URLs and tokens expire far in the future.
const ISSUED: OffsetDateTime = datetime!(2026-01-01 00:00 UTC);
const FAR: OffsetDateTime = datetime!(2100-01-01 00:00 UTC);

#[derive(Debug, Clone, PartialEq)]
pub enum LinkCall {
    Register { capabilities: Capabilities, held: Vec<HeldRun> },
    Lease { request_id: Uuid, free: mage_core::wire::runner::SlotCounts },
    Heartbeat { run: RunId, attempt: u16, phase: RunnerPhase },
    PushEvents { run: RunId, attempt: u16, local_seqs: Vec<u64> },
    PollCommands { run: RunId, attempt: u16, after: u64 },
    RequestUpload { run: RunId, attempt: u16, kind: ArtifactKind, name: String },
    Upload { artifact: ArtifactId },
    CompleteUpload { run: RunId, attempt: u16, artifact: ArtifactId },
    RefreshDownloads { run: RunId, attempt: u16, refs: Vec<DownloadRef> },
    Download { run: RunId, attempt: u16, what: DownloadRef },
    SubmitResult { run: RunId, attempt: u16, submission_id: Uuid, result: WireRunResult },
    Finish { run: RunId, attempt: u16, report: FinishReport },
}

/// How `heartbeat` answers.
#[derive(Debug, Clone)]
pub enum HeartbeatScript {
    /// `{status, lease, commands_pending}` after applying the phase rows.
    Answer,
    /// Never answers (network partition).
    Hang,
    Fail(LinkError),
}

#[derive(Debug, Clone, PartialEq)]
pub struct FakeArtifact {
    pub id: ArtifactId,
    pub run: RunId,
    pub attempt: u16,
    pub kind: ArtifactKind,
    pub name: String,
    pub bytes: Vec<u8>,
    pub complete: bool,
}

#[derive(Default)]
struct RunState {
    status: Option<RunStatus>,
    commands: Vec<CommandEnvelope>,
    events: Vec<OutboundEvent>,
    acked: u64,
}

struct State {
    calls: Vec<LinkCall>,
    runs: HashMap<RunId, RunState>,
    assignments: VecDeque<RunAssignment>,
    released: Vec<ReleasedRun>,
    submits: VecDeque<Result<SubmitAck, LinkError>>,
    heartbeat: HeartbeatScript,
    lease_error: Option<LinkError>,
    terms: LeaseTerms,
    blobs: HashMap<DownloadRef, (Option<String>, Vec<u8>)>,
    artifacts: Vec<FakeArtifact>,
    runner_id: RunnerId,
}

pub struct FakeServerLink {
    state: Mutex<State>,
    /// Commands appended, statuses changed, assignments queued: wakes long-polls.
    server_changed: Notify,
    /// Any call recorded: wakes [`FakeServerLink::wait_for`].
    called: Notify,
}

impl FakeServerLink {
    pub fn new() -> Arc<Self> {
        Arc::new(FakeServerLink {
            state: Mutex::new(State {
                calls: Vec::new(),
                runs: HashMap::new(),
                assignments: VecDeque::new(),
                released: Vec::new(),
                submits: VecDeque::new(),
                heartbeat: HeartbeatScript::Answer,
                lease_error: None,
                terms: LeaseTerms { ttl: Duration::from_secs(60), heartbeat_every: Duration::from_secs(15) },
                blobs: HashMap::new(),
                artifacts: Vec::new(),
                runner_id: Uuid::from_u128(0x0190_0000_0000_7000_8000_0000_0000_0001).into(),
            }),
            server_changed: Notify::new(),
            called: Notify::new(),
        })
    }

    fn record(&self, call: LinkCall) {
        self.state.lock().calls.push(call);
        self.called.notify_waiters();
    }

    fn changed(&self) {
        self.server_changed.notify_waiters();
    }

    /// Lease terms of heartbeat answers and registrations.
    pub fn set_terms(&self, terms: LeaseTerms) {
        self.state.lock().terms = terms;
    }

    pub fn set_status(&self, run: RunId, status: RunStatus) {
        self.state.lock().runs.entry(run).or_default().status = Some(status);
        self.changed();
    }

    pub fn status(&self, run: RunId) -> Option<RunStatus> {
        self.state.lock().runs.get(&run).and_then(|r| r.status)
    }

    /// Appends a command for `(run, attempt)`; returns its seq.
    pub fn push_command(&self, run: RunId, attempt: u16, command: RunCommand) -> u64 {
        let seq = {
            let mut state = self.state.lock();
            let commands = &mut state.runs.entry(run).or_default().commands;
            let seq = commands.len() as u64 + 1;
            commands.push(CommandEnvelope { seq, attempt, issued_at: ISSUED, command });
            seq
        };
        self.changed();
        seq
    }

    pub fn queue_assignment(&self, a: RunAssignment) {
        {
            let mut state = self.state.lock();
            state.runs.entry(a.run_id).or_default().status = Some(RunStatus::Leased);
            state.assignments.push_back(a);
        }
        self.changed();
    }

    /// Next `submit_result` answer; without a script the answer is `Succeeded`, no gate.
    /// An `Ok` ack moves the run to `ack.status`.
    pub fn script_submit(&self, answer: Result<SubmitAck, LinkError>) {
        self.state.lock().submits.push_back(answer);
    }

    pub fn script_heartbeat(&self, script: HeartbeatScript) {
        self.state.lock().heartbeat = script;
    }

    /// Every `lease` fails with `error` (e.g. 403 for a revoked runner) until reset with `None`.
    pub fn fail_leases(&self, error: Option<LinkError>) {
        self.state.lock().lease_error = error;
    }

    /// Runs `register` reports as released.
    pub fn set_released(&self, released: Vec<ReleasedRun>) {
        self.state.lock().released = released;
    }

    /// Serves `bytes` for `what`; returns the signed download the server would hand out.
    pub fn add_blob(&self, what: DownloadRef, name: Option<&str>, bytes: Vec<u8>) -> Download {
        let dl = download(what, name.map(str::to_owned), &bytes);
        self.state.lock().blobs.insert(what, (name.map(str::to_owned), bytes));
        dl
    }

    pub fn calls(&self) -> Vec<LinkCall> {
        self.state.lock().calls.clone()
    }

    /// Events ingested for `run`, in `local_seq` order.
    pub fn events(&self, run: RunId) -> Vec<OutboundEventKind> {
        self.state.lock().runs.get(&run).map(|r| r.events.iter().map(|e| e.kind.clone()).collect()).unwrap_or_default()
    }

    pub fn artifacts(&self) -> Vec<FakeArtifact> {
        self.state.lock().artifacts.clone()
    }

    pub fn finishes(&self) -> Vec<(RunId, u16, FinishReport)> {
        self.state
            .lock()
            .calls
            .iter()
            .filter_map(|c| match c {
                LinkCall::Finish { run, attempt, report } => Some((*run, *attempt, report.clone())),
                _ => None,
            })
            .collect()
    }

    /// Waits until `pred` holds for the recorded calls (or panics after `timeout`).
    pub async fn wait_for(&self, timeout: Duration, pred: impl Fn(&[LinkCall]) -> bool) {
        let wait = async {
            loop {
                let notified = self.called.notified();
                if pred(&self.state.lock().calls) {
                    return;
                }
                notified.await;
            }
        };
        if tokio::time::timeout(timeout, wait).await.is_err() {
            panic!("condition on link calls not met within {timeout:?}; calls: {:#?}", self.calls());
        }
    }

    fn run_status(&self, run: RunId) -> RunStatus {
        self.status(run).unwrap_or(RunStatus::Leased)
    }
}

fn download(what: DownloadRef, name: Option<String>, bytes: &[u8]) -> Download {
    Download { what, url: format!("fake://{what:?}"), bytes: bytes.len() as u64, sha256: Sha256::digest(bytes).into(), expires_at: FAR, name }
}

#[async_trait]
impl ServerLink for FakeServerLink {
    async fn register(&self, capabilities: &Capabilities, held: &[HeldRun]) -> Result<RegistrationAck, LinkError> {
        self.record(LinkCall::Register { capabilities: capabilities.clone(), held: held.to_vec() });
        let state = self.state.lock();
        Ok(RegistrationAck {
            runner_id: state.runner_id,
            runner_token: "mgr_fake-runner-token".into(),
            expires_at: FAR,
            released: state.released.clone(),
            lease: state.terms,
            long_poll_max: Duration::from_secs(25),
            min_version: "0.1.0".into(),
        })
    }

    async fn lease(&self, req: &LeaseRequest) -> Result<Option<RunAssignment>, LinkError> {
        self.record(LinkCall::Lease { request_id: req.request_id, free: req.free });
        if let Some(error) = self.state.lock().lease_error.clone() {
            return Err(error);
        }
        let wait = async {
            loop {
                let notified = self.server_changed.notified();
                if let Some(a) = self.state.lock().assignments.pop_front() {
                    return a;
                }
                notified.await;
            }
        };
        Ok(tokio::time::timeout(req.wait, wait).await.ok())
    }

    async fn heartbeat(&self, run: RunId, attempt: u16, phase: RunnerPhase) -> Result<HeartbeatAck, LinkError> {
        self.record(LinkCall::Heartbeat { run, attempt, phase });
        let script = self.state.lock().heartbeat.clone();
        match script {
            HeartbeatScript::Hang => std::future::pending().await,
            HeartbeatScript::Fail(e) => Err(e),
            HeartbeatScript::Answer => {
                let ack = {
                    let mut state = self.state.lock();
                    let terms = state.terms;
                    let entry = state.runs.entry(run).or_default();
                    let mut status = entry.status.unwrap_or(RunStatus::Leased);
                    if status == RunStatus::Leased && phase >= RunnerPhase::Preparing {
                        status = RunStatus::Preparing;
                    }
                    if status == RunStatus::Preparing && phase >= RunnerPhase::Running {
                        status = RunStatus::Running;
                    }
                    entry.status = Some(status);
                    HeartbeatAck { status, lease: terms, commands_pending: false }
                };
                self.changed();
                Ok(ack)
            }
        }
    }

    async fn push_events(&self, run: RunId, attempt: u16, events: &[OutboundEvent]) -> Result<EventsAck, LinkError> {
        self.record(LinkCall::PushEvents { run, attempt, local_seqs: events.iter().map(|e| e.local_seq).collect() });
        let mut state = self.state.lock();
        let entry = state.runs.entry(run).or_default();
        for event in events {
            if event.local_seq > entry.acked {
                entry.acked = event.local_seq;
                entry.events.push(event.clone());
            }
        }
        Ok(EventsAck { acked_local_seq: entry.acked, last_seq: entry.events.len() as u64, status: entry.status.unwrap_or(RunStatus::Leased) })
    }

    async fn poll_commands(&self, run: RunId, attempt: u16, after: u64, wait: Duration) -> Result<CommandBatch, LinkError> {
        self.record(LinkCall::PollCommands { run, attempt, after });
        let wait_for_commands = async {
            loop {
                let notified = self.server_changed.notified();
                {
                    let state = self.state.lock();
                    if let Some(entry) = state.runs.get(&run) {
                        let commands: Vec<CommandEnvelope> =
                            entry.commands.iter().filter(|c| c.seq > after && c.attempt == attempt).cloned().collect();
                        if !commands.is_empty() {
                            return commands;
                        }
                    }
                }
                notified.await;
            }
        };
        let commands = tokio::time::timeout(wait, wait_for_commands).await.unwrap_or_default();
        Ok(CommandBatch { commands, status: self.run_status(run) })
    }

    async fn request_upload(&self, run: RunId, attempt: u16, spec: &ArtifactSpec) -> Result<UploadGrant, LinkError> {
        self.record(LinkCall::RequestUpload { run, attempt, kind: spec.kind, name: spec.name.clone() });
        if !ArtifactSpec::name_valid(&spec.name) || !ArtifactSpec::kind_allowed(spec.kind) {
            return Err(LinkError::Invalid(format!("artifact {:?} {}", spec.kind, spec.name)));
        }
        let mut state = self.state.lock();
        let existing = state.artifacts.iter().find(|a| a.run == run && a.attempt == attempt && a.kind == spec.kind && a.name == spec.name);
        let (id, complete) = match existing {
            Some(a) => (a.id, a.complete),
            None => {
                let id: ArtifactId = Uuid::from_u128(0x0190_0000_0000_7000_8000_0000_0001_0000 + state.artifacts.len() as u128).into();
                state.artifacts.push(FakeArtifact { id, run, attempt, kind: spec.kind, name: spec.name.clone(), bytes: Vec::new(), complete: false });
                (id, false)
            }
        };
        Ok(UploadGrant {
            artifact_id: id,
            url: format!("fake://upload/{id}"),
            method: HttpMethod::Put,
            headers: vec![("content-type".into(), spec.content_type.clone())],
            expires_at: FAR,
            already_complete: complete,
        })
    }

    async fn upload(&self, grant: &UploadGrant, file: &Path) -> Result<(), LinkError> {
        self.record(LinkCall::Upload { artifact: grant.artifact_id });
        let bytes = tokio::fs::read(file).await.map_err(|e| LinkError::Invalid(format!("read {}: {e}", file.display())))?;
        let mut state = self.state.lock();
        let artifact = state.artifacts.iter_mut().find(|a| a.id == grant.artifact_id).ok_or(LinkError::NotFound)?;
        artifact.bytes = bytes;
        Ok(())
    }

    async fn complete_upload(&self, run: RunId, attempt: u16, artifact: ArtifactId) -> Result<(), LinkError> {
        self.record(LinkCall::CompleteUpload { run, attempt, artifact });
        let mut state = self.state.lock();
        let found = state.artifacts.iter_mut().find(|a| a.id == artifact && a.run == run).ok_or(LinkError::NotFound)?;
        found.complete = true;
        Ok(())
    }

    async fn refresh_downloads(&self, run: RunId, attempt: u16, refs: &[DownloadRef]) -> Result<Vec<Download>, LinkError> {
        self.record(LinkCall::RefreshDownloads { run, attempt, refs: refs.to_vec() });
        let state = self.state.lock();
        refs.iter()
            .map(|what| state.blobs.get(what).map(|(name, bytes)| download(*what, name.clone(), bytes)).ok_or(LinkError::Forbidden))
            .collect()
    }

    async fn download(&self, run: RunId, attempt: u16, dl: &Download, dest: &Path) -> Result<(), LinkError> {
        self.record(LinkCall::Download { run, attempt, what: dl.what });
        let bytes = self.state.lock().blobs.get(&dl.what).map(|(_, b)| b.clone()).ok_or(LinkError::NotFound)?;
        let digest: [u8; 32] = Sha256::digest(&bytes).into();
        if digest != dl.sha256 || bytes.len() as u64 != dl.bytes {
            return Err(LinkError::Invalid("download integrity check failed".into()));
        }
        tokio::fs::write(dest, &bytes).await.map_err(|e| LinkError::Invalid(format!("write {}: {e}", dest.display())))
    }

    async fn submit_result(&self, run: RunId, attempt: u16, submission_id: Uuid, result: &WireRunResult) -> Result<SubmitAck, LinkError> {
        self.record(LinkCall::SubmitResult { run, attempt, submission_id, result: result.clone() });
        let answer = self.state.lock().submits.pop_front();
        let answer = answer.unwrap_or_else(|| Ok(SubmitAck { status: RunStatus::Succeeded, gate: None, message: "Accepted. End your turn.".into() }));
        match &answer {
            Ok(ack) => self.set_status(run, ack.status),
            Err(LinkError::Conflict(LinkConflict::RunTerminal(s) | LinkConflict::InvalidState(s))) => self.set_status(run, *s),
            Err(_) => {}
        }
        answer
    }

    async fn finish(&self, run: RunId, attempt: u16, report: &FinishReport) -> Result<RunStatus, LinkError> {
        self.record(LinkCall::Finish { run, attempt, report: report.clone() });
        Ok(self.run_status(run))
    }
}
