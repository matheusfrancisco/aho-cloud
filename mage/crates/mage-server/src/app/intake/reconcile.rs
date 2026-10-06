//! Use case 7, reconciliation (intake.md): the periodic chains `intake.github_resync`,
//! `intake.linear_resync`, `intake.github_redeliver`, `intake.inbox_purge`, and their bootstrap.
//! Each run returns its successor, inserted by `JobQueue::complete` in the same transaction.

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use mage_core::domain::{InstallationId, IntegrationKind, IntegrationStatus, JobPayload, OrgId, RepoAccess, WebhookSource};
use mage_core::ports::source_host::SourceHostError;
use mage_core::ports::{Clock, JobQueue, NewJob, RepoStore, SourceHost};
use time::OffsetDateTime;

use crate::app::intake::config::IntakeMode;
use crate::app::intake::error::IntakeError;
use crate::app::intake::github::GithubIngest;
use crate::app::intake::lifecycle::IntegrationEvents;
use crate::ports::deployments::{DeploymentError, DeploymentResolver};
use crate::ports::inbox::{InboxError, WebhookInbox};
use crate::ports::org::OrgStore;
use crate::ports::source_host_admin::{GithubApp, SourceHostAdmin};
use crate::ports::ticket::TicketStore;
use crate::ports::tracker::{IssueQuery, TicketTracker};

const GITHUB_RESYNC_EVERY: Duration = Duration::from_secs(60 * 60);
const REDELIVER_EVERY: Duration = Duration::from_secs(10 * 60);
const PURGE_EVERY: Duration = Duration::from_secs(24 * 60 * 60);
/// GitHub keeps deliveries for 3 days [unverified]; never page further back.
const REDELIVER_WINDOW: time::Duration = time::Duration::days(3);
/// Successor `since` = newest delivery seen minus this overlap.
const REDELIVER_OVERLAP: time::Duration = time::Duration::minutes(15);
const REDELIVER_CAP: usize = 50;
const LINEAR_OVERLAP: time::Duration = time::Duration::minutes(5);
/// A revived Linear chain starts this far back.
const LINEAR_REVIVE: time::Duration = time::Duration::hours(24);
/// A change older than this that no ingest picked up means the webhook is likely broken.
const LAG_ALARM: time::Duration = time::Duration::minutes(30);
const INBOX_BODY_RETENTION: time::Duration = time::Duration::hours(72);
const INBOX_ROW_RETENTION: time::Duration = time::Duration::days(30);

/// What a chain run did, for logs and tests.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReconcileReport {
    pub repos_synced: u32,
    pub repos_lost: u32,
    pub deployments_recorded: u32,
    pub issues_enqueued: u32,
    pub lagging_issues: u32,
    pub redelivered: u32,
    pub purged: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainRun {
    pub report: ReconcileReport,
    /// The chain's next link (`None` ends the chain: integration gone).
    pub successor: Option<NewJob>,
}

pub struct Reconcile {
    orgs: Arc<dyn OrgStore>,
    repos: Arc<dyn RepoStore>,
    tickets: Arc<dyn TicketStore>,
    inbox: Arc<dyn WebhookInbox>,
    jobs: Arc<dyn JobQueue>,
    source: Arc<dyn SourceHost>,
    admin: Arc<dyn SourceHostAdmin>,
    tracker: Arc<dyn TicketTracker>,
    /// The live GitHub Deployments API resolver (`driven::github`).
    github_deployments: Arc<dyn DeploymentResolver>,
    github: Arc<GithubIngest>,
    lifecycle: Arc<IntegrationEvents>,
    clock: Arc<dyn Clock>,
    mode: IntakeMode,
    /// Whether the Mage Fix App is configured (its deliveries are reconciled too).
    fix_app: bool,
}

