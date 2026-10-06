//! `MemTicketStore`: in-memory `TicketStore`. Outbox jobs go to a shared `MemJobQueue` and feed
//! entries to a shared [`MemChangeFeed`] inside the same lock scope as the write, after
//! validation. Cause confidences go through `f32` like the `real` column, so values read back
//! equal what the pg adapter returns. The pg adapter's foreign keys (runs, gates, artifacts,
//! users, repos) are not modelled; `pull_request_by_number` takes the repo from the ticket's
//! target (the pg adapter from the fix run).

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use mage_core::domain::fix::{FixPrState, FixPullRequest};
use mage_core::domain::{
    Cause, CauseId, CauseStatus, Confidence, ContentHash, FixBrief, FixBriefId, FixPullRequestId, OrgId, RepoId, Resolution, RunId, Ticket,
    TicketId, TicketSource, TicketStatus, TicketTarget, TargetOrigin,
};
use mage_core::ports::clock::Clock;
use mage_core::ports::common::{Page, PageRequest, Versioned};
use mage_core::ports::jobs::NewJob;
use mage_core::testing::MemJobQueue;
use parking_lot::Mutex;
use uuid::Uuid;

use crate::domain::intake::ThreadSnapshot;
use crate::ports::feed::FeedKind;
use crate::ports::ticket::{
    SimilarMatch, SimilarQuery, SimilarResolved, TicketFilter, TicketStore, TicketStoreError, TicketTransition, TriageWrite, SIMILAR_LIMIT_MAX,
    check_pull_request_payload, error_overlap, error_tokens, similarity_score,
};
use crate::testing::feed::MemChangeFeed;
use crate::testing::page::keyset_page;

pub struct MemTicketStore {
    clock: Arc<dyn Clock>,
    jobs: Arc<MemJobQueue>,
    feed: Arc<MemChangeFeed>,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    tickets: BTreeMap<(OrgId, TicketId), Versioned<Ticket>>,
    snapshots: BTreeMap<(OrgId, TicketId), ThreadSnapshot>,
    causes: BTreeMap<(OrgId, CauseId), Cause>,
    briefs: BTreeMap<(OrgId, FixBriefId), FixBrief>,
    prs: BTreeMap<(OrgId, FixPullRequestId), FixPullRequest>,
    resolutions: BTreeMap<(OrgId, TicketId), Resolution>,
}

fn check_jobs(jobs: &[NewJob]) -> Result<(), TicketStoreError> {
    MemJobQueue::validate_all(jobs).map_err(|e| TicketStoreError::Invalid(e.to_string()))
}

fn issue_id(ticket: &Ticket) -> &str {
    match &ticket.source {
        TicketSource::Linear { issue_id, .. } => issue_id,
    }
}

/// Lowercase alphanumeric words (the `simple` text-search configuration, approximately).
fn words(text: &str) -> Vec<String> {
    let mut out: Vec<String> =
        text.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_lowercase).collect();
    out.sort_unstable();
    out.dedup();
    out
}

fn matches_all(query: &[String], doc: &[String]) -> bool {
    !query.is_empty() && query.iter().all(|w| doc.binary_search(w).is_ok())
}

/// The confidence the `real` column hands back.
fn as_stored(cause: Cause) -> Result<Cause, TicketStoreError> {
    let mut data = cause.into_data();
    data.confidence =
        Confidence::new(f64::from(data.confidence.get() as f32)).map_err(|e| TicketStoreError::Invalid(e.to_string()))?;
    Ok(Cause::rehydrate(data))
}

impl State {
    fn ticket_mut(&mut self, org: OrgId, id: TicketId) -> Result<&mut Versioned<Ticket>, TicketStoreError> {
        self.tickets.get_mut(&(org, id)).ok_or(TicketStoreError::NotFound)
    }

    fn check_transition(&self, org: OrgId, ticket: TicketId, t: Option<TicketTransition>) -> Result<(), TicketStoreError> {
        if let Some(t) = t {
            let row = self.tickets.get(&(org, ticket)).ok_or(TicketStoreError::NotFound)?;
            if row.value.status != t.expect {
                return Err(TicketStoreError::Conflict);
            }
        }
        Ok(())
    }
}

