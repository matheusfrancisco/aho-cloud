//! Triage evaluation harness (triage.md "Evaluation harness"): ground-truth labels, eval runs
//! (`StartTriageEval` + job `triage.eval_case` → `StartEvalCase`), per-case results (recorded by
//! `ApplyTriageOutcome`) and the report behind `mage eval triage report`.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use mage_core::domain::{
    CauseStatus, CommitSha, GateKind, GateStatus, HarnessKind, JobPayload, Layer, OrgId, RepoId, RepoPath, RepoVersionKey, RunId, RunKind,
    RunResult, RunStatus, RunSubject, TicketId, TicketStatus, TriageEvalId, TriageTrigger, UserId, VersionReason,
};
use mage_core::domain::{Budget, CauseDraft};
use mage_core::ports::common::PageRequest;
use mage_core::ports::jobs::{JobQueue, LeasedJob, NewJob};
use mage_core::ports::knowledge::LinkLookup;
use mage_core::ports::repos::{RepoStore, RepoStoreError};
use mage_core::ports::{Clock, IdGen, KnowledgeStore};
use mage_core::wire::token::MemoryCutoff;

use crate::app::knowledge_query::Retriever;
use crate::app::runs::RunManager;
use crate::app::triage::config::TriageConfig;
use crate::app::triage::gate::newest_evidence_details;
use crate::app::triage::prompt::{self, TRIAGE_PROMPT_VERSION};
use crate::app::triage::start::{
    DeferReason, LayerVerdict, NewTriageRun, SkipReason, Snapshot, StartOutcome, create_triage_run, layer_policy, live_triage_runs,
};
use crate::app::triage::{job_error, knowledge_error, repo_error};
use crate::domain::triage::{TriageInput, TriageMode, TriageSpec};
use crate::error::AppError;
use crate::ports::org::OrgStore;
use crate::ports::runs::{GateRecord, RunQuery, RunRecord, RunStore};
use crate::ports::ticket::{TicketFilter, TicketStore, TicketStoreError};
use crate::ports::triage_eval::{
    EvalCase, EvalCaseResult, GroundTruth, LabelSource, LabelStrength, NewTriageEval, TriageEval, TriageEvalStore,
};

// ---------------------------------------------------------------------------------------------
// Variant
// ---------------------------------------------------------------------------------------------

/// A variant file (`mage eval triage run --variant <file.toml>`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvalVariant {
    pub name: String,
    /// `None`: the org's triage harness.
    #[serde(default)]
    pub harness: Option<HarnessKind>,
    /// `None`: the org's triage model, else the server default.
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default = "default_prompt_version")]
    pub prompt_version: u16,
    /// Ablation: `KnowledgeScope.disabled_layers`.
    #[serde(default)]
    pub disabled_layers: Vec<Layer>,
    /// `false`: no server-side candidates in the prompt.
    #[serde(default = "default_true")]
    pub seeds: bool,
    #[serde(default)]
    pub budget: BudgetOverrides,
    /// At most this many eval runs of the eval in flight (`--concurrency`); `None` = unbounded.
    #[serde(default)]
    pub concurrency: Option<u16>,
}

