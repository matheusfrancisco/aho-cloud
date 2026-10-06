//! Fix use cases over fakes (fix.md Testing "Use cases"): the real `RunManager` with the fix hooks
//! (`AcceptFixResult`, `ApplyFixOutcome`) driven through the runner protocol for F2, and
//! `OpenPullRequest` against the in-memory GitHub (`FakePullRequests`) for F3, including a crash
//! after every persisted step.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use mage_core::domain::fix::{BranchName, FileMode, FixPrState, FixSettings, Patch};
use mage_core::domain::recipe::RecipeData;
use mage_core::domain::{
    ArtifactId, ArtifactKind, AttemptKind, BlobOid, CauseId, CauseStatus, Citation, CommitSha, DiagnoseResult, EntryKind, FailureSignature,
    FileClass, FixChecks, FixResult, Gate, GateKind, GateStatus, InstallationId, JobKind, JobPayload, Language, LineRange, ManifestEntry,
    Matcher, MemberRole, Notice, OrgId, Recipe, RecipeStatus, Repo, RepoAccess, RepoId, RepoPath, RepoVersionKey, Run, RunCommand, RunId,
    RunInit, RunKind, RunResult, RunStatus, RunSubject, SignatureKind, TicketId, TicketStatus, UserId, Verdict, VerifyAttempt,
    VerifyOutcome, VerifyResult, VerifySpec,
};
use mage_core::ports::blob::{ArtifactStore, BlobKey, PutOptions};
use mage_core::ports::jobs::{JobQueue, LeaseRequest as JobLeaseRequest};
use mage_core::ports::knowledge::{BlobMeta, KnowledgeStore};
use mage_core::ports::repos::RepoStore;
use mage_core::testing::MemKnowledgeStore;
use mage_core::wire::runner::{
    ArtifactSpec, CommandsQuery, FinishOutcome, FinishReport, FinishRequest, HeartbeatRequest, LeaseRequest, RunAssignment, RunnerPhase,
    SlotCounts, SubmitResultRequest, UploadRequest, WireRunResult,
};
use sha2::{Digest, Sha256};
use url::Url;
use uuid::Uuid;

use super::*;
use crate::app::runs::{ProtocolError, RunHooks, RunnerCaller};
use crate::ports::org::OrgStore;
use crate::ports::pull_requests::{PullRequestsError, WriteAccess};
use crate::ports::runs::{GateRecord, GateWrite, NewArtifact, NewRun, RunQuery, RunUpdate, RunWrite};
use crate::ports::ticket::{TicketStore, TicketTransition};
use crate::ports::tokens::RunnerScope;
use crate::testing::fix::replace_patch;
use crate::testing::pull_requests::{FakePullRequests, GitHubWorld};
use crate::testing::runs_harness::{FakeTriageGate, RecordingOutcome, RunsHarness, SeededRepo};

const FREE: SlotCounts = SlotCounts { light: 1, work: 1, verify: 1 };
const VERIFY_ONLY: SlotCounts = SlotCounts { light: 0, work: 0, verify: 1 };

const LEAKED_TOKEN: &str = "ghp_16C7e42F292c6912E7710c838347Ae178B4a";
const RECIPE: &str = "version = 1\n\n[test]\nrun = \"npm test\"\n";
const SCRIPT: &[u8] = b"import { test, expect } from '@playwright/test';\ntest('total', async ({ page }) => {\n  await expect(page.getByTestId('total')).toHaveText('$10');\n});\n";

fn cart(fixed: bool) -> String {
    (1..=20)
        .map(|i| match (i, fixed) {
            (11, false) => "  return total * 2; // applied twice\n".to_owned(),
            (11, true) => "  return total;\n".to_owned(),
            _ => format!("const line{i} = computeCartTotal({i});\n"),
        })
        .collect()
}

fn sig() -> FailureSignature {
    FailureSignature { kind: SignatureKind::AssertionText, value: "Expected: \"$10\" Received: \"$20\"".into(), matcher: Matcher::Contains }
}

fn script_attempts(n: u8, passed: bool) -> Vec<VerifyAttempt> {
    (1..=n)
        .map(|index| VerifyAttempt {
            index,
            kind: AttemptKind::Script,
            script_exit: Some(if passed { 0 } else { 1 }),
            script_passed: passed,
            status: Some(if passed { "passed" } else { "failed" }.into()),
            observed: if passed { Vec::new() } else { vec![FailureSignature { matcher: Matcher::Exact, ..sig() }] },
            health_ok: true,
            duration_ms: 10,
        })
        .collect()
}

pub(crate) struct W {
    pub(crate) h: RunsHarness,
    pub(crate) d: Arc<FixDeps>,
    knowledge: Arc<MemKnowledgeStore>,
    gh: GitHubWorld,
    pub(crate) org: OrgId,
    repo: Repo,
    sha: CommitSha,
    pub(crate) ticket: TicketId,
    pub(crate) cause: CauseId,
    diagnose: RunId,
    pub(crate) user: UserId,
}

