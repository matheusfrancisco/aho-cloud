//! UC `index.history` (knowledge.md "Layer 6: History"): commits reachable from the version's
//! sha and not yet walked, closed pull requests updated since the last run, and ticket links
//! found in both (ticket-id scanner of `mage_core::domain::knowledge`). Progress is kept in the
//! repo's `HistoryCursor`; every write is an upsert, so a retried job only repeats work.

use std::sync::Arc;

use mage_core::domain::knowledge::{ticket_ids_in_branch, ticket_ids_in_text};
use mage_core::domain::{CommitSha, IndexPriority, Layer, LayerEvent, OrgId, RepoId};
use mage_core::ports::knowledge::{CommitRecord, HistoryCursor, LinkSource, PullRequestRecord, TicketLink};
use mage_core::ports::source_host::{PrListQuery, PrSort, PrState, PullRequestInfo, RepoCoords, SortDirection};
use mage_core::ports::{Clock, KnowledgeStore, RepoStore, SourceHost};
use mage_core::wire::indexer::{CommitInfo, LogQuery};

use super::layers::{self, Started};
use super::{IndexTarget, JobDone};
use crate::error::AppError;
use crate::ports::git::GitRepoReader;

/// Commits walked per run.
pub const MAX_COMMITS: u32 = 20_000;
/// First backfill reaches this far back.
pub const BACKFILL: time::Duration = time::Duration::days(2 * 365);
/// Pull requests read per run (the cursor continues on the next run).
pub const MAX_PRS: usize = 1000;
/// Rows per `put_*` call.
const WRITE_BATCH: usize = 1000;

/// The `index.history` handler.
pub struct IndexHistory {
    repos: Arc<dyn RepoStore>,
    knowledge: Arc<dyn KnowledgeStore>,
    git: Arc<dyn GitRepoReader>,
    source: Arc<dyn SourceHost>,
    clock: Arc<dyn Clock>,
}

impl IndexHistory {
    pub fn new(
        repos: Arc<dyn RepoStore>,
        knowledge: Arc<dyn KnowledgeStore>,
        git: Arc<dyn GitRepoReader>,
        source: Arc<dyn SourceHost>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self { repos, knowledge, git, source, clock }
    }

    pub async fn run(&self, org_id: OrgId, repo_id: RepoId, sha: CommitSha, prio: IndexPriority) -> Result<JobDone, AppError> {
        let t = IndexTarget::new(org_id, repo_id, sha, prio);
        let key = t.key();
        let version = self.repos.version(org_id, key).await?;
        let state = version.layer(Layer::History).cloned();
        if layers::start(&*self.repos, org_id, key, Layer::History, state.as_ref()).await? == Started::Closed {
            return Ok(JobDone::done());
        }
        let repo = self.repos.repo(org_id, repo_id).await?;
        let cursor = self.knowledge.history_cursor(org_id, repo_id).await?;

        // Commits: `sha --not <walked heads still in the mirror>`.
        let mirror = t.mirror();
        let mut exclude = Vec::with_capacity(cursor.walked_heads.len());
        for head in &cursor.walked_heads {
            if *head != sha && self.git.has_commit(&mirror, *head).await? {
                exclude.push(*head);
            }
        }
        let first_backfill = cursor.walked_heads.is_empty();
        let query = LogQuery {
            include: vec![sha],
            exclude,
            path_prefix: None,
            since: first_backfill.then(|| self.clock.now() - BACKFILL),
            skip: 0,
            max: MAX_COMMITS,
            with_files: true,
        };
        let mut commits = self.git.log(&mirror, &query).await?.into_iter();
        loop {
            let chunk: Vec<CommitInfo> = commits.by_ref().take(WRITE_BATCH).collect();
            if chunk.is_empty() {
                break;
            }
            let links: Vec<TicketLink> = chunk.iter().flat_map(commit_links).collect();
            let records: Vec<CommitRecord> = chunk.into_iter().map(|info| CommitRecord { info }).collect();
            self.knowledge.put_commits(org_id, repo_id, &records).await?;
            if !links.is_empty() {
                self.knowledge.put_ticket_links(org_id, repo_id, &links).await?;
            }
        }

        // Pull requests closed and updated since the last run, oldest update first.
        let coords = RepoCoords { owner: repo.owner.clone(), name: repo.name.clone() };
        let mut prs_updated_after = cursor.prs_updated_after;
        let mut page_token = None;
        let mut read = 0usize;
        loop {
            let query = PrListQuery {
                state: PrState::Closed,
                sort: PrSort::Updated,
                direction: SortDirection::Asc,
                updated_since: cursor.prs_updated_after,
                page_token: page_token.take(),
                per_page: mage_core::ports::source_host::MAX_PR_PAGE,
            };
            let page = self.source.list_pull_requests(repo.installation_id, &coords, &query).await?;
            read += page.items.len();
            let links: Vec<TicketLink> = page.items.iter().flat_map(pr_links).collect();
            prs_updated_after = page.items.iter().map(|pr| pr.updated_at).chain(prs_updated_after).max();
            let records: Vec<PullRequestRecord> = page.items.into_iter().map(pr_record).collect();
            if !records.is_empty() {
                self.knowledge.put_pull_requests(org_id, repo_id, &records).await?;
            }
            if !links.is_empty() {
                self.knowledge.put_ticket_links(org_id, repo_id, &links).await?;
            }
            match page.next {
                Some(next) if read < MAX_PRS => page_token = Some(next),
                _ => break,
            }
        }

        let merged = HistoryCursor { walked_heads: vec![sha], prs_updated_after };
        self.knowledge.merge_history_cursor(org_id, repo_id, &merged).await?;
        let complete = LayerEvent::Complete { at: self.clock.now() };
        self.repos.apply_layer_event(org_id, key, Layer::History, complete, &[]).await?;
        Ok(JobDone::done())
    }
}

