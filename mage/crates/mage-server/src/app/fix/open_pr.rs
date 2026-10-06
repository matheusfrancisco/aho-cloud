//! `OpenPullRequest` (fix.md F3), the `fix.open_pr` job, and `RequestPullRequest` (the UI's
//! **Open PR**, which only enqueues the job).
//!
//! Every step is idempotent and its progress is persisted in the `fix_pull_requests` row
//! (`FixPrState`), so a crash resumes from the last completed step without a second branch or
//! PR:
//!
//! ```text
//! Planned ──create_commit──▶ CommitCreated{C} ──create_branch──▶ BranchCreated ──find_open/open──▶ Opened{n}
//! ```
//!
//! The row and branch are keyed by the **gated** change set (`T0`, the passed Fix gate). The
//! commit is built from the latest proof `T`: `T0` when the parent is the gated base, or the
//! rebase proof at the moved default head `H` (`branch_from = default_head`). `CommitPlan` can only
//! be built from `T`, and it re-checks `T`'s digest against the patch bytes being committed.

use std::sync::Arc;

use mage_core::domain::fix::{BranchFrom, BranchName, CommitPlan, FixPrEvent, FixPrState, FixPullRequest, FixSettings, Patch, RegressionContent};
use mage_core::domain::{
    CommitSha, FixGatePassed, GateFailureCode, MemberRole, Notice, OrgId, Repo, RunId, RunKind, RunStatus, RunSubject, Ticket,
    TicketId, TicketSource, TicketStatus, UserId, VerifySpec,
};
use mage_core::domain::fix::ApplyMode;
use mage_core::ports::jobs::Enqueued;
use mage_core::wire::api::JobQueued;
use time::OffsetDateTime;
use url::Url;

use crate::app::fix::accept::SCRIPT_MAX_BYTES;
use crate::app::fix::brief::script_command;
use crate::app::fix::media::{MediaCache, PrMedia};
use crate::app::fix::pr_body::{self, LintRow, PrBodyInput};
use crate::app::fix::proof::{FixProof, GateProof, verify_result};
use crate::app::fix::{FixDeps, ScriptInfo, job_err, notify, open_pr_job, recipe_fake_values, repo_err};
use crate::app::runs::CreateRun;
use crate::error::AppError;
use crate::ports::pull_requests::{BranchOutcome, OpenPullRequest as OpenRequest, PullRequestsError, Reviewers};
use crate::ports::runs::RunRecord;
use crate::ports::secretscan::{ScanInput, SecretFinding};
use crate::ports::ticket::{TicketStoreError, TicketTransition};

/// `-2` … `-5` suffixes tried when the branch is taken by someone else's commits.
const MAX_BRANCH_SUFFIX: u32 = 5;
/// Failed / errored rebase verifies at one head before giving up.
const MAX_REBASE_RUNS: usize = 2;
/// Labels every Mage PR carries.
const LABELS: [&str; 2] = ["mage", "mage:fix"];

/// What a `fix.open_pr` run achieved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenPrOutcome {
    Opened { number: u64, url: Url },
    /// A rebase Verify run at the moved default head decides; look again at `until`.
    WaitingForRebase { verify_run: RunId, until: OffsetDateTime },
    /// No PR (and no retry): already fixed upstream, rebase conflict, rebase gate failed, or the
    /// change set's PR was closed.
    NotOpened { reason: String },
}

#[derive(Clone)]
pub struct OpenPullRequest {
    d: Arc<FixDeps>,
    proof: FixProof,
    media: Arc<PrMedia>,
}

/// Everything resolved before touching GitHub.
struct Ctx {
    org: OrgId,
    ticket: Ticket,
    identifier: String,
    ticket_url: Url,
    repo: Repo,
    settings: FixSettings,
    gated: GateProof,
    requested_by: Option<UserId>,
}

fn pr_err(e: PullRequestsError) -> AppError {
    e.into()
}

impl OpenPullRequest {
    pub fn new(d: Arc<FixDeps>) -> Self {
        let proof = FixProof::new(d.runs.clone());
        let media = Arc::new(PrMedia {
            runs: d.runs.clone(),
            artifacts: d.artifacts.clone(),
            pull_requests: d.pull_requests.clone(),
            cache: MediaCache::new(d.artifacts.clone()),
        });
        Self { d, proof, media }
    }