pub(crate) async fn world() -> W {
    let gh = GitHubWorld::new("acme", "shop");
    let mut slot: Option<(Arc<FixDeps>, Arc<MemKnowledgeStore>)> = None;
    let h = RunsHarness::with_hooks(|h| {
        let knowledge = Arc::new(MemKnowledgeStore::new(h.clock.clone(), h.repos.clone()));
        let d = Arc::new(FixDeps {
            tickets: h.tickets.clone(),
            runs: h.runs.clone(),
            repos: h.repos.clone(),
            orgs: h.orgs.clone(),
            knowledge: knowledge.clone(),
            reader: SourceReader::new(knowledge.clone(), h.artifacts.clone()),
            artifacts: h.artifacts.clone(),
            jobs: h.jobs.clone(),
            pull_requests: Arc::new(FakePullRequests::new(gh.clone())),
            // app/ never imports adapters: the fake flags the planted token like the real rule set.
            scanner: Arc::new(crate::testing::FakeSecretScanner::new().flagging("github_token", LEAKED_TOKEN)),
            run_manager: LateRunManager::new(),
            clock: h.clock.clone(),
            ids: h.ids.clone(),
            config: FixConfig::new(Url::parse("https://mage.example.com").unwrap()),
        });
        slot = Some((d.clone(), knowledge));
        RunHooks {
            triage_gate: Arc::new(FakeTriageGate::default()),
            triage_outcome: Arc::new(RecordingOutcome::default()),
            fix_gate: Arc::new(AcceptFixResult::new(d.clone())),
            fix_outcome: Arc::new(ApplyFixOutcome::new(d)),
        }
    });
    let (d, knowledge) = slot.unwrap();
    d.run_manager.set(h.manager.clone());

    let org = h.org();
    let base = gh.seed_commit(&[("src/cart.ts", FileMode::Regular, cart(false).as_bytes())], "base", &[]);
    gh.set_branch("main", base);
    let repo = Repo {
        id: RepoId::from_uuid(Uuid::now_v7()),
        org_id: org,
        external_id: 4242,
        owner: "acme".into(),
        name: "shop".into(),
        default_branch: "main".into(),
        installation_id: InstallationId(1),
        access: RepoAccess::Granted,
        created_at: h.now(),
    };
    let repo = h.repos.upsert_repo(&repo, &[]).await.unwrap();
    h.repos.set_default_head(org, repo.id, base).await.unwrap();
    h.bundle(org, repo.id, base).await;
    let draft = Recipe::draft(org, repo.id, RECIPE.into()).unwrap();
    let recipe = Recipe::rehydrate(RecipeData { status: RecipeStatus::Validated { at: h.now() }, validated_at_sha: Some(base), ..draft.into_data() });
    h.repos.put_recipe(org, &recipe).await.unwrap();

    // Index: the cited file at the target sha.
    let oid = BlobOid::from_bytes([0x42; 20]);
    let text = cart(false);
    h.artifacts.put(&BlobKey::blob_content(org, oid), text.clone().into_bytes(), &PutOptions { content_type: "text/plain".into(), if_absent: false }).await.unwrap();
    let entry = ManifestEntry {
        path: RepoPath::new("src/cart.ts").unwrap(),
        kind: EntryKind::File,
        executable: false,
        blob: oid,
        lang: Some(Language::TypeScript),
        class: FileClass::Source,
        size: text.len() as u64,
        is_text: true,
        line_count: 20,
    };
    let meta = BlobMeta { oid, size: entry.size, line_count: 20, is_text: true, content_stored: true };
    knowledge.put_blob_meta(org, &[meta]).await.unwrap();
    knowledge.put_manifest(org, &RepoVersionKey::new(repo.id, base), &[entry]).await.unwrap();

    let seeded = SeededRepo { repo: repo.clone(), sha: base, recipe: recipe.hash };
    let t = h.ticket(org, &seeded, 1).await;
    let mut w = W { h, d, knowledge, gh, org, repo, sha: base, ticket: t.ticket, cause: t.causes[0], diagnose: RunId::from_uuid(Uuid::nil()), user: UserId::from_uuid(Uuid::now_v7()) };
    w.diagnose = w.reproduce().await;
    w
}

impl W {
    async fn put_artifact(&self, run: RunId, kind: ArtifactKind, name: &str, bytes: &[u8]) -> ArtifactId {
        let sha256: [u8; 32] = Sha256::digest(bytes).into();
        let rec = self
            .h
            .runs
            .reserve_artifact(
                self.org,
                NewArtifact { run_id: run, attempt: 1, kind, name: name.into(), bytes: bytes.len() as u64, sha256, content_type: "text/plain".into() },
            )
            .await
            .unwrap();
        let key = BlobKey::parse(&rec.artifact.key).unwrap();
        self.h.artifacts.put(&key, bytes.to_vec(), &PutOptions { content_type: "text/plain".into(), if_absent: false }).await.unwrap();
        self.h.runs.commit(self.org, RunWrite { complete_artifacts: vec![(rec.artifact.id, bytes.len() as u64)], ..RunWrite::default() }).await.unwrap();
        rec.artifact.id
    }

