//! Use case 3, IngestLinearIssue (job `intake.linear_issue`, intake.md): normalize one Linear
//! issue into a `Ticket` with a defined target sha, pin its `RepoVersion`, and decide the
//! `index.version` / `triage.ticket` jobs, saved with the ticket in one transaction.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use mage_core::domain::{
    Customer, ExternalRef, IntegrationKind, IntegrationStatus, JobPayload, OrgId, Repo, RepoAccess, SlackThreadRef, TargetOrigin, Ticket,
    TicketContent, TicketEvent, TicketId, TicketSource, TicketStatus, TicketTarget, TriageTrigger, VersionReason,
};
use mage_core::ports::common::Versioned;
use mage_core::ports::{Clock, IdGen, NewJob, RepoStore};
use parking_lot::Mutex;
use time::OffsetDateTime;
use url::Url;

use crate::app::intake::error::IntakeError;
use crate::app::intake::extract;
use crate::app::intake::normalize::{BodyComment, BodyMessage, build_body, content_hash};
use crate::app::intake::target::{TargetInput, TargetResolver};
use crate::domain::intake::{IntakeConfig, ThreadMessage, ThreadSnapshot};
use crate::ports::chat::{ChatAuthor, ChatError, ChatNotifier};
use crate::ports::org::OrgStore;
use crate::ports::outbound::{OutboundLog, OutboundTarget};
use crate::ports::ticket::TicketStore;
use crate::ports::tracker::{Author, TicketTracker, TrackerError, TrackerIdentity, TrackerIssue, TrackerStateKind};

/// Thread messages read per refresh.
const THREAD_MAX_MESSAGES: u16 = 100;
/// Retry delay after a transient Slack failure while the stale snapshot is used.
const THREAD_RETRY_AFTER: Duration = Duration::from_secs(60);
/// `save_ingested` conflict (a UI edit or triage write bumped `rev`): retry with fresh reads.
const CONFLICT_RETRY_AFTER: Duration = Duration::from_secs(30);
const IDENTITY_TTL: Duration = Duration::from_secs(60 * 60);

/// `TicketTracker::identity` per org, cached 1 h (own-content detection in ingest and
/// ProcessDelivery).
pub struct TrackerIdentityCache {
    tracker: Arc<dyn TicketTracker>,
    clock: Arc<dyn Clock>,
    cache: Mutex<HashMap<OrgId, (TrackerIdentity, OffsetDateTime)>>,
}

impl TrackerIdentityCache {
    pub fn new(tracker: Arc<dyn TicketTracker>, clock: Arc<dyn Clock>) -> Self {
        Self { tracker, clock, cache: Mutex::new(HashMap::new()) }
    }

    pub async fn get(&self, org: OrgId) -> Result<TrackerIdentity, TrackerError> {
        let now = self.clock.now();
        if let Some((identity, at)) = self.cache.lock().get(&org) {
            if now - *at < IDENTITY_TTL {
                return Ok(identity.clone());
            }
        }
        let identity = self.tracker.identity(org).await?;
        self.cache.lock().insert(org, (identity.clone(), now));
        Ok(identity)
    }

    /// Bypass and drop the cache (revocation check).
    pub async fn fresh(&self, org: OrgId) -> Result<TrackerIdentity, TrackerError> {
        self.cache.lock().remove(&org);
        self.get(org).await
    }

    pub fn is_ours(identity: &TrackerIdentity, actor_id: &str) -> bool {
        identity.user_id == actor_id || identity.app_id.as_deref() == Some(actor_id)
    }
}

/// What one ingest did (logged by the job handler; asserted by tests).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IngestResult {
    /// Nothing to do (reason).
    Ignored(String),
    /// The existing ticket was closed (`closed upstream`).
    Closed(TicketId),
    /// Stored content equals the new content and no job is due.
    Unchanged(TicketId),
    Saved { ticket: TicketId, created: bool, index: bool, triage: bool },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IngestOutcome {
    pub result: IngestResult,
    /// Jobs to insert when this job completes (a later re-ingest after a Slack rate limit).
    pub follow_ups: Vec<NewJob>,
}

