//! CreateRun (runs.md "CreateRun"): resolve the subject to a frozen `RunSpec`, check the kind's
//! preconditions, set requirements, priority and source readiness, and commit the `Queued` run
//! with its `Status(Queued)` event and the creation `runs.apply_outcome` job.

use time::OffsetDateTime;

use mage_core::domain::{
    Budget, CauseStatus, CommitSha, HarnessKind, JobPayload, OrgId, RecipeHash, RecipeStatus, RepoId, RepoVersionKey, Run, RunEventKind,
    RunId, RunInit, RunKind, RunStatus, RunSubject, ScriptKind, TicketId, TicketStatus, UserId, VerifySpec, VersionReason, ensure_same_org,
};
use mage_core::domain::recipe::RecipeSpec;
use mage_core::ports::jobs::NewJob;
use mage_core::ports::repos::RepoStoreError;
use mage_core::wire::runner::{FixSpec, SlotClass};

use crate::app::runs::machine::apply_outcome_job;
use crate::app::runs::{RunManager, prompt, repo_err};
use crate::domain::runs::{RunRequirements, RunSpec};
use crate::domain::triage::TriageSpec;
use crate::error::AppError;
use crate::ports::audit::AuditAction;
use crate::ports::runs::{Affinity, NewRun, RunConflict, RunEventDraft, RunQuery, RunRecord, RunStoreError, RunWrite};

/// Prompt and system append rendered by the module that owns the kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunPrompt {
    pub prompt: String,
    pub system_append: String,
}

/// Input of `RunManager::create_run`. Build with [`CreateRun::new`] and set the fields the kind
/// needs:
///
/// | Kind | Required | Optional (default) |
/// |---|---|---|
/// | Learn | subject `RepoVersion` | harness/budget (org general settings), model (policy), prompt (rendered here) |
/// | Triage | subject `Ticket`, `triage`, `prompt` | `id`, harness/model/budget (org triage settings) |
/// | Diagnose | subject `Cause` | harness/budget (org general settings), prompt (rendered here) |
/// | Fix | subject `Cause`, `fix`, `prompt` | harness/budget (org general settings) |
/// | Verify | subject `Run(parent)`, `parent`, `verify` | `sha` (parent's), budget (policy) |
#[derive(Debug, Clone, PartialEq)]
pub struct CreateRun {
    pub org: OrgId,
    /// Preset id (triage records it on the ticket before creating the run).
    pub id: Option<RunId>,
    pub kind: RunKind,
    pub subject: RunSubject,
    pub harness: Option<HarnessKind>,
    pub model: Option<String>,
    pub budget: Option<Budget>,
    pub parent: Option<RunId>,
    pub requested_by: Option<UserId>,
    /// Creation idempotency key; a reused key returns the existing run.
    pub create_key: Option<String>,
    pub prompt: Option<RunPrompt>,
    pub verify: Option<VerifySpec>,
    pub triage: Option<TriageSpec>,
    pub fix: Option<FixSpec>,
    /// Verify only: run at this sha instead of the parent's (fix rebase flow).
    pub sha: Option<CommitSha>,
    /// `Notice` events written with the creation.
    pub notices: Vec<String>,
    pub trace: Option<String>,
}

impl CreateRun {
    pub fn new(org: OrgId, kind: RunKind, subject: RunSubject) -> CreateRun {
        CreateRun {
            org,
            id: None,
            kind,
            subject,
            harness: None,
            model: None,
            budget: None,
            parent: None,
            requested_by: None,
            create_key: None,
            prompt: None,
            verify: None,
            triage: None,
            fix: None,
            sha: None,
            notices: Vec::new(),
            trace: None,
        }
    }
}

/// The resolved parts of a new run.
pub(crate) struct Planned {
    pub spec: RunSpec,
    pub harness: Option<HarnessKind>,
    pub budget: Budget,
    pub source_ready: bool,
    pub affinity: Option<Affinity>,
    pub jobs: Vec<NewJob>,
}

const OPEN_STATUSES: [RunStatus; 5] = [RunStatus::Queued, RunStatus::Leased, RunStatus::Preparing, RunStatus::Running, RunStatus::AwaitingGate];

