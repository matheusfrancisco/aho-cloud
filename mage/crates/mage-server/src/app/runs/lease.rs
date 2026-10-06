//! RegisterRunner, LeaseRun, Heartbeat (runs.md) and the `RunAssignment` builder, plus runner
//! token authentication for `/runner/v1`.

use std::time::Duration;

use time::OffsetDateTime;
use uuid::Uuid;

use mage_core::domain::{ArtifactId, GateStatus, OrgId, RunKind, RunStatus, RunStatusEvent, RunSubject, VerifySpec};
use mage_core::ports::blob::{BlobKey, DownloadPolicy};
use mage_core::wire::runner::{
    Download, DownloadRef, GatewayAccess, HarnessAssignment, HeartbeatAck, HeartbeatRequest, KnowledgeAccess, LeaseRequest, LeaseTerms,
    RecipeRef, RegisterGrant, Registration, RegistrationAck, ReleasedRun, RepoRef, RunAssignment, RunnerPhase,
};
use mage_core::wire::token::{MemoryCutoff, TokenKind, TokenScope};

use crate::app::runs::machine::{Initiator, step};
use crate::app::runs::{MAX_COMMIT_RETRIES, ProtocolError, RunManager, RunnerCaller, blob_err, contention, is_stale, repo_err, request_hash, to_time};
use crate::error::AppError;
use crate::ports::audit::{Actor, AuditAction, AuditEvent, AuditOutcome, AuditTarget};
use crate::ports::runs::{LeaseQuery, RunConflict, RunRecord, RunStoreError, RunUpdate, RunWrite, RunnerSession, SignalTopic, Woke};
use crate::ports::tokens::{Jti, Revocation, RunnerRegistration, RunnerStatus, TokenError};

/// Waiters re-check the store at least this often (signals are hints).
pub(crate) const RECHECK: Duration = Duration::from_secs(2);

impl RunManager {
    /// Verifies a runner token (`mgr_`): signature and time, `jti` not revoked, the identity's
    /// generation and status (`Active`, or `Draining` for per-run calls).
    pub async fn authenticate_runner(&self, token: &str) -> Result<RunnerCaller, ProtocolError> {
        let now = self.now();
        let claims = self.d.signer.verify(token, TokenKind::Runner, now).map_err(|_| AppError::Unauthorized)?;
        let TokenScope::Runner { runner, generation, instance } = claims.scope else { return Err(AppError::Unauthorized.into()) };
        if self.d.token_store.is_revoked(&Jti(*claims.jti.as_bytes())).await.map_err(AppError::from)? {
            return Err(AppError::Unauthorized.into());
        }
        let identity = self.runner_identity(runner).await?;
        if identity.generation != generation || identity.status == RunnerStatus::Revoked {
            self.forget_runner(runner);
            return Err(AppError::Unauthorized.into());
        }
        Ok(RunnerCaller { runner: identity, instance_id: instance })
    }