/// Rewrites a stored ticket: `edit` changes the data, `updated_at` and `rev` follow.
fn rewrite(row: &mut Versioned<Ticket>, at: time::OffsetDateTime, edit: impl FnOnce(&mut mage_core::domain::TicketData)) -> Ticket {
    let mut data = row.value.clone().into_data();
    edit(&mut data);
    data.updated_at = at;
    row.value = Ticket::rehydrate(data);
    row.rev += 1;
    row.value.clone()
}

impl MemTicketStore {
    pub fn new(clock: Arc<dyn Clock>, jobs: Arc<MemJobQueue>, feed: Arc<MemChangeFeed>) -> Self {
        Self { clock, jobs, feed, state: Mutex::new(State::default()) }
    }

    fn ticket_feed(&self, org: OrgId, ticket: TicketId) {
        self.feed.push(org, FeedKind::Ticket, ticket.to_string());
    }

    /// Ticket transition shared by cause and PR writes (checked before by `check_transition`).
    fn apply_transition(&self, state: &mut State, org: OrgId, ticket: TicketId, t: Option<TicketTransition>) -> Result<(), TicketStoreError> {
        if let Some(t) = t {
            let now = self.clock.now();
            rewrite(state.ticket_mut(org, ticket)?, now, |d| d.status = t.next);
            self.ticket_feed(org, ticket);
        }
        Ok(())
    }
}

#[async_trait]
impl TicketStore for MemTicketStore {
    async fn get_ticket(&self, org: OrgId, id: TicketId) -> Result<Versioned<Ticket>, TicketStoreError> {
        self.state.lock().tickets.get(&(org, id)).cloned().ok_or(TicketStoreError::NotFound)
    }

    async fn get_by_linear_issue(&self, org: OrgId, issue: &str) -> Result<Option<Versioned<Ticket>>, TicketStoreError> {
        let state = self.state.lock();
        Ok(state.tickets.iter().find(|((o, _), row)| *o == org && issue_id(&row.value) == issue).map(|(_, row)| row.clone()))
    }

    async fn list_tickets(&self, org: OrgId, filter: &TicketFilter, page: PageRequest) -> Result<Page<Ticket>, TicketStoreError> {
        let after = page
            .after
            .as_ref()
            .map(|c| Uuid::try_parse(&c.0).map_err(|_| TicketStoreError::Invalid(format!("bad cursor {:?}", c.0))))
            .transpose()?;
        let text = filter.text.as_deref().map(words);
        let state = self.state.lock();
        let items = state
            .tickets
            .iter()
            .filter(|((o, _), _)| *o == org)
            .map(|(_, row)| &row.value)
            .filter(|t| filter.status.as_ref().is_none_or(|s| s.contains(&t.status)))
            .filter(|t| filter.customer.is_none_or(|c| t.customer == Some(c)))
            .filter(|t| filter.repo.is_none_or(|r| t.target.as_ref().map(|x| x.repo_id) == Some(r)))
            .filter(|t| text.as_ref().is_none_or(|q| matches_all(q, &words(&format!("{} {}", t.title, t.body)))))
            .map(|t| (std::cmp::Reverse(t.id.as_uuid()), t.clone()))
            .collect();
        let after = after.map(std::cmp::Reverse);
        Ok(keyset_page(items, after.as_ref(), &page, |k| k.0.to_string()))
    }

    async fn list_open_assumed(&self, org: OrgId, repo: RepoId) -> Result<Vec<TicketId>, TicketStoreError> {
        let state = self.state.lock();
        Ok(state
            .tickets
            .iter()
            .filter(|((o, _), row)| {
                *o == org
                    && !row.value.status.is_terminal()
                    && row.value.target.as_ref().is_some_and(|t| t.repo_id == repo && t.origin == TargetOrigin::DefaultHead)
            })
            .map(|((_, id), _)| *id)
            .collect())
    }

    async fn save_ingested(&self, org: OrgId, ticket: &Ticket, expected_rev: Option<u64>, jobs: &[NewJob]) -> Result<u64, TicketStoreError> {
        if ticket.org_id != org {
            return Err(TicketStoreError::Invalid("ticket belongs to another org".into()));
        }
        check_jobs(jobs)?;
        let mut state = self.state.lock();
        let issue = issue_id(ticket);
        let issue_taken = state.tickets.iter().any(|((o, id), row)| *o == org && *id != ticket.id && issue_id(&row.value) == issue);
        let rev = match expected_rev {
            None => {
                if issue_taken || state.tickets.contains_key(&(org, ticket.id)) {
                    return Err(TicketStoreError::Conflict);
                }
                state.tickets.insert((org, ticket.id), Versioned { value: ticket.clone(), rev: 1 });
                1
            }
            Some(expected) => {
                let row = state.tickets.get(&(org, ticket.id)).ok_or(TicketStoreError::NotFound)?;
                if row.rev != expected || issue_taken {
                    return Err(TicketStoreError::Conflict);
                }
                let rev = expected + 1;
                state.tickets.insert((org, ticket.id), Versioned { value: ticket.clone(), rev });
                rev
            }
        };
        self.jobs.insert_validated(jobs);
        self.ticket_feed(org, ticket.id);
        Ok(rev)
    }