    /// The `fix.open_pr` job. `requested_by` is unknown to the job payload; the body says
    /// "auto (org setting)" unless the caller passes the member.
    pub async fn run(&self, org: OrgId, ticket: TicketId, fix_run: RunId, requested_by: Option<UserId>) -> Result<OpenPrOutcome, AppError> {
        let ctx = self.preconditions(org, ticket, fix_run, requested_by).await?;
        let digest = ctx.gated.token.digest();
        let now = self.d.clock.now();
        let planned = FixPullRequest::plan(self.d.ids.new_id(), org, ctx.ticket.id, &ctx.identifier, &ctx.gated.token, now)?;
        let (mut row, _created) = self.d.tickets.upsert_pull_request(org, planned, &[]).await?;
        if row.change_set != digest {
            return Err(AppError::Internal("pull request row for another change set".into()));
        }
        match row.state.clone() {
            FixPrState::Closed { merged } => {
                Ok(OpenPrOutcome::NotOpened { reason: format!("the PR for this change set was closed{}", if merged { " (merged)" } else { "" }) })
            }
            FixPrState::Opened { number } => {
                let url = row.url.clone().ok_or_else(|| AppError::Internal("opened PR row without url".into()))?;
                self.finish_ticket(&ctx, &row).await?;
                Ok(OpenPrOutcome::Opened { number, url })
            }
            FixPrState::Planned => {
                // Step 2: parent per policy (maybe the rebase flow), then the gated plan.
                let token = match self.decide_parent(&ctx).await? {
                    Ok(token) => token,
                    Err(outcome) => return Ok(outcome),
                };
                let (plan, body) = self.build(&ctx, &token).await?;
                let commit = self.d.pull_requests.create_commit(org, &ctx.repo, &plan).await.map_err(pr_err)?;
                let mut next = row.clone();
                next.apply(FixPrEvent::CommitCreated { commit }, self.d.clock.now())?;
                row = self.d.tickets.update_pull_request(org, &next, &FixPrState::Planned, None, &[]).await?;
                row = self.branch(&ctx, row, commit).await?;
                self.open(&ctx, row, &body).await
            }
            FixPrState::CommitCreated { commit } => {
                row = self.branch(&ctx, row, commit).await?;
                let body = self.body_for_resume(&ctx, &row).await?;
                self.open(&ctx, row, &body).await
            }
            FixPrState::BranchCreated => {
                let body = self.body_for_resume(&ctx, &row).await?;
                self.open(&ctx, row, &body).await
            }
        }
    }

    async fn preconditions(&self, org: OrgId, ticket_id: TicketId, fix_run: RunId, requested_by: Option<UserId>) -> Result<Ctx, AppError> {
        let d = &self.d;
        let settings = d.orgs.fix_settings(org).await?;
        if !settings.pull_requests {
            return Err(AppError::Invalid("fix pull requests are disabled".into()));
        }
        let ticket = d.tickets.get_ticket(org, ticket_id).await?.value;
        let gated = self.proof.gate_proof(org, fix_run).await?;
        let cause = d.tickets.get_cause(org, gated.token.cause()).await?;
        if cause.ticket_id != ticket.id {
            return Err(AppError::Invalid("the fix run belongs to another ticket".into()));
        }
        // The latest succeeded Fix run of the cause.
        let q = crate::ports::runs::RunQuery {
            kind: Some(RunKind::Fix),
            subject: Some(RunSubject::Cause(cause.id)),
            statuses: vec![RunStatus::Succeeded],
            newest_first: true,
            limit: 1,
            ..Default::default()
        };
        let latest = d.runs.list(org, &q).await?;
        if latest.first().map(RunRecord::id) != Some(fix_run) {
            return Err(AppError::Conflict("a newer fix run exists for this cause".into()));
        }
        if !matches!(ticket.status, TicketStatus::FixReady | TicketStatus::PrOpen) {
            return Err(AppError::Conflict(format!("ticket is {}; a PR needs fix ready", ticket.status.name())));
        }
        let target = ticket.target.clone().ok_or_else(|| AppError::Invalid("the ticket has no target".into()))?;
        let repo = d.repos.repo(org, target.repo_id).await.map_err(repo_err)?;
        let patch_paths = self.patch_paths(org, &gated.token).await?;
        let needs_workflows = patch_paths.iter().any(|p| p.starts_with(".github/workflows/") || p.starts_with(".github/actions/"));
        let access = d.pull_requests.write_access(org, &repo).await.map_err(pr_err)?;
        let missing = access.missing(needs_workflows);
        if !missing.is_empty() {
            return Err(PullRequestsError::PermissionDenied { missing }.into());
        }
        let TicketSource::Linear { identifier, url, .. } = &ticket.source;
        let (identifier, ticket_url) = (identifier.clone(), url.clone());
        Ok(Ctx { org, ticket, identifier, ticket_url, repo, settings, gated, requested_by })
    }