    /// `POST /register`: bootstrap or assertion grant, session record, release of a previous
    /// instance's leases, runner token.
    pub async fn register_runner(&self, reg: Registration) -> Result<RegistrationAck, ProtocolError> {
        let now = self.now();
        let runner = match &reg.grant {
            RegisterGrant::Bootstrap { bootstrap_token, name, public_key } => {
                let hash = request_hash(bootstrap_token.as_bytes());
                let registration = RunnerRegistration {
                    id: self.d.ids.new_id(),
                    name: name.clone(),
                    public_key: *public_key,
                    capabilities: reg.capabilities.clone(),
                };
                let runner = match self.d.token_store.register_runner(&hash, &registration).await {
                    Ok(r) => r,
                    Err(TokenError::Invalid(_) | TokenError::NotFound) => return Err(AppError::Unauthorized.into()),
                    Err(e) => return Err(AppError::from(e).into()),
                };
                let event = AuditEvent {
                    org: None,
                    actor: Actor::Runner(runner.id),
                    action: AuditAction::RunnerRegistered,
                    target: AuditTarget::new("runner", runner.id.to_string()),
                    outcome: AuditOutcome::Success,
                    ip: None,
                    request_id: None,
                    details: vec![("name".into(), runner.name.clone()), ("generation".into(), runner.generation.to_string())],
                };
                if let Err(err) = self.d.audit.record(event).await {
                    tracing::warn!(error = %err, "audit runner.registered failed");
                }
                runner
            }
            RegisterGrant::Assertion { runner_id, assertion } => {
                let runner = self.d.token_store.runner(*runner_id).await.map_err(|e| match e {
                    TokenError::NotFound => AppError::Unauthorized,
                    e => AppError::from(e),
                })?;
                if runner.status == RunnerStatus::Revoked {
                    return Err(AppError::Forbidden("runner revoked".into()).into());
                }
                let a = self.d.signer.verify_assertion(assertion, &runner.public_key, now).map_err(|_| AppError::Unauthorized)?;
                if a.runner != *runner_id {
                    return Err(AppError::Unauthorized.into());
                }
                let jti = Jti(*a.jti.as_bytes());
                if self.d.token_store.is_revoked(&jti).await.map_err(AppError::from)? {
                    return Err(AppError::Unauthorized.into());
                }
                let used = Revocation { jti, org_id: None, reason: "runner assertion used".into(), expires_at: a.exp };
                self.d.token_store.revoke(&used).await.map_err(AppError::from)?;
                runner
            }
        };
        self.forget_runner(runner.id);

        let session = RunnerSession { runner_id: runner.id, instance_id: reg.instance_id, version: reg.version.clone(), capabilities: reg.capabilities.clone(), at: now };
        self.d.runs.record_runner_session(&session).await?;
        let mut released = Vec::new();
        for (org, run) in self.d.runs.held_by(runner.id).await? {
            if let Some(r) = self.release_instance_lease(org, run, reg.instance_id).await? {
                released.push(r);
            }
        }

        let ttl = Duration::from_secs(self.d.policy.runner_token_ttl_secs);
        let scope = TokenScope::Runner { runner: runner.id, generation: runner.generation, instance: reg.instance_id };
        let (token, claims) = self.mint(scope, ttl)?;
        Ok(RegistrationAck {
            runner_id: runner.id,
            runner_token: token.into_string(),
            expires_at: claims.exp,
            released,
            lease: self.lease_terms(),
            long_poll_max: self.d.policy.long_poll_max(),
            min_version: self.d.policy.min_runner_version.clone(),
        })
    }

    pub(crate) fn lease_terms(&self) -> LeaseTerms {
        LeaseTerms { ttl: self.d.policy.lease_ttl(), heartbeat_every: self.d.policy.heartbeat_every() }
    }

    /// A lease held by another instance of the runner is lost: `LeaseLost` in `Leased | Preparing
    /// | Running`, `session_lost` in `AwaitingGate`, lease cleared when terminal.
    async fn release_instance_lease(&self, org: OrgId, run: mage_core::domain::RunId, instance: Uuid) -> Result<Option<ReleasedRun>, AppError> {
        for _ in 0..MAX_COMMIT_RETRIES {
            let mut rec = self.d.runs.get(org, run).await?;
            let Some(lease) = rec.lease else { return Ok(None) };
            if lease.instance_id == instance {
                return Ok(None);
            }
            let now = self.now();
            let mut w = RunWrite::default();
            match rec.run.status {
                RunStatus::Leased | RunStatus::Preparing | RunStatus::Running => {
                    let max = self.d.policy.max_lease_attempts;
                    rec = self.step_and_queue(rec, RunStatusEvent::LeaseLost { max_attempts: max }, Initiator::Server, now, &mut w).await?;
                }
                RunStatus::AwaitingGate => {
                    release(&mut rec);
                    rec.session_lost = true;
                    w.events.push(notice(&rec, now, "runner restarted; gate continues"));
                    w.updates.push(RunUpdate::of(rec.clone()));
                }
                _ => {
                    release(&mut rec);
                    w.updates.push(RunUpdate::of(rec.clone()));
                }
            }
            match self.d.runs.commit(org, w).await {
                Ok(_) => return Ok(Some(ReleasedRun { run_id: run, attempt: lease.attempt, status: rec.run.status })),
                Err(e) if is_stale(&e) => continue,
                Err(e) => return Err(e.into()),
            }
        }
        Err(contention())
    }