    async fn transition(&self, org: OrgId, ticket: TicketId, expect: TicketStatus, next: TicketStatus, jobs: &[NewJob])
    -> Result<Ticket, TicketStoreError> {
        check_jobs(jobs)?;
        let now = self.clock.now();
        let mut state = self.state.lock();
        let row = state.ticket_mut(org, ticket)?;
        if row.value.status != expect {
            return Err(TicketStoreError::Conflict);
        }
        let out = rewrite(row, now, |d| d.status = next);
        self.jobs.insert_validated(jobs);
        self.ticket_feed(org, ticket);
        Ok(out)
    }

    async fn set_target(&self, org: OrgId, ticket: TicketId, target: Option<TicketTarget>, expected_rev: u64, jobs: &[NewJob])
    -> Result<u64, TicketStoreError> {
        check_jobs(jobs)?;
        let now = self.clock.now();
        let mut state = self.state.lock();
        let row = state.ticket_mut(org, ticket)?;
        if row.rev != expected_rev {
            return Err(TicketStoreError::Conflict);
        }
        rewrite(row, now, |d| d.target = target);
        let rev = row.rev;
        self.jobs.insert_validated(jobs);
        self.ticket_feed(org, ticket);
        Ok(rev)
    }

    async fn get_thread_snapshot(&self, org: OrgId, ticket: TicketId) -> Result<Option<ThreadSnapshot>, TicketStoreError> {
        Ok(self.state.lock().snapshots.get(&(org, ticket)).cloned())
    }

    async fn put_thread_snapshot(&self, org: OrgId, ticket: TicketId, s: &ThreadSnapshot) -> Result<(), TicketStoreError> {
        let mut state = self.state.lock();
        if !state.tickets.contains_key(&(org, ticket)) {
            return Err(TicketStoreError::NotFound);
        }
        state.snapshots.insert((org, ticket), s.clone());
        self.ticket_feed(org, ticket);
        Ok(())
    }

    async fn begin_triage(
        &self,
        org: OrgId,
        ticket: TicketId,
        expect: &[TicketStatus],
        content_hash: ContentHash,
        triage_run: RunId,
    ) -> Result<Ticket, TicketStoreError> {
        let now = self.clock.now();
        let mut state = self.state.lock();
        let row = state.ticket_mut(org, ticket)?;
        if !expect.contains(&row.value.status) || row.value.content_hash != content_hash {
            return Err(TicketStoreError::Conflict);
        }
        let out = rewrite(row, now, |d| {
            d.status = TicketStatus::Triaging;
            d.triaged_hash = Some(content_hash);
            d.triage_run = Some(triage_run);
        });
        self.ticket_feed(org, ticket);
        Ok(out)
    }

    async fn apply_triage(&self, org: OrgId, w: TriageWrite, jobs: &[NewJob]) -> Result<Ticket, TicketStoreError> {
        check_jobs(jobs)?;
        if let Some(bad) = w.causes.iter().find(|c| c.org_id != org || c.ticket_id != w.ticket) {
            return Err(TicketStoreError::Invalid(format!("cause {} belongs to another ticket or org", bad.id)));
        }
        let causes = w.causes.into_iter().map(as_stored).collect::<Result<Vec<_>, _>>()?;
        let now = self.clock.now();
        let mut state = self.state.lock();
        let row = state.tickets.get(&(org, w.ticket)).ok_or(TicketStoreError::NotFound)?;
        let t = &row.value;
        if t.status != w.expect || t.triage_run != Some(w.triage_run) || t.content_hash != w.content_hash {
            return Err(TicketStoreError::Conflict);
        }
        if causes.iter().any(|c| state.causes.contains_key(&(org, c.id))) {
            return Err(TicketStoreError::Conflict);
        }
        for cause in state.causes.values_mut() {
            if cause.org_id == org && cause.ticket_id == w.ticket && cause.status == CauseStatus::Untested && cause.superseded_at.is_none() {
                let mut data = cause.clone().into_data();
                data.superseded_at = Some(now);
                *cause = Cause::rehydrate(data);
            }
        }
        for cause in causes {
            state.causes.insert((org, cause.id), cause);
        }
        let out = rewrite(state.ticket_mut(org, w.ticket)?, now, |d| d.status = w.next);
        self.jobs.insert_validated(jobs);
        self.ticket_feed(org, w.ticket);
        Ok(out)
    }