    async fn patch_paths(&self, org: OrgId, token: &FixGatePassed) -> Result<Vec<String>, AppError> {
        let (_, raw) = self.d.artifact_bytes(org, token.change_set().patch, u64::MAX).await?;
        let patch = Patch::parse(&raw).map_err(|e| AppError::Internal(format!("gated patch no longer parses: {e}")))?;
        Ok(patch.paths().map(|p| p.as_str().to_owned()).collect())
    }

    /// Step 2: the proof whose base becomes the commit parent, or why there is no PR (yet).
    async fn decide_parent(&self, ctx: &Ctx) -> Result<Result<FixGatePassed, OpenPrOutcome>, AppError> {
        let gated = &ctx.gated;
        if ctx.settings.pr.branch_from == BranchFrom::GatedSha {
            return Ok(Ok(gated.token.clone()));
        }
        let head = self
            .d
            .pull_requests
            .branch_head(ctx.org, &ctx.repo, &ctx.repo.default_branch)
            .await
            .map_err(pr_err)?
            .ok_or_else(|| AppError::Invalid(format!("default branch {} not found", ctx.repo.default_branch)))?;
        if head == gated.token.base_sha() {
            return Ok(Ok(gated.token.clone()));
        }
        let runs = self.proof.rebase_runs(ctx.org, gated.fix_run.id(), head).await?;
        let until = self.d.clock.now() + time::Duration::seconds(i64::try_from(self.d.config.rebase_poll_secs).unwrap_or(120));
        // Newest first: a running one is waited for; the newest finished one decides.
        match runs.first() {
            Some(open) if !open.run.status.is_terminal() => {
                return Ok(Err(OpenPrOutcome::WaitingForRebase { verify_run: open.id(), until }));
            }
            Some(done) if done.run.status == RunStatus::Succeeded => {
                return match self.proof.rebase_proof(ctx.org, gated, done).await? {
                    Ok(token) => Ok(Ok(token)),
                    Err(failure) => {
                        let reason = match failure.code {
                            GateFailureCode::AlreadyFixedUpstream => format!("already fixed upstream: the repro passes unpatched on {head}"),
                            GateFailureCode::RebaseConflict => {
                                format!("the fix does not apply to {head} (rebase conflict); re-run the fix on the new head")
                            }
                            _ => format!("the fix did not pass the gate on {head}: {failure}"),
                        };
                        self.d.notice(ctx.org, gated.fix_run.id(), format!("No PR opened: {reason}.")).await?;
                        Ok(Err(OpenPrOutcome::NotOpened { reason }))
                    }
                };
            }
            _ => {}
        }
        if runs.len() >= MAX_REBASE_RUNS {
            let reason = format!("the rebase verify at {head} failed {} times", runs.len());
            self.d.notice(ctx.org, gated.fix_run.id(), format!("No PR opened: {reason}.")).await?;
            return Ok(Err(OpenPrOutcome::NotOpened { reason }));
        }
        // Start the rebase flow: the gated patch onto H, AllowOffset, upstream check.
        let fix_spec = gated.fix_run.spec.fix.as_ref().ok_or_else(|| AppError::Internal("fix run has no FixSpec".into()))?;
        let spec = VerifySpec::Fix {
            patch: gated.fix.patch,
            script: fix_spec.script,
            signature: fix_spec.signature.clone(),
            repetitions: ctx.settings.repetitions.max(1),
            regression: gated.details.change_set.regression.clone(),
            mode: ApplyMode::AllowOffset,
            upstream_check: true,
        };
        let mut create = CreateRun::new(ctx.org, RunKind::Verify, RunSubject::Run(gated.fix_run.id()));
        create.parent = Some(gated.fix_run.id());
        create.verify = Some(spec);
        create.sha = Some(head);
        create.notices = vec![format!("Rebase verify for the fix PR: default branch moved from {} to {head}", gated.token.base_sha())];
        let verify = self.d.run_manager.get()?.create_run(create).await?;
        Ok(Err(OpenPrOutcome::WaitingForRebase { verify_run: verify.id(), until }))
    }