impl IngestOutcome {
    fn done(result: IngestResult) -> Self {
        Self { result, follow_ups: Vec::new() }
    }
}

pub struct IngestLinearIssue {
    orgs: Arc<dyn OrgStore>,
    tickets: Arc<dyn TicketStore>,
    repos: Arc<dyn RepoStore>,
    tracker: Arc<dyn TicketTracker>,
    chat: Arc<dyn ChatNotifier>,
    outbound: Arc<dyn OutboundLog>,
    targets: Arc<TargetResolver>,
    identities: Arc<TrackerIdentityCache>,
    clock: Arc<dyn Clock>,
    ids: Arc<dyn IdGen>,
}

/// Linear integration of the org and Slack integrations with their team domains.
struct Integrations {
    linear_workspace: Option<String>,
    slack: Vec<(String, Option<String>)>,
}

impl IngestLinearIssue {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        orgs: Arc<dyn OrgStore>,
        tickets: Arc<dyn TicketStore>,
        repos: Arc<dyn RepoStore>,
        tracker: Arc<dyn TicketTracker>,
        chat: Arc<dyn ChatNotifier>,
        outbound: Arc<dyn OutboundLog>,
        targets: Arc<TargetResolver>,
        identities: Arc<TrackerIdentityCache>,
        clock: Arc<dyn Clock>,
        ids: Arc<dyn IdGen>,
    ) -> Self {
        Self { orgs, tickets, repos, tracker, chat, outbound, targets, identities, clock, ids }
    }

    pub async fn run(&self, org: OrgId, issue_id: &str) -> Result<IngestOutcome, IntakeError> {
        // 1. Load.
        let cfg = self.orgs.intake_config(org).await?;
        let integrations = self.integrations(org).await?;
        let Some(workspace) = integrations.linear_workspace.clone() else {
            return Ok(IngestOutcome::done(IngestResult::Ignored("linear integration not active".into())));
        };
        let existing = self.tickets.get_by_linear_issue(org, issue_id).await?;
        let issue = match self.tracker.get_issue(org, issue_id).await {
            Ok(issue) => issue,
            Err(TrackerError::NotFound) => return self.close_upstream(org, existing, "issue gone").await,
            Err(TrackerError::Unauthorized) => return Err(self.check_revocation(org, &workspace).await),
            Err(other) => return Err(other.into()),
        };

        // 2. Upstream close.
        if issue.archived || matches!(issue.state_kind, TrackerStateKind::Completed | TrackerStateKind::Canceled) {
            return self.close_upstream(org, existing, "closed upstream").await;
        }

        // 3. Filter.
        if !passes_filters(&cfg, &issue) {
            return Ok(IngestOutcome::done(match existing {
                Some(t) => IngestResult::Unchanged(t.value.id),
                None => IngestResult::Ignored("filtered".into()),
            }));
        }
        if let Some(t) = &existing {
            if t.value.status.is_terminal() {
                tracing::info!(%org, ticket = %t.value.id, issue = %issue_id, status = t.value.status.name(), "issue active again but ticket is terminal; left unchanged");
                return Ok(IngestOutcome::done(IngestResult::Unchanged(t.value.id)));
            }
        }

        // 4. Own content.
        let me = match self.identities.get(org).await {
            Ok(me) => me,
            Err(TrackerError::Unauthorized) => return Err(self.check_revocation(org, &workspace).await),
            Err(other) => return Err(other.into()),
        };
        let mut kept_comments = Vec::new();
        for c in &issue.comments {
            let Author::Human { id } = &c.author else { continue };
            if TrackerIdentityCache::is_ours(&me, id) {
                continue;
            }
            if self.outbound.find_by_external(org, OutboundTarget::LinearComment, &c.id).await?.is_some() {
                continue;
            }
            kept_comments.push(c);
        }

        let now = self.clock.now();
        let ticket_id: TicketId = existing.as_ref().map_or_else(|| self.ids.new_id(), |t| t.value.id);

        // 5. Slack thread.
        let mut follow_ups = Vec::new();
        let thread = find_thread(&issue, &integrations.slack);
        let (snapshot, snapshot_to_store) = match &thread {
            Some(thread) => self.thread_snapshot(org, existing.as_ref(), thread, &cfg, now, issue_id, &mut follow_ups).await?,
            None => (None, None),
        };

        // 6. Normalize, extract, customer, route, target.
        let comment_views: Vec<BodyComment<'_>> =
            kept_comments.iter().map(|c| BodyComment { author_id: c.author.id(), at: c.created_at, body: &c.body }).collect();
        let thread_messages: &[ThreadMessage] = snapshot.as_ref().map_or(&[], |s| s.messages.as_slice());
        let message_views: Vec<BodyMessage<'_>> = thread_messages
            .iter()
            .map(|m| BodyMessage { author_id: &m.author_id, at: slack_ts_time(&m.ts).unwrap_or(now), text: &m.text, file_names: &m.file_names })
            .collect();
        let body = build_body(&issue.description, &comment_views, &message_views);
        let comment_texts: Vec<&str> = kept_comments.iter().map(|c| c.body.as_str()).collect();
        let thread_texts: Vec<&str> = thread_messages.iter().map(|m| m.text.as_str()).collect();
        let error_strings = extract::error_strings(&issue.description, &comment_texts, &thread_texts);
        let customer = self.resolve_customer(org, &cfg, &issue, thread.as_ref()).await?;
        let routed = self.route(org, &cfg, &issue).await?;
        let environment = cfg.environments.canonical(customer.as_ref().and_then(|c| c.environment.as_deref()).unwrap_or(&cfg.environments.default)).to_owned();
        let existing_target = existing.as_ref().and_then(|t| t.value.target.clone());
        let target = self
            .targets
            .resolve(&TargetInput {
                org,
                existing: existing_target.as_ref(),
                repo: routed.as_ref(),
                description: &issue.description,
                directives: cfg.linear.directives,
                environment: &environment,
                as_of: issue.created_at,
            })
            .await?;
        let target_repo = match &target {
            Some(t) if routed.as_ref().is_some_and(|r| r.id == t.repo_id) => routed.clone(),
            Some(t) => Some(self.repos.repo(org, t.repo_id).await?),
            None => None,
        };

        // 7. Version pin.
        let target_changed = existing.is_none() || existing_target.as_ref().map(|t| (t.repo_id, t.sha)) != target.as_ref().map(|t| (t.repo_id, t.sha));
        if let (Some(t), true) = (&target, target_changed) {
            self.pin_version(org, ticket_id, t, now).await?;
        }

        // 8. Hash and decide.
        let title = issue.title.trim().to_owned();
        let customer_id = customer.as_ref().map(|c| c.id);
        let hash = content_hash(&title, &body, &error_strings, target.as_ref(), customer_id);
        let content = TicketContent {
            title,
            body,
            content_hash: hash,
            slack_thread: thread.clone(),
            customer: customer_id,
            target: target.clone(),
            error_strings,
        };
        let (ticket, expected_rev, created, changed) = match existing {
            Some(Versioned { value, rev }) => {
                let unchanged = same_content(&value, &content);
                let mut ticket = value;
                if !unchanged {
                    ticket.update_content(content, now).map_err(|e| IntakeError::permanent(e.to_string()))?;
                }
                (ticket, Some(rev), false, !unchanged)
            }
            None => {
                let source = TicketSource::Linear { issue_id: issue.issue_id.clone(), identifier: issue.identifier.clone(), url: issue.url.clone() };
                (Ticket::new(ticket_id, org, source, content, now), None, true, true)
            }
        };

        let mut jobs = Vec::new();
        let mut index = false;
        if let (Some(t), true) = (&target, target_changed) {
            let reason = match &t.origin {
                TargetOrigin::Deployment { environment } => VersionReason::Deployed(environment.clone()),
                _ => VersionReason::OnDemand(ticket.id),
            };
            jobs.push(NewJob::new(JobPayload::IndexVersion { org_id: org, repo_id: t.repo_id, sha: t.sha, reason }));
            index = true;
        }
        let triage = should_triage(&cfg, &issue, &ticket, target_repo.as_ref());
        if triage {
            jobs.push(NewJob::new(JobPayload::TriageTicket {
                org_id: org,
                ticket_id: ticket.id,
                content_hash: ticket.content_hash,
                trigger: TriageTrigger::Auto,
            }));
        }

        // 9. Persist.
        if !changed && jobs.is_empty() {
            if let Some(s) = &snapshot_to_store {
                self.tickets.put_thread_snapshot(org, ticket.id, s).await?;
            }
            return Ok(IngestOutcome { result: IngestResult::Unchanged(ticket.id), follow_ups });
        }
        if !created {
            if let Some(s) = &snapshot_to_store {
                self.tickets.put_thread_snapshot(org, ticket.id, s).await?;
            }
        }
        match self.tickets.save_ingested(org, &ticket, expected_rev, &jobs).await {
            Ok(_) => {}
            Err(crate::ports::ticket::TicketStoreError::Conflict) => {
                return Err(IntakeError::retry_at("ticket changed concurrently", now + CONFLICT_RETRY_AFTER));
            }
            Err(other) => return Err(other.into()),
        }
        if created {
            if let Some(s) = &snapshot_to_store {
                self.tickets.put_thread_snapshot(org, ticket.id, s).await?;
            }
        }
        Ok(IngestOutcome { result: IngestResult::Saved { ticket: ticket.id, created, index, triage }, follow_ups })
    }

    async fn integrations(&self, org: OrgId) -> Result<Integrations, IntakeError> {
        let all = self.orgs.integrations(org).await?;
        let linear_workspace =
            all.iter().find(|i| i.kind == IntegrationKind::Linear && i.status == IntegrationStatus::Active).map(|i| i.external_id.clone());
        let mut slack = Vec::new();
        for i in all.iter().filter(|i| i.kind == IntegrationKind::Slack && i.status == IntegrationStatus::Active) {
            let settings = self.orgs.integration_settings(org, IntegrationKind::Slack, &i.external_id).await?;
            slack.push((i.external_id.clone(), settings.slack_team_domain));
        }
        Ok(Integrations { linear_workspace, slack })
    }

    /// `closed upstream` → `Closed` for a non-terminal existing ticket.
    async fn close_upstream(&self, org: OrgId, existing: Option<Versioned<Ticket>>, why: &str) -> Result<IngestOutcome, IntakeError> {
        let Some(Versioned { value: ticket, .. }) = existing else {
            return Ok(IngestOutcome::done(IngestResult::Ignored(why.into())));
        };
        if ticket.status.is_terminal() {
            return Ok(IngestOutcome::done(IngestResult::Unchanged(ticket.id)));
        }
        let next = ticket.status.apply(&TicketEvent::ClosedUpstream).map_err(|e| IntakeError::permanent(e.to_string()))?;
        match self.tickets.transition(org, ticket.id, ticket.status, next, &[]).await {
            Ok(_) => Ok(IngestOutcome::done(IngestResult::Closed(ticket.id))),
            Err(crate::ports::ticket::TicketStoreError::Conflict) => {
                Err(IntakeError::retry_at("ticket changed concurrently", self.clock.now() + CONFLICT_RETRY_AFTER))
            }
            Err(other) => Err(other.into()),
        }
    }

    /// Key rejected: confirmed by a fresh `identity` call → integration `Revoked` (UI banner).
    async fn check_revocation(&self, org: OrgId, workspace: &str) -> IntakeError {
        match self.identities.fresh(org).await {
            Err(TrackerError::Unauthorized) => {
                if let Err(e) = self.orgs.set_integration_status(org, IntegrationKind::Linear, workspace, IntegrationStatus::Revoked, &[]).await {
                    return e.into();
                }
                tracing::warn!(%org, workspace, "Linear credential rejected twice; integration revoked");
                IntakeError::permanent("Linear credential rejected; integration revoked")
            }
            Err(other) => other.into(),
            Ok(_) => IntakeError::retry("Linear rejected the credential once"),
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn thread_snapshot(
        &self,
        org: OrgId,
        existing: Option<&Versioned<Ticket>>,
        thread: &SlackThreadRef,
        cfg: &IntakeConfig,
        now: OffsetDateTime,
        issue_id: &str,
        follow_ups: &mut Vec<NewJob>,
    ) -> Result<(Option<ThreadSnapshot>, Option<ThreadSnapshot>), IntakeError> {
        let stored = match existing {
            Some(t) => self.tickets.get_thread_snapshot(org, t.value.id).await?.filter(|s| &s.thread == thread),
            None => None,
        };
        let fresh = stored.as_ref().is_some_and(|s| now - s.fetched_at < cfg.debounce.thread_refresh());
        if fresh {
            return Ok((stored, None));
        }
        let failed = |reason: String| ThreadSnapshot { thread: thread.clone(), fetched_at: now, messages: Vec::new(), error: Some(reason) };
        let reingest = |at: OffsetDateTime| NewJob::new(JobPayload::IntakeLinearIssue { org_id: org, issue_id: issue_id.to_owned() }).after(at);
        let snapshot = match self.chat.read_thread(org, thread, THREAD_MAX_MESSAGES).await {
            Ok(t) => ThreadSnapshot {
                thread: thread.clone(),
                fetched_at: now,
                messages: t
                    .messages
                    .into_iter()
                    .filter_map(|m| match m.author {
                        ChatAuthor::Human { user_id } => Some(ThreadMessage { ts: m.ts, author_id: user_id, text: m.text, file_names: m.file_names }),
                        ChatAuthor::Bot { .. } => None,
                    })
                    .collect(),
                error: None,
            },
            Err(ChatError::NoAccess(reason)) => failed(reason),
            Err(ChatError::NotFound) => failed("thread_not_found".into()),
            Err(ChatError::Unauthorized) => failed("slack_unauthorized".into()),
            Err(ChatError::Invalid(reason)) => failed(reason),
            Err(ChatError::RateLimited { retry_at }) => {
                follow_ups.push(reingest(retry_at));
                return Ok((stored, None));
            }
            Err(ChatError::Unavailable(_) | ChatError::Conflict) => {
                follow_ups.push(reingest(now + THREAD_RETRY_AFTER));
                return Ok((stored, None));
            }
        };
        Ok((Some(snapshot.clone()), Some(snapshot)))
    }

    /// 3.3: Linear customers, then customer labels, then the Slack channel; one match wins, an
    /// ambiguous step stops with no customer.
    async fn resolve_customer(&self, org: OrgId, cfg: &IntakeConfig, issue: &TrackerIssue, thread: Option<&SlackThreadRef>) -> Result<Option<Customer>, IntakeError> {
        let prefix = cfg.linear.customer_label_prefix.as_str();
        let steps: [Vec<ExternalRef>; 3] = [
            issue.customer_ids.iter().map(|id| ExternalRef::LinearCustomer(id.clone())).collect(),
            issue
                .labels
                .iter()
                .filter(|l| !prefix.is_empty() && l.to_lowercase().starts_with(&prefix.to_lowercase()))
                .map(|l| ExternalRef::LinearLabel(l.clone()))
                .collect(),
            thread.map(|t| ExternalRef::SlackChannel { team: t.team.clone(), channel: t.channel.clone() }).into_iter().collect(),
        ];
        for (step, refs) in steps.iter().enumerate() {
            if refs.is_empty() {
                continue;
            }
            let mut found = self.orgs.find_customers_by_refs(org, refs).await?;
            found.sort_by_key(|c| c.id);
            found.dedup_by_key(|c| c.id);
            match found.len() {
                0 => continue,
                1 => return Ok(found.pop()),
                n => {
                    tracing::info!(%org, issue = %issue.issue_id, step = step + 1, matches = n, "ambiguous customer; none assigned");
                    return Ok(None);
                }
            }
        }
        Ok(None)
    }

    /// 3.5: routes in order → default repo → the org's only granted repo → none.
    async fn route(&self, org: OrgId, cfg: &IntakeConfig, issue: &TrackerIssue) -> Result<Option<Repo>, IntakeError> {
        let by_name = |full: &str| {
            let full = full.to_owned();
            async move {
                let Some((owner, name)) = full.split_once('/') else {
                    tracing::warn!(%org, repo = %full, "route repo is not owner/name");
                    return Ok::<_, IntakeError>(None);
                };
                let repo = self.repos.repo_by_name(org, owner, name).await?;
                if repo.is_none() {
                    tracing::warn!(%org, repo = %full, "routed repo is not connected");
                }
                Ok(repo)
            }
        };
        if let Some(route) = cfg.routes.iter().find(|r| r.when.matches(&issue.team_key, &issue.labels, issue.project.as_deref())) {
            return by_name(&route.repo).await;
        }
        if let Some(default) = &cfg.default_repo {
            return by_name(default).await;
        }
        let granted: Vec<Repo> = self.repos.list_repos(org).await?.into_iter().filter(|r| r.access == RepoAccess::Granted).collect();
        Ok(if granted.len() == 1 { granted.into_iter().next() } else { None })
    }

    /// Step 7: `OnDemand(ticket)` keeps the version alive while the ticket is open. A deployment
    /// target also takes `Deployed(env)` when it is still that environment's newest deployment
    /// (moving the singleton reason back to an older sha would unpin the current deployment).
    async fn pin_version(&self, org: OrgId, ticket: TicketId, target: &TicketTarget, now: OffsetDateTime) -> Result<(), IntakeError> {
        self.repos.add_version_reason(org, target.repo_id, target.sha, VersionReason::OnDemand(ticket)).await?;
        if let TargetOrigin::Deployment { environment } = &target.origin {
            let newest = self.repos.latest_deployment(org, target.repo_id, environment, now).await?;
            if newest.is_some_and(|d| d.sha == target.sha) {
                self.repos.add_version_reason(org, target.repo_id, target.sha, VersionReason::Deployed(environment.clone())).await?;
            }
        }
        Ok(())
    }
}

fn eq_ci(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

/// Step 3: team in `teams`, any of `require_labels` (if set), none of `exclude_labels`.
pub fn passes_filters(cfg: &IntakeConfig, issue: &TrackerIssue) -> bool {
    let l = &cfg.linear;
    let has = |label: &String| issue.labels.iter().any(|x| eq_ci(x, label));
    l.teams.iter().any(|t| eq_ci(t, &issue.team_key))
        && (l.require_labels.is_empty() || l.require_labels.iter().any(has))
        && !l.exclude_labels.iter().any(has)
}

/// 3.7 `should_triage`.
pub fn should_triage(cfg: &IntakeConfig, issue: &TrackerIssue, ticket: &Ticket, target_repo: Option<&Repo>) -> bool {
    cfg.linear.triage_states.iter().any(|s| eq_ci(s, issue.state_kind.as_str()))
        && ticket.target.is_some()
        && target_repo.is_some_and(|r| r.access == RepoAccess::Granted)
        && matches!(ticket.status, TicketStatus::New | TicketStatus::Triaging | TicketStatus::Triaged | TicketStatus::TriageFailed)
        && ticket.triaged_hash != Some(ticket.content_hash)
}

fn same_content(ticket: &Ticket, c: &TicketContent) -> bool {
    ticket.title == c.title
        && ticket.body == c.body
        && ticket.content_hash == c.content_hash
        && ticket.slack_thread == c.slack_thread
        && ticket.customer == c.customer
        && ticket.target == c.target
        && ticket.error_strings == c.error_strings
}

/// A Slack thread permalink: `https://{team_domain}.slack.com/archives/{channel}/p{digits}[?thread_ts={ts}]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlackLink {
    pub team_domain: String,
    pub channel: String,
    /// Thread root ts (`thread_ts` if present, else the message ts).
    pub thread_ts: String,
}

pub fn parse_slack_link(url: &str) -> Option<SlackLink> {
    let url = Url::parse(url).ok()?;
    if url.scheme() != "https" {
        return None;
    }
    let host = url.host_str()?;
    let team_domain = host.strip_suffix(".slack.com")?;
    if team_domain.is_empty() || team_domain.contains('.') {
        return None;
    }
    let mut segments = url.path_segments()?;
    if segments.next()? != "archives" {
        return None;
    }
    let channel = segments.next()?;
    let message = segments.next()?;
    let digits = message.strip_prefix('p')?;
    if channel.is_empty() || !channel.bytes().all(|b| b.is_ascii_alphanumeric()) || digits.len() <= 6 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let ts = format!("{}.{}", &digits[..digits.len() - 6], &digits[digits.len() - 6..]);
    let thread_ts = url.query_pairs().find(|(k, _)| k == "thread_ts").map(|(_, v)| v.into_owned()).filter(|v| valid_ts(v)).unwrap_or(ts);
    Some(SlackLink { team_domain: team_domain.to_owned(), channel: channel.to_owned(), thread_ts })
}

fn valid_ts(ts: &str) -> bool {
    ts.split_once('.').is_some_and(|(a, b)| !a.is_empty() && !b.is_empty() && a.bytes().chain(b.bytes()).all(|c| c.is_ascii_digit()))
}

/// Slack `ts` (`1700000000.000100`) as a time.
fn slack_ts_time(ts: &str) -> Option<OffsetDateTime> {
    let (secs, frac) = ts.split_once('.')?;
    let secs: i64 = secs.parse().ok()?;
    let micros: i64 = frac.get(..6).unwrap_or(frac).parse().ok()?;
    OffsetDateTime::from_unix_timestamp(secs).ok().map(|t| t + time::Duration::microseconds(micros))
}

/// 3.4: candidates (slack attachments, other attachments, description URLs), first match wins;
/// team by matching `team_domain`, else the org's only Slack integration.
pub fn find_thread(issue: &TrackerIssue, slack: &[(String, Option<String>)]) -> Option<SlackThreadRef> {
    if slack.is_empty() {
        return None;
    }
    let team_for = |domain: &str| -> Option<String> {
        slack
            .iter()
            .find(|(_, d)| d.as_deref().is_some_and(|d| d.eq_ignore_ascii_case(domain)))
            .map(|(team, _)| team.clone())
            .or_else(|| (slack.len() == 1).then(|| slack[0].0.clone()))
    };
    let from_attachment = |a: &crate::ports::tracker::TrackerAttachment| -> Option<SlackThreadRef> {
        let link = parse_slack_link(a.url.as_str())?;
        let team = team_for(&link.team_domain)?;
        // Metadata channel / ts, when present, are preferred over the URL's.
        let meta = |keys: &[&str]| keys.iter().find_map(|k| a.metadata.get(*k).filter(|v| !v.is_empty()).cloned());
        let channel = meta(&["channelId", "channel_id", "channel"]).unwrap_or(link.channel);
        let ts = meta(&["threadTs", "thread_ts", "messageTs", "ts"]).filter(|v| valid_ts(v)).unwrap_or(link.thread_ts);
        Some(SlackThreadRef { team, channel, ts })
    };
    let slack_first = issue.attachments.iter().filter(|a| a.source_kind.as_deref().is_some_and(|k| k.eq_ignore_ascii_case("slack")));
    if let Some(t) = slack_first.filter_map(from_attachment).next() {
        return Some(t);
    }
    if let Some(t) = issue.attachments.iter().filter_map(from_attachment).next() {
        return Some(t);
    }
    static URL: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"https://[A-Za-z0-9-]+\.slack\.com/archives/[A-Za-z0-9]+/p[0-9]+(\?[^\s)\]>]*)?").expect("valid regex"));
    URL.find_iter(&issue.description).find_map(|m| {
        let link = parse_slack_link(m.as_str())?;
        Some(SlackThreadRef { team: team_for(&link.team_domain)?, channel: link.channel, ts: link.thread_ts })
    })
}

#[cfg(test)]
mod tests;