impl RunManager {
    pub async fn create_run(&self, req: CreateRun) -> Result<RunRecord, AppError> {
        if !req.subject.fits(req.kind) {
            return Err(AppError::Invalid(format!("subject {:?} does not fit a {:?} run", req.subject, req.kind)));
        }
        let plan = match req.kind {
            RunKind::Learn => self.plan_learn(&req).await?,
            RunKind::Triage => self.plan_triage(&req).await?,
            RunKind::Diagnose | RunKind::Fix => self.plan_cause_run(&req).await?,
            RunKind::Verify => self.plan_verify(&req).await?,
        };
        let now = self.now();
        let id = req.id.unwrap_or_else(|| RunId::from_uuid(self.d.ids.uuid_v7()));
        let run = Run::new(RunInit {
            id,
            org_id: req.org,
            kind: req.kind,
            subject: req.subject,
            harness: plan.harness,
            budget: plan.budget,
            created_at: now,
        })?;
        let mut w = RunWrite::default();
        w.events.push(RunEventDraft { run_id: id, attempt: 1, at: now, kind: RunEventKind::Status(RunStatus::Queued) });
        for notice in &req.notices {
            w.events.push(RunEventDraft { run_id: id, attempt: 1, at: now, kind: RunEventKind::Notice(notice.clone()) });
        }
        if req.kind != RunKind::Verify {
            w.jobs.push(apply_outcome_job(req.org, id, RunStatus::Queued));
        }
        w.jobs.extend(plan.jobs);
        w.inserts.push(NewRun { run, spec: plan.spec, create_key: req.create_key.clone(), source_ready: plan.source_ready, affinity: plan.affinity });
        match self.d.runs.commit(req.org, w).await {
            Ok(_) => {}
            Err(RunStoreError::Conflict(RunConflict::Duplicate { run })) => return Ok(self.d.runs.get(req.org, run).await?),
            Err(e) => return Err(e.into()),
        }
        if let Some(user) = req.requested_by {
            self.audit_user(req.org, user, AuditAction::RunStarted, id).await;
        }
        Ok(self.d.runs.get(req.org, id).await?)
    }

    fn requirements(&self, harness: HarnessKind, browser: bool) -> RunRequirements {
        RunRequirements { arch: None, executor: self.d.policy.executor, harness: Some((harness, self.d.policy.credential_mode)), browser }
    }

    fn spec(&self, kind: RunKind, prompt: RunPrompt, model: String, requirements: RunRequirements) -> RunSpec {
        RunSpec {
            repo_id: None,
            sha: None,
            ticket_id: None,
            recipe_hash: None,
            prompt: prompt.prompt,
            system_append: prompt.system_append,
            model,
            requirements,
            verify: None,
            triage: None,
            fix: None,
            trace: None,
            slot: SlotClass::of(kind),
            priority: RunSpec::priority_of(kind),
        }
    }

    /// `source_ready` for `(repo, sha)`, and the `index.version` job when no version exists.
    pub(crate) async fn source_state(
        &self,
        org: OrgId,
        key: RepoVersionKey,
        ticket: Option<TicketId>,
    ) -> Result<(bool, Option<NewJob>), AppError> {
        match self.d.repos.version(org, key).await {
            Ok(v) => Ok((v.bundle.is_some(), None)),
            Err(RepoStoreError::NotFound) => {
                let job = ticket.map(|t| {
                    NewJob::new(JobPayload::IndexVersion { org_id: org, repo_id: key.repo_id, sha: key.sha, reason: VersionReason::OnDemand(t) })
                });
                Ok((false, job))
            }
            Err(e) => Err(repo_err(e)),
        }
    }

    async fn plan_learn(&self, req: &CreateRun) -> Result<Planned, AppError> {
        let RunSubject::RepoVersion(key) = req.subject else { unreachable!("checked by fits") };
        let repo = self.d.repos.repo(req.org, key.repo_id).await.map_err(repo_err)?;
        ensure_same_org(repo.org_id, req.org)?;
        let version = match self.d.repos.version(req.org, key).await {
            Ok(v) => v,
            Err(RepoStoreError::NotFound) => return Err(AppError::Invalid(format!("{}@{} is not indexed", repo.name, key.sha))),
            Err(e) => return Err(repo_err(e)),
        };
        let recipe = self.d.repos.latest_recipe(req.org, key.repo_id).await.map_err(repo_err)?;
        let general = self.d.orgs.general_settings(req.org).await?;
        let harness = req.harness.unwrap_or(general.harness);
        let prompt = req.prompt.clone().unwrap_or_else(|| prompt::learn(&repo, key.sha, recipe.as_ref()));
        let model = req.model.clone().unwrap_or_else(|| self.d.policy.default_model.clone());
        let mut spec = self.spec(RunKind::Learn, prompt, model, self.requirements(harness, false));
        spec.repo_id = Some(key.repo_id);
        spec.sha = Some(key.sha);
        spec.recipe_hash = recipe.map(|r| r.hash);
        spec.trace = req.trace.clone();
        Ok(Planned {
            spec,
            harness: Some(harness),
            budget: req.budget.unwrap_or(general.budgets.learn),
            source_ready: version.bundle.is_some(),
            affinity: None,
            jobs: Vec::new(),
        })
    }