    /// Step 3–4: the gated plan and the PR body, both secret-scanned.
    async fn build(&self, ctx: &Ctx, token: &FixGatePassed) -> Result<(CommitPlan, String), AppError> {
        let d = &self.d;
        let org = ctx.org;
        let cs = token.change_set();
        let (_, raw) = d.artifact_bytes(org, cs.patch, u64::MAX).await?;
        let patch = Patch::parse(&raw).map_err(|e| AppError::Internal(format!("gated patch no longer parses: {e}")))?;
        let regression = match &cs.regression {
            Some(reg) => {
                let (_, bytes) = d.artifact_bytes(org, reg.script, SCRIPT_MAX_BYTES).await?;
                Some(RegressionContent { path: reg.path.clone(), bytes })
            }
            None => None,
        };
        let paths: Vec<_> = patch.paths().cloned().collect();
        let base_files = d.pull_requests.read_files(org, &ctx.repo, token.base_sha(), &paths).await.map_err(pr_err)?;
        let message = pr_body::commit_message(
            &self.cause_title(ctx).await?,
            &ctx.identifier,
            &ctx.gated.fix.summary,
            &ctx.ticket_url,
            ctx.gated.fix_run.id(),
            ctx.gated.gate.gate.id,
            &ctx.gated.token.digest(),
        );
        let plan = CommitPlan::build(token, base_files, &patch, regression, message).map_err(|e| AppError::Invalid(format!("cannot build the gated commit: {e}")))?;
        let body = self.body(ctx, token).await?;

        // Step 4: final scan of post-image added lines and the body; a hit aborts without retry.
        let recipe = d.recipe_spec(org, ctx.repo.id, ctx.gated.fix_run.spec.recipe_hash.ok_or_else(|| AppError::Internal("fix run has no recipe".into()))?).await?;
        let allowed = recipe_fake_values(&recipe);
        let mut findings: Vec<SecretFinding> = Vec::new();
        for file in patch.files() {
            let text: String = file.added_lines().map(|(_, l)| String::from_utf8_lossy(l).into_owned()).collect();
            findings.extend(d.scanner.scan(&ScanInput { path: Some(file.path()), text: &text, known: &[], allowed: &allowed }));
        }
        for f in plan.files() {
            if let mage_core::domain::fix::CommitChange::Write { bytes, .. } = &f.change {
                if cs.regression.as_ref().is_some_and(|r| r.path == f.path) {
                    let text = String::from_utf8_lossy(bytes);
                    findings.extend(d.scanner.scan(&ScanInput { path: Some(&f.path), text: &text, known: &[], allowed: &allowed }));
                }
            }
        }
        findings.extend(d.scanner.scan(&ScanInput { path: None, text: &body, known: &[], allowed: &allowed }));
        if !findings.is_empty() {
            let list: Vec<String> = findings.iter().map(ToString::to_string).collect();
            let text = format!("No PR opened: possible secret(s) in the change or PR body ({}). An admin must review the fix.", list.join(", "));
            d.notice(org, ctx.gated.fix_run.id(), text).await?;
            return Err(AppError::Invalid("secret scanner hit in the PR change; not opening a PR".into()));
        }
        Ok((plan, body))
    }

    async fn cause_title(&self, ctx: &Ctx) -> Result<String, AppError> {
        Ok(self.d.tickets.get_cause(ctx.org, ctx.gated.token.cause()).await?.title.clone())
    }

    /// Body when resuming after the commit exists: same proof source as when it was built.
    async fn body_for_resume(&self, ctx: &Ctx, row: &FixPullRequest) -> Result<String, AppError> {
        // The parent of our commit tells which proof built it.
        let token = match row.head_commit {
            Some(head) => self.token_for_parent(ctx, head).await?,
            None => ctx.gated.token.clone(),
        };
        let body = self.body(ctx, &token).await?;
        let recipe = self.d.recipe_spec(ctx.org, ctx.repo.id, ctx.gated.fix_run.spec.recipe_hash.ok_or_else(|| AppError::Internal("fix run has no recipe".into()))?).await?;
        let allowed = recipe_fake_values(&recipe);
        if !self.d.scanner.scan(&ScanInput { path: None, text: &body, known: &[], allowed: &allowed }).is_empty() {
            return Err(AppError::Invalid("secret scanner hit in the PR body; not opening a PR".into()));
        }
        Ok(body)
    }