impl Reconcile {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        orgs: Arc<dyn OrgStore>,
        repos: Arc<dyn RepoStore>,
        tickets: Arc<dyn TicketStore>,
        inbox: Arc<dyn WebhookInbox>,
        jobs: Arc<dyn JobQueue>,
        source: Arc<dyn SourceHost>,
        admin: Arc<dyn SourceHostAdmin>,
        tracker: Arc<dyn TicketTracker>,
        github_deployments: Arc<dyn DeploymentResolver>,
        github: Arc<GithubIngest>,
        lifecycle: Arc<IntegrationEvents>,
        clock: Arc<dyn Clock>,
        mode: IntakeMode,
        fix_app: bool,
    ) -> Self {
        Self { orgs, repos, tickets, inbox, jobs, source, admin, tracker, github_deployments, github, lifecycle, clock, mode, fix_app }
    }

    /// Enqueue the head of every chain (start and hourly): `Duplicate` while a chain is alive,
    /// revives a dead one. `orgs` = the tenants to keep chains for.
    pub async fn bootstrap(&self, orgs: &[OrgId]) -> Result<(), IntakeError> {
        let now = self.clock.now();
        self.jobs.enqueue(NewJob::new(JobPayload::IntakeGithubRedeliver { since: now - REDELIVER_WINDOW })).await?;
        self.jobs.enqueue(NewJob::new(JobPayload::IntakeInboxPurge)).await?;
        for &org in orgs {
            let integrations = self.orgs.integrations(org).await?;
            let active = |kind| integrations.iter().any(|i| i.kind == kind && i.status == IntegrationStatus::Active);
            if active(IntegrationKind::GitHub) {
                self.jobs.enqueue(NewJob::new(JobPayload::IntakeGithubResync { org_id: org })).await?;
            }
            if active(IntegrationKind::Linear) {
                self.jobs.enqueue(NewJob::new(JobPayload::IntakeLinearResync { org_id: org, since: now - LINEAR_REVIVE })).await?;
            }
        }
        Ok(())
    }

    /// `intake.github_resync { org }`: repos of the installation upserted, missing ones `Lost`;
    /// default heads synced; deployments of the allowed environments recorded (`as_of = now`).
    pub async fn github_resync(&self, org: OrgId) -> Result<ChainRun, IntakeError> {
        let integrations = self.orgs.integrations(org).await?;
        let Some(integration) = integrations.into_iter().find(|i| i.kind == IntegrationKind::GitHub && i.status == IntegrationStatus::Active) else {
            return Ok(ChainRun { report: ReconcileReport::default(), successor: None });
        };
        let inst = InstallationId(integration.external_id.parse().map_err(|_| IntakeError::permanent("GitHub integration id is not numeric"))?);
        let cfg = self.orgs.intake_config(org).await?;
        let mut report = ReconcileReport::default();
        let remote = self.source.installation_repos(inst).await?;
        let mut seen = HashSet::new();
        for r in &remote {
            seen.insert(r.external_id);
            self.lifecycle.upsert_remote(org, inst, r).await?;
        }
        let now = self.clock.now();
        for repo in self.repos.list_repos(org).await? {
            if repo.installation_id != inst {
                continue;
            }
            if !seen.contains(&repo.external_id) {
                if repo.access != RepoAccess::Lost {
                    self.repos.set_access(org, repo.id, RepoAccess::Lost).await?;
                    report.repos_lost += 1;
                }
                continue;
            }
            if repo.access != RepoAccess::Granted {
                continue;
            }
            match self.github.sync_default_head(org, &repo, &cfg).await {
                Ok(_) => report.repos_synced += 1,
                // An empty repo has no default head yet.
                Err(IntakeError::Permanent(reason)) => tracing::info!(%org, repo = %repo.id, reason = %reason, "default head not synced"),
                Err(retry) => return Err(retry),
            }
            for environment in resync_environments(&cfg.environments) {
                match self.github_deployments.deployed_sha(org, &repo, &environment, now).await {
                    Ok(Some(d)) => {
                        if self.github.record_deployment(org, &d).await? {
                            report.deployments_recorded += 1;
                        }
                    }
                    Ok(None) | Err(DeploymentError::NotFound) => {}
                    Err(e) => return Err(e.into()),
                }
            }
        }
        let successor = NewJob::new(JobPayload::IntakeGithubResync { org_id: org }).after(now + GITHUB_RESYNC_EVERY);
        Ok(ChainRun { report, successor: Some(successor) })
    }

    /// `intake.linear_resync { org, since }`: issues of the configured teams updated since the
    /// checkpoint (minus overlap) re-ingested; successor carries the newest `updated_at`.
    pub async fn linear_resync(&self, org: OrgId, since: OffsetDateTime) -> Result<ChainRun, IntakeError> {
        let integrations = self.orgs.integrations(org).await?;
        if !integrations.iter().any(|i| i.kind == IntegrationKind::Linear && i.status == IntegrationStatus::Active) {
            return Ok(ChainRun { report: ReconcileReport::default(), successor: None });
        }
        let cfg = self.orgs.intake_config(org).await?;
        let now = self.clock.now();
        let mut report = ReconcileReport::default();
        let mut newest = since;
        if !cfg.linear.teams.is_empty() {
            let mut cursor = None;
            loop {
                let q = IssueQuery { team_keys: cfg.linear.teams.clone(), updated_since: since - LINEAR_OVERLAP, cursor, page_size: 50 };
                let page = self.tracker.updated_issues(org, &q).await?;
                for stub in &page.items {
                    newest = newest.max(stub.updated_at);
                    let enq = self.jobs.enqueue(NewJob::new(JobPayload::IntakeLinearIssue { org_id: org, issue_id: stub.issue_id.clone() })).await?;
                    report.issues_enqueued += u32::from(enq.is_new());
                    if stub.updated_at < now - LAG_ALARM {
                        let behind = match self.tickets.get_by_linear_issue(org, &stub.issue_id).await? {
                            Some(t) => t.value.updated_at < stub.updated_at,
                            None => false,
                        };
                        report.lagging_issues += u32::from(behind);
                    }
                }
                match page.next {
                    Some(next) => cursor = Some(next),
                    None => break,
                }
            }
        }
        if report.lagging_issues > 0 {
            tracing::warn!(%org, lagging = report.lagging_issues, "Linear changes older than 30 min were not ingested by webhook; the Linear webhook is likely broken");
        }
        let successor = NewJob::new(JobPayload::IntakeLinearResync { org_id: org, since: newest }).after(now + self.mode.linear_resync_every());
        Ok(ChainRun { report, successor: Some(successor) })
    }

    /// `intake.github_redeliver { since }` (platform): GitHub never retries, so failed deliveries
    /// with no inbox row are redelivered (latest attempt per guid, at most 50 per run).
    pub async fn github_redeliver(&self, since: OffsetDateTime) -> Result<ChainRun, IntakeError> {
        let now = self.clock.now();
        let floor = since.max(now - REDELIVER_WINDOW);
        let mut report = ReconcileReport::default();
        let mut newest_seen: Option<OffsetDateTime> = None;
        let apps: &[(GithubApp, WebhookSource)] =
            if self.fix_app { &[(GithubApp::Read, WebhookSource::GitHub), (GithubApp::Fix, WebhookSource::GitHubFix)] } else { &[(GithubApp::Read, WebhookSource::GitHub)] };
        for &(app, source) in apps {
            // guid → (any 2xx attempt, latest attempt id, latest attempt time)
            let mut by_guid: BTreeMap<String, (bool, u64, OffsetDateTime)> = BTreeMap::new();
            let mut cursor = None;
            'pages: loop {
                let page = self.admin.hook_deliveries(app, cursor, 100).await?;
                for d in &page.items {
                    if d.delivered_at < floor {
                        break 'pages;
                    }
                    newest_seen = Some(newest_seen.map_or(d.delivered_at, |n| n.max(d.delivered_at)));
                    let ok = (200..300).contains(&d.status_code);
                    let entry = by_guid.entry(d.guid.clone()).or_insert((false, d.id, d.delivered_at));
                    entry.0 |= ok;
                    if d.delivered_at > entry.2 {
                        entry.1 = d.id;
                        entry.2 = d.delivered_at;
                    }
                }
                match page.next {
                    Some(next) => cursor = Some(next),
                    None => break,
                }
            }
            for (guid, (succeeded, latest, _)) in by_guid {
                if succeeded || report.redelivered as usize >= REDELIVER_CAP {
                    continue;
                }
                match self.inbox.get(source, &guid).await {
                    Ok(_) => continue,
                    Err(InboxError::NotFound) => {}
                    Err(e) => return Err(e.into()),
                }
                match self.admin.redeliver(app, latest).await {
                    Ok(()) => report.redelivered += 1,
                    Err(SourceHostError::NotFound) => {}
                    Err(e) => return Err(e.into()),
                }
            }
        }
        let next_since = newest_seen.map_or(floor, |n| (n - REDELIVER_OVERLAP).max(floor));
        let successor = NewJob::new(JobPayload::IntakeGithubRedeliver { since: next_since }).after(now + REDELIVER_EVERY);
        Ok(ChainRun { report, successor: Some(successor) })
    }

    /// `intake.inbox_purge` (platform): bodies after 72 h, rows after 30 days.
    pub async fn inbox_purge(&self) -> Result<ChainRun, IntakeError> {
        let now = self.clock.now();
        let purged = self.inbox.purge(now - INBOX_BODY_RETENTION, now - INBOX_ROW_RETENTION).await?;
        let report = ReconcileReport { purged, ..ReconcileReport::default() };
        Ok(ChainRun { report, successor: Some(NewJob::new(JobPayload::IntakeInboxPurge).after(now + PURGE_EVERY)) })
    }
}

/// Concrete environment names to poll: the non-glob `allow` entries plus the default.
fn resync_environments(env: &crate::domain::intake::Environments) -> Vec<String> {
    let mut out: Vec<String> = env.allow.iter().filter(|g| !g.contains(['*', '?'])).cloned().collect();
    if env.allowed(&env.default).is_some() && !out.contains(&env.default) {
        out.push(env.default.clone());
    }
    out
}

#[cfg(test)]
mod tests;