    /// A Diagnose run that reproduced the cause through a passed Repro gate (stored results), the
    /// cause `Reproduced`, the ticket `Diagnosed`.
    async fn reproduce(&self) -> RunId {
        let (h, org) = (&self.h, self.org);
        let diag = h.manager.start_diagnose(org, self.ticket, self.cause, None, self.user).await.unwrap();
        self.drain().await;
        let script = self.put_artifact(diag.id(), ArtifactKind::Script, "repro.spec.ts", SCRIPT).await;
        let diagnosis = DiagnoseResult {
            verdict: Verdict::Reproduced,
            script: Some(script),
            signature: Some(sig()),
            summary: "Reproduced: the coupon is applied twice.".into(),
            suggested_fix: Some("Return `total` once in `computeCartTotal`.".into()),
            fix_sites: vec![
                Citation { repo_id: self.repo.id, sha: self.sha, path: RepoPath::new("src/cart.ts").unwrap(), lines: LineRange::new(11, 11).unwrap(), snippet_hash: None, note: None },
                Citation { repo_id: self.repo.id, sha: self.sha, path: RepoPath::new("src/gone.ts").unwrap(), lines: LineRange::new(1, 2).unwrap(), snippet_hash: None, note: None },
            ],
        };
        let now = h.now();
        let verify = Run::new(RunInit {
            id: RunId::from_uuid(Uuid::now_v7()),
            org_id: org,
            kind: RunKind::Verify,
            subject: RunSubject::Run(diag.id()),
            harness: None,
            budget: diag.run.budget,
            created_at: now,
        })
        .unwrap();
        let mut vdata = verify.into_data();
        vdata.status = RunStatus::Succeeded;
        vdata.result = Some(RunResult::Verify(VerifyResult {
            outcome: VerifyOutcome::Completed,
            observed: None,
            attempts: script_attempts(2, false),
            artifacts: Vec::new(),
            fix_checks: None,
            rebased_patch: None,
        }));
        let mut vspec = diag.spec.clone();
        vspec.verify = Some(VerifySpec::Repro { script, signature: sig(), repetitions: 2 });
        let verify_id = vdata.id;
        let gate = Gate { id: Uuid::now_v7().into(), org_id: org, run_id: diag.id(), kind: GateKind::Repro, attempt: 1, status: GateStatus::Passed { at: now }, verify_run: Some(verify_id) };
        let mut rec = h.runs.get(org, diag.id()).await.unwrap();
        let mut data = rec.run.clone().into_data();
        data.status = RunStatus::Succeeded;
        data.result = Some(RunResult::Diagnose(diagnosis));
        rec.run = Run::rehydrate(data);
        h.runs
            .commit(
                org,
                RunWrite {
                    inserts: vec![NewRun { run: Run::rehydrate(vdata), spec: vspec, create_key: None, source_ready: true, affinity: None }],
                    updates: vec![RunUpdate::of(rec)],
                    gates: vec![GateWrite::Insert(GateRecord { gate: gate.clone(), feedback: None, details: None, created_at: now })],
                    ..RunWrite::default()
                },
            )
            .await
            .unwrap();
        let running = h.tickets.get_cause(org, self.cause).await.unwrap().status.clone();
        let from = h.tickets.get_ticket(org, self.ticket).await.unwrap().value.status;
        h.tickets
            .set_cause_status(
                org,
                self.cause,
                &running,
                CauseStatus::Reproduced { run: diag.id(), gate: gate.id },
                Some(TicketTransition { expect: from, next: TicketStatus::Diagnosed }),
                &[],
            )
            .await
            .unwrap();
        diag.id()
    }

    pub(crate) async fn settings(&self, f: impl FnOnce(&mut FixSettings)) {
        let mut s = FixSettings { run: true, pull_requests: true, ..FixSettings::default() };
        f(&mut s);
        self.h.orgs.set_fix_settings(self.org, &s).await.unwrap();
    }

    pub(crate) async fn brief(&self) -> BriefOutcome {
        BuildFixBrief::new(self.d.clone()).run(self.org, self.cause, self.diagnose).await.unwrap()
    }

    async fn start_fix(&self) -> Result<crate::ports::runs::RunRecord, AppError> {
        StartFixRun::new(self.d.clone())
            .run(StartFix { org: self.org, ticket: self.ticket, cause: self.cause, requested_by: self.user, role: MemberRole::Member, harness: None })
            .await
    }

    /// Every ready `runs.apply_outcome` job, like the worker; panics on errors.
    async fn drain(&self) {
        loop {
            let req = JobLeaseRequest { lane: mage_core::domain::JobLane::Server, kinds: Some(vec![JobKind::RunsApplyOutcome]), worker: "t".into(), max: 50 };
            let leased = self.h.jobs.lease(&req).await.unwrap();
            if leased.is_empty() {
                return;
            }
            for job in leased {
                let JobPayload::RunsApplyOutcome { org_id, run_id } = job.payload else { unreachable!() };
                self.h.manager.apply_outcome(org_id, run_id).await.unwrap_or_else(|e| panic!("apply_outcome {run_id}: {e}"));
                self.h.jobs.complete(&job.lease, &[]).await.unwrap();
            }
        }
    }

    async fn lease(&self, r: &RunnerCaller, free: SlotCounts) -> Option<RunAssignment> {
        let req = LeaseRequest { request_id: Uuid::now_v7(), wait: Duration::ZERO, free, prepared: Vec::new() };
        self.h.manager.lease(r, req).await.unwrap()
    }

    async fn begin(&self, r: &RunnerCaller, a: &RunAssignment) {
        for phase in [RunnerPhase::Preparing, RunnerPhase::Running] {
            self.h.manager.heartbeat(r, a.run_id, HeartbeatRequest { attempt: a.attempt, phase }).await.unwrap();
        }
    }

    async fn upload(&self, r: &RunnerCaller, a: &RunAssignment, kind: ArtifactKind, name: &str, body: &[u8], content_type: &str) -> ArtifactId {
        let spec = ArtifactSpec { kind, name: name.into(), bytes: body.len() as u64, sha256: Sha256::digest(body).into(), content_type: content_type.into() };
        let grant = self.h.manager.request_upload(r, a.run_id, UploadRequest { attempt: a.attempt, spec }).await.unwrap();
        self.h.artifacts.upload_signed(&grant.url, &grant.headers, body.to_vec()).unwrap();
        self.h.manager.complete_upload(r, a.run_id, grant.artifact_id, a.attempt).await.unwrap();
        grant.artifact_id
    }

    async fn submit(&self, r: &RunnerCaller, a: &RunAssignment, result: WireRunResult) -> Result<mage_core::wire::runner::SubmitAck, ProtocolError> {
        self.h.manager.submit_result(r, a.run_id, SubmitResultRequest { attempt: a.attempt, submission_id: Uuid::now_v7(), result }).await
    }

    async fn finish(&self, r: &RunnerCaller, a: &RunAssignment) {
        let req = FinishRequest { attempt: a.attempt, report: FinishReport { outcome: FinishOutcome::Done, message: None } };
        self.h.manager.finish_run(r, a.run_id, req).await.unwrap();
    }

