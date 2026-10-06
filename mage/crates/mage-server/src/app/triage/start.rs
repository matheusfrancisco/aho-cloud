//! UC1 `StartTriage` (job `triage.ticket`; triage.md "Preconditions", "Layer readiness", "Harness
//! choice", "Steps", "Fencing").

use std::sync::Arc;
use std::time::Duration;

use time::OffsetDateTime;

use mage_core::domain::{
    CommitSha, HarnessKind, JobPayload, Layer, LayerState, MemberRole, OrgId, RepoVersion, RepoVersionKey, RunId, RunKind, RunStatus,
    RunSubject, TargetOrigin, Ticket, TicketId, TicketSource, TicketStatus, TicketTarget, TriageTrigger, VersionReason,
};
use mage_core::domain::{Budget, CauseStatus, ContentHash};
use mage_core::ports::common::PageRequest;
use mage_core::ports::jobs::{JobQueue, LeasedJob, NewJob};
use mage_core::ports::repos::{RepoStore, RepoStoreError};
use mage_core::ports::{Clock, IdGen};
use mage_core::wire::runner::RepoRef;
use mage_core::wire::token::MemoryCutoff;

use crate::app::knowledge_query::{KnowledgeScope, RetrievalLimits, Retriever};
use crate::app::runs::{CancelRun, CreateRun, RunManager, RunPrompt};
use crate::app::triage::config::TriageConfig;
use crate::app::triage::prompt::{self, RenderedPrompt, TRIAGE_PROMPT_VERSION};
use crate::app::triage::{job_error, repo_error};
use crate::domain::knowledge::{CandidateSet, TicketContext};
use crate::domain::triage::{CustomerRef, PriorHypothesis, TriageInput, TriageMode, TriageSpec};
use crate::error::AppError;
use crate::ports::org::{OrgStore, OrgStoreError};
use crate::ports::runs::{RunQuery, RunRecord, RunStore, RunStoreError};
use crate::ports::ticket::{TicketStore, TicketStoreError};

/// Statuses a triage may start from (`TicketStatus::apply(TriageStarted)` accepts exactly these).
pub const TRIAGE_FROM: [TicketStatus; 4] = [TicketStatus::New, TicketStatus::Triaged, TicketStatus::TriageFailed, TicketStatus::Triaging];

/// Window of the auto-retriage cap.
const RETRIAGE_WINDOW: time::Duration = time::Duration::hours(24);

/// `triage.ticket` payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TriageTicketPayload {
    pub org_id: OrgId,
    pub ticket_id: TicketId,
    pub content_hash: ContentHash,
    pub trigger: TriageTrigger,
}