    /// The gated token, or the rebase token whose base is the parent of `commit`.
    async fn token_for_parent(&self, ctx: &Ctx, commit: CommitSha) -> Result<FixGatePassed, AppError> {
        let cmp = self.d.pull_requests.compare(ctx.org, &ctx.repo, ctx.gated.token.base_sha(), commit).await.map_err(pr_err)?;
        if cmp.behind_by == 0 && cmp.ahead_by <= 1 {
            return Ok(ctx.gated.token.clone());
        }
        let q = crate::ports::runs::RunQuery { kind: Some(RunKind::Verify), parent: Some(ctx.gated.fix_run.id()), statuses: vec![RunStatus::Succeeded], newest_first: true, ..Default::default() };
        for run in self.d.runs.list(ctx.org, &q).await? {
            if !crate::app::fix::proof::is_rebase_spec(run.spec.verify.as_ref()) {
                continue;
            }
            if let Ok(token) = self.proof.rebase_proof(ctx.org, &ctx.gated, &run).await? {
                let cmp = self.d.pull_requests.compare(ctx.org, &ctx.repo, token.base_sha(), commit).await.map_err(pr_err)?;
                if cmp.behind_by == 0 && cmp.ahead_by <= 1 {
                    return Ok(token);
                }
            }
        }
        Ok(ctx.gated.token.clone())
    }

    /// The PR body for the change `token` binds (media uploaded or linked).
    async fn body(&self, ctx: &Ctx, token: &FixGatePassed) -> Result<String, AppError> {
        let d = &self.d;
        let org = ctx.org;
        let cause = d.tickets.get_cause(org, token.cause()).await?;
        let recipe_hash = ctx.gated.fix_run.spec.recipe_hash.ok_or_else(|| AppError::Internal("fix run has no recipe".into()))?;
        let recipe = d.recipe_spec(org, ctx.repo.id, recipe_hash).await?;

        // Before: the repro gate's verify run of the diagnosis that reproduced the cause.
        let before = match cause.status {
            mage_core::domain::CauseStatus::Reproduced { run, gate } => {
                let gates = d.runs.gates(org, run).await?;
                match gates.into_iter().find(|g| g.gate.id == gate).and_then(|g| g.gate.verify_run) {
                    Some(v) => verify_result(&d.runs.get(org, v).await?).ok(),
                    None => None,
                }
            }
            _ => None,
        };
        // After: the proof's verify run (rebase verify when rebased).
        let rebased = token.base_sha() != ctx.gated.token.base_sha();
        let after = if rebased {
            let q = crate::ports::runs::RunQuery { kind: Some(RunKind::Verify), parent: Some(ctx.gated.fix_run.id()), sha: Some(token.base_sha()), statuses: vec![RunStatus::Succeeded], newest_first: true, ..Default::default() };
            match d.runs.list(org, &q).await?.into_iter().find(|r| crate::app::fix::proof::is_rebase_spec(r.spec.verify.as_ref())) {
                Some(r) => verify_result(&r)?,
                None => ctx.gated.verify.clone(),
            }
        } else {
            ctx.gated.verify.clone()
        };
        let pick = self.media.pick(org, before.as_ref(), &after).await;
        let media = self.media.links(org, &ctx.repo, &pick).await;

        let requester = match ctx.requested_by {
            Some(user) => Some(d.orgs.get_user(user).await.map(|u| u.name).unwrap_or_else(|_| "a member".into())),
            None => None,
        };
        let script_rec = d.runs.artifact(org, ctx.gated.fix_run.spec.fix.as_ref().map(|f| f.script).ok_or_else(|| AppError::Internal("fix run has no FixSpec".into()))?).await?;
        let script = ScriptInfo::from_record(&script_rec)?;
        let regression = token.change_set().regression.as_ref().map(|r| (&r.path, script_command(script.kind, r.path.as_str())));
        let (repetitions, signature) = match ctx.gated.verify_run.spec.verify.as_ref() {
            Some(VerifySpec::Fix { repetitions, signature, .. }) => (*repetitions, signature.value.clone()),
            _ => (1, String::new()),
        };
        let lint = match recipe.fix.as_ref().and_then(|f| f.lint.clone().map(|l| (l, f.lint_mode))) {
            None => LintRow::NotConfigured,
            Some((command, mage_core::domain::recipe::LintMode::Required)) => LintRow::Required { command },
            Some((command, mage_core::domain::recipe::LintMode::Advisory)) => {
                LintRow::Advisory { command, exit: after.fix_checks.as_ref().and_then(|c| c.lint_exit) }
            }
        };
        let input = PrBodyInput {
            ui_url: &d.config.public_url,
            github_web: &d.config.github_web,
            ticket_identifier: &ctx.identifier,
            ticket_url: &ctx.ticket_url,
            ticket_title: &ctx.ticket.title,
            requester: requester.as_deref(),
            cause_explanation: &cause.explanation,
            evidence: &cause.evidence,
            owner: &ctx.repo.owner,
            name: &ctx.repo.name,
            evidence_sha: ctx.gated.token.base_sha(),
            summary: &ctx.gated.fix.summary,
            stats: ctx.gated.fix.stats,
            excluded: &ctx.gated.fix.excluded,
            rebased: rebased.then(|| (ctx.gated.token.base_sha(), token.base_sha())),
            regression,
            parent: token.base_sha(),
            repetitions,
            signature_value: &signature,
            test_command: recipe.test.as_ref().map(|t| t.run.as_str()),
            lint,
            media: &media,
            trace: pick.trace,
            test_log: pick.test_log,
            fix_run: ctx.gated.fix_run.id(),
            gate: ctx.gated.gate.gate.id,
            digest: ctx.gated.token.digest(),
        };
        Ok(pr_body::render(&input))
    }