    async fn plan_triage(&self, req: &CreateRun) -> Result<Planned, AppError> {
        let RunSubject::Ticket(ticket_id) = req.subject else { unreachable!("checked by fits") };
        let triage = req.triage.clone().ok_or_else(|| AppError::Invalid("a Triage run needs its TriageSpec".into()))?;
        let prompt = req.prompt.clone().ok_or_else(|| AppError::Invalid("a Triage run needs its rendered prompt".into()))?;
        let ticket = self.d.tickets.get_ticket(req.org, ticket_id).await?.value;
        ensure_same_org(ticket.org_id, req.org)?;
        let settings = self.d.orgs.triage_settings(req.org).await?;
        let harness = req.harness.unwrap_or(settings.harness);
        let model = req.model.clone().or(settings.model.clone()).unwrap_or_else(|| self.d.policy.default_model.clone());
        let mut spec = self.spec(RunKind::Triage, prompt, model, self.requirements(harness, false));
        spec.repo_id = Some(triage.input.target.repo_id);
        spec.sha = Some(triage.input.target.sha);
        spec.ticket_id = Some(ticket_id);
        spec.trace = req.trace.clone();
        spec.triage = Some(triage);
        Ok(Planned { spec, harness: Some(harness), budget: req.budget.unwrap_or(settings.budget), source_ready: true, affinity: None, jobs: Vec::new() })
    }

    /// Diagnose and Fix: cause → ticket target → validated recipe.
    async fn plan_cause_run(&self, req: &CreateRun) -> Result<Planned, AppError> {
        let RunSubject::Cause(cause_id) = req.subject else { unreachable!("checked by fits") };
        let cause = self.d.tickets.get_cause(req.org, cause_id).await?;
        ensure_same_org(cause.org_id, req.org)?;
        let ticket = self.d.tickets.get_ticket(req.org, cause.ticket_id).await?.value;
        ensure_same_org(ticket.org_id, req.org)?;
        if req.kind == RunKind::Diagnose {
            match &cause.status {
                CauseStatus::Untested | CauseStatus::NotReproduced { .. } | CauseStatus::Inconclusive { .. } => {}
                CauseStatus::Running(_) => return Err(AppError::Conflict("already running".into())),
                CauseStatus::Reproduced { .. } => return Err(AppError::Invalid("the cause is already reproduced".into())),
            }
            if !matches!(ticket.status, TicketStatus::Triaged | TicketStatus::Diagnosing) {
                return Err(AppError::Invalid(format!("ticket is {}; diagnose needs triaged or diagnosing", ticket.status.name())));
            }
        }
        let active = self
            .d
            .runs
            .list(req.org, &RunQuery { kind: Some(req.kind), subject: Some(req.subject), statuses: OPEN_STATUSES.to_vec(), limit: 1, ..RunQuery::default() })
            .await?;
        if !active.is_empty() {
            return Err(AppError::Conflict("already running".into()));
        }
        let target = ticket.target.clone().ok_or_else(|| AppError::Invalid("the ticket has no target repo/sha".into()))?;
        let repo = self.d.repos.repo(req.org, target.repo_id).await.map_err(repo_err)?;
        ensure_same_org(repo.org_id, req.org)?;
        let recipe = self
            .d
            .repos
            .latest_recipe(req.org, target.repo_id)
            .await
            .map_err(repo_err)?
            .filter(|r| matches!(r.status, RecipeStatus::Validated { .. }))
            .ok_or_else(|| AppError::Invalid("recipe not validated".into()))?;
        let browser = RecipeSpec::parse(&recipe.body).map(|s| s.browser.is_some()).unwrap_or(false);
        let general = self.d.orgs.general_settings(req.org).await?;
        let harness = req.harness.unwrap_or(general.harness);
        let prompt = match (req.kind, req.prompt.clone()) {
            (_, Some(p)) => p,
            (RunKind::Diagnose, None) => prompt::diagnose(&ticket, &cause, &repo, target.sha, self.d.ids.nonce()),
            _ => return Err(AppError::Invalid("a Fix run needs its rendered prompt".into())),
        };
        if req.kind == RunKind::Fix && req.fix.is_none() {
            return Err(AppError::Invalid("a Fix run needs its FixSpec".into()));
        }
        let model = req.model.clone().unwrap_or_else(|| self.d.policy.default_model.clone());
        let mut spec = self.spec(req.kind, prompt, model, self.requirements(harness, browser));
        spec.repo_id = Some(target.repo_id);
        spec.sha = Some(target.sha);
        spec.ticket_id = Some(ticket.id);
        spec.recipe_hash = Some(recipe.hash);
        spec.fix = req.fix.clone();
        spec.trace = req.trace.clone();
        let (source_ready, job) = self.source_state(req.org, RepoVersionKey::new(target.repo_id, target.sha), Some(ticket.id)).await?;
        let budget = req.budget.unwrap_or(if req.kind == RunKind::Fix { general.budgets.fix } else { general.budgets.diagnose });
        Ok(Planned { spec, harness: Some(harness), budget, source_ready, affinity: None, jobs: job.into_iter().collect() })
    }

