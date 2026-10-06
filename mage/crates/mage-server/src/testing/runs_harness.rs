//! A `RunManager` over in-memory fakes for use-case tests (runs, triage, fix): the stores, a
//! fake clock and id generator, scripted kind hooks, and seeding helpers (repo with an indexed,
//! bundled version and a validated recipe; a triaged ticket with causes; a registered runner).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use parking_lot::Mutex;
use time::OffsetDateTime;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use mage_core::domain::recipe::RecipeData;
use mage_core::domain::{
    Cause, CauseData, CauseStatus, Citation, CommitSha, Confidence, ContentHash, FixResult, GateFailure, InstallationId, LineRange, OrgId, Recipe,
    RecipeHash, RecipeStatus, Repo, RepoAccess, RepoId, RepoPath, RepoVersionKey, ReproMethod, RunId, SourceBundleRef, TargetOrigin, Ticket,
    TicketData, TicketId, TicketSource, TicketStatus, TicketTarget, TreeOid, TriageResult, VersionReason,
};
use mage_core::ports::repos::RepoStore;
use mage_core::testing::{FakeClock, MemArtifactStore, MemJobQueue, MemRepoStore, SeqIdGen};
use mage_core::wire::runner::{
    Arch, Capabilities, CredentialModeKind, ExecutorKind, HarnessAvailability, Os, RegisterGrant, Registration, RegistrationAck, SlotCounts,
};
use mage_core::wire::token::BOOTSTRAP_PREFIX;

use crate::app::runs::{EvidenceDecision, FixAcceptance, FixGate, KindOutcome, RunDeps, RunHooks, RunManager, RunPolicy, RunnerCaller, TriageGate};
use crate::error::AppError;
use crate::ports::audit::Actor;
use crate::ports::runs::{GateRecord, RunRecord};
use crate::ports::ticket::{TicketStore, TriageWrite};
use crate::ports::tokens::{NewBootstrapToken, RunnerScope, TokenStore};
use crate::testing::{FakeTokenSigner, MemAuditLog, MemChangeFeed, MemOrgStore, MemRunStore, MemTicketStore, MemTokenStore};

/// Scripted Evidence gate: `evaluate` pops the next decision; `salvage` returns the set result.
#[derive(Default)]
pub struct FakeTriageGate {
    pub decisions: Mutex<VecDeque<EvidenceDecision>>,
    pub salvage: Mutex<Option<TriageResult>>,
    /// `(attempt, run)` of every `evaluate` call.
    pub calls: Mutex<Vec<(u8, RunId)>>,
}

#[async_trait]
impl TriageGate for FakeTriageGate {
    async fn evaluate(
        &self,
        _org: OrgId,
        run: &RunRecord,
        attempt: u8,
        _submission: mage_core::wire::runner::TriageSubmission,
    ) -> Result<EvidenceDecision, AppError> {
        self.calls.lock().push((attempt, run.id()));
        self.decisions.lock().pop_front().ok_or_else(|| AppError::Internal("no scripted evidence decision".into()))
    }

    fn salvage(&self, _run: &RunRecord, _gates: &[GateRecord]) -> Option<TriageResult> {
        self.salvage.lock().clone()
    }
}

/// Scripted fix hooks: `accept` pops the next acceptance; `judge` pops the next verdict.
#[derive(Default)]
pub struct FakeFixGate {
    pub acceptances: Mutex<VecDeque<FixAcceptance>>,
    pub verdicts: Mutex<VecDeque<Result<(), GateFailure>>>,
}

#[async_trait]
impl FixGate for FakeFixGate {
    async fn accept(&self, _org: OrgId, _run: &RunRecord, _attempt: u8, _result: &FixResult) -> Result<FixAcceptance, AppError> {
        self.acceptances.lock().pop_front().ok_or_else(|| AppError::Internal("no scripted fix acceptance".into()))
    }

    async fn judge(&self, _org: OrgId, _fix_run: &RunRecord, _gate: &GateRecord, _verify_run: &RunRecord) -> Result<Result<(), GateFailure>, AppError> {
        self.verdicts.lock().pop_front().ok_or_else(|| AppError::Internal("no scripted fix verdict".into()))
    }
}

/// Records every `apply` call.
#[derive(Default)]
pub struct RecordingOutcome {
    pub calls: Mutex<Vec<(OrgId, RunId)>>,
}

#[async_trait]
impl KindOutcome for RecordingOutcome {
    async fn apply(&self, org: OrgId, run: RunId) -> Result<(), AppError> {
        self.calls.lock().push((org, run));
        Ok(())
    }
}