fn default_prompt_version() -> u16 {
    TRIAGE_PROMPT_VERSION
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BudgetOverrides {
    #[serde(default)]
    pub max_wall_ms: Option<u64>,
    #[serde(default)]
    pub max_turns: Option<u32>,
    #[serde(default)]
    pub max_cost_micros: Option<u64>,
    #[serde(default)]
    pub max_gate_attempts: Option<u8>,
}

impl BudgetOverrides {
    fn apply(&self, mut b: Budget) -> Budget {
        if let Some(ms) = self.max_wall_ms {
            b.max_wall = Duration::from_millis(ms);
        }
        if let Some(v) = self.max_turns {
            b.max_turns = v;
        }
        if let Some(v) = self.max_cost_micros {
            b.max_cost_micros = v;
        }
        if let Some(v) = self.max_gate_attempts {
            b.max_gate_attempts = v;
        }
        b
    }
}

impl EvalVariant {
    pub fn parse(toml_text: &str) -> Result<Self, AppError> {
        let v: EvalVariant = toml::from_str(toml_text).map_err(|e| AppError::Invalid(format!("eval variant: {e}")))?;
        if v.name.trim().is_empty() {
            return Err(AppError::Invalid("eval variant: name is empty".into()));
        }
        if v.concurrency == Some(0) {
            return Err(AppError::Invalid("eval variant: concurrency must be at least 1".into()));
        }
        Ok(v)
    }
}

/// `variant_toml` with `concurrency` set (comments are not preserved when it is set).
fn with_concurrency(variant_toml: &str, concurrency: Option<u16>) -> Result<String, AppError> {
    let Some(c) = concurrency else { return Ok(variant_toml.to_owned()) };
    let mut table: toml::Table = toml::from_str(variant_toml).map_err(|e| AppError::Invalid(format!("eval variant: {e}")))?;
    table.insert("concurrency".into(), toml::Value::Integer(i64::from(c)));
    toml::to_string(&table).map_err(|e| AppError::Internal(format!("eval variant: {e}")))
}

// ---------------------------------------------------------------------------------------------
// StartTriageEval (`mage eval triage run`)
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartTriageEvalRequest {
    /// Recorded corpus name.
    pub corpus: String,
    pub variant_toml: String,
    /// Repeats per ticket (`--repeat`, ≥ 1).
    pub repeat: u8,
    /// At most this many tickets (`--limit`).
    pub limit: Option<u32>,
    pub concurrency: Option<u16>,
    pub created_by: UserId,
}

/// Creates a `TriageEval` over the org's resolved, labeled tickets and enqueues one
/// `triage.eval_case` per (ticket, repeat).
pub struct StartTriageEval {
    tickets: Arc<dyn TicketStore>,
    runs: Arc<dyn RunStore>,
    evals: Arc<dyn TriageEvalStore>,
    jobs: Arc<dyn JobQueue>,
}

impl StartTriageEval {
    pub fn new(tickets: Arc<dyn TicketStore>, runs: Arc<dyn RunStore>, evals: Arc<dyn TriageEvalStore>, jobs: Arc<dyn JobQueue>) -> Self {
        Self { tickets, runs, evals, jobs }
    }

    pub async fn run(&self, org: OrgId, req: StartTriageEvalRequest) -> Result<TriageEval, AppError> {
        if req.repeat == 0 {
            return Err(AppError::Invalid("repeat must be at least 1".into()));
        }
        let variant_toml = with_concurrency(&req.variant_toml, req.concurrency)?;
        let variant = EvalVariant::parse(&variant_toml)?;
        let resolved = resolved_tickets(self.tickets.as_ref(), org).await?;
        let labeled: BTreeSet<TicketId> = self.evals.labels(org, &resolved).await?.into_iter().map(|l| l.ticket).collect();
        let limit = req.limit.map_or(usize::MAX, |l| l as usize);
        let mut cases = Vec::new();
        for ticket in resolved.into_iter().filter(|t| labeled.contains(t)).take(limit) {
            let input_run = live_triage_runs(self.runs.as_ref(), org, ticket).await?.last().map(|r| r.run.id);
            cases.push(EvalCase { ticket, input_run, repeats: req.repeat });
        }
        if cases.is_empty() {
            return Err(AppError::Invalid("no resolved ticket with a ground-truth label; run `mage eval triage label` first".into()));
        }
        let eval = self
            .evals
            .create_eval(
                org,
                NewTriageEval { name: variant.name.clone(), corpus: req.corpus, variant_toml, repeat: req.repeat, created_by: req.created_by },
            )
            .await?;
        self.evals.add_cases(org, eval, &cases).await?;
        let jobs: Vec<NewJob> = cases
            .iter()
            .flat_map(|c| (0..c.repeats).map(move |repeat| NewJob::new(JobPayload::TriageEvalCase { org_id: org, eval_id: eval, ticket_id: c.ticket, repeat })))
            .collect();
        self.jobs.enqueue_all(&jobs).await.map_err(job_error)?;
        Ok(self.evals.eval(org, eval).await?)
    }
}

/// Every `Resolved` ticket of the org, oldest id first.
async fn resolved_tickets(tickets: &dyn TicketStore, org: OrgId) -> Result<Vec<TicketId>, AppError> {
    let filter = TicketFilter { status: Some(vec![TicketStatus::Resolved]), ..TicketFilter::default() };
    let mut page = PageRequest::first(PageRequest::MAX_LIMIT);
    let mut out = Vec::new();
    loop {
        let got = tickets.list_tickets(org, &filter, page.clone()).await?;
        out.extend(got.items.iter().map(|t| t.id));
        match got.next {
            Some(cursor) => page = page.after(cursor),
            None => break,
        }
    }
    out.sort();
    out.dedup();
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// StartEvalCase (job `triage.eval_case`)
// ---------------------------------------------------------------------------------------------

pub struct StartEvalCaseDeps {
    pub tickets: Arc<dyn TicketStore>,
    pub runs: Arc<dyn RunStore>,
    pub run_manager: RunManager,
    pub repos: Arc<dyn RepoStore>,
    pub orgs: Arc<dyn OrgStore>,
    pub retriever: Arc<Retriever>,
    pub evals: Arc<dyn TriageEvalStore>,
    pub jobs: Arc<dyn JobQueue>,
    pub clock: Arc<dyn Clock>,
    pub ids: Arc<dyn IdGen>,
}

/// UC1 without ticket preconditions or transitions: replays the case's stored input with the
/// variant's overrides as a `TriageMode::Eval` run.
pub struct StartEvalCase {
    d: StartEvalCaseDeps,
    cfg: TriageConfig,
}

impl StartEvalCase {
    pub fn new(deps: StartEvalCaseDeps, cfg: TriageConfig) -> Self {
        Self { d: deps, cfg }
    }

    pub async fn run(&self, job: &LeasedJob) -> Result<StartOutcome, AppError> {
        let JobPayload::TriageEvalCase { org_id: org, eval_id, ticket_id, repeat } = job.payload else {
            return Err(AppError::Invalid(format!("job {} is not triage.eval_case", job.kind)));
        };
        let now = self.d.clock.now();
        let eval = self.d.evals.eval(org, eval_id).await?;
        let variant = EvalVariant::parse(&eval.variant_toml)?;
        if let Some(run) = self.d.evals.case_run(org, eval_id, ticket_id, repeat).await? {
            tracing::info!(eval = %eval_id, ticket = %ticket_id, repeat, %run, "eval case already started");
            return Ok(StartOutcome::Started { run, cancelled: Vec::new() });
        }
        if let Some(limit) = variant.concurrency {
            let running = in_flight(self.d.runs.as_ref(), org, eval_id).await?;
            if running >= usize::from(limit) {
                let in_flight = u16::try_from(running).unwrap_or(u16::MAX);
                return Ok(StartOutcome::Deferred { until: now + self.cfg.defer_step, why: DeferReason::EvalConcurrency { in_flight } });
            }
        }
        let case = self
            .d
            .evals
            .cases(org, eval_id)
            .await?
            .into_iter()
            .find(|c| c.ticket == ticket_id)
            .ok_or_else(|| AppError::NotFound(format!("eval case {eval_id}/{ticket_id}")))?;
        let settings = self.cfg.clamp(self.d.orgs.triage_settings(org).await?);
        let harness = variant.harness.unwrap_or(settings.harness);
        if !self.cfg.harness_allowed(harness) {
            return Err(AppError::Invalid(format!("eval variant harness {harness:?} is not allowed for triage on this server")));
        }
        let run_id: RunId = self.d.ids.new_id();
        let mut notices = Vec::new();

        let mut input = match case.input_run {
            Some(input_run) => {
                let rec = self.d.runs.get(org, input_run).await?;
                rec.spec.triage.map(|t| t.input).ok_or_else(|| AppError::Internal(format!("input run {input_run} has no triage spec")))?
            }
            None => {
                notices.push("eval input: current ticket (no stored triage input)".to_owned());
                self.current_input(org, ticket_id, now).await?
            }
        };

        let key = RepoVersionKey::new(input.target.repo_id, input.target.sha);
        let version = match self.d.repos.version(org, key).await {
            Ok(v) => v,
            Err(RepoStoreError::NotFound) => {
                let index =
                    JobPayload::IndexVersion { org_id: org, repo_id: key.repo_id, sha: key.sha, reason: VersionReason::OnDemand(ticket_id) };
                self.d.jobs.enqueue(NewJob::new(index)).await.map_err(job_error)?;
                return Ok(StartOutcome::Deferred {
                    until: now + self.cfg.defer_step,
                    why: DeferReason::IndexPending { waiting_on: vec![Layer::Files] },
                });
            }
            Err(e) => return Err(repo_error(e)),
        };
        let mut degraded = match layer_policy(&version, now, settings.layer_wait, self.cfg.files_wait) {
            LayerVerdict::Proceed { degraded } => degraded,
            LayerVerdict::Wait { waiting_on } => {
                return Ok(StartOutcome::Deferred { until: now + self.cfg.defer_step, why: DeferReason::IndexPending { waiting_on } });
            }
            LayerVerdict::Failed(layer) => {
                tracing::warn!(eval = %eval_id, ticket = %ticket_id, ?layer, "eval case skipped: index failed");
                return Ok(StartOutcome::Skipped(SkipReason::IndexFailed(layer)));
            }
            LayerVerdict::TimedOut => {
                tracing::warn!(eval = %eval_id, ticket = %ticket_id, "eval case skipped: index timed out");
                return Ok(StartOutcome::Skipped(SkipReason::IndexTimeout));
            }
        };
        degraded.extend(variant.disabled_layers.iter().copied());
        degraded.sort();
        degraded.dedup();

        // Seeds are recomputed under the eval scope (memory cut at the snapshot), never reused.
        input.seeds = None;
        if variant.seeds {
            let memory = MemoryCutoff { exclude_tickets: vec![ticket_id], as_of: Some(input.snapshot_at) };
            input.seeds = self.snapshot().seeds_for(org, &input, run_id, memory, variant.disabled_layers.clone(), &mut notices).await;
        }

        let spec = TriageSpec {
            mode: TriageMode::Eval { eval: eval_id, repeat, disabled_layers: variant.disabled_layers.clone() },
            trigger: TriageTrigger::Auto,
            input,
            prompt_version: variant.prompt_version,
            max_causes: settings.max_causes,
            degraded_layers: degraded,
        };
        let rendered = prompt::render_with_fresh_nonce(&spec.input, &spec, self.d.ids.as_ref())?;
        let budget = self.cfg.clamp(mage_core::wire::api::TriageSettings { budget: variant.budget.apply(settings.budget), ..settings.clone() }).budget;
        let model = variant.model.clone().filter(|m| !m.trim().is_empty()).unwrap_or_else(|| self.cfg.model(&settings));
        let record = create_triage_run(
            &self.d.run_manager,
            NewTriageRun {
                org,
                ticket: ticket_id,
                id: run_id,
                create_key: job.lease.job_id.to_string(),
                harness,
                model,
                budget,
                requested_by: None,
                spec,
                rendered,
                notices,
            },
        )
        .await?;
        // A retry after a crash finds the run created by the first attempt (same create key).
        self.d.evals.record_case_run(org, eval_id, ticket_id, repeat, record.run.id).await?;
        Ok(StartOutcome::Started { run: record.run.id, cancelled: Vec::new() })
    }

    fn snapshot(&self) -> Snapshot<'_> {
        Snapshot {
            tickets: self.d.tickets.as_ref(),
            repos: self.d.repos.as_ref(),
            orgs: self.d.orgs.as_ref(),
            retriever: self.d.retriever.as_ref(),
            cfg: &self.cfg,
        }
    }

    /// The current ticket as input, for cases without a stored first triage (reported through
    /// the run notice `eval input: current ticket`).
    async fn current_input(&self, org: OrgId, ticket_id: TicketId, now: time::OffsetDateTime) -> Result<TriageInput, AppError> {
        let ticket = self.d.tickets.get_ticket(org, ticket_id).await?.value;
        let target = ticket.target.clone().ok_or_else(|| AppError::Invalid(format!("eval ticket {ticket_id} has no target")))?;
        self.snapshot().input(org, &ticket, &target, now).await
    }
}

/// Non-terminal runs of one eval.
async fn in_flight(runs: &dyn RunStore, org: OrgId, eval: TriageEvalId) -> Result<usize, AppError> {
    let q = RunQuery {
        kind: Some(RunKind::Triage),
        statuses: vec![RunStatus::Queued, RunStatus::Leased, RunStatus::Preparing, RunStatus::Running, RunStatus::AwaitingGate],
        limit: RunQuery::MAX_LIMIT,
        ..RunQuery::default()
    };
    let all = runs.list(org, &q).await?;
    Ok(all.iter().filter(|r| matches!(r.spec.triage.as_ref().map(|t| &t.mode), Some(TriageMode::Eval { eval: e, .. }) if *e == eval)).count())
}

// ---------------------------------------------------------------------------------------------
// Case result (recorded by ApplyTriageOutcome)
// ---------------------------------------------------------------------------------------------

/// Paths of a cause; the primary path is the first citation with a quote (the gate verified it).
fn cause_paths(c: &CauseDraft) -> impl Iterator<Item = &RepoPath> {
    c.evidence.iter().map(|e| &e.path)
}

fn primary_path(c: &CauseDraft) -> Option<&RepoPath> {
    c.evidence.iter().find(|e| e.quote.is_some()).or_else(|| c.evidence.first()).map(|e| &e.path)
}

/// `hit(c) = min { i : paths(C_i) ∩ G ≠ ∅ }` (1-based), and the same on primary paths only.
pub fn hit_ranks(causes: &[CauseDraft], label: &BTreeSet<RepoPath>) -> (Option<u16>, Option<u16>) {
    let rank = |i: usize| u16::try_from(i + 1).unwrap_or(u16::MAX);
    let hit = causes.iter().position(|c| cause_paths(c).any(|p| label.contains(p))).map(rank);
    let primary = causes.iter().position(|c| primary_path(c).is_some_and(|p| label.contains(p))).map(rank);
    (hit, primary)
}

fn accepted_causes(rec: &RunRecord) -> &[CauseDraft] {
    match (&rec.run.status, &rec.run.result) {
        (RunStatus::Succeeded, Some(RunResult::Triage(result))) => &result.causes,
        _ => &[],
    }
}

fn non_negative(d: time::Duration) -> Duration {
    Duration::try_from(d).unwrap_or_default()
}

/// The case row of one finished eval run.
pub fn eval_case_result(
    eval: TriageEvalId,
    ticket: TicketId,
    repeat: u8,
    rec: &RunRecord,
    label: Option<&GroundTruth>,
    gates: &[GateRecord],
) -> Result<EvalCaseResult, AppError> {
    let causes = accepted_causes(rec);
    let (hit_rank, primary_hit_rank) = label.map_or((None, None), |l| hit_ranks(causes, &l.paths));
    let created = rec.run.created_at;
    let finished = rec.run.finished_at.unwrap_or(created);
    let (queue_wait, agent_time) = match rec.run.started_at {
        Some(started) => (non_negative(started - created), non_negative(finished - started)),
        None => (non_negative(finished - created), Duration::ZERO),
    };
    let evidence_gates = gates.iter().filter(|g| g.gate.kind == GateKind::Evidence).count();
    let details = if rec.run.status == RunStatus::Succeeded { newest_evidence_details(gates)? } else { None };
    let (dropped, adjusted) = details.map_or((0, 0), |d| (d.invalid_causes(), d.adjusted_citations()));
    let small = |n: usize| u8::try_from(n).unwrap_or(u8::MAX);
    Ok(EvalCaseResult {
        eval,
        ticket,
        repeat,
        run: rec.run.id,
        status: rec.run.status,
        hit_rank,
        primary_hit_rank,
        causes: small(causes.len()),
        usage: rec.effective_usage(finished),
        queue_wait,
        agent_time,
        gate_attempts: small(evidence_gates),
        dropped_causes: small(dropped),
        adjusted_citations: small(adjusted),
    })
}

// ---------------------------------------------------------------------------------------------
// Labels (`mage eval triage label`)
// ---------------------------------------------------------------------------------------------

/// Builds and stores ground-truth labels.
pub struct LabelTriageEval {
    tickets: Arc<dyn TicketStore>,
    knowledge: Arc<dyn KnowledgeStore>,
    evals: Arc<dyn TriageEvalStore>,
}

impl LabelTriageEval {
    pub fn new(tickets: Arc<dyn TicketStore>, knowledge: Arc<dyn KnowledgeStore>, evals: Arc<dyn TriageEvalStore>) -> Self {
        Self { tickets, knowledge, evals }
    }

    /// A manual label (`Strong`), replacing any other label of the ticket.
    pub async fn put_manual(&self, org: OrgId, ticket: TicketId, paths: BTreeSet<RepoPath>) -> Result<GroundTruth, AppError> {
        if paths.is_empty() {
            return Err(AppError::Invalid("a label needs at least one path".into()));
        }
        self.tickets.get_ticket(org, ticket).await?;
        let label = GroundTruth { ticket, paths, strength: LabelStrength::Strong, source: LabelSource::Manual };
        self.evals.put_label(org, &label).await?;
        Ok(label)
    }

    /// `--auto`: labels every resolved ticket that has no manual label, from its fix PR's changed
    /// files (`Strong`), else from a reproduced confirmed cause's evidence paths (`Medium`).
    /// Returns the labels written; tickets with neither stay unlabeled (excluded from evals).
    pub async fn auto(&self, org: OrgId) -> Result<Vec<GroundTruth>, AppError> {
        let resolved = resolved_tickets(self.tickets.as_ref(), org).await?;
        let manual: BTreeSet<TicketId> =
            self.evals.labels(org, &resolved).await?.into_iter().filter(|l| l.source == LabelSource::Manual).map(|l| l.ticket).collect();
        let mut written = Vec::new();
        for ticket_id in resolved.into_iter().filter(|t| !manual.contains(t)) {
            if let Some(label) = self.auto_label(org, ticket_id).await? {
                self.evals.put_label(org, &label).await?;
                written.push(label);
            }
        }
        Ok(written)
    }

    async fn auto_label(&self, org: OrgId, ticket_id: TicketId) -> Result<Option<GroundTruth>, AppError> {
        let Some(resolution) = self.tickets.resolution(org, ticket_id).await? else { return Ok(None) };
        let ticket = self.tickets.get_ticket(org, ticket_id).await?.value;
        if let Some(repo) = ticket.target.as_ref().map(|t| t.repo_id) {
            let pr = resolution.fix_pr_url.as_ref().and_then(|u| pr_number(u.path()));
            let paths = self.fix_files(org, repo, &crate::app::triage::start::identifier(&ticket), pr).await?;
            if !paths.is_empty() {
                return Ok(Some(GroundTruth { ticket: ticket_id, paths, strength: LabelStrength::Strong, source: LabelSource::FixPr }));
            }
        }
        if let Some(cause_id) = resolution.confirmed_cause {
            let cause = match self.tickets.get_cause(org, cause_id).await {
                Ok(c) => c,
                Err(TicketStoreError::NotFound) => return Ok(None),
                Err(e) => return Err(e.into()),
            };
            if matches!(cause.status, CauseStatus::Reproduced { .. }) {
                let paths: BTreeSet<RepoPath> = cause.evidence.iter().map(|c| c.path.clone()).collect();
                if !paths.is_empty() {
                    return Ok(Some(GroundTruth {
                        ticket: ticket_id,
                        paths,
                        strength: LabelStrength::Medium,
                        source: LabelSource::ConfirmedCause,
                    }));
                }
            }
        }
        Ok(None)
    }

    /// Files changed by the commits History linked to the ticket (identifier) or to the fix PR,
    /// minus tests, docs, lockfiles and generated files.
    async fn fix_files(&self, org: OrgId, repo: RepoId, identifier: &str, pr: Option<u32>) -> Result<BTreeSet<RepoPath>, AppError> {
        let mut links = self.knowledge.ticket_links(org, repo, &LinkLookup::Identifier(identifier.to_owned())).await.map_err(knowledge_error)?;
        if let Some(n) = pr {
            links.extend(self.knowledge.ticket_links(org, repo, &LinkLookup::Prs(vec![n])).await.map_err(knowledge_error)?);
        }
        let shas: Vec<CommitSha> = links.iter().filter_map(|l| l.commit).collect::<BTreeSet<_>>().into_iter().collect();
        if shas.is_empty() {
            return Ok(BTreeSet::new());
        }
        let commits = self.knowledge.commits(org, repo, &shas).await.map_err(knowledge_error)?;
        Ok(commits
            .iter()
            .flat_map(|c| c.info.files.iter().flatten())
            .map(|f| f.path.clone())
            .filter(|p| !excluded_from_label(p))
            .collect())
    }
}

/// PR number from a GitHub PR URL path (`/{owner}/{repo}/pull/{n}`).
fn pr_number(path: &str) -> Option<u32> {
    let mut segments = path.trim_end_matches('/').rsplit('/');
    let n = segments.next()?.parse().ok()?;
    (segments.next()? == "pull").then_some(n)
}

/// Tests, docs, lockfiles and generated files never count as ground truth.
pub fn excluded_from_label(path: &RepoPath) -> bool {
    const LOCKFILES: [&str; 10] = [
        "package-lock.json",
        "yarn.lock",
        "pnpm-lock.yaml",
        "Cargo.lock",
        "go.sum",
        "Gemfile.lock",
        "poetry.lock",
        "composer.lock",
        "Pipfile.lock",
        "bun.lockb",
    ];
    const DOC_EXT: [&str; 5] = [".md", ".mdx", ".rst", ".adoc", ".txt"];
    const TEST_DIRS: [&str; 5] = ["test", "tests", "__tests__", "spec", "e2e"];
    const GENERATED_DIRS: [&str; 5] = ["generated", "__generated__", "dist", "vendor", "node_modules"];
    let p = path.as_str();
    let file = path.file_name();
    let lower = file.to_ascii_lowercase();
    let dirs: Vec<&str> = path.segments().collect::<Vec<_>>().split_last().map(|(_, d)| d.to_vec()).unwrap_or_default();
    LOCKFILES.contains(&file)
        || DOC_EXT.iter().any(|e| lower.ends_with(e))
        || dirs.iter().any(|d| *d == "docs" || *d == "doc")
        || dirs.iter().any(|d| TEST_DIRS.contains(d))
        || lower.contains(".test.")
        || lower.contains(".spec.")
        || lower.contains("_test.")
        || lower.starts_with("test_")
        || dirs.iter().any(|d| GENERATED_DIRS.contains(d))
        || lower.contains(".gen.")
        || lower.contains("_generated.")
        || lower.ends_with(".pb.go")
        || lower.ends_with(".min.js")
        || p.ends_with(".snap")
}

// ---------------------------------------------------------------------------------------------
// Report (`mage eval triage report`)
// ---------------------------------------------------------------------------------------------

/// recall@1, @3, @5 and MRR over one set of cases.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize)]
pub struct RecallMetrics {
    pub cases: u32,
    /// k = 1, 3, 5.
    pub recall_at: [f64; 3],
    pub primary_recall_at: [f64; 3],
    pub mrr: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize)]