    async fn fail_triage(&self, org: OrgId, ticket: TicketId, triage_run: RunId, jobs: &[NewJob]) -> Result<Ticket, TicketStoreError> {
        check_jobs(jobs)?;
        let now = self.clock.now();
        let mut state = self.state.lock();
        let row = state.ticket_mut(org, ticket)?;
        if row.value.status != TicketStatus::Triaging || row.value.triage_run != Some(triage_run) {
            return Err(TicketStoreError::Conflict);
        }
        let out = rewrite(row, now, |d| d.status = TicketStatus::TriageFailed);
        self.jobs.insert_validated(jobs);
        self.ticket_feed(org, ticket);
        Ok(out)
    }

    async fn causes(&self, org: OrgId, ticket: TicketId, include_superseded: bool) -> Result<Vec<Cause>, TicketStoreError> {
        let state = self.state.lock();
        let of_ticket = || state.causes.values().filter(move |c| c.org_id == org && c.ticket_id == ticket);
        let mut current: Vec<Cause> = of_ticket().filter(|c| c.superseded_at.is_none()).cloned().collect();
        current.sort_by_key(|c| (c.rank, c.id));
        if include_superseded {
            let mut old: Vec<Cause> = of_ticket().filter(|c| c.superseded_at.is_some()).cloned().collect();
            old.sort_by_key(|c| (std::cmp::Reverse(c.superseded_at), c.rank, c.id));
            current.extend(old);
        }
        Ok(current)
    }

    async fn get_cause(&self, org: OrgId, id: CauseId) -> Result<Cause, TicketStoreError> {
        self.state.lock().causes.get(&(org, id)).cloned().ok_or(TicketStoreError::NotFound)
    }

    async fn set_cause_status(
        &self,
        org: OrgId,
        id: CauseId,
        expect: &CauseStatus,
        next: CauseStatus,
        ticket: Option<TicketTransition>,
        jobs: &[NewJob],
    ) -> Result<Cause, TicketStoreError> {
        check_jobs(jobs)?;
        let mut state = self.state.lock();
        let cause = state.causes.get(&(org, id)).ok_or(TicketStoreError::NotFound)?;
        if cause.status.name() != expect.name() {
            return Err(TicketStoreError::Conflict);
        }
        let ticket_id = cause.ticket_id;
        state.check_transition(org, ticket_id, ticket)?;
        let mut data = cause.clone().into_data();
        data.status = next;
        let updated = Cause::rehydrate(data);
        state.causes.insert((org, id), updated.clone());
        self.apply_transition(&mut state, org, ticket_id, ticket)?;
        self.jobs.insert_validated(jobs);
        self.feed.push(org, FeedKind::Cause, id.to_string());
        Ok(updated)
    }

    async fn put_fix_brief(&self, org: OrgId, brief: FixBrief, jobs: &[NewJob]) -> Result<FixBrief, TicketStoreError> {
        if brief.org_id != org {
            return Err(TicketStoreError::Invalid("fix brief belongs to another org".into()));
        }
        check_jobs(jobs)?;
        let mut state = self.state.lock();
        if !state.causes.contains_key(&(org, brief.cause_id)) {
            return Err(TicketStoreError::NotFound);
        }
        if state.briefs.contains_key(&(org, brief.id)) {
            return Err(TicketStoreError::Conflict);
        }
        let version = state.briefs.values().filter(|b| b.org_id == org && b.cause_id == brief.cause_id).map(|b| b.version).max().unwrap_or(0) + 1;
        let stored = FixBrief { version, ..brief };
        state.briefs.insert((org, stored.id), stored.clone());
        self.jobs.insert_validated(jobs);
        self.feed.push(org, FeedKind::Cause, stored.cause_id.to_string());
        Ok(stored)
    }