/// The default hooks of the harness.
#[derive(Clone)]
pub struct ScriptedHooks {
    pub triage: Arc<FakeTriageGate>,
    pub triage_outcome: Arc<RecordingOutcome>,
    pub fix: Arc<FakeFixGate>,
    pub fix_outcome: Arc<RecordingOutcome>,
}

impl ScriptedHooks {
    pub fn hooks(&self) -> RunHooks {
        RunHooks {
            triage_gate: self.triage.clone(),
            triage_outcome: self.triage_outcome.clone(),
            fix_gate: self.fix.clone(),
            fix_outcome: self.fix_outcome.clone(),
        }
    }
}

/// A repo seeded for runs: version at `sha` with a source bundle, and a validated recipe.
#[derive(Debug, Clone)]
pub struct SeededRepo {
    pub repo: Repo,
    pub sha: CommitSha,
    pub recipe: RecipeHash,
}

/// A triaged ticket with its causes.
#[derive(Debug, Clone)]
pub struct SeededTicket {
    pub ticket: TicketId,
    pub causes: Vec<mage_core::domain::CauseId>,
    pub triage_run: RunId,
}

pub const RECIPE_TOML: &str = "version = 1\n";

pub struct RunsHarness {
    pub clock: Arc<FakeClock>,
    pub ids: Arc<SeqIdGen>,
    pub jobs: Arc<MemJobQueue>,
    pub runs: Arc<MemRunStore>,
    pub tickets: Arc<MemTicketStore>,
    pub orgs: Arc<MemOrgStore>,
    pub repos: Arc<MemRepoStore>,
    pub artifacts: Arc<MemArtifactStore>,
    pub token_store: Arc<MemTokenStore>,
    pub signer: Arc<FakeTokenSigner>,
    pub audit: Arc<MemAuditLog>,
    pub feed: Arc<MemChangeFeed>,
    pub hooks: ScriptedHooks,
    pub policy: RunPolicy,
    pub shutdown: CancellationToken,
    pub manager: RunManager,
}

impl Default for RunsHarness {
    fn default() -> Self {
        Self::new()
    }
}

impl RunsHarness {
    pub fn new() -> RunsHarness {
        Self::with_policy(RunPolicy::default())
    }

    pub fn with_policy(policy: RunPolicy) -> RunsHarness {
        let clock = Arc::new(FakeClock::new());
        let ids = Arc::new(SeqIdGen::new());
        let jobs = Arc::new(MemJobQueue::new(clock.clone(), ids.clone()));
        let runs = Arc::new(MemRunStore::new(clock.clone(), jobs.clone()).with_thresholds(policy.due_thresholds()));
        let feed = Arc::new(MemChangeFeed::new(clock.clone()));
        let tickets = Arc::new(MemTicketStore::new(clock.clone(), jobs.clone(), feed.clone()));
        let orgs = Arc::new(MemOrgStore::new(clock.clone(), jobs.clone(), feed.clone()));
        let repos = Arc::new(MemRepoStore::new(clock.clone(), jobs.clone()));
        let artifacts = Arc::new(MemArtifactStore::new(clock.clone()));
        let token_store = Arc::new(MemTokenStore::new(clock.clone()));
        let signer = Arc::new(FakeTokenSigner::new(&policy.public_url));
        let audit = Arc::new(MemAuditLog::new(clock.clone()));
        let hooks = ScriptedHooks {
            triage: Arc::new(FakeTriageGate::default()),
            triage_outcome: Arc::new(RecordingOutcome::default()),
            fix: Arc::new(FakeFixGate::default()),
            fix_outcome: Arc::new(RecordingOutcome::default()),
        };
        let shutdown = CancellationToken::new();
        let mut h = RunsHarness {
            manager: RunManager::new(RunDeps {
                runs: runs.clone(),
                signals: runs.signals(),
                tickets: tickets.clone(),
                orgs: orgs.clone(),
                repos: repos.clone(),
                artifacts: artifacts.clone(),
                jobs: jobs.clone(),
                token_store: token_store.clone(),
                signer: signer.clone(),
                audit: audit.clone(),
                clock: clock.clone(),
                ids: ids.clone(),
                policy: policy.clone(),
                hooks: hooks.hooks(),
                shutdown: shutdown.clone(),
            }),
            clock,
            ids,
            jobs,
            runs,
            tickets,
            orgs,
            repos,
            artifacts,
            token_store,
            signer,
            audit,
            feed,
            hooks,
            policy,
            shutdown,
        };
        h.rebuild(h.hooks.hooks());
        h
    }