pub struct Distribution {
    pub mean: f64,
    pub p50: f64,
    pub p95: f64,
    pub total: f64,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize)]
pub struct GateStats {
    /// Share of gated runs whose first evidence attempt was rejected.
    pub attempt1_rejection_rate: f64,
    pub dropped_causes: u64,
    pub dropped_citations: u64,
    pub adjusted_citations: u64,
    pub salvaged: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TriageEvalReport {
    pub eval: TriageEvalId,
    pub name: String,
    pub cases: u32,
    pub done: u32,
    /// Case results without a ground-truth label (excluded from recall).
    pub unlabeled: u32,
    pub all: RecallMetrics,
    /// Headline: recall@3 on `Strong` labels.
    pub strong: RecallMetrics,
    /// Bootstrap 95% interval of `strong.recall_at[1]` (tickets resampled with their repeats).
    pub strong_recall3_ci95: Option<(f64, f64)>,
    pub citation_precision: f64,
    pub zero_cause_rate: f64,
    pub gate: GateStats,
    pub outcomes: BTreeMap<String, u32>,
    pub cost_micros: Distribution,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub turns: Distribution,
    pub tool_calls: BTreeMap<String, u64>,
    pub answering_layers: BTreeMap<String, u64>,
    pub queue_wait_secs: Distribution,
    pub agent_time_secs: Distribution,
}

/// One finished case with what the report needs beyond the stored row.
#[derive(Debug, Clone)]
pub struct CaseOutcome {
    pub result: EvalCaseResult,
    pub label: Option<(LabelStrength, BTreeSet<RepoPath>)>,
    /// Distinct cited paths of the accepted causes.
    pub cited: BTreeSet<RepoPath>,
    pub salvaged: bool,
    pub dropped_citations: u64,
    pub first_attempt_rejected: bool,
    pub tool_calls: BTreeMap<String, u64>,
    pub answering_layers: BTreeMap<String, u64>,
}

pub struct EvalReport {
    runs: Arc<dyn RunStore>,
    evals: Arc<dyn TriageEvalStore>,
}

impl EvalReport {
    pub fn new(runs: Arc<dyn RunStore>, evals: Arc<dyn TriageEvalStore>) -> Self {
        Self { runs, evals }
    }