    async fn submit_patch(&self, r: &RunnerCaller, a: &RunAssignment, name: &str, raw: &[u8]) -> Result<mage_core::wire::runner::SubmitAck, ProtocolError> {
        let patch = self.upload(r, a, ArtifactKind::Patch, name, raw, "text/x-diff").await;
        let stats = Patch::parse(raw).unwrap().stats();
        self.submit(r, a, WireRunResult::Fix(FixResult { patch, summary: "Return the total once. cc @lead".into(), excluded: Vec::new(), stats })).await
    }

    fn good_patch() -> Vec<u8> {
        replace_patch(&[("src/cart.ts", Some(cart(false).as_bytes()), Some(cart(true).as_bytes()))])
    }

    /// Starts the Fix run, answers the test baseline green and leases the Fix run.
    async fn leased_fix(&self) -> (RunnerCaller, RunAssignment) {
        let run = self.start_fix().await.unwrap();
        self.drain().await;
        let r = self.h.runner("r1", RunnerScope::Shared).await.0;
        loop {
            let a = self.lease(&r, FREE).await.expect("a run to lease");
            match (a.kind, &a.verify) {
                (RunKind::Fix, _) => {
                    assert_eq!(a.run_id, run.id());
                    self.begin(&r, &a).await;
                    return (r, a);
                }
                (RunKind::Verify, Some(VerifySpec::TestBaseline)) => {
                    self.begin(&r, &a).await;
                    let result = VerifyResult {
                        outcome: VerifyOutcome::Completed,
                        observed: None,
                        attempts: Vec::new(),
                        artifacts: Vec::new(),
                        fix_checks: Some(FixChecks { applied_patch_sha256: None, boot: true, test_exit: Some(0), lint_exit: None, source_applies: None, upstream_already_fixed: None }),
                        rebased_patch: None,
                    };
                    self.submit(&r, &a, WireRunResult::Verify(result)).await.unwrap();
                    self.finish(&r, &a).await;
                    self.drain().await;
                }
                other => panic!("unexpected lease {other:?}"),
            }
        }
    }

    /// F2 end to end: a correct patch passes the fix gate; the ticket ends `FixReady`.
    async fn fix_ready(&self, gif: bool) -> RunId {
        let (r, a) = self.leased_fix().await;
        let raw = Self::good_patch();
        let ack = self.submit_patch(&r, &a, "attempt-1.diff", &raw).await.unwrap();
        assert_eq!(ack.status, RunStatus::AwaitingGate);
        let v = self.lease(&r, VERIFY_ONLY).await.expect("fix gate verify run");
        let Some(VerifySpec::Fix { regression, repetitions, .. }) = &v.verify else { panic!("not a fix gate: {:?}", v.verify) };
        assert_eq!(regression.as_ref().map(|r| r.path.as_str()), Some(".mage/regressions/sup-1-cause-0.spec.ts"));
        let repetitions = *repetitions;
        self.begin(&r, &v).await;
        let mut artifacts = Vec::new();
        if gif {
            artifacts.push(self.upload(&r, &v, ArtifactKind::Gif, "after.gif", b"GIF89a-after", "image/gif").await);
        }
        let result = VerifyResult {
            outcome: VerifyOutcome::Completed,
            observed: None,
            attempts: script_attempts(repetitions, true),
            artifacts,
            fix_checks: Some(FixChecks {
                applied_patch_sha256: Some(Patch::parse(&raw).unwrap().sha256()),
                boot: true,
                test_exit: Some(0),
                lint_exit: None,
                source_applies: Some(true),
                upstream_already_fixed: None,
            }),
            rebased_patch: None,
        };
        self.submit(&r, &v, WireRunResult::Verify(result)).await.unwrap();
        self.finish(&r, &v).await;
        self.drain().await;
        assert_eq!(self.h.runs.get(self.org, a.run_id).await.unwrap().run.status, RunStatus::Succeeded);
        self.finish(&r, &a).await;
        self.drain().await;
        a.run_id
    }

    pub(crate) async fn ticket_status(&self) -> TicketStatus {
        self.h.tickets.get_ticket(self.org, self.ticket).await.unwrap().value.status
    }

    fn notices(&self) -> Vec<Notice> {
        self.h
            .jobs
            .jobs()
            .into_iter()
            .filter_map(|j| match j.payload {
                JobPayload::NotifyTicket { notice, .. } => Some(notice),
                _ => None,
            })
            .collect()
    }

    fn jobs_of(&self, kind: JobKind) -> Vec<JobPayload> {
        self.h.jobs.jobs().into_iter().filter(|j| j.kind == kind).map(|j| j.payload).collect()
    }

    async fn open_pr(&self, fix_run: RunId) -> Result<OpenPrOutcome, AppError> {
        OpenPullRequest::new(self.d.clone()).run(self.org, self.ticket, fix_run, None).await
    }

    async fn pr_row(&self) -> Option<mage_core::domain::fix::FixPullRequest> {
        self.h.tickets.pull_requests(self.org, self.ticket).await.unwrap().into_iter().next()
    }

    /// Commits Mage created (carrying a change-set trailer) and `mage/` branches.
    fn mage_objects(&self) -> (usize, Vec<String>) {
        let branches: Vec<String> = self.gh.branches().into_iter().filter(|b| b.starts_with("mage/")).collect();
        let commits = branches.iter().filter_map(|b| self.gh.branch(b)).filter(|c| self.gh.commit(*c).is_some_and(|c| c.message.contains("Mage-Change-Set:"))).count();
        (commits, branches)
    }