    /// The harness with the kind hooks `make` returns (it may share the harness's stores).
    pub fn with_hooks(make: impl FnOnce(&RunsHarness) -> RunHooks) -> RunsHarness {
        let mut h = Self::new();
        let hooks = make(&h);
        h.rebuild(hooks);
        h
    }

    fn rebuild(&mut self, hooks: RunHooks) {
        self.manager = RunManager::new(RunDeps {
            runs: self.runs.clone(),
            signals: self.runs.signals(),
            tickets: self.tickets.clone(),
            orgs: self.orgs.clone(),
            repos: self.repos.clone(),
            artifacts: self.artifacts.clone(),
            jobs: self.jobs.clone(),
            token_store: self.token_store.clone(),
            signer: self.signer.clone(),
            audit: self.audit.clone(),
            clock: self.clock.clone(),
            ids: self.ids.clone(),
            policy: self.policy.clone(),
            hooks,
            shutdown: self.shutdown.clone(),
        });
    }

    pub fn now(&self) -> OffsetDateTime {
        mage_core::ports::clock::Clock::now(&*self.clock)
    }

    pub fn org(&self) -> OrgId {
        OrgId::from_uuid(Uuid::now_v7())
    }

    /// A repo with an indexed version at a fixed sha (bundle recorded) and a validated recipe.
    pub async fn repo(&self, org: OrgId) -> SeededRepo {
        let id = RepoId::from_uuid(Uuid::now_v7());
        let repo = Repo {
            id,
            org_id: org,
            external_id: id.as_uuid().as_u128() as u64 & 0xffff_ffff,
            owner: "acme".into(),
            name: "shop".into(),
            default_branch: "main".into(),
            installation_id: InstallationId(1),
            access: RepoAccess::Granted,
            created_at: self.now(),
        };
        let repo = self.repos.upsert_repo(&repo, &[]).await.unwrap();
        let sha = CommitSha([7; 20]);
        self.repos.set_default_head(org, id, sha).await.unwrap();
        self.bundle(org, id, sha).await;
        let draft = Recipe::draft(org, id, RECIPE_TOML.into()).unwrap();
        let data = RecipeData { status: RecipeStatus::Validated { at: self.now() }, validated_at_sha: Some(sha), ..draft.into_data() };
        let recipe = Recipe::rehydrate(data);
        self.repos.put_recipe(org, &recipe).await.unwrap();
        SeededRepo { repo, sha, recipe: recipe.hash }
    }

    /// Records a source bundle for `(repo, sha)` (creating the version if needed).
    pub async fn bundle(&self, org: OrgId, repo: RepoId, sha: CommitSha) {
        self.repos.add_version_reason(org, repo, sha, VersionReason::DefaultHead).await.unwrap();
        let bundle = SourceBundleRef {
            key: format!("org/{}/repos/{}/bundles/{}.tar.zst", org.as_uuid().hyphenated(), repo.as_uuid().hyphenated(), TreeOid([1; 20])),
            tree: TreeOid([1; 20]),
            bytes: 1234,
            sha256: [3; 32],
        };
        self.repos.set_source_bundle(org, RepoVersionKey::new(repo, sha), &bundle, &[]).await.unwrap();
    }

