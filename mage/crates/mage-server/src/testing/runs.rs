//! In-memory `RunStore` + `RunSignals` (runs.md "Testing"): one broadcast hub shared by both, so a
//! commit wakes subscribers exactly like the pg adapter's `NOTIFY`. Jobs go to a `MemJobQueue`
//! in the same "transaction" (all or nothing). Run the contract suite
//! ([`super::runs_contract::contract_run_store`]) against it and against `PgRunStore`.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use parking_lot::Mutex;
use time::OffsetDateTime;
use uuid::Uuid;

use mage_core::domain::{
    Artifact, ArtifactId, Budget, CommitSha, GateStatus, HarnessKind, OrgId, RepoId, Run, RunEvent, RunId, RunInit, RunKind, RunStatus,
    RunSubject, RunnerId,
};
use mage_core::ports::blob::BlobKey;
use mage_core::ports::clock::Clock;
use mage_core::testing::MemJobQueue;
use mage_core::wire::runner::{CredentialModeKind, ExecutorKind, SlotClass};

use crate::adapters::driven::pg::signals::SignalHub;
use crate::domain::runs::{RunRequirements, RunSpec};
use crate::ports::runs::{
    ArtifactRecord, ArtifactState, CommandRecord, CommitReceipt, DueItem, DueReason, DueThresholds, GateRecord, GateWrite, LeaseFence,
    LeaseQuery, NewArtifact, RunConflict, RunQuery, RunRecord, RunSignals, RunStore, RunStoreError, RunWrite, RunnerSession, SignalTopic,
    RunUsage, StoredReply,
};

/// In-memory `RunSignals` (the hub the fake store notifies).
pub type MemRunSignals = SignalHub;

#[derive(Debug, Clone)]
struct StoredRun {
    rec: RunRecord,
    create_key: Option<String>,
    /// Creation or the last requeue (`QueueWait`).
    queued_since: OffsetDateTime,
}

#[derive(Debug, Clone)]
struct StoredGate {
    rec: GateRecord,
}

#[derive(Debug, Clone)]
struct AttemptRow {
    run: RunId,
    attempt: u16,
    runner: RunnerId,
    ended_at: Option<OffsetDateTime>,
}

#[derive(Debug, Clone, Default)]
struct State {
    runs: BTreeMap<RunId, StoredRun>,
    events: HashMap<RunId, Vec<RunEvent>>,
    commands: HashMap<RunId, Vec<CommandRecord>>,
    gates: Vec<StoredGate>,
    artifacts: Vec<ArtifactRecord>,
    replies: HashMap<(String, String), StoredReply>,
    sessions: HashMap<RunnerId, RunnerSession>,
    attempts: Vec<AttemptRow>,
}

impl State {
    fn run(&self, org: OrgId, id: RunId) -> Result<&StoredRun, RunStoreError> {
        self.runs.get(&id).filter(|s| s.rec.org() == org).ok_or(RunStoreError::NotFound)
    }

    fn run_mut(&mut self, org: OrgId, id: RunId) -> Result<&mut StoredRun, RunStoreError> {
        self.runs.get_mut(&id).filter(|s| s.rec.org() == org).ok_or(RunStoreError::NotFound)
    }

    fn append_event(&mut self, run: RunId, attempt: u16, at: OffsetDateTime, kind: mage_core::domain::RunEventKind) -> u64 {
        let stored = self.runs.get_mut(&run).expect("checked by caller");
        stored.rec.last_seq += 1;
        let seq = stored.rec.last_seq;
        let _ = attempt;
        self.events.entry(run).or_default().push(RunEvent { run_id: run, seq, at, kind });
        seq
    }

    fn held_work_slots(&self, org: OrgId, repo: RepoId) -> usize {
        self.runs.values().filter(|s| s.rec.lease.is_some() && s.rec.work_slot() == Some((org, repo))).count()
    }
}

/// Fake `RunStore`.
pub struct MemRunStore {
    clock: Arc<dyn Clock>,
    jobs: Arc<MemJobQueue>,
    hub: Arc<SignalHub>,
    thresholds: Mutex<DueThresholds>,
    next_artifact: AtomicU64,
    state: Mutex<State>,
}