    /// Completes a Verify run directly in the store (the runner's part is not under test).
    async fn complete_verify(&self, id: RunId, result: VerifyResult) {
        let mut rec = self.h.runs.get(self.org, id).await.unwrap();
        let mut data = rec.run.clone().into_data();
        data.status = RunStatus::Succeeded;
        data.result = Some(RunResult::Verify(result));
        data.finished_at = Some(self.h.now());
        rec.run = Run::rehydrate(data);
        self.h.runs.commit(self.org, RunWrite { updates: vec![RunUpdate::of(rec)], ..RunWrite::default() }).await.unwrap();
    }

    /// Moves `main` to a new commit on top of the gated base.
    fn move_head(&self) -> CommitSha {
        let files = [("src/cart.ts", FileMode::Regular, cart(false).into_bytes()), ("README.md", FileMode::Regular, b"# shop\n".to_vec())];
        let files: Vec<(&str, FileMode, &[u8])> = files.iter().map(|(p, m, b)| (*p, *m, b.as_slice())).collect();
        let head = self.gh.seed_commit(&files, "docs: readme", &[self.sha]);
        self.gh.set_branch("main", head);
        head
    }
}

// ---------------------------------------------------------------------------------------------
// F1
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn brief_is_versioned_drops_stale_citations_and_notifies() {
    let w = world().await;
    let BriefOutcome::Written { version, dropped, redacted } = w.brief().await else { panic!("brief not written") };
    assert_eq!((version, dropped, redacted), (1, 1, 0), "src/gone.ts is not in the index at the ticket sha");
    let brief = w.h.tickets.latest_fix_brief(w.org, w.cause).await.unwrap().unwrap();
    assert!(brief.markdown.contains("src/cart.ts:10-12"), "cause evidence printed");
    assert!(brief.markdown.contains("src/cart.ts:11-11"), "surviving fix site printed");
    assert!(!brief.markdown.contains("src/gone.ts"), "stale citation dropped");
    let artifact = w.h.runs.artifact(w.org, brief.artifact).await.unwrap();
    assert_eq!((artifact.artifact.kind, artifact.name.as_str(), artifact.artifact.run_id), (ArtifactKind::FixBrief, "brief-v1.md", w.diagnose));
    let stored = w.h.artifacts.get(&BlobKey::parse(&artifact.artifact.key).unwrap(), 1 << 20).await.unwrap();
    assert_eq!(stored, brief.markdown.as_bytes());
    let events = w.h.runs.events_after(w.org, w.diagnose, 0, 100).await.unwrap();
    assert!(events.iter().any(|e| matches!(&e.kind, mage_core::domain::RunEventKind::Notice(t) if t.contains("src/gone.ts:1-2"))));
    assert!(w.notices().contains(&Notice::FixBriefReady { cause_id: w.cause, version: 1 }));

    // Regenerating bumps the version.
    assert!(matches!(w.brief().await, BriefOutcome::Written { version: 2, .. }));
    let view = GetFixBrief::new(w.d.clone()).run(w.org, w.ticket, w.cause, Some(1)).await.unwrap();
    assert_eq!((view.version, view.latest_version), (1, 2));
}

#[tokio::test]
async fn brief_refuses_a_cause_that_is_not_reproduced_by_the_run() {
    let w = world().await;
    let other = RunId::from_uuid(Uuid::now_v7());
    let out = BuildFixBrief::new(w.d.clone()).run(w.org, w.cause, other).await.unwrap();
    assert_eq!(out, BriefOutcome::NotReproduced);
    w.h.tickets.set_cause_status(w.org, w.cause, &w.h.tickets.get_cause(w.org, w.cause).await.unwrap().status.clone(), CauseStatus::Untested, None, &[]).await.unwrap();
    assert_eq!(w.brief().await, BriefOutcome::NotReproduced);
    assert!(w.h.tickets.latest_fix_brief(w.org, w.cause).await.unwrap().is_none());
}

#[tokio::test]
async fn brief_waits_for_the_index_when_every_evidence_citation_fails() {
    let w = world().await;
    w.knowledge.put_manifest(w.org, &RepoVersionKey::new(w.repo.id, w.sha), &[]).await.unwrap();
    let err = BuildFixBrief::new(w.d.clone()).run(w.org, w.cause, w.diagnose).await.unwrap_err();
    assert!(matches!(err, AppError::Unavailable(_)), "{err:?}");
}

// ---------------------------------------------------------------------------------------------
// F2
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn start_fix_checks_preconditions() {
    let w = world().await;
    w.brief().await;
    assert!(matches!(w.start_fix().await, Err(AppError::Invalid(m)) if m.contains("disabled")));
    w.settings(|_| {}).await;
    let run = w.start_fix().await.unwrap();
    assert_eq!(run.run.kind, RunKind::Fix);
    assert_eq!(w.ticket_status().await, TicketStatus::Fixing);
    let spec = run.spec.fix.clone().unwrap();
    assert_eq!(spec.signature, sig());
    assert!(run.spec.prompt.contains("# Fix brief: SUP-1") && run.spec.prompt.contains("submit_fix"));
    let baseline = w.h.runs.list(w.org, &RunQuery { parent: Some(run.id()), ..RunQuery::default() }).await.unwrap();
    assert_eq!(baseline.iter().filter(|r| r.spec.verify == Some(VerifySpec::TestBaseline)).count(), 1, "first fix run starts a baseline");
    assert!(matches!(w.start_fix().await, Err(AppError::Conflict(_))), "one fix run per ticket");
}