    /// A ticket in `Triaged` targeting `repo` with `n` untested causes.
    pub async fn ticket(&self, org: OrgId, repo: &SeededRepo, n: usize) -> SeededTicket {
        let id = TicketId::from_uuid(Uuid::now_v7());
        let triage_run = RunId::from_uuid(Uuid::now_v7());
        let hash = ContentHash([5; 32]);
        let now = self.now();
        let data = TicketData {
            id,
            org_id: org,
            source: TicketSource::Linear {
                issue_id: id.as_uuid().to_string(),
                identifier: "SUP-1".into(),
                url: "https://linear.app/acme/issue/SUP-1".parse().unwrap(),
            },
            title: "Checkout total is wrong".into(),
            body: "Customers see a wrong total after applying a coupon.".into(),
            content_hash: hash,
            slack_thread: None,
            customer: None,
            target: Some(TicketTarget { repo_id: repo.repo.id, sha: repo.sha, origin: TargetOrigin::DefaultHead }),
            error_strings: Vec::new(),
            status: TicketStatus::Triaging,
            triaged_hash: Some(hash),
            triage_run: Some(triage_run),
            created_at: now,
            updated_at: now,
        };
        self.tickets.save_ingested(org, &Ticket::rehydrate(data), None, &[]).await.unwrap();
        let causes: Vec<Cause> = (0..n)
            .map(|i| {
                Cause::rehydrate(CauseData {
                    id: Uuid::now_v7().into(),
                    ticket_id: id,
                    org_id: org,
                    triage_run,
                    rank: u16::try_from(i + 1).unwrap(),
                    title: format!("cause {i}"),
                    explanation: "coupon applied twice".into(),
                    evidence: vec![Citation {
                        repo_id: repo.repo.id,
                        sha: repo.sha,
                        path: RepoPath::new("src/cart.ts").unwrap(),
                        lines: LineRange::new(10, 12).unwrap(),
                        snippet_hash: None,
                        note: None,
                    }],
                    repro_steps: vec!["apply coupon".into()],
                    confirm_if: "total is doubled".into(),
                    rule_out_if: "total is right".into(),
                    confidence: Confidence::new(0.7).unwrap(),
                    method: ReproMethod::Ui,
                    status: CauseStatus::Untested,
                    superseded_at: None,
                })
            })
            .collect();
        let cause_ids = causes.iter().map(|c| c.id).collect();
        let w = TriageWrite { ticket: id, expect: TicketStatus::Triaging, content_hash: hash, triage_run, causes, next: TicketStatus::Triaged };
        self.tickets.apply_triage(org, w, &[]).await.unwrap();
        SeededTicket { ticket: id, causes: cause_ids, triage_run }
    }

    pub fn capabilities(&self) -> Capabilities {
        Capabilities {
            arch: Arch::Aarch64,
            os: Os::Linux,
            executor: ExecutorKind::Host,
            harnesses: vec![
                HarnessAvailability { kind: mage_core::domain::HarnessKind::Omp, version: "1".into(), credential_modes: vec![CredentialModeKind::Gateway, CredentialModeKind::LocalLogin] },
                HarnessAvailability { kind: mage_core::domain::HarnessKind::ClaudeCode, version: "1".into(), credential_modes: vec![CredentialModeKind::Gateway] },
            ],
            browser: true,
            slots: SlotCounts { light: 2, work: 2, verify: 2 },
        }
    }

    /// Registers a runner through `RegisterRunner` (bootstrap grant) and authenticates its token.
    pub async fn runner(&self, name: &str, scope: RunnerScope) -> (RunnerCaller, RegistrationAck) {
        self.runner_with(name, scope, Uuid::now_v7(), self.capabilities()).await
    }

    pub async fn runner_with(&self, name: &str, scope: RunnerScope, instance: Uuid, caps: Capabilities) -> (RunnerCaller, RegistrationAck) {
        let token = format!("{BOOTSTRAP_PREFIX}{}", Uuid::now_v7().simple());
        let hash = crate::app::runs::request_hash(token.as_bytes());
        let b = NewBootstrapToken {
            token_hash: hash,
            name: name.into(),
            scope,
            expires_at: self.now() + time::Duration::hours(24),
            created_by: Actor::Operator { os_user: "test".into() },
        };
        self.token_store.create_bootstrap(&b).await.unwrap();
        let reg = Registration {
            grant: RegisterGrant::Bootstrap { bootstrap_token: token, name: name.into(), public_key: Self::runner_key(name).verifying_key().to_bytes() },
            instance_id: instance,
            version: "0.1.0".into(),
            capabilities: caps,
            held: Vec::new(),
        };
        let ack = self.manager.register_runner(reg).await.unwrap();
        let caller = self.manager.authenticate_runner(&ack.runner_token).await.unwrap();
        (caller, ack)
    }

    /// The Ed25519 key of runner `name` (deterministic per name).
    pub fn runner_key(name: &str) -> ed25519_dalek::SigningKey {
        ed25519_dalek::SigningKey::from_bytes(&crate::app::runs::request_hash(name.as_bytes()))
    }

    /// A fresh runner assertion (restart / token refresh grant) of runner `name`.
    pub fn assertion(&self, name: &str, runner: mage_core::domain::RunnerId) -> String {
        let now = self.now();
        let a = mage_core::wire::token::RunnerAssertion { runner, iat: now, exp: now + time::Duration::seconds(60), jti: Uuid::now_v7() };
        mage_core::wire::token::TokenCodec::new(&self.policy.public_url).sign_assertion(&Self::runner_key(name), &a).unwrap()
    }

    pub fn advance(&self, by: Duration) {
        self.clock.advance(by);
    }
}