impl MemRunStore {
    pub fn new(clock: Arc<dyn Clock>, jobs: Arc<MemJobQueue>) -> MemRunStore {
        MemRunStore {
            clock,
            jobs,
            hub: Arc::new(SignalHub::new()),
            thresholds: Mutex::new(DueThresholds::default()),
            next_artifact: AtomicU64::new(0),
            state: Mutex::new(State::default()),
        }
    }

    pub fn with_thresholds(self, thresholds: DueThresholds) -> MemRunStore {
        *self.thresholds.lock() = thresholds;
        self
    }

    /// The `RunSignals` this store notifies.
    pub fn signals(&self) -> Arc<MemRunSignals> {
        self.hub.clone()
    }

    pub fn jobs(&self) -> &Arc<MemJobQueue> {
        &self.jobs
    }

    /// Test inspection: every stored run.
    pub fn all(&self) -> Vec<RunRecord> {
        self.state.lock().runs.values().map(|s| s.rec.clone()).collect()
    }

    /// Test inspection: the runner session last recorded for `runner`.
    pub fn session(&self, runner: RunnerId) -> Option<RunnerSession> {
        self.state.lock().sessions.get(&runner).cloned()
    }

    /// Test inspection: `(attempt, runner, ended)` rows of `run_attempts`.
    pub fn attempt_rows(&self, run: RunId) -> Vec<(u16, RunnerId, bool)> {
        self.state.lock().attempts.iter().filter(|a| a.run == run).map(|a| (a.attempt, a.runner, a.ended_at.is_some())).collect()
    }

    fn artifact_id(&self) -> ArtifactId {
        let n = self.next_artifact.fetch_add(1, Ordering::Relaxed) + 1;
        ArtifactId::from_uuid(Uuid::from_u128((0xa27f_u128 << 112) | u128::from(n)))
    }
}

/// Lease identity of an update equals the stored one: keep the later `until`.
fn merge_lease(stored: &RunRecord, rec: &mut RunRecord) {
    if let (Some(old), Some(new)) = (stored.lease, rec.lease.as_mut())
        && old.fence() == new.fence()
        && old.until > new.until
    {
        new.until = old.until;
        if let Some(runner) = rec.run.runner {
            let mut data = rec.run.clone().into_data();
            data.runner = Some(mage_core::domain::RunnerLease { until: old.until, ..runner });
            rec.run = Run::rehydrate(data);
        }
    }
}

#[async_trait]
impl RunStore for MemRunStore {
    async fn get(&self, org: OrgId, id: RunId) -> Result<RunRecord, RunStoreError> {
        Ok(self.state.lock().run(org, id)?.rec.clone())
    }

    async fn list(&self, org: OrgId, q: &RunQuery) -> Result<Vec<RunRecord>, RunStoreError> {
        let state = self.state.lock();
        let mut rows: Vec<&RunRecord> = state.runs.values().map(|s| &s.rec).filter(|r| r.org() == org && q.matches(r)).collect();
        rows.sort_by_key(|r| (r.run.created_at, r.run.id));
        if q.newest_first {
            rows.reverse();
        }
        if let Some(cursor) = q.cursor {
            match rows.iter().position(|r| r.run.id == cursor) {
                Some(pos) => {
                    rows.drain(..=pos);
                }
                None => return Err(RunStoreError::Invalid("unknown cursor".into())),
            }
        }
        Ok(rows.into_iter().take(q.effective_limit() as usize).cloned().collect())
    }