#[tokio::test]
async fn fix_gate_pass_marks_fix_ready_once_and_notifies() {
    let w = world().await;
    w.brief().await;
    w.settings(|_| {}).await;
    let run = w.fix_ready(false).await;
    assert_eq!(w.ticket_status().await, TicketStatus::FixReady);
    let ready = Notice::FixReady { cause_id: w.cause, run_id: run };
    assert_eq!(w.notices().iter().filter(|n| **n == ready).count(), 1);
    assert!(w.jobs_of(JobKind::FixOpenPr).is_empty(), "open_pr = manual");
    // Replay is a no-op.
    ApplyFixOutcome::new(w.d.clone()).apply_run(w.org, run).await.unwrap();
    assert_eq!(w.notices().iter().filter(|n| **n == ready).count(), 1);
    assert_eq!(w.ticket_status().await, TicketStatus::FixReady);
    // The token re-derives from the stored results only.
    let proof = FixProof::new(w.d.runs.clone()).gate_proof(w.org, run).await.unwrap();
    assert_eq!(proof.token.base_sha(), w.sha);
}

#[tokio::test]
async fn auto_open_pr_enqueues_with_the_change_set_dedupe_key() {
    let w = world().await;
    w.brief().await;
    w.settings(|s| s.open_pr = mage_core::domain::fix::OpenPrMode::Auto).await;
    let run = w.fix_ready(false).await;
    let digest = FixProof::new(w.d.runs.clone()).gate_proof(w.org, run).await.unwrap().token.digest();
    let job = w.h.jobs.jobs().into_iter().find(|j| j.kind == JobKind::FixOpenPr).expect("fix.open_pr");
    assert_eq!(job.dedupe_key, Some(format!("fix.open_pr:{}:{}", w.ticket, digest.to_hex())));
}

#[tokio::test]
async fn policy_and_secret_violations_are_failed_attempts_with_feedback() {
    let w = world().await;
    w.brief().await;
    w.settings(|_| {}).await;
    let (r, a) = w.leased_fix().await;

    let workflow = replace_patch(&[(".github/workflows/ci.yml", None, Some(b"on: push\n"))]);
    let Err(ProtocolError::Rejected(rej)) = w.submit_patch(&r, &a, "attempt-1.diff", &workflow).await else { panic!("policy violation accepted") };
    assert!(rej.message.contains(".github/workflows/ci.yml") && rej.message.contains("workflow"), "{}", rej.message);

    let secret = LEAKED_TOKEN;
    let leak = replace_patch(&[("src/cart.ts", Some(cart(false).as_bytes()), Some(format!("{}const token = \"{secret}\";\n", cart(true)).as_bytes()))]);
    let Err(ProtocolError::Rejected(rej)) = w.submit_patch(&r, &a, "attempt-2.diff", &leak).await else { panic!("secret accepted") };
    assert!(rej.message.contains("github_token") && rej.message.contains("src/cart.ts:21"), "{}", rej.message);
    assert!(!rej.message.contains(secret), "secret values are never echoed");

    let gates = w.h.runs.gates(w.org, a.run_id).await.unwrap();
    let failed: Vec<u8> = gates.iter().filter(|g| matches!(g.gate.status, GateStatus::Failed { .. }) && g.gate.verify_run.is_none()).map(|g| g.gate.attempt).collect();
    assert_eq!(failed, vec![1, 2], "each rejection consumes a fix gate attempt");
    assert_eq!(w.h.runs.get(w.org, a.run_id).await.unwrap().run.status, RunStatus::Running, "attempts left: back to the agent");
    let cmds = w.h.manager.poll_commands(&r, a.run_id, CommandsQuery { attempt: a.attempt, after: 0, wait: Duration::ZERO }).await.unwrap();
    assert!(cmds.commands.iter().any(|c| matches!(&c.command, RunCommand::GateFeedback(f) if f.kind == GateKind::Fix)));

    // Third rejection exhausts the attempts: Failed → ticket Diagnosed + FixFailed.
    assert!(w.submit_patch(&r, &a, "attempt-3.diff", &workflow).await.is_err());
    w.drain().await;
    assert_eq!(w.h.runs.get(w.org, a.run_id).await.unwrap().run.status, RunStatus::Failed);
    assert_eq!(w.ticket_status().await, TicketStatus::Diagnosed);
    assert!(w.notices().contains(&Notice::FixFailed { cause_id: w.cause, run_id: a.run_id }));
}

#[tokio::test]
async fn red_test_baseline_cancels_the_fix_run() {
    let w = world().await;
    w.brief().await;
    w.settings(|_| {}).await;
    let run = w.start_fix().await.unwrap();
    w.drain().await;
    let baseline = w.h.runs.list(w.org, &RunQuery { parent: Some(run.id()), ..RunQuery::default() }).await.unwrap().remove(0);
    let red = VerifyResult {
        outcome: VerifyOutcome::Completed,
        observed: None,
        attempts: Vec::new(),
        artifacts: Vec::new(),
        fix_checks: Some(FixChecks { applied_patch_sha256: None, boot: true, test_exit: Some(1), lint_exit: None, source_applies: None, upstream_already_fixed: None }),
        rebased_patch: None,
    };
    w.complete_verify(baseline.id(), red).await;
    ApplyFixOutcome::new(w.d.clone()).apply_run(w.org, baseline.id()).await.unwrap();
    w.drain().await;
    assert_eq!(w.h.runs.get(w.org, run.id()).await.unwrap().run.status, RunStatus::Cancelled);
    assert_eq!(w.ticket_status().await, TicketStatus::Diagnosed);
    // The cached red baseline now refuses new fix runs at this (repo, sha, recipe).
    assert!(matches!(w.start_fix().await, Err(AppError::Invalid(m)) if m.contains("project tests fail at base")));
}

// ---------------------------------------------------------------------------------------------
// F3
// ---------------------------------------------------------------------------------------------

async fn ready_world(gif: bool) -> (W, RunId) {
    let w = world().await;
    w.brief().await;
    w.settings(|s| {
        s.pr.labels = vec!["bug".into()];
        s.pr.reviewers.users = vec!["octo".into()];
    })
    .await;
    let run = w.fix_ready(gif).await;
    (w, run)
}