    /// Step 6: `create_branch`, recognising our own commit on a taken branch and suffixing
    /// `-2`…`-5` otherwise. Persists `BranchCreated`.
    async fn branch(&self, ctx: &Ctx, row: FixPullRequest, commit: CommitSha) -> Result<FixPullRequest, AppError> {
        let d = &self.d;
        let digest_hex = ctx.gated.token.digest().to_hex();
        let base = row.branch.as_str().to_owned();
        let mut chosen: Option<(BranchName, CommitSha)> = None;
        for k in 1..=MAX_BRANCH_SUFFIX {
            let name = if k == 1 { row.branch.clone() } else { BranchName::new(format!("{base}-{k}"))? };
            match d.pull_requests.create_branch(ctx.org, &ctx.repo, &name, commit).await.map_err(pr_err)? {
                BranchOutcome::Created | BranchOutcome::AlreadyAt => {
                    chosen = Some((name, commit));
                    break;
                }
                BranchOutcome::ExistsAt(other) => {
                    let trailers = d.pull_requests.commit_trailers(ctx.org, &ctx.repo, other).await.map_err(pr_err)?;
                    if trailers.iter().any(|(k, v)| k == "Mage-Change-Set" && *v == digest_hex) {
                        chosen = Some((name, other));
                        break;
                    }
                }
            }
        }
        let (branch, head) = chosen.ok_or_else(|| AppError::Invalid(format!("branch {base} and its -2…-{MAX_BRANCH_SUFFIX} suffixes are taken by other commits")))?;
        let expect = row.state.clone();
        let mut next = row;
        next.apply(FixPrEvent::BranchCreated { branch, head }, d.clock.now())?;
        Ok(d.tickets.update_pull_request(ctx.org, &next, &expect, None, &[]).await?)
    }