    async fn commit(&self, org: OrgId, w: RunWrite) -> Result<CommitReceipt, RunStoreError> {
        MemJobQueue::validate_all(&w.jobs).map_err(|e| RunStoreError::Invalid(e.to_string()))?;
        let now = self.clock.now();
        let mut guard = self.state.lock();
        let mut s = guard.clone();
        let mut receipt = CommitReceipt::default();
        let mut topics: Vec<SignalTopic> = Vec::new();

        if let Some(reply) = &w.reply
            && let Some(stored) = s.replies.get(&(reply.scope.clone(), reply.key.clone()))
        {
            return Err(RunStoreError::Conflict(RunConflict::ReplyExists(stored.clone())));
        }

        for new in w.inserts {
            if new.run.org_id != org {
                return Err(RunStoreError::Invalid("run of another org".into()));
            }
            if s.runs.contains_key(&new.run.id) {
                return Err(RunStoreError::Conflict(RunConflict::Duplicate { run: new.run.id }));
            }
            if let Some(key) = &new.create_key
                && let Some(existing) = s.runs.values().find(|r| r.rec.org() == org && r.create_key.as_ref() == Some(key))
            {
                return Err(RunStoreError::Conflict(RunConflict::Duplicate { run: existing.rec.id() }));
            }
            if let Some(parent) = new.run.parent {
                s.run(org, parent).map_err(|_| RunStoreError::Invalid(format!("dangling reference: parent {parent}")))?;
            }
            let busy = |s: &State, kind: RunKind, subject: RunSubject, id: RunId| {
                matches!(kind, RunKind::Diagnose | RunKind::Fix)
                    && s.runs.values().any(|r| {
                        r.rec.org() == org
                            && r.rec.id() != id
                            && r.rec.run.kind == kind
                            && r.rec.run.subject == subject
                            && !r.rec.run.status.is_terminal()
                    })
            };
            if !new.run.status.is_terminal() && busy(&s, new.run.kind, new.run.subject, new.run.id) {
                return Err(RunStoreError::Conflict(RunConflict::SubjectBusy));
            }
            let mut rec = RunRecord::new(new.run, new.spec, new.source_ready);
            rec.version = 1;
            rec.affinity = new.affinity;
            if rec.leasable() {
                topics.push(SignalTopic::Queue);
            }
            receipt.versions.push((rec.id(), 1));
            s.runs.insert(rec.id(), StoredRun { rec, create_key: new.create_key, queued_since: now });
        }

        for update in w.updates {
            let id = update.record.id();
            let stored = s.run(org, id)?.clone();
            if stored.rec.version != update.expected_version {
                return Err(RunStoreError::Conflict(RunConflict::StaleVersion { run: id }));
            }
            let mut rec = update.record;
            if rec.org() != org {
                return Err(RunStoreError::Invalid("run of another org".into()));
            }
            rec.last_seq = stored.rec.last_seq;
            rec.last_cmd_seq = stored.rec.last_cmd_seq;
            rec.ingest_cursor = stored.rec.ingest_cursor;
            rec.version = stored.rec.version + 1;
            merge_lease(&stored.rec, &mut rec);
            let old = &stored.rec;
            if old.lease.is_some() && rec.lease.is_none() {
                topics.push(SignalTopic::Queue);
                if let Some(l) = old.lease {
                    for row in s.attempts.iter_mut().filter(|a| a.run == id && a.attempt == l.attempt && a.ended_at.is_none()) {
                        row.ended_at = Some(now);
                    }
                }
            }
            let requeued = rec.run.status == RunStatus::Queued && old.run.status != RunStatus::Queued;
            if rec.leasable() && (!old.leasable() || requeued) {
                topics.push(SignalTopic::Queue);
            }
            if rec.run.kind != old.run.kind || rec.run.subject != old.run.subject || rec.run.org_id != old.run.org_id {
                return Err(RunStoreError::Invalid("kind, subject and org are immutable".into()));
            }
            receipt.versions.push((id, rec.version));
            let entry = s.runs.get_mut(&id).expect("checked");
            entry.rec = rec;
            if requeued {
                entry.queued_since = now;
            }
        }

        for gate in w.gates {
            match gate {
                GateWrite::Insert(g) => {
                    s.run(org, g.gate.run_id)?;
                    if g.gate.org_id != org {
                        return Err(RunStoreError::Invalid("gate of another org".into()));
                    }
                    if s.gates.iter().any(|x| {
                        x.rec.gate.id == g.gate.id
                            || (x.rec.gate.run_id == g.gate.run_id && x.rec.gate.kind == g.gate.kind && x.rec.gate.attempt == g.gate.attempt)
                    }) {
                        return Err(RunStoreError::Invalid(format!("gate {:?} attempt {} exists", g.gate.kind, g.gate.attempt)));
                    }
                    if let Some(v) = g.gate.verify_run {
                        s.run(org, v).map_err(|_| RunStoreError::Invalid(format!("dangling reference: verify run {v}")))?;
                    }
                    s.gates.push(StoredGate { rec: g });
                }
                GateWrite::Update(g) => {
                    if let Some(v) = g.gate.verify_run {
                        s.run(org, v).map_err(|_| RunStoreError::Invalid(format!("dangling reference: verify run {v}")))?;
                    }
                    let slot = s.gates.iter_mut().find(|x| x.rec.gate.id == g.gate.id && x.rec.gate.org_id == org).ok_or(RunStoreError::NotFound)?;
                    let created_at = slot.rec.created_at;
                    slot.rec = GateRecord { created_at, ..g };
                }
            }
        }

        if let Some(ingest) = w.ingest {
            let run = ingest.run_id;
            let cursor = s.run(org, run)?.rec.ingest_cursor;
            let mut first = None;
            let mut acked = cursor.filter(|c| c.attempt == ingest.attempt).map_or(0, |c| c.local_seq);
            let mut newest = cursor;
            for (local_seq, ev) in ingest.events {
                let key = crate::ports::runs::IngestCursor { attempt: ingest.attempt, local_seq };
                if cursor.is_some_and(|c| key <= c) {
                    continue;
                }
                let seq = s.append_event(run, ev.attempt, ev.at, ev.kind);
                first.get_or_insert(seq);
                acked = acked.max(local_seq);
                if newest.is_none_or(|n| key > n) {
                    newest = Some(key);
                }
            }
            let stored = s.run_mut(org, run)?;
            stored.rec.ingest_cursor = newest;
            let last = stored.rec.last_seq;
            if let Some(first) = first {
                receipt.seqs.push((run, first, last));
                topics.push(SignalTopic::RunEvents(run));
            }
            receipt.ingested = Some((acked, last));
        }

        let mut ranges: BTreeMap<RunId, (u64, u64)> = BTreeMap::new();
        for ev in w.events {
            s.run(org, ev.run_id)?;
            let seq = s.append_event(ev.run_id, ev.attempt, ev.at, ev.kind);
            ranges.entry(ev.run_id).and_modify(|r| r.1 = seq).or_insert((seq, seq));
        }
        for (run, (first, last)) in ranges {
            receipt.seqs.push((run, first, last));
            topics.push(SignalTopic::RunEvents(run));
        }

        for cmd in w.commands {
            let stored = s.run_mut(org, cmd.run_id)?;
            stored.rec.last_cmd_seq += 1;
            let seq = stored.rec.last_cmd_seq;
            s.commands.entry(cmd.run_id).or_default().push(CommandRecord { seq, attempt: cmd.attempt, issued_at: now, command: cmd.command });
            receipt.command_seqs.push((cmd.run_id, seq));
            topics.push(SignalTopic::RunCommands(cmd.run_id));
        }

        for (id, bytes) in w.complete_artifacts {
            let a = s.artifacts.iter_mut().find(|a| a.artifact.id == id && a.artifact.org_id == org).ok_or(RunStoreError::NotFound)?;
            a.state = ArtifactState::Complete;
            a.artifact.bytes = bytes;
        }

        if let Some(reply) = w.reply {
            s.replies.insert((reply.scope, reply.key), StoredReply { request_hash: reply.request_hash, body: reply.body });
        }

        *guard = s;
        drop(guard);
        self.jobs.insert_validated(&w.jobs);
        for topic in topics {
            self.hub.notify(topic);
        }
        Ok(receipt)
    }