    /// Verify runs created outside a gate (fix's `TestBaseline` and rebase verify).
    async fn plan_verify(&self, req: &CreateRun) -> Result<Planned, AppError> {
        let RunSubject::Run(parent_id) = req.subject else { unreachable!("checked by fits") };
        if req.parent.is_some_and(|p| p != parent_id) {
            return Err(AppError::Invalid("parent and subject differ".into()));
        }
        if req.harness.is_some() {
            return Err(AppError::Invalid("verify runs have no harness".into()));
        }
        let verify = req.verify.clone().ok_or_else(|| AppError::Invalid("a Verify run needs its VerifySpec".into()))?;
        let parent = self.d.runs.get(req.org, parent_id).await?;
        let now = self.now();
        let mut plan = self.verify_plan(&parent, verify, parent.spec.recipe_hash, now).await?;
        if let Some(sha) = req.sha.filter(|s| Some(*s) != parent.spec.sha) {
            let repo = parent.spec.repo_id.ok_or_else(|| AppError::Invalid("the parent run has no repo".into()))?;
            let (ready, job) = self.source_state(req.org, RepoVersionKey::new(repo, sha), parent.spec.ticket_id).await?;
            plan.spec.sha = Some(sha);
            plan.source_ready = ready;
            plan.jobs.extend(job);
        }
        if let Some(budget) = req.budget {
            plan.budget = budget;
        }
        plan.spec.trace = req.trace.clone().or(parent.spec.trace.clone());
        Ok(plan)
    }

    /// A Verify run checking `parent` with `verify` (gate verify and the fix module's): repo,
    /// sha and ticket from the parent, `recipe_hash` (Boot: the proposal's; else the parent's),
    /// arch pinned to the parent runner's, affinity to that runner, source readiness copied.
    pub(crate) async fn verify_plan(
        &self,
        parent: &RunRecord,
        verify: VerifySpec,
        recipe_hash: Option<RecipeHash>,
        now: OffsetDateTime,
    ) -> Result<Planned, AppError> {
        let repo_id: Option<RepoId> = parent.spec.repo_id;
        if repo_id.is_none() || parent.spec.sha.is_none() {
            return Err(AppError::Invalid("the parent run has no repo/sha".into()));
        }
        let browser = self.verify_needs_browser(parent.org(), &verify).await?;
        let (arch, affinity) = match parent.lease {
            Some(lease) => {
                let arch = self.d.runs.runner_session(lease.runner_id).await?.map(|s| s.capabilities.arch);
                (arch, Some(Affinity { runner_id: lease.runner_id, until: now + crate::app::runs::to_time(self.d.policy.verify_affinity_wait()) }))
            }
            None => (None, None),
        };
        let budget = self.d.policy.verify_budget(&verify);
        let spec = RunSpec {
            repo_id,
            sha: parent.spec.sha,
            ticket_id: parent.spec.ticket_id,
            recipe_hash,
            prompt: String::new(),
            system_append: String::new(),
            model: String::new(),
            requirements: RunRequirements { arch, executor: parent.spec.requirements.executor, harness: None, browser },
            verify: Some(verify),
            triage: None,
            fix: None,
            trace: parent.spec.trace.clone(),
            slot: SlotClass::Verify,
            priority: RunSpec::priority_of(RunKind::Verify),
        };
        Ok(Planned { spec, harness: None, budget, source_ready: parent.source_ready, affinity, jobs: Vec::new() })
    }

    /// Repro/Fix scripts named `*.spec.ts` etc. run through Playwright.
    async fn verify_needs_browser(&self, org: OrgId, verify: &VerifySpec) -> Result<bool, AppError> {
        let script = match verify {
            VerifySpec::Repro { script, .. } | VerifySpec::Fix { script, .. } => *script,
            VerifySpec::Boot | VerifySpec::TestBaseline => return Ok(false),
        };
        let artifact = self.d.runs.artifact(org, script).await?;
        Ok(ScriptKind::from_file_name(&artifact.name) == Some(ScriptKind::Playwright))
    }
}