#[tokio::test]
async fn open_pr_commits_exactly_the_gated_change_and_opens_a_draft() {
    let (w, run) = ready_world(true).await;
    let OpenPrOutcome::Opened { number, url } = w.open_pr(run).await.unwrap() else { panic!("not opened") };
    assert_eq!(url.as_str(), w.gh.pull_url(number));
    let row = w.pr_row().await.unwrap();
    assert_eq!(row.state, FixPrState::Opened { number });
    let head = w.gh.branch(row.branch.as_str()).unwrap();
    let commit = w.gh.commit(head).unwrap();
    assert_eq!(commit.parents, vec![w.sha], "default head == gated base: parent is the base");
    let files = w.gh.files_at(head);
    assert_eq!(files["src/cart.ts"].1, cart(true).into_bytes());
    let regression = &files[".mage/regressions/sup-1-cause-0.spec.ts"].1;
    assert!(String::from_utf8_lossy(regression).starts_with("// Mage regression test for SUP-1"));
    assert!(commit.message.contains(&format!("Mage-Change-Set: {}", row.change_set.to_hex())));
    let pull = w.gh.pull(number).unwrap();
    assert!(pull.draft && pull.base == "main");
    assert_eq!(pull.labels, vec!["mage".to_owned(), "mage:fix".to_owned(), "bug".to_owned()]);
    assert_eq!(pull.reviewers.users, vec!["octo".to_owned()]);
    assert!(pull.body.contains("![after](https://github.com/user-attachments/assets/"), "uploaded GIF inline");
    assert!(!pull.body.contains("@lead"), "mentions neutralised");
    assert_eq!(w.ticket_status().await, TicketStatus::PrOpen);
    assert!(w.notices().contains(&Notice::PrOpened { cause_id: w.cause, url }));
    // Running the job again changes nothing.
    assert!(matches!(w.open_pr(run).await.unwrap(), OpenPrOutcome::Opened { number: n, .. } if n == number));
    assert_eq!(w.gh.pulls().len(), 1);
    assert_eq!(w.gh.uploads().len(), 1, "media upload cached");
}

#[tokio::test]
async fn refused_media_upload_falls_back_to_mage_links() {
    let (w, run) = ready_world(true).await;
    w.gh.set_uploads_accepted(false);
    let OpenPrOutcome::Opened { number, .. } = w.open_pr(run).await.unwrap() else { panic!("media must never block the PR") };
    let body = w.gh.pull(number).unwrap().body;
    assert!(!body.contains("user-attachments"));
    assert!(body.contains("[after recording (Mage)](https://mage.example.com/artifacts/"), "{body}");
}

#[tokio::test]
async fn crash_after_each_step_resumes_without_duplicates() {
    let (w, run) = ready_world(true).await;
    let crash = || PullRequestsError::Unavailable("connection reset".into());
    for (step, state_after) in [("create_commit", "planned"), ("create_branch", "commit_created"), ("find_open", "branch_created"), ("add_labels", "branch_created")] {
        w.gh.fail_on(step, crash());
        let err = w.open_pr(run).await.unwrap_err();
        assert!(matches!(err, AppError::Unavailable(_)), "{step}: {err:?}");
        assert_eq!(w.pr_row().await.unwrap().state.name(), state_after, "persisted progress after a crash at {step}");
    }
    let OpenPrOutcome::Opened { number, .. } = w.open_pr(run).await.unwrap() else { panic!("resume did not open") };
    let (commits, branches) = w.mage_objects();
    assert_eq!((commits, branches.len(), w.gh.pulls().len()), (1, 1, 1), "no duplicate commit, branch or PR");
    assert_eq!(w.gh.pull(number).unwrap().labels, vec!["mage".to_owned(), "mage:fix".to_owned(), "bug".to_owned()], "labels added once");
    assert_eq!(w.gh.uploads().len(), 1, "retries never upload twice");
    assert_eq!(w.ticket_status().await, TicketStatus::PrOpen);
}

#[tokio::test]
async fn swapped_patch_bytes_are_refused_before_any_commit() {
    let (w, run) = ready_world(false).await;
    let proof = FixProof::new(w.d.runs.clone()).gate_proof(w.org, run).await.unwrap();
    let rec = w.h.runs.artifact(w.org, proof.token.change_set().patch).await.unwrap();
    let swapped = replace_patch(&[("src/cart.ts", Some(cart(false).as_bytes()), Some(b"export {};\n"))]);
    w.h.artifacts.put(&BlobKey::parse(&rec.artifact.key).unwrap(), swapped, &PutOptions { content_type: "text/x-diff".into(), if_absent: false }).await.unwrap();
    let err = w.open_pr(run).await.unwrap_err();
    assert!(matches!(&err, AppError::Invalid(m) if m.contains("sha256")), "{err:?}");
    assert_eq!(w.mage_objects().0, 0);
    assert_eq!(w.gh.calls("create_commit"), 0);
}

#[tokio::test]
async fn branch_taken_by_foreign_commits_gets_a_suffix() {
    let (w, run) = ready_world(false).await;
    let digest = FixProof::new(w.d.runs.clone()).gate_proof(w.org, run).await.unwrap().token.digest();
    let name = BranchName::for_change_set("SUP-1", &digest).unwrap();
    let human = w.gh.seed_commit(&[("x", FileMode::Regular, b"x\n")], "wip: human work", &[w.sha]);
    w.gh.set_branch(name.as_str(), human);
    assert!(matches!(w.open_pr(run).await.unwrap(), OpenPrOutcome::Opened { .. }));
    let row = w.pr_row().await.unwrap();
    assert_eq!(row.branch.as_str(), format!("{name}-2"));
    assert_eq!(w.gh.branch(name.as_str()), Some(human), "never force-pushed");
}