    async fn locate(&self, id: RunId) -> Result<OrgId, RunStoreError> {
        self.state.lock().runs.get(&id).map(|s| s.rec.org()).ok_or(RunStoreError::NotFound)
    }

    async fn lease_next(&self, q: &LeaseQuery) -> Result<Option<RunRecord>, RunStoreError> {
        let mut s = self.state.lock();
        let mut candidates: Vec<&RunRecord> = s
            .runs
            .values()
            .map(|r| &r.rec)
            .filter(|r| q.admits(r))
            .filter(|r| r.work_slot().is_none_or(|(org, repo)| s.held_work_slots(org, repo) < usize::from(q.repo_limit)))
            .collect();
        candidates.sort_by_key(|r| q.order_key(r));
        let Some(id) = candidates.first().map(|r| r.id()) else { return Ok(None) };
        let stored = s.runs.get_mut(&id).expect("candidate exists");
        let draft = stored.rec.apply_lease(q).map_err(|e| RunStoreError::Invalid(e.to_string()))?;
        stored.rec.version += 1;
        let attempt = stored.rec.run.attempt;
        s.append_event(id, draft.attempt, draft.at, draft.kind);
        s.attempts.push(AttemptRow { run: id, attempt, runner: q.runner_id, ended_at: None });
        let rec = s.runs[&id].rec.clone();
        drop(s);
        self.hub.notify(SignalTopic::RunEvents(id));
        Ok(Some(rec))
    }