    async fn latest_fix_brief(&self, org: OrgId, cause: CauseId) -> Result<Option<FixBrief>, TicketStoreError> {
        let state = self.state.lock();
        Ok(state.briefs.values().filter(|b| b.org_id == org && b.cause_id == cause).max_by_key(|b| b.version).cloned())
    }

    async fn fix_briefs(&self, org: OrgId, cause: CauseId) -> Result<Vec<FixBrief>, TicketStoreError> {
        let state = self.state.lock();
        let mut out: Vec<FixBrief> = state.briefs.values().filter(|b| b.org_id == org && b.cause_id == cause).cloned().collect();
        out.sort_by_key(|b| b.version);
        Ok(out)
    }

    async fn upsert_pull_request(&self, org: OrgId, pr: FixPullRequest, jobs: &[NewJob]) -> Result<(FixPullRequest, bool), TicketStoreError> {
        if pr.org_id != org {
            return Err(TicketStoreError::Invalid("pull request belongs to another org".into()));
        }
        check_pull_request_payload(&pr)?;
        check_jobs(jobs)?;
        let mut state = self.state.lock();
        if !state.tickets.contains_key(&(org, pr.ticket_id)) {
            return Err(TicketStoreError::NotFound);
        }
        if let Some(existing) = state.prs.values().find(|p| p.org_id == org && p.ticket_id == pr.ticket_id && p.change_set == pr.change_set) {
            return Ok((existing.clone(), false));
        }
        if state.prs.contains_key(&(org, pr.id)) {
            return Err(TicketStoreError::Conflict);
        }
        state.prs.insert((org, pr.id), pr.clone());
        self.jobs.insert_validated(jobs);
        self.ticket_feed(org, pr.ticket_id);
        Ok((pr, true))
    }

    async fn set_pull_request_state(
        &self,
        org: OrgId,
        id: FixPullRequestId,
        expect: &FixPrState,
        next: FixPrState,
        jobs: &[NewJob],
    ) -> Result<FixPullRequest, TicketStoreError> {
        check_jobs(jobs)?;
        let now = self.clock.now();
        let mut state = self.state.lock();
        let pr = state.prs.get_mut(&(org, id)).ok_or(TicketStoreError::NotFound)?;
        if pr.state.name() != expect.name() {
            return Err(TicketStoreError::Conflict);
        }
        match &next {
            FixPrState::CommitCreated { commit } => pr.head_commit = Some(*commit),
            FixPrState::Opened { number } => pr.number = Some(*number),
            FixPrState::Planned | FixPrState::BranchCreated | FixPrState::Closed { .. } => {}
        }
        pr.state = next;
        pr.updated_at = now;
        let out = pr.clone();
        self.jobs.insert_validated(jobs);
        self.ticket_feed(org, out.ticket_id);
        Ok(out)
    }

    async fn update_pull_request(
        &self,
        org: OrgId,
        pr: &FixPullRequest,
        expect: &FixPrState,
        ticket: Option<TicketTransition>,
        jobs: &[NewJob],
    ) -> Result<FixPullRequest, TicketStoreError> {
        check_pull_request_payload(pr)?;
        check_jobs(jobs)?;
        let now = self.clock.now();
        let mut state = self.state.lock();
        let stored = state.prs.get(&(org, pr.id)).ok_or(TicketStoreError::NotFound)?;
        if stored.state.name() != expect.name() {
            return Err(TicketStoreError::Conflict);
        }
        let ticket_id = stored.ticket_id;
        state.check_transition(org, ticket_id, ticket)?;
        let row = state.prs.get_mut(&(org, pr.id)).ok_or(TicketStoreError::NotFound)?;
        row.branch = pr.branch.clone();
        row.head_commit = pr.head_commit;
        row.number = pr.number;
        row.url = pr.url.clone();
        row.state = pr.state.clone();
        row.updated_at = now;
        let out = row.clone();
        self.apply_transition(&mut state, org, ticket_id, ticket)?;
        self.jobs.insert_validated(jobs);
        self.ticket_feed(org, ticket_id);
        Ok(out)
    }

    async fn pull_requests(&self, org: OrgId, ticket: TicketId) -> Result<Vec<FixPullRequest>, TicketStoreError> {
        let state = self.state.lock();
        let mut out: Vec<FixPullRequest> = state.prs.values().filter(|p| p.org_id == org && p.ticket_id == ticket).cloned().collect();
        out.sort_by_key(|p| (p.created_at, p.id));
        Ok(out)
    }