fn pr_record(pr: PullRequestInfo) -> PullRequestRecord {
    PullRequestRecord {
        number: pr.number,
        title: pr.title,
        body_head: pr.body_head,
        head_ref: pr.head_ref,
        base_ref: pr.base_ref,
        merge_commit: pr.merge_commit_sha,
        merged_at: pr.merged_at,
        updated_at: pr.updated_at,
        url: pr.url,
    }
}

/// `Merge pull request #12 from owner/branch` → `(12, "owner/branch")`.
pub fn merge_subject(subject: &str) -> Option<(u32, &str)> {
    let rest = subject.strip_prefix("Merge pull request #")?;
    let digits = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
    let number = rest[..digits].parse().ok()?;
    let branch = rest[digits..].strip_prefix(" from ")?.split_whitespace().next()?;
    Some((number, branch))
}

/// Squash-merge subject ending `(#123)` → `123`.
pub fn squash_number(subject: &str) -> Option<u32> {
    let inner = subject.trim_end().strip_suffix(')')?;
    let open = inner.rfind("(#")?;
    inner[open + 2..].parse().ok()
}

/// Ticket links of one commit: subject and body (text scanner), the merged branch (branch
/// scanner), each tagged with the PR number the subject names.
pub fn commit_links(info: &CommitInfo) -> Vec<TicketLink> {
    let merge = merge_subject(&info.subject);
    let pr = merge.map(|(n, _)| n).or_else(|| squash_number(&info.subject));
    let link = |identifier: String, source| TicketLink { identifier, source, commit: Some(info.sha), pr };
    let mut links: Vec<TicketLink> = ticket_ids_in_text(&info.subject).into_iter().map(|id| link(id, LinkSource::CommitSubject)).collect();
    links.extend(ticket_ids_in_text(&info.body).into_iter().map(|id| link(id, LinkSource::CommitBody)));
    if let Some((_, branch)) = merge {
        links.extend(ticket_ids_in_branch(branch).into_iter().map(|id| link(id, LinkSource::MergeBranch)));
    }
    links
}

/// Ticket links of one pull request: title, head branch, body head.
pub fn pr_links(pr: &PullRequestInfo) -> Vec<TicketLink> {
    let link = |identifier: String, source| TicketLink { identifier, source, commit: pr.merge_commit_sha, pr: Some(pr.number) };
    let mut links: Vec<TicketLink> = ticket_ids_in_text(&pr.title).into_iter().map(|id| link(id, LinkSource::PrTitle)).collect();
    links.extend(ticket_ids_in_branch(&pr.head_ref).into_iter().map(|id| link(id, LinkSource::PrBranch)));
    links.extend(ticket_ids_in_text(&pr.body_head).into_iter().map(|id| link(id, LinkSource::PrBody)));
    links
}

#[cfg(test)]
mod tests;