    async fn lease_by_request(&self, runner: RunnerId, request_id: uuid::Uuid) -> Result<Option<RunRecord>, RunStoreError> {
        let s = self.state.lock();
        Ok(s.runs
            .values()
            .map(|r| &r.rec)
            .find(|r| {
                r.run.status == RunStatus::Leased && r.lease.is_some_and(|l| l.runner_id == runner && l.request_id == request_id && l.attempt == r.run.attempt)
            })
            .cloned())
    }

    async fn extend_lease(&self, org: OrgId, id: RunId, fence: LeaseFence, until: OffsetDateTime) -> Result<RunRecord, RunStoreError> {
        let mut s = self.state.lock();
        let stored = s.run_mut(org, id)?;
        if !stored.rec.holds(&fence) {
            return Err(RunStoreError::Conflict(RunConflict::LeaseLost));
        }
        if let Some(lease) = stored.rec.lease.as_mut() {
            lease.until = until;
        }
        let mut run = stored.rec.run.clone();
        run.extend_lease(fence.runner_id, until).map_err(|_| RunStoreError::Conflict(RunConflict::LeaseLost))?;
        stored.rec.run = run;
        Ok(stored.rec.clone())
    }

    async fn due(&self, now: OffsetDateTime, limit: u32) -> Result<Vec<DueItem>, RunStoreError> {
        let thresholds = *self.thresholds.lock();
        let s = self.state.lock();
        let mut out = Vec::new();
        for stored in s.runs.values() {
            let rec = &stored.rec;
            let status = rec.run.status;
            let reason = if matches!(status, RunStatus::Preparing | RunStatus::Running) && rec.wall_at(now) > rec.run.budget.max_wall {
                Some(DueReason::WallExceeded)
            } else if rec.lease.is_some_and(|l| l.until < now) {
                Some(DueReason::LeaseExpired)
            } else if status == RunStatus::AwaitingGate
                && s.gates.iter().any(|g| {
                    g.rec.gate.run_id == rec.id() && g.rec.gate.status == GateStatus::Pending && g.rec.created_at + thresholds.gate_wait_max < now
                })
            {
                Some(DueReason::GateWait)
            } else if status == RunStatus::Queued && stored.queued_since + thresholds.queue_max_wait < now {
                Some(DueReason::QueueWait)
            } else {
                None
            };
            if let Some(reason) = reason {
                out.push(DueItem { org_id: rec.org(), run_id: rec.id(), reason });
            }
            if out.len() >= limit as usize {
                break;
            }
        }
        Ok(out)
    }