    async fn pull_request_by_number(&self, org: OrgId, repo: RepoId, number: u64) -> Result<Option<FixPullRequest>, TicketStoreError> {
        let state = self.state.lock();
        Ok(state
            .prs
            .values()
            .find(|p| {
                p.org_id == org
                    && p.number == Some(number)
                    && state.tickets.get(&(org, p.ticket_id)).and_then(|t| t.value.target.as_ref()).is_some_and(|t| t.repo_id == repo)
            })
            .cloned())
    }

    async fn resolve(&self, org: OrgId, resolution: Resolution, expect: TicketStatus, jobs: &[NewJob]) -> Result<Ticket, TicketStoreError> {
        if resolution.org_id != org {
            return Err(TicketStoreError::Invalid("resolution belongs to another org".into()));
        }
        check_jobs(jobs)?;
        let now = self.clock.now();
        let mut state = self.state.lock();
        let ticket = resolution.ticket_id;
        let row = state.tickets.get(&(org, ticket)).ok_or(TicketStoreError::NotFound)?;
        if state.resolutions.contains_key(&(org, ticket)) || row.value.status != expect {
            return Err(TicketStoreError::Conflict);
        }
        if let Some(cause) = resolution.confirmed_cause {
            if !state.causes.get(&(org, cause)).is_some_and(|c| c.ticket_id == ticket) {
                return Err(TicketStoreError::Invalid(format!("confirmed cause {cause} is not a cause of {ticket}")));
            }
        }
        state.resolutions.insert((org, ticket), resolution);
        let out = rewrite(state.ticket_mut(org, ticket)?, now, |d| d.status = TicketStatus::Resolved);
        self.jobs.insert_validated(jobs);
        self.ticket_feed(org, ticket);
        Ok(out)
    }

    async fn resolution(&self, org: OrgId, ticket: TicketId) -> Result<Option<Resolution>, TicketStoreError> {
        Ok(self.state.lock().resolutions.get(&(org, ticket)).cloned())
    }

    async fn similar_resolved(&self, org: OrgId, q: &SimilarQuery, limit: u16) -> Result<Vec<SimilarResolved>, TicketStoreError> {
        let query_words = words(&q.text);
        let query_errors = error_tokens(q.error_strings.iter().map(String::as_str));
        let state = self.state.lock();
        let mut out: Vec<SimilarResolved> = Vec::new();
        for ((o, id), resolution) in &state.resolutions {
            let Some(row) = state.tickets.get(&(*o, *id)) else { continue };
            let t = &row.value;
            let in_scope = *o == org
                && t.status == TicketStatus::Resolved
                && t.target.as_ref().is_some_and(|x| q.repos.contains(&x.repo_id))
                && !q.exclude.contains(id)
                && q.as_of.is_none_or(|as_of| resolution.at <= as_of);
            if !in_scope {
                continue;
            }
            let text = matches_all(&query_words, &words(&format!("{} {}", t.title, t.body)))
                || matches_all(&query_words, &words(&resolution.root_cause));
            let errors = error_overlap(&query_errors, &error_tokens(t.error_strings.iter().map(String::as_str)));
            let mut matched_on = Vec::new();
            if text {
                matched_on.push(SimilarMatch::Text);
            }
            if errors > 0.0 {
                matched_on.push(SimilarMatch::ErrorString);
            }
            if matched_on.is_empty() {
                continue;
            }
            let TicketSource::Linear { identifier, url, .. } = &t.source;
            let cause_citations = resolution
                .confirmed_cause
                .and_then(|c| state.causes.get(&(org, c)))
                .map(|c| c.evidence.clone())
                .unwrap_or_default();
            out.push(SimilarResolved {
                ticket: *id,
                identifier: identifier.clone(),
                title: t.title.clone(),
                url: url.to_string(),
                resolved_at: resolution.at,
                root_cause: resolution.root_cause.clone(),
                cause_citations,
                fix_pr_url: resolution.fix_pr_url.as_ref().map(|u| u.to_string()),
                score: similarity_score(if text { 1.0 } else { 0.0 }, errors),
                matched_on,
            });
        }
        out.sort_by(|a, b| b.score.total_cmp(&a.score).then(b.resolved_at.cmp(&a.resolved_at)).then(a.ticket.cmp(&b.ticket)));
        out.truncate(usize::from(limit.clamp(1, SIMILAR_LIMIT_MAX)));
        Ok(out)
    }
}