#[tokio::test]
async fn missing_permissions_fail_without_side_effects() {
    let (w, run) = ready_world(false).await;
    w.gh.set_access(WriteAccess { contents: true, pull_requests: false, workflows: false, issues: false });
    let err = w.open_pr(run).await.unwrap_err();
    assert!(matches!(&err, AppError::Forbidden(m) if m.contains("pull_requests")), "{err:?}");
    assert!(w.pr_row().await.is_none());
}

#[tokio::test]
async fn moved_default_head_runs_a_rebase_verify_then_commits_on_head() {
    let (w, run) = ready_world(false).await;
    let head = w.move_head();
    let OpenPrOutcome::WaitingForRebase { verify_run, .. } = w.open_pr(run).await.unwrap() else { panic!("expected a rebase verify") };
    let v = w.h.runs.get(w.org, verify_run).await.unwrap();
    assert_eq!((v.run.kind, v.run.parent, v.spec.sha), (RunKind::Verify, Some(run), Some(head)));
    assert!(matches!(v.spec.verify, Some(VerifySpec::Fix { mode: mage_core::domain::fix::ApplyMode::AllowOffset, upstream_check: true, .. })));
    assert!(w.jobs_of(JobKind::IndexVersion).iter().any(|p| matches!(p, JobPayload::IndexVersion { sha, .. } if *sha == head)), "bundle for H requested");
    assert_eq!(w.gh.calls("create_commit"), 0);
    // A second look while it runs starts nothing new.
    assert!(matches!(w.open_pr(run).await.unwrap(), OpenPrOutcome::WaitingForRebase { verify_run: again, .. } if again == verify_run));

    // The rebase verify passes with a re-diffed patch at H.
    let raw = W::good_patch();
    let rebased = w.put_artifact(verify_run, ArtifactKind::Patch, "rebased-1.diff", &raw).await;
    let mut attempts = script_attempts(2, true);
    attempts.push(VerifyAttempt { kind: AttemptKind::UpstreamScript, ..script_attempts(1, false).remove(0) });
    let result = VerifyResult {
        outcome: VerifyOutcome::Completed,
        observed: None,
        attempts,
        artifacts: Vec::new(),
        fix_checks: Some(FixChecks {
            applied_patch_sha256: Some(Patch::parse(&raw).unwrap().sha256()),
            boot: true,
            test_exit: Some(0),
            lint_exit: None,
            source_applies: Some(true),
            upstream_already_fixed: Some(false),
        }),
        rebased_patch: Some(rebased),
    };
    w.complete_verify(verify_run, result).await;
    let OpenPrOutcome::Opened { number, .. } = w.open_pr(run).await.unwrap() else { panic!("not opened after rebase") };
    let row = w.pr_row().await.unwrap();
    let commit = w.gh.commit(w.gh.branch(row.branch.as_str()).unwrap()).unwrap();
    assert_eq!(commit.parents, vec![head], "committed on the moved head");
    assert!(w.gh.pull(number).unwrap().body.contains("and re-verified"));
}

#[tokio::test]
async fn already_fixed_upstream_opens_no_pr() {
    let (w, run) = ready_world(false).await;
    w.move_head();
    let OpenPrOutcome::WaitingForRebase { verify_run, .. } = w.open_pr(run).await.unwrap() else { panic!("expected a rebase verify") };
    let upstream = VerifyAttempt { kind: AttemptKind::UpstreamScript, ..script_attempts(1, true).remove(0) };
    let result = VerifyResult { outcome: VerifyOutcome::Completed, observed: None, attempts: vec![upstream], artifacts: Vec::new(), fix_checks: None, rebased_patch: None };
    w.complete_verify(verify_run, result).await;
    let OpenPrOutcome::NotOpened { reason } = w.open_pr(run).await.unwrap() else { panic!("a PR was opened") };
    assert!(reason.contains("already fixed upstream"), "{reason}");
    assert!(w.gh.pulls().is_empty());
    assert_eq!(w.ticket_status().await, TicketStatus::FixReady);
}

#[tokio::test]
async fn closed_unmerged_pr_returns_the_ticket_to_fix_ready() {
    let (w, run) = ready_world(false).await;
    let OpenPrOutcome::Opened { number, .. } = w.open_pr(run).await.unwrap() else { panic!("not opened") };
    let ev = OnPullRequestEvent::new(w.d.clone());
    assert_eq!(ev.run(w.org, w.repo.id, number, "synchronize", false).await.unwrap(), PrEventOutcome::Recorded);
    assert_eq!(ev.run(w.org, w.repo.id, number, "closed", false).await.unwrap(), PrEventOutcome::Closed { merged: false, ticket_reopened: true });
    assert_eq!(w.pr_row().await.unwrap().state, FixPrState::Closed { merged: false });
    assert_eq!(w.ticket_status().await, TicketStatus::FixReady);
    assert_eq!(ev.run(w.org, w.repo.id, number, "closed", false).await.unwrap(), PrEventOutcome::Ignored("already closed"));
    assert_eq!(ev.run(w.org, w.repo.id, 999, "closed", true).await.unwrap(), PrEventOutcome::Ignored("not a Mage fix PR"));
}

#[tokio::test]
async fn request_pull_request_enqueues_once_per_change_set() {
    let (w, _run) = ready_world(false).await;
    let rq = RequestPullRequest::new(w.d.clone());
    let a = rq.run(w.org, w.ticket, w.cause, MemberRole::Member).await.unwrap();
    let b = rq.run(w.org, w.ticket, w.cause, MemberRole::Member).await.unwrap();
    assert_eq!(a.job_id, b.job_id, "deduplicated by change set");
    w.settings(|s| s.pull_requests = false).await;
    assert!(matches!(rq.run(w.org, w.ticket, w.cause, MemberRole::Member).await, Err(AppError::Invalid(_))));
}