    async fn held_by(&self, runner: RunnerId) -> Result<Vec<(OrgId, RunId)>, RunStoreError> {
        let s = self.state.lock();
        Ok(s.runs.values().filter(|r| r.rec.lease.is_some_and(|l| l.runner_id == runner)).map(|r| (r.rec.org(), r.rec.id())).collect())
    }

    async fn mark_source_ready(&self, org: OrgId, repo: RepoId, sha: CommitSha) -> Result<u32, RunStoreError> {
        let mut s = self.state.lock();
        let mut n = 0;
        for stored in s.runs.values_mut() {
            let rec = &mut stored.rec;
            if rec.org() == org && rec.run.status == RunStatus::Queued && !rec.source_ready && rec.spec.repo_id == Some(repo) && rec.spec.sha == Some(sha) {
                rec.source_ready = true;
                rec.version += 1;
                n += 1;
            }
        }
        drop(s);
        if n > 0 {
            self.hub.notify(SignalTopic::Queue);
        }
        Ok(n)
    }

    async fn runner_session(&self, runner: RunnerId) -> Result<Option<RunnerSession>, RunStoreError> {
        Ok(self.state.lock().sessions.get(&runner).cloned())
    }

    async fn record_runner_session(&self, session: &RunnerSession) -> Result<Option<Uuid>, RunStoreError> {
        let mut s = self.state.lock();
        let previous = s.sessions.insert(session.runner_id, session.clone());
        Ok(previous.map(|p| p.instance_id))
    }

    async fn events_after(&self, org: OrgId, run: RunId, after: u64, limit: u32) -> Result<Vec<RunEvent>, RunStoreError> {
        let s = self.state.lock();
        s.run(org, run)?;
        Ok(s.events.get(&run).map_or_else(Vec::new, |evs| evs.iter().filter(|e| e.seq > after).take(limit as usize).cloned().collect()))
    }

    async fn commands_after(&self, org: OrgId, run: RunId, attempt: u16, after: u64, limit: u32) -> Result<Vec<CommandRecord>, RunStoreError> {
        let s = self.state.lock();
        s.run(org, run)?;
        Ok(s.commands
            .get(&run)
            .map_or_else(Vec::new, |cs| cs.iter().filter(|c| c.attempt == attempt && c.seq > after).take(limit as usize).cloned().collect()))
    }

    async fn gates(&self, org: OrgId, run: RunId) -> Result<Vec<GateRecord>, RunStoreError> {
        let s = self.state.lock();
        s.run(org, run)?;
        Ok(s.gates.iter().filter(|g| g.rec.gate.run_id == run && g.rec.gate.org_id == org).map(|g| g.rec.clone()).collect())
    }

    async fn gate_for_verify(&self, org: OrgId, verify_run: RunId) -> Result<Option<GateRecord>, RunStoreError> {
        let s = self.state.lock();
        Ok(s.gates.iter().find(|g| g.rec.gate.org_id == org && g.rec.gate.verify_run == Some(verify_run)).map(|g| g.rec.clone()))
    }

    async fn reserve_artifact(&self, org: OrgId, a: NewArtifact) -> Result<ArtifactRecord, RunStoreError> {
        let key = BlobKey::run_artifact(org, a.run_id, a.kind, &a.name).map_err(|e| RunStoreError::Invalid(e.to_string()))?;
        let id = self.artifact_id();
        let now = self.clock.now();
        let mut s = self.state.lock();
        s.run(org, a.run_id)?;
        if let Some(existing) =
            s.artifacts.iter().find(|x| x.artifact.org_id == org && x.artifact.run_id == a.run_id && x.attempt == a.attempt && x.artifact.kind == a.kind && x.name == a.name)
        {
            return Ok(existing.clone());
        }
        if s.artifacts.iter().any(|x| x.artifact.key == key.as_str()) {
            return Err(RunStoreError::Invalid(format!("artifact key {key} is taken by another attempt")));
        }
        let rec = ArtifactRecord {
            artifact: Artifact {
                id,
                org_id: org,
                run_id: a.run_id,
                kind: a.kind,
                key: key.as_str().to_owned(),
                bytes: a.bytes,
                sha256: a.sha256,
                created_at: now,
            },
            attempt: a.attempt,
            name: a.name,
            content_type: a.content_type,
            state: ArtifactState::Pending,
        };
        s.artifacts.push(rec.clone());
        Ok(rec)
    }