    /// `POST /lease` (long-poll).
    pub async fn lease(&self, caller: &RunnerCaller, req: LeaseRequest) -> Result<Option<RunAssignment>, ProtocolError> {
        if caller.runner.status != RunnerStatus::Active {
            return Ok(None);
        }
        if let Some(rec) = self.d.runs.lease_by_request(caller.id(), req.request_id).await?
            && rec.lease.is_some_and(|l| l.instance_id == caller.instance_id)
        {
            return Ok(Some(self.assignment(&rec).await?));
        }
        let session = self
            .d
            .runs
            .runner_session(caller.id())
            .await?
            .filter(|s| s.instance_id == caller.instance_id)
            .ok_or_else(|| AppError::Unauthorized)?;
        if let Err(err) = self.reap_leases(20).await {
            tracing::warn!(error = %err, "bounded reaper pass failed");
        }
        let wait = req.wait.min(self.d.policy.long_poll_max());
        let deadline = tokio::time::Instant::now() + wait;
        let mut sub = self.d.signals.subscribe(SignalTopic::Queue).await.map_err(|e| AppError::Unavailable(e.to_string()))?;
        loop {
            let now = self.now();
            let q = LeaseQuery {
                runner_id: caller.id(),
                instance_id: caller.instance_id,
                scope: caller.runner.scope.clone(),
                capabilities: session.capabilities.clone(),
                free: req.free,
                prepared: req.prepared.iter().take(mage_core::wire::runner::MAX_PREPARED_BASES).copied().collect(),
                request_id: req.request_id,
                until: now + to_time(self.d.policy.lease_ttl()),
                now,
                repo_limit: self.d.policy.repo_exec_concurrency,
            };
            if let Some(rec) = self.d.runs.lease_next(&q).await? {
                return Ok(Some(self.assignment(&rec).await?));
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() || self.d.shutdown.is_cancelled() {
                return Ok(None);
            }
            tokio::select! {
                () = self.d.shutdown.cancelled() => return Ok(None),
                woke = sub.wait(remaining.min(RECHECK)) => { let _: Woke = woke; }
            }
        }
    }

    /// The assignment of the current attempt (also rebuilt on a retried lease; tokens re-minted).
    pub async fn assignment(&self, rec: &RunRecord) -> Result<RunAssignment, AppError> {
        let org = rec.org();
        let spec = &rec.spec;
        let kind = rec.run.kind;
        let repo = match (kind, spec.repo_id) {
            (RunKind::Triage, _) | (_, None) => None,
            (_, Some(id)) => {
                let repo = self.d.repos.repo(org, id).await.map_err(repo_err)?;
                Some(RepoRef { repo_id: repo.id, owner: repo.owner, name: repo.name })
            }
        };
        let source = match (&repo, spec.sha) {
            (Some(r), Some(sha)) => Some(self.download(org, rec, DownloadRef::SourceBundle { repo_id: r.repo_id, sha }).await?),
            _ => None,
        };
        let recipe = match (kind, spec.repo_id, spec.recipe_hash) {
            (RunKind::Triage, ..) => None,
            (_, Some(repo_id), Some(hash)) => {
                let recipe = self.d.repos.get_recipe(org, repo_id, hash).await.map_err(repo_err)?;
                Some(RecipeRef { hash, toml: recipe.body.clone() })
            }
            _ => None,
        };
        let harness = match (rec.run.harness, spec.requirements.harness) {
            (Some(h), Some((_, mode))) => Some(HarnessAssignment {
                kind: h,
                model: spec.model.clone(),
                credential_mode: mode,
                prompt: spec.prompt.clone(),
                system_append: spec.system_append.clone(),
            }),
            _ => None,
        };
        let mut downloads = Vec::new();
        for artifact in referenced_artifacts(rec) {
            downloads.push(self.download(org, rec, DownloadRef::Artifact(artifact)).await?);
        }
        let ttl = self.d.policy.run_token_ttl(kind, &rec.run.budget);
        let knowledge = match (kind, spec.repo_id, spec.sha) {
            (RunKind::Verify, ..) | (_, None, _) | (_, _, None) => None,
            (_, Some(repo_id), Some(sha)) => {
                let (memory, disabled_layers) = match (&spec.triage, rec.run.subject) {
                    (Some(t), RunSubject::Ticket(ticket)) => (t.memory_cutoff(ticket), t.disabled_layers().to_vec()),
                    _ => (MemoryCutoff { exclude_tickets: spec.ticket_id.into_iter().collect(), as_of: None }, Vec::new()),
                };
                let scope = TokenScope::Knowledge { org, run: rec.id(), attempt: rec.run.attempt, repo: repo_id, sha, memory, disabled_layers };
                let (token, _) = self.mint(scope, ttl)?;
                Some(KnowledgeAccess { url: self.d.policy.knowledge_url(), knowledge_token: token.into_string() })
            }
        };
        let gateway = match (&harness, spec.gateway_metered()) {
            (Some(h), true) => {
                let mut models = vec![spec.model.clone()];
                if h.kind == mage_core::domain::HarnessKind::ClaudeCode
                    && let Some(small) = &self.d.policy.claude_code_small_model
                    && !models.contains(small)
                {
                    models.push(small.clone());
                }
                let scope = TokenScope::Gateway { org, run: rec.id(), attempt: rec.run.attempt, models };
                let (token, _) = self.mint(scope, ttl)?;
                Some(GatewayAccess { base_url: self.d.policy.gateway_url(), gateway_token: token.into_string() })
            }
            _ => None,
        };
        let prior_feedback = if rec.run.attempt > 1 {
            self.d
                .runs
                .gates(org, rec.id())
                .await?
                .into_iter()
                .filter(|g| matches!(g.gate.status, GateStatus::Failed { .. }))
                .filter_map(|g| g.feedback)
                .collect()
        } else {
            Vec::new()
        };
        Ok(RunAssignment {
            run_id: rec.id(),
            org_id: org,
            attempt: rec.run.attempt,
            kind,
            lease: self.lease_terms(),
            repo,
            sha: if kind == RunKind::Triage { None } else { spec.sha },
            source,
            recipe,
            harness,
            verify: spec.verify.clone(),
            downloads,
            knowledge,
            gateway,
            budget: rec.run.budget,
            prior_feedback,
            fix: spec.fix.clone(),
            trace: spec.trace.clone(),
        })
    }

    /// A signed GET URL for something `rec` may read; `Forbidden` otherwise.
    pub(crate) async fn download(&self, org: OrgId, rec: &RunRecord, what: DownloadRef) -> Result<Download, AppError> {
        match what {
            DownloadRef::SourceBundle { repo_id, sha } => {
                if rec.run.kind == RunKind::Triage || rec.spec.repo_id != Some(repo_id) || rec.spec.sha != Some(sha) {
                    return Err(AppError::Forbidden("download not allowed for this run".into()));
                }
                let version = self.d.repos.version(org, mage_core::domain::RepoVersionKey::new(repo_id, sha)).await.map_err(repo_err)?;
                let bundle = version.bundle.ok_or_else(|| AppError::Unavailable("source bundle not ready".into()))?;
                let key = BlobKey::parse(&bundle.key).map_err(blob_err)?;
                let ttl = Duration::from_secs(self.d.policy.bundle_url_ttl_secs);
                let url = self.d.artifacts.signed_download(&key, &DownloadPolicy { ttl }).await.map_err(blob_err)?;
                Ok(Download { what, url: url.url, bytes: bundle.bytes, sha256: bundle.sha256, expires_at: url.expires_at, name: None })
            }
            DownloadRef::Artifact(id) => {
                if !referenced_artifacts(rec).contains(&id) {
                    return Err(AppError::Forbidden("download not allowed for this run".into()));
                }
                let a = self.d.runs.artifact(org, id).await?;
                if a.state != crate::ports::runs::ArtifactState::Complete {
                    return Err(AppError::Unavailable(format!("artifact {id} is not complete")));
                }
                let key = BlobKey::parse(&a.artifact.key).map_err(blob_err)?;
                let ttl = Duration::from_secs(self.d.policy.download_url_ttl_secs);
                let url = self.d.artifacts.signed_download(&key, &DownloadPolicy { ttl }).await.map_err(blob_err)?;
                Ok(Download { what, url: url.url, bytes: a.artifact.bytes, sha256: a.artifact.sha256, expires_at: url.expires_at, name: Some(a.name) })
            }
        }
    }

    /// `POST /runs/{id}/heartbeat`.
    pub async fn heartbeat(&self, caller: &RunnerCaller, run: mage_core::domain::RunId, req: HeartbeatRequest) -> Result<HeartbeatAck, ProtocolError> {
        let rec = self.runner_run(caller, run).await?;
        let org = rec.org();
        let fence = caller.fence(req.attempt);
        let until = self.now() + to_time(self.d.policy.lease_ttl());
        let mut rec = match self.d.runs.extend_lease(org, run, fence, until).await {
            Ok(r) => r,
            Err(RunStoreError::Conflict(RunConflict::LeaseLost)) => return Err(ProtocolError::LeaseLost),
            Err(e) => return Err(e.into()),
        };
        for _ in 0..MAX_COMMIT_RETRIES {
            let now = self.now();
            let mut w = RunWrite::default();
            let mut changed = false;
            let target = match req.phase {
                RunnerPhase::Preparing => RunStatus::Preparing,
                RunnerPhase::Running | RunnerPhase::AwaitingGate | RunnerPhase::Finishing => RunStatus::Running,
            };
            if rec.run.status == RunStatus::Leased {
                step(&mut rec, RunStatusEvent::EnvPreparing, Initiator::Runner, now, &mut w)?;
                changed = true;
            }
            if rec.run.status == RunStatus::Preparing && target == RunStatus::Running {
                step(&mut rec, RunStatusEvent::Started, Initiator::Runner, now, &mut w)?;
                changed = true;
            }
            if matches!(rec.run.status, RunStatus::Preparing | RunStatus::Running) && rec.wall_at(now) > rec.run.budget.max_wall {
                if !self.try_salvage(&mut rec, now, &mut w).await? {
                    let limit = mage_core::domain::BudgetLimit::Wall;
                    let stepped = step(&mut rec, RunStatusEvent::BudgetExceeded { limit }, Initiator::Server, now, &mut w)?;
                    if stepped.cascade {
                        self.cascade(&rec, now, &mut w).await?;
                    }
                }
                changed = true;
            }
            if !changed {
                break;
            }
            w.updates.push(RunUpdate::of(rec.clone()));
            match self.d.runs.commit(org, w).await {
                Ok(_) => break,
                Err(e) if is_stale(&e) => {
                    rec = self.d.runs.get(org, run).await?;
                    if !rec.holds(&fence) {
                        return Err(ProtocolError::LeaseLost);
                    }
                }
                Err(e) => return Err(e.into()),
            }
        }
        let now = self.now();
        let window = to_time(self.d.policy.heartbeat_every()) * 2;
        let pending = self
            .d
            .runs
            .commands_after(org, run, req.attempt, rec.last_cmd_seq.saturating_sub(1), 1)
            .await?
            .iter()
            .any(|c| c.issued_at + window >= now);
        Ok(HeartbeatAck { status: rec.run.status, lease: self.lease_terms(), commands_pending: pending })
    }
}

/// Drops the lease of a run whose status has no lease-lost row (gate wait, terminal).
pub(crate) fn release(rec: &mut RunRecord) {
    rec.lease = None;
    rec.run.release_lease();
}

pub(crate) fn notice(rec: &RunRecord, at: OffsetDateTime, text: &str) -> crate::ports::runs::RunEventDraft {
    crate::ports::runs::RunEventDraft { run_id: rec.id(), attempt: rec.run.attempt, at, kind: mage_core::domain::RunEventKind::Notice(text.into()) }
}

/// Artifacts the run may download: those referenced by its `VerifySpec` and `FixSpec`.
pub(crate) fn referenced_artifacts(rec: &RunRecord) -> Vec<ArtifactId> {
    let mut out = Vec::new();
    match &rec.spec.verify {
        Some(VerifySpec::Repro { script, .. }) => out.push(*script),
        Some(VerifySpec::Fix { patch, script, regression, .. }) => {
            out.push(*patch);
            out.push(*script);
            out.extend(regression.as_ref().map(|r| r.script));
        }
        Some(VerifySpec::Boot | VerifySpec::TestBaseline) | None => {}
    }
    if let Some(fix) = &rec.spec.fix {
        out.push(fix.brief);
        out.push(fix.script);
    }
    out.sort();
    out.dedup();
    out
}