    /// Steps 7–9: reuse or open the PR, labels and reviewers, then `Opened` + ticket
    /// `FixReady → PrOpen` + `notify.ticket{PrOpened}` in one transaction.
    async fn open(&self, ctx: &Ctx, row: FixPullRequest, body: &str) -> Result<OpenPrOutcome, AppError> {
        let d = &self.d;
        let org = ctx.org;
        let pr = match d.pull_requests.find_open(org, &ctx.repo, &row.branch).await.map_err(pr_err)? {
            Some(pr) => pr,
            None => {
                let title = pr_body::title(&self.cause_title(ctx).await?, &ctx.identifier);
                let req = OpenRequest { title, body: body.to_owned(), head: row.branch.clone(), base_branch: ctx.repo.default_branch.clone(), draft: ctx.settings.pr.draft };
                d.pull_requests.open(org, &ctx.repo, &req).await.map_err(pr_err)?
            }
        };
        let mut labels: Vec<String> = LABELS.iter().map(|l| (*l).to_owned()).collect();
        for l in &ctx.settings.pr.labels {
            if !labels.contains(l) {
                labels.push(l.clone());
            }
        }
        d.pull_requests.add_labels(org, &ctx.repo, pr.number, &labels).await.map_err(pr_err)?;
        let who = Reviewers { users: ctx.settings.pr.reviewers.users.clone(), teams: ctx.settings.pr.reviewers.teams.clone() };
        if !who.is_empty() {
            d.pull_requests.request_reviewers(org, &ctx.repo, pr.number, &who).await.map_err(pr_err)?;
        }

        let expect = row.state.clone();
        let mut next = row;
        next.apply(FixPrEvent::Opened { number: pr.number, url: pr.url.clone() }, d.clock.now())?;
        let jobs = [notify(org, ctx.ticket.id, Notice::PrOpened { cause_id: ctx.gated.token.cause(), url: pr.url.clone() })];
        let transition = Some(TicketTransition { expect: TicketStatus::FixReady, next: TicketStatus::PrOpen });
        match d.tickets.update_pull_request(org, &next, &expect, transition, &jobs).await {
            Ok(_) => {}
            Err(TicketStoreError::Conflict) => {
                // Either the ticket moved on (resolved, closed, already PrOpen) or the row did.
                let ticket = d.tickets.get_ticket(org, ctx.ticket.id).await?.value;
                if ticket.status == TicketStatus::FixReady {
                    return Err(AppError::Conflict("pull request row changed concurrently".into()));
                }
                // The PR stays open (it exists); record it and notify, no ticket change.
                match d.tickets.update_pull_request(org, &next, &expect, None, &jobs).await {
                    Ok(_) | Err(TicketStoreError::Conflict) => {}
                    Err(e) => return Err(e.into()),
                }
            }
            Err(e) => return Err(e.into()),
        }
        Ok(OpenPrOutcome::Opened { number: pr.number, url: pr.url })
    }

    /// Resume at `Opened`: the ticket transition happened with it (same transaction); a ticket
    /// still in `FixReady` only means another writer intervened, so apply it now.
    async fn finish_ticket(&self, ctx: &Ctx, _row: &FixPullRequest) -> Result<(), AppError> {
        if ctx.ticket.status == TicketStatus::FixReady {
            match self.d.tickets.transition(ctx.org, ctx.ticket.id, TicketStatus::FixReady, TicketStatus::PrOpen, &[]).await {
                Ok(_) | Err(TicketStoreError::Conflict) => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }
}

/// UI **Open PR**: preconditions that need no GitHub call, then enqueue `fix.open_pr`.
#[derive(Clone)]
pub struct RequestPullRequest {
    d: Arc<FixDeps>,
    proof: FixProof,
}

impl RequestPullRequest {
    pub fn new(d: Arc<FixDeps>) -> Self {
        let proof = FixProof::new(d.runs.clone());
        Self { d, proof }
    }

    pub async fn run(&self, org: OrgId, ticket: TicketId, cause: mage_core::domain::CauseId, role: MemberRole) -> Result<JobQueued, AppError> {
        let d = &self.d;
        let settings = d.orgs.fix_settings(org).await?;
        if !settings.pull_requests {
            return Err(AppError::Invalid("fix pull requests are disabled".into()));
        }
        if !role.at_least(MemberRole::Member) {
            return Err(AppError::Forbidden("requires the member role".into()));
        }
        let cause = d.tickets.get_cause(org, cause).await?;
        if cause.ticket_id != ticket {
            return Err(AppError::NotFound("cause".into()));
        }
        let t = d.tickets.get_ticket(org, ticket).await?.value;
        if t.status != TicketStatus::FixReady {
            return Err(AppError::Conflict(format!("ticket is {}; Open PR needs fix ready", t.status.name())));
        }
        let q = crate::ports::runs::RunQuery {
            kind: Some(RunKind::Fix),
            subject: Some(RunSubject::Cause(cause.id)),
            statuses: vec![RunStatus::Succeeded],
            newest_first: true,
            limit: 1,
            ..Default::default()
        };
        let fix_run = d.runs.list(org, &q).await?.into_iter().next().ok_or_else(|| AppError::Conflict("no succeeded fix run for this cause".into()))?;
        let proof = self.proof.gate_proof(org, fix_run.id()).await?;
        let job = open_pr_job(org, ticket, fix_run.id(), &proof.token.digest().to_hex());
        let enqueued = d.jobs.enqueue(job).await.map_err(job_err)?;
        let job_id = match enqueued {
            Enqueued::New(id) | Enqueued::Duplicate(id) => id,
        };
        Ok(JobQueued { job_id })
    }
}