    async fn artifact(&self, org: OrgId, id: ArtifactId) -> Result<ArtifactRecord, RunStoreError> {
        let s = self.state.lock();
        s.artifacts.iter().find(|a| a.artifact.id == id && a.artifact.org_id == org).cloned().ok_or(RunStoreError::NotFound)
    }

    async fn artifacts(&self, org: OrgId, run: RunId) -> Result<Vec<ArtifactRecord>, RunStoreError> {
        let s = self.state.lock();
        s.run(org, run)?;
        Ok(s.artifacts.iter().filter(|a| a.artifact.org_id == org && a.artifact.run_id == run).cloned().collect())
    }

    async fn reply(&self, scope: &str, key: &str) -> Result<Option<StoredReply>, RunStoreError> {
        Ok(self.state.lock().replies.get(&(scope.to_owned(), key.to_owned())).cloned())
    }

    async fn usage(&self, org: OrgId, from: OffsetDateTime, to: OffsetDateTime) -> Result<Vec<RunUsage>, RunStoreError> {
        let s = self.state.lock();
        let mut rows: Vec<&RunRecord> =
            s.runs.values().map(|r| &r.rec).filter(|r| r.org() == org && r.run.created_at >= from && r.run.created_at < to).collect();
        rows.sort_by_key(|r| (r.run.created_at, r.run.id));
        Ok(rows.into_iter().map(RunUsage::of).collect())
    }
}

#[async_trait]
impl RunSignals for MemRunStore {
    async fn subscribe(&self, topic: SignalTopic) -> Result<Box<dyn crate::ports::runs::Subscription>, crate::ports::runs::SignalError> {
        self.hub.subscribe(topic).await
    }
}

// ---------------------------------------------------------------------------------------------
// Builders for tests
// ---------------------------------------------------------------------------------------------

/// A minimal `RunSpec` for `kind` (repo/sha/recipe left empty; set them as needed).
pub fn sample_spec(kind: RunKind) -> RunSpec {
    let harness = (kind != RunKind::Verify).then_some((HarnessKind::Omp, CredentialModeKind::Gateway));
    RunSpec {
        repo_id: None,
        sha: None,
        ticket_id: None,
        recipe_hash: None,
        prompt: if kind == RunKind::Verify { String::new() } else { "do the thing".into() },
        system_append: String::new(),
        model: if kind == RunKind::Verify { String::new() } else { "claude-test".into() },
        requirements: RunRequirements { arch: None, executor: ExecutorKind::Host, harness, browser: false },
        verify: None,
        triage: None,
        fix: None,
        trace: None,
        slot: SlotClass::of(kind),
        priority: RunSpec::priority_of(kind),
    }
}

/// A `Queued` run (attempt 1) for tests.
pub fn sample_run(org: OrgId, id: RunId, kind: RunKind, subject: RunSubject, budget: Budget, at: OffsetDateTime) -> Run {
    let harness = (kind != RunKind::Verify).then_some(HarnessKind::Omp);
    Run::new(RunInit { id, org_id: org, kind, subject, harness, budget, created_at: at }).expect("valid sample run")
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use mage_core::testing::{MemJobQueue, SeqIdGen};

    use super::MemRunStore;
    use crate::testing::runs_contract::{NoRunnerSeed, RunStoreFixture, contract_run_store};
    use crate::testing::seed::NoSeed;

    #[tokio::test]
    async fn mem_run_store_contract() {
        contract_run_store(|clock| async move {
            let jobs = Arc::new(MemJobQueue::new(clock.clone(), Arc::new(SeqIdGen::new())));
            let store = Arc::new(MemRunStore::new(clock, jobs));
            RunStoreFixture { signals: store.signals(), store, seed: Arc::new(NoSeed), runners: Arc::new(NoRunnerSeed) }
        })
        .await;
    }
}