impl TriageTicketPayload {
    pub fn from_payload(payload: &JobPayload) -> Option<Self> {
        match payload {
            JobPayload::TriageTicket { org_id, ticket_id, content_hash, trigger } => {
                Some(Self { org_id: *org_id, ticket_id: *ticket_id, content_hash: *content_hash, trigger: *trigger })
            }
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartOutcome {
    Started { run: RunId, cancelled: Vec<RunId> },
    /// The worker calls `JobQueue::defer(&lease, until)`.
    Deferred { until: OffsetDateTime, why: DeferReason },
    Skipped(SkipReason),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeferReason {
    IndexPending { waiting_on: Vec<Layer> },
    /// Eval only: the variant's `concurrency` eval runs are in flight.
    EvalConcurrency { in_flight: u16 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    TicketMissing,
    Stale,
    AlreadyTriaged,
    Refused(TicketStatus),
    NoTarget,
    AutoDisabled,
    RetriageCapReached,
    TicketCostCapReached,
    IndexFailed(Layer),
    IndexTimeout,
}

impl SkipReason {
    pub fn name(&self) -> &'static str {
        match self {
            Self::TicketMissing => "ticket_missing",
            Self::Stale => "stale",
            Self::AlreadyTriaged => "already_triaged",
            Self::Refused(_) => "refused",
            Self::NoTarget => "no_target",
            Self::AutoDisabled => "auto_disabled",
            Self::RetriageCapReached => "retriage_cap_reached",
            Self::TicketCostCapReached => "ticket_cost_cap_reached",
            Self::IndexFailed(_) => "index_failed",
            Self::IndexTimeout => "index_timeout",
        }
    }
}

pub struct StartTriageDeps {
    pub tickets: Arc<dyn TicketStore>,
    pub runs: Arc<dyn RunStore>,
    pub run_manager: RunManager,
    pub repos: Arc<dyn RepoStore>,
    pub orgs: Arc<dyn OrgStore>,
    pub retriever: Arc<Retriever>,
    pub jobs: Arc<dyn JobQueue>,
    pub clock: Arc<dyn Clock>,
    pub ids: Arc<dyn IdGen>,
}

pub struct StartTriage {
    d: StartTriageDeps,
    cfg: TriageConfig,
}

/// How one pass through the steps ended when it did not produce an outcome.
enum Halt {
    /// `begin_triage` lost its CAS: re-read and restart once.
    BeginConflict,
    /// Failed after `begin_triage` recorded `run` on the ticket, before the run existed.
    AfterBegin { run: RunId, error: AppError },
    Error(AppError),
}

impl From<AppError> for Halt {
    fn from(e: AppError) -> Self {
        Halt::Error(e)
    }
}

impl From<TicketStoreError> for Halt {
    fn from(e: TicketStoreError) -> Self {
        Halt::Error(e.into())
    }
}

impl From<RunStoreError> for Halt {
    fn from(e: RunStoreError) -> Self {
        Halt::Error(e.into())
    }
}

impl From<OrgStoreError> for Halt {
    fn from(e: OrgStoreError) -> Self {
        Halt::Error(e.into())
    }
}

impl StartTriage {
    pub fn new(deps: StartTriageDeps, cfg: TriageConfig) -> Self {
        Self { d: deps, cfg }
    }

    /// Job `triage.ticket`. Idempotent per job: the run's create key is the job id, the run id is
    /// recorded on the ticket before the run is created and reused by a retry.
    pub async fn run(&self, job: &LeasedJob, p: &TriageTicketPayload) -> Result<StartOutcome, AppError> {
        let mut restarted = false;
        loop {
            match self.pass(job, p).await {
                Ok(outcome) => return Ok(outcome),
                Err(Halt::BeginConflict) if !restarted => restarted = true,
                Err(Halt::BeginConflict) => return Err(AppError::Conflict(format!("ticket {} changed while triage was starting", p.ticket_id))),
                Err(Halt::AfterBegin { run, error }) => {
                    if job.attempt >= job.max_attempts {
                        // Last attempt: never leave the ticket Triaging behind a run that does not exist.
                        match self.d.tickets.fail_triage(p.org_id, p.ticket_id, run, &[]).await {
                            Ok(_) | Err(TicketStoreError::Conflict) => {}
                            Err(e) => tracing::error!(ticket = %p.ticket_id, run = %run, error = %e, "fail_triage after the last attempt failed"),
                        }
                    }
                    return Err(error);
                }
                Err(Halt::Error(e)) => return Err(e),
            }
        }
    }

    async fn pass(&self, job: &LeasedJob, p: &TriageTicketPayload) -> Result<StartOutcome, Halt> {
        let org = p.org_id;
        let now = self.d.clock.now();
        let ticket = match self.d.tickets.get_ticket(org, p.ticket_id).await {
            Ok(versioned) => versioned.value,
            Err(TicketStoreError::NotFound) => return Ok(logged_skip(p, SkipReason::TicketMissing)),
            Err(e) => return Err(e.into()),
        };
        if ticket.content_hash != p.content_hash {
            return Ok(logged_skip(p, SkipReason::Stale));
        }
        if !TRIAGE_FROM.contains(&ticket.status) {
            return Ok(self.skip(p, &ticket, &[], SkipReason::Refused(ticket.status)).await?);
        }
        let settings = self.cfg.clamp(self.d.orgs.triage_settings(org).await?);
        let runs = live_triage_runs(self.d.runs.as_ref(), org, ticket.id).await?;

        if let Some(newest) = runs.first() {
            let same_hash = newest.spec.triage.as_ref().is_some_and(|t| t.input.content_hash == p.content_hash);
            if same_hash && (p.trigger == TriageTrigger::Auto || newest.run.created_at >= job.enqueued_at) {
                let cancelled = self.cancel_others(org, &runs, newest.run.id).await?;
                if !cancelled.is_empty() {
                    tracing::info!(ticket = %ticket.id, ?cancelled, "cancelled superseded triage runs");
                }
                return Ok(self.skip(p, &ticket, &runs, SkipReason::AlreadyTriaged).await?);
            }
        }
        if p.trigger == TriageTrigger::Auto && !settings.auto_triage {
            return Ok(self.skip(p, &ticket, &runs, SkipReason::AutoDisabled).await?);
        }
        if p.trigger == TriageTrigger::Auto && auto_retriages(&runs, now) >= usize::from(settings.auto_retriage_per_day) {
            return Ok(self.skip(p, &ticket, &runs, SkipReason::RetriageCapReached).await?);
        }
        let spent: u64 = runs.iter().map(|r| r.effective_usage(now).cost_micros).sum();
        if spent >= settings.ticket_cost_cap_micros && !self.may_exceed_cost_cap(org, p.trigger).await? {
            return Ok(self.skip(p, &ticket, &runs, SkipReason::TicketCostCapReached).await?);
        }
        let Some(target) = ticket.target.clone() else {
            return Ok(self.skip(p, &ticket, &runs, SkipReason::NoTarget).await?);
        };

        let key = RepoVersionKey::new(target.repo_id, target.sha);
        let version = match self.d.repos.version(org, key).await {
            Ok(version) => version,
            Err(RepoStoreError::NotFound) => {
                let index = JobPayload::IndexVersion { org_id: org, repo_id: target.repo_id, sha: target.sha, reason: VersionReason::OnDemand(ticket.id) };
                self.d.jobs.enqueue(NewJob::new(index)).await.map_err(job_error)?;
                return Ok(self.defer(p, now, vec![Layer::Files]));
            }
            Err(e) => return Err(repo_error(e).into()),
        };
        let degraded = match layer_policy(&version, now, settings.layer_wait, self.cfg.files_wait) {
            LayerVerdict::Proceed { degraded } => degraded,
            LayerVerdict::Wait { waiting_on } => return Ok(self.defer(p, now, waiting_on)),
            LayerVerdict::Failed(layer) => return Ok(self.skip(p, &ticket, &runs, SkipReason::IndexFailed(layer)).await?),
            LayerVerdict::TimedOut => return Ok(self.skip(p, &ticket, &runs, SkipReason::IndexTimeout).await?),
        };

        let run_id = self.choose_run_id(org, &ticket, p.content_hash, &runs).await?;
        let (harness, harness_notice) = self.cfg.choose_harness(settings.harness);
        let mut notices: Vec<String> = harness_notice.into_iter().collect();
        let snapshot = Snapshot {
            tickets: self.d.tickets.as_ref(),
            repos: self.d.repos.as_ref(),
            orgs: self.d.orgs.as_ref(),
            retriever: self.d.retriever.as_ref(),
            cfg: &self.cfg,
        };
        let mut input = snapshot.input(org, &ticket, &target, now).await?;
        let memory = MemoryCutoff { exclude_tickets: vec![ticket.id], as_of: None };
        input.seeds = snapshot.seeds_for(org, &input, run_id, memory, Vec::new(), &mut notices).await;
        let spec = TriageSpec {
            mode: TriageMode::Live,
            trigger: p.trigger,
            input,
            prompt_version: TRIAGE_PROMPT_VERSION,
            max_causes: settings.max_causes,
            degraded_layers: degraded,
        };
        let rendered = prompt::render_with_fresh_nonce(&spec.input, &spec, self.d.ids.as_ref())?;

        match self.d.tickets.begin_triage(org, ticket.id, &TRIAGE_FROM, p.content_hash, run_id).await {
            Ok(_) => {}
            Err(TicketStoreError::Conflict) => return Err(Halt::BeginConflict),
            Err(e) => return Err(e.into()),
        }

        let requested_by = match p.trigger {
            TriageTrigger::Manual { by } => Some(by),
            TriageTrigger::Auto | TriageTrigger::Requeue => None,
        };
        let new_run = NewTriageRun {
            org,
            ticket: ticket.id,
            id: run_id,
            create_key: job.lease.job_id.to_string(),
            harness,
            model: self.cfg.model(&settings),
            budget: settings.budget,
            requested_by,
            spec,
            rendered,
            notices,
        };
        match create_triage_run(&self.d.run_manager, new_run).await {
            Ok(record) if record.run.id == run_id => {}
            // The create key (this job) already made another run: cannot normally happen (the
            // AlreadyTriaged precondition skips first); the job retries.
            Ok(record) => {
                let error = AppError::Conflict(format!("create key already used by triage run {}", record.run.id));
                return Err(Halt::AfterBegin { run: run_id, error });
            }
            Err(error) => return Err(Halt::AfterBegin { run: run_id, error }),
        }

        let cancelled = self.cancel_others(org, &runs, run_id).await?;
        tracing::info!(ticket = %ticket.id, run = %run_id, ?cancelled, trigger = ?p.trigger, "triage started");
        Ok(StartOutcome::Started { run: run_id, cancelled })
    }

    /// Manual retriage by an Admin or Owner bypasses the ticket cost cap.
    async fn may_exceed_cost_cap(&self, org: OrgId, trigger: TriageTrigger) -> Result<bool, AppError> {
        let TriageTrigger::Manual { by } = trigger else { return Ok(false) };
        match self.d.orgs.membership(org, by).await {
            Ok(Some(m)) => Ok(m.role.at_least(MemberRole::Admin)),
            Ok(None) | Err(OrgStoreError::NotFound) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    /// Step 4: reuse the run id an earlier attempt of this job recorded on the ticket when that run
    /// was never created; otherwise a fresh id.
    async fn choose_run_id(&self, org: OrgId, ticket: &Ticket, hash: ContentHash, runs: &[RunRecord]) -> Result<RunId, AppError> {
        if ticket.status == TicketStatus::Triaging && ticket.triaged_hash == Some(hash) {
            if let Some(recorded) = ticket.triage_run {
                if !runs.iter().any(|r| r.run.id == recorded) {
                    match self.d.runs.get(org, recorded).await {
                        Err(RunStoreError::NotFound) => return Ok(recorded),
                        Ok(_) => {}
                        Err(e) => return Err(e.into()),
                    }
                }
            }
        }
        Ok(self.d.ids.new_id())
    }

    /// Step 8: cancel every non-terminal live Triage run of the ticket except `keep`.
    async fn cancel_others(&self, org: OrgId, runs: &[RunRecord], keep: RunId) -> Result<Vec<RunId>, AppError> {
        let mut cancelled = Vec::new();
        for r in runs.iter().filter(|r| r.run.id != keep && !r.run.status.is_terminal()) {
            self.d.run_manager.cancel_run(CancelRun { org, run: r.run.id, by: None, notice: Some(format!("superseded by triage run {keep}")) }).await?;
            cancelled.push(r.run.id);
        }
        Ok(cancelled)
    }

    fn defer(&self, p: &TriageTicketPayload, now: OffsetDateTime, waiting_on: Vec<Layer>) -> StartOutcome {
        tracing::info!(ticket = %p.ticket_id, ?waiting_on, "triage deferred: index pending");
        StartOutcome::Deferred { until: now + self.cfg.defer_step, why: DeferReason::IndexPending { waiting_on } }
    }

    /// Skips never change the ticket, except a `Triaging` ticket whose fencing run already ended
    /// without success (a `Requeue` that can no longer start): it is failed so it cannot hang.
    /// A `Succeeded` fencing run is left to `ApplyTriageOutcome`, which owns that transition.
    async fn skip(&self, p: &TriageTicketPayload, ticket: &Ticket, runs: &[RunRecord], reason: SkipReason) -> Result<StartOutcome, AppError> {
        if ticket.status == TicketStatus::Triaging {
            if let Some(fence) = ticket.triage_run {
                let status = match runs.iter().find(|r| r.run.id == fence) {
                    Some(r) => Some(r.run.status),
                    None => match self.d.runs.get(p.org_id, fence).await {
                        Ok(r) => Some(r.run.status),
                        Err(RunStoreError::NotFound) => None,
                        Err(e) => return Err(e.into()),
                    },
                };
                if status.is_some_and(|s| s.is_terminal() && s != RunStatus::Succeeded) {
                    match self.d.tickets.fail_triage(p.org_id, ticket.id, fence, &[]).await {
                        Ok(_) | Err(TicketStoreError::Conflict) => {}
                        Err(e) => return Err(e.into()),
                    }
                }
            }
        }
        Ok(logged_skip(p, reason))
    }
}

fn logged_skip(p: &TriageTicketPayload, reason: SkipReason) -> StartOutcome {
    tracing::info!(
        target: "mage.triage",
        metric = "mage.triage.skips",
        ticket = %p.ticket_id,
        reason = reason.name(),
        detail = ?reason,
        trigger = ?p.trigger,
        "triage skipped"
    );
    StartOutcome::Skipped(reason)
}

/// The ticket's live (non-eval) Triage runs, newest first.
pub(crate) async fn live_triage_runs(runs: &dyn RunStore, org: OrgId, ticket: TicketId) -> Result<Vec<RunRecord>, AppError> {
    let q = RunQuery {
        kind: Some(RunKind::Triage),
        subject: Some(RunSubject::Ticket(ticket)),
        newest_first: true,
        limit: RunQuery::MAX_LIMIT,
        ..RunQuery::default()
    };
    let all = runs.list(org, &q).await?;
    Ok(all.into_iter().filter(|r| r.spec.triage.as_ref().is_some_and(|t| !t.is_eval())).collect())
}

/// `Auto` retriages created in the last 24 h; the ticket's first triage run is not counted.
fn auto_retriages(runs_newest_first: &[RunRecord], now: OffsetDateTime) -> usize {
    let since = now - RETRIAGE_WINDOW;
    let Some((_first, later)) = runs_newest_first.split_last() else { return 0 };
    later
        .iter()
        .filter(|r| r.run.created_at > since && r.spec.triage.as_ref().is_some_and(|t| t.trigger == TriageTrigger::Auto))
        .count()
}

/// Outcome of the layer table (triage.md "Layer readiness").
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LayerVerdict {
    Proceed { degraded: Vec<Layer> },
    Wait { waiting_on: Vec<Layer> },
    Failed(Layer),
    TimedOut,
}

const PREFERRED: [Layer; 2] = [Layer::Symbols, Layer::ErrorSites];
const OPTIONAL: [Layer; 4] = [Layer::Refs, Layer::AppMap, Layer::Semantic, Layer::History];

/// `Files` required, `Symbols`/`ErrorSites` waited on for `layer_wait`, the rest optional.
pub(crate) fn layer_policy(version: &RepoVersion, now: OffsetDateTime, layer_wait: Duration, files_wait: Duration) -> LayerVerdict {
    let state = |layer: Layer| version.layer(layer).cloned().unwrap_or(LayerState::Pending);
    let in_progress = |s: &LayerState| matches!(s, LayerState::Pending | LayerState::Running);
    let mut waiting = Vec::new();
    match state(Layer::Files) {
        LayerState::Ready { .. } => {}
        LayerState::Failed { .. } | LayerState::Skipped { .. } => return LayerVerdict::Failed(Layer::Files),
        LayerState::Pending | LayerState::Running => {
            if now >= version.created_at + files_wait {
                return LayerVerdict::TimedOut;
            }
            waiting.push(Layer::Files);
        }
    }
    let mut degraded = Vec::new();
    for layer in PREFERRED {
        let s = state(layer);
        if s.is_ready() {
            continue;
        }
        if in_progress(&s) && now < version.created_at + layer_wait {
            waiting.push(layer);
        } else {
            degraded.push(layer);
        }
    }
    if !waiting.is_empty() {
        return LayerVerdict::Wait { waiting_on: waiting };
    }
    degraded.extend(OPTIONAL.into_iter().filter(|l| !state(*l).is_ready()));
    degraded.sort();
    LayerVerdict::Proceed { degraded }
}

/// Builds the `TriageInput` snapshot (shared with eval cases).
pub(crate) struct Snapshot<'a> {
    pub tickets: &'a dyn TicketStore,
    pub repos: &'a dyn RepoStore,
    pub orgs: &'a dyn OrgStore,
    pub retriever: &'a Retriever,
    pub cfg: &'a TriageConfig,
}

impl Snapshot<'_> {
    /// The ticket as Mage sees it now, without seeds (see [`Self::seeds_for`]).
    pub(crate) async fn input(&self, org: OrgId, ticket: &Ticket, target: &TicketTarget, now: OffsetDateTime) -> Result<TriageInput, AppError> {
        let repo = self.repos.repo(org, target.repo_id).await.map_err(repo_error)?;
        let customer = match ticket.customer {
            None => None,
            Some(id) => match self.orgs.get_customer(org, id).await {
                Ok(c) => Some(CustomerRef { name: c.name, environment: c.environment }),
                Err(OrgStoreError::NotFound) => None,
                Err(e) => return Err(e.into()),
            },
        };
        let environment = match &target.origin {
            TargetOrigin::Deployment { environment } => Some(environment.clone()),
            _ => customer.as_ref().and_then(|c| c.environment.clone()),
        };
        let prior_sha = match environment {
            Some(env) => prior_deployment(self.repos, org, target, &env).await?,
            None => None,
        };
        let prior_hypotheses = prior_hypotheses(self.tickets, org, ticket.id).await?;
        Ok(TriageInput {
            content_hash: ticket.content_hash,
            identifier: identifier(ticket),
            title: ticket.title.clone(),
            body: ticket.body.clone(),
            error_strings: ticket.error_strings.clone(),
            target: target.clone(),
            repo: RepoRef { repo_id: repo.id, owner: repo.owner, name: repo.name },
            customer,
            prior_sha,
            prior_hypotheses,
            seeds: None,
            snapshot_at: now,
        })
    }

    /// `Retriever::triage_candidates` for `input` under the run's knowledge scope; `None` (and a run
    /// notice) when retrieval fails or times out.
    pub(crate) async fn seeds_for(
        &self,
        org: OrgId,
        input: &TriageInput,
        run: RunId,
        memory: MemoryCutoff,
        disabled_layers: Vec<Layer>,
        notices: &mut Vec<String>,
    ) -> Option<CandidateSet> {
        let ctx = TicketContext {
            title: input.title.clone(),
            body: input.body.clone(),
            error_strings: input.error_strings.clone(),
            last_good: input.prior_sha,
        };
        let scope = KnowledgeScope { org, run, repo: input.target.repo_id, sha: input.target.sha, memory, disabled_layers };
        match seed_candidates(self.retriever, &scope, &ctx, self.cfg).await {
            Ok(set) => Some(set),
            Err(reason) => {
                tracing::warn!(%run, %reason, "triage seeding unavailable");
                notices.push(format!("candidate retrieval unavailable ({reason}); the prompt has no seeds"));
                None
            }
        }
    }
}

/// Server-side seeding with the configured timeout; `Err` carries a short reason for the notice.
async fn seed_candidates(retriever: &Retriever, scope: &KnowledgeScope, ctx: &TicketContext, cfg: &TriageConfig) -> Result<CandidateSet, String> {
    let limits = RetrievalLimits { max_candidates: cfg.seed_candidates, ..RetrievalLimits::default() };
    match tokio::time::timeout(cfg.seed_timeout, retriever.triage_candidates(scope, ctx, &limits)).await {
        Ok(Ok(set)) => Ok(set),
        Ok(Err(e)) => Err(e.to_string()),
        Err(_) => Err(format!("timed out after {} s", cfg.seed_timeout.as_secs())),
    }
}

pub(crate) fn identifier(ticket: &Ticket) -> String {
    match &ticket.source {
        TicketSource::Linear { identifier, .. } => identifier.clone(),
    }
}

/// The previous deployment of `env`: the first older observation with a different sha than the
/// target (or the newest one when the target was never observed there).
async fn prior_deployment(repos: &dyn RepoStore, org: OrgId, target: &TicketTarget, env: &str) -> Result<Option<CommitSha>, AppError> {
    let page = repos.list_deployments(org, target.repo_id, PageRequest::first(PageRequest::MAX_LIMIT)).await.map_err(repo_error)?;
    let in_env: Vec<CommitSha> = page.items.iter().filter(|d| d.environment == env).map(|d| d.sha).collect();
    let start = in_env.iter().position(|sha| *sha == target.sha).map_or(0, |i| i + 1);
    Ok(in_env[start..].iter().copied().find(|sha| *sha != target.sha))
}

/// Earlier causes of the ticket a diagnostic run tested (`NotReproduced` / `Inconclusive`).
async fn prior_hypotheses(tickets: &dyn TicketStore, org: OrgId, ticket: TicketId) -> Result<Vec<PriorHypothesis>, AppError> {
    let causes = tickets.causes(org, ticket, true).await?;
    Ok(causes
        .into_iter()
        .filter_map(|c| {
            let verdict = match &c.status {
                CauseStatus::NotReproduced { .. } => "not reproduced".to_owned(),
                CauseStatus::Inconclusive { reason, .. } => format!("inconclusive ({reason})"),
                _ => return None,
            };
            Some(PriorHypothesis { title: c.title.clone(), verdict, summary: c.explanation.clone() })
        })
        .collect())
}

/// Inputs of one Triage run creation (live or eval).
pub(crate) struct NewTriageRun {
    pub org: OrgId,
    pub ticket: TicketId,
    pub id: RunId,
    pub create_key: String,
    pub harness: HarnessKind,
    pub model: String,
    pub budget: Budget,
    pub requested_by: Option<mage_core::domain::UserId>,
    pub spec: TriageSpec,
    pub rendered: RenderedPrompt,
    pub notices: Vec<String>,
}

/// Step 7 through the run manager. A reused create key returns the run created with it (which
/// may carry another id than `n.id`).
pub(crate) async fn create_triage_run(manager: &RunManager, n: NewTriageRun) -> Result<RunRecord, AppError> {
    let mut create = CreateRun::new(n.org, RunKind::Triage, RunSubject::Ticket(n.ticket));
    create.id = Some(n.id);
    create.harness = Some(n.harness);
    create.model = Some(n.model);
    create.budget = Some(n.budget);
    create.requested_by = n.requested_by;
    create.create_key = Some(n.create_key);
    create.prompt = Some(RunPrompt { prompt: n.rendered.initial_prompt, system_append: n.rendered.system_append });
    create.triage = Some(n.spec);
    create.notices = n.notices;
    manager.create_run(create).await
}

#[cfg(test)]
#[path = "start_tests.rs"]
mod tests;