    pub async fn run(&self, org: OrgId, eval: TriageEvalId) -> Result<TriageEvalReport, AppError> {
        let header = self.evals.eval(org, eval).await?;
        let results = self.evals.case_results(org, eval).await?;
        let tickets: Vec<TicketId> = results.iter().map(|r| r.ticket).collect::<BTreeSet<_>>().into_iter().collect();
        let labels: BTreeMap<TicketId, GroundTruth> =
            self.evals.labels(org, &tickets).await?.into_iter().map(|l| (l.ticket, l)).collect();
        let mut outcomes = Vec::with_capacity(results.len());
        for result in results {
            let rec = self.runs.get(org, result.run).await?;
            let gates = self.runs.gates(org, result.run).await?;
            let (tool_calls, answering_layers) = self.tool_stats(org, result.run).await?;
            outcomes.push(case_outcome(result, &rec, &gates, labels.get(&rec_ticket(&rec)?), tool_calls, answering_layers)?);
        }
        Ok(compute_report(&header, &outcomes))
    }

    /// Tool calls by name and Knowledge MCP answering layers from the run's event log.
    async fn tool_stats(&self, org: OrgId, run: RunId) -> Result<(BTreeMap<String, u64>, BTreeMap<String, u64>), AppError> {
        const PAGE: u32 = 1000;
        let mut calls = BTreeMap::new();
        let mut layers = BTreeMap::new();
        let mut after = 0;
        loop {
            let events = self.runs.events_after(org, run, after, PAGE).await?;
            for e in &events {
                match &e.kind {
                    mage_core::domain::RunEventKind::ToolCall { name, .. } => *calls.entry(name.clone()).or_insert(0) += 1,
                    mage_core::domain::RunEventKind::ToolResult { ok: true, preview, .. } => {
                        if let Some(layer) = answering_layer(preview) {
                            *layers.entry(layer.to_owned()).or_insert(0) += 1;
                        }
                    }
                    _ => {}
                }
            }
            match events.last() {
                Some(last) if events.len() == PAGE as usize => after = last.seq,
                _ => break,
            }
        }
        Ok((calls, layers))
    }
}

fn rec_ticket(rec: &RunRecord) -> Result<TicketId, AppError> {
    match rec.run.subject {
        RunSubject::Ticket(t) => Ok(t),
        other => Err(AppError::Internal(format!("eval run {} has subject {other:?}", rec.run.id))),
    }
}

/// `"layer":"ErrorSites"` of a Knowledge MCP response envelope preview.
fn answering_layer(preview: &str) -> Option<&str> {
    let start = preview.find("\"layer\":\"")? + "\"layer\":\"".len();
    let len = preview[start..].find('"')?;
    let layer = &preview[start..start + len];
    (!layer.is_empty() && layer.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')).then_some(layer)
}

/// Builds the report row of one case from its run and gates.
pub fn case_outcome(
    result: EvalCaseResult,
    rec: &RunRecord,
    gates: &[GateRecord],
    label: Option<&GroundTruth>,
    tool_calls: BTreeMap<String, u64>,
    answering_layers: BTreeMap<String, u64>,
) -> Result<CaseOutcome, AppError> {
    let cited = accepted_causes(rec).iter().flat_map(cause_paths).cloned().collect();
    let mut evidence: Vec<&GateRecord> = gates.iter().filter(|g| g.gate.kind == GateKind::Evidence).collect();
    evidence.sort_by_key(|g| g.gate.attempt);
    let newest_passed = evidence.last().is_some_and(|g| matches!(g.gate.status, GateStatus::Passed { .. }));
    let salvaged = rec.run.status == RunStatus::Succeeded && !evidence.is_empty() && !newest_passed;
    let first_attempt_rejected = evidence.first().is_some_and(|g| matches!(g.gate.status, GateStatus::Failed { .. }));
    let dropped_citations = match newest_evidence_details(gates)? {
        Some(d) => d.causes.iter().filter(|c| c.verdict == mage_core::domain::evidence::CauseEvidence::Valid).map(|c| {
            c.citations.iter().filter(|x| matches!(x, mage_core::domain::evidence::CitationCheck::Rejected(_))).count() as u64
        }).sum(),
        None => 0,
    };
    Ok(CaseOutcome {
        result,
        label: label.map(|l| (l.strength, l.paths.clone())),
        cited,
        salvaged,
        dropped_citations,
        first_attempt_rejected,
        tool_calls,
        answering_layers,
    })
}

const KS: [u16; 3] = [1, 3, 5];

/// recall@k = share of cases with `hit ≤ k`; MRR = mean of `1/hit` (0 for no hit).
pub fn recall_metrics<'a>(cases: impl IntoIterator<Item = &'a EvalCaseResult>) -> RecallMetrics {
    let mut m = RecallMetrics::default();
    let mut rr = 0.0;
    for c in cases {
        m.cases += 1;
        for (i, k) in KS.iter().enumerate() {
            if c.hit_rank.is_some_and(|h| h <= *k) {
                m.recall_at[i] += 1.0;
            }
            if c.primary_hit_rank.is_some_and(|h| h <= *k) {
                m.primary_recall_at[i] += 1.0;
            }
        }
        rr += c.hit_rank.map_or(0.0, |h| 1.0 / f64::from(h));
    }
    if m.cases > 0 {
        let n = f64::from(m.cases);
        for v in m.recall_at.iter_mut().chain(m.primary_recall_at.iter_mut()) {
            *v /= n;
        }
        m.mrr = rr / n;
    }
    m
}

fn distribution(mut values: Vec<f64>) -> Distribution {
    if values.is_empty() {
        return Distribution::default();
    }
    values.sort_by(f64::total_cmp);
    let total: f64 = values.iter().sum();
    Distribution { mean: total / values.len() as f64, p50: percentile(&values, 0.50), p95: percentile(&values, 0.95), total }
}

/// Nearest-rank percentile of sorted values.
fn percentile(sorted: &[f64], q: f64) -> f64 {
    let rank = (q * sorted.len() as f64).ceil().max(1.0) as usize;
    sorted[rank.min(sorted.len()) - 1]
}

/// Deterministic bootstrap (xorshift64*) over tickets: 95% interval of recall@3.
fn bootstrap_recall3(by_ticket: &[Vec<&EvalCaseResult>]) -> Option<(f64, f64)> {
    const ROUNDS: usize = 1000;
    if by_ticket.is_empty() {
        return None;
    }
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut next = || {
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        state.wrapping_mul(0x2545_F491_4F6C_DD1D)
    };
    let mut samples = Vec::with_capacity(ROUNDS);
    for _ in 0..ROUNDS {
        let (mut hits, mut n) = (0usize, 0usize);
        for _ in 0..by_ticket.len() {
            let pick = &by_ticket[(next() % by_ticket.len() as u64) as usize];
            n += pick.len();
            hits += pick.iter().filter(|c| c.hit_rank.is_some_and(|h| h <= 3)).count();
        }
        if n > 0 {
            samples.push(hits as f64 / n as f64);
        }
    }
    samples.sort_by(f64::total_cmp);
    if samples.is_empty() {
        return None;
    }
    Some((percentile(&samples, 0.025), percentile(&samples, 0.975)))
}

/// Pure report over the finished cases.
pub fn compute_report(eval: &TriageEval, cases: &[CaseOutcome]) -> TriageEvalReport {
    let labeled: Vec<&CaseOutcome> = cases.iter().filter(|c| c.label.is_some()).collect();
    let strong: Vec<&CaseOutcome> = labeled.iter().copied().filter(|c| matches!(c.label, Some((LabelStrength::Strong, _)))).collect();
    let mut strong_by_ticket: BTreeMap<TicketId, Vec<&EvalCaseResult>> = BTreeMap::new();
    for c in &strong {
        strong_by_ticket.entry(c.result.ticket).or_default().push(&c.result);
    }
    let clusters: Vec<Vec<&EvalCaseResult>> = strong_by_ticket.into_values().collect();

    let (mut cited, mut cited_in_label) = (0usize, 0usize);
    for c in &labeled {
        if let Some((_, paths)) = &c.label {
            cited += c.cited.len();
            cited_in_label += c.cited.iter().filter(|p| paths.contains(*p)).count();
        }
    }
    let accepted: Vec<&CaseOutcome> = cases.iter().filter(|c| c.result.status == RunStatus::Succeeded).collect();
    let zero = accepted.iter().filter(|c| c.result.causes == 0).count();
    let gated: Vec<&CaseOutcome> = cases.iter().filter(|c| c.result.gate_attempts > 0).collect();

    let mut outcomes = BTreeMap::new();
    let mut tool_calls = BTreeMap::new();
    let mut answering_layers = BTreeMap::new();
    for c in cases {
        *outcomes.entry(c.result.status.name().to_owned()).or_insert(0) += 1;
        for (k, v) in &c.tool_calls {
            *tool_calls.entry(k.clone()).or_insert(0) += v;
        }
        for (k, v) in &c.answering_layers {
            *answering_layers.entry(k.clone()).or_insert(0) += v;
        }
    }
    let share = |a: usize, b: usize| if b == 0 { 0.0 } else { a as f64 / b as f64 };
    TriageEvalReport {
        eval: eval.id,
        name: eval.name.clone(),
        cases: eval.cases,
        done: eval.done,
        unlabeled: u32::try_from(cases.len() - labeled.len()).unwrap_or(u32::MAX),
        all: recall_metrics(labeled.iter().map(|c| &c.result)),
        strong: recall_metrics(strong.iter().map(|c| &c.result)),
        strong_recall3_ci95: bootstrap_recall3(&clusters),
        citation_precision: share(cited_in_label, cited),
        zero_cause_rate: share(zero, accepted.len()),
        gate: GateStats {
            attempt1_rejection_rate: share(gated.iter().filter(|c| c.first_attempt_rejected).count(), gated.len()),
            dropped_causes: cases.iter().map(|c| u64::from(c.result.dropped_causes)).sum(),
            dropped_citations: cases.iter().map(|c| c.dropped_citations).sum(),
            adjusted_citations: cases.iter().map(|c| u64::from(c.result.adjusted_citations)).sum(),
            salvaged: u32::try_from(cases.iter().filter(|c| c.salvaged).count()).unwrap_or(u32::MAX),
        },
        outcomes,
        cost_micros: distribution(cases.iter().map(|c| c.result.usage.cost_micros as f64).collect()),
        input_tokens: cases.iter().map(|c| c.result.usage.input_tokens).sum(),
        output_tokens: cases.iter().map(|c| c.result.usage.output_tokens).sum(),
        turns: distribution(cases.iter().map(|c| f64::from(c.result.usage.turns)).collect()),
        tool_calls,
        answering_layers,
        queue_wait_secs: distribution(cases.iter().map(|c| c.result.queue_wait.as_secs_f64()).collect()),
        agent_time_secs: distribution(cases.iter().map(|c| c.result.agent_time.as_secs_f64()).collect()),
    }
}

#[cfg(test)]
#[path = "eval_tests.rs"]
mod tests;
