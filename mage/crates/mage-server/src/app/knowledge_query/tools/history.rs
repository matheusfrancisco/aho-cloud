//! `changes_between`, `blame` (layers Files + History; git work runs on the indexer, knowledge.md).

use std::collections::{BTreeMap, BTreeSet};

use mage_core::domain::{CommitSha, Layer, LineRange, RepoPath, RepoVersionKey};
use mage_core::ports::knowledge::{LinkLookup, PullRequestRecord, TicketLink};
use mage_core::ports::repos::RepoStoreError;
use mage_core::wire::indexer::{CommitInfo, FileChange, LogQuery, MAX_BLAME_LINES, MAX_DIFF_BYTES_PER_FILE, MAX_DIFF_PATHS, TreeDiffOptions};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::super::envelope::{CursorScope, Degraded, Envelope, ListField, StoreKey, ToolError};
use super::super::inputs::{BlameInput, ChangeInclude, ChangesBetweenInput, check_range, parse_path};
use super::super::scope::KnowledgeScope;
use super::super::service::{Ctx, Service, check_scope, parse_sha};
use super::{resume_key, rfc3339, store_page};
use crate::ports::indexer_link::IndexerLinkError;

/// Commits per `changes_between` answer.
const COMMITS_MAX: u32 = 50;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "f", rename_all = "snake_case")]
enum FilesKey {
    Manifest {
        #[serde(flatten)]
        page: StoreKey,
    },
    Diff {
        o: u32,
    },
}

fn file_json(path: &RepoPath, change: impl Serialize, old_path: Option<&RepoPath>) -> Value {
    let mut out = json!({ "path": path, "change": change });
    if let Some(old) = old_path {
        out["old_path"] = json!(old);
    }
    out
}

fn pr_json(pr: &PullRequestRecord) -> Value {
    json!({ "number": pr.number, "title": pr.title, "url": pr.url })
}

impl Service {
    pub(crate) async fn changes_between(&self, scope: &KnowledgeScope, mut input: ChangesBetweenInput) -> Result<Envelope, ToolError> {
        check_scope(scope, input.repo.as_deref(), input.to.as_deref())?;
        check_range("limit", input.limit, 1, 200)?;
        let from = parse_sha(&input.from)?;
        let to = scope.sha;
        let prefix = input.path_prefix.as_deref().map(parse_path).transpose()?;
        let cursor = input.cursor.take();
        let cursors = CursorScope::new("changes_between", &input);
        let resumed = match cursors.decode_opt(cursor.as_deref())? {
            Some(k) => Some(serde_json::from_str::<FilesKey>(&k).map_err(|_| ToolError::bad_cursor())?),
            None => None,
        };
        let ctx = self.ctx(scope).await?;
        let (org, key) = (ctx.org(), ctx.key());
        let from_key = RepoVersionKey::new(scope.repo, from);
        let from_indexed = match self.repos.version(org, from_key).await {
            Ok(version) => version.layer(Layer::Files).is_some_and(|s| s.is_ready()),
            Err(RepoStoreError::NotFound) => false,
            Err(e) => return Err(e.into()),
        };
        if !from_indexed && !self.knowledge.has_commit(org, scope.repo, from).await? {
            return Err(ToolError::out_of_scope("`from` is not a known commit of this repo"));
        }
        let include: BTreeSet<ChangeInclude> = input.include.iter().copied().collect();
        let mut env = ctx.envelope(Layer::Files);
        let mut indexer_down = None;

        // Files.
        let mut page_files: Vec<RepoPath> = Vec::new();
        if include.contains(&ChangeInclude::Files) || include.contains(&ChangeInclude::Hunks) {
            let use_manifests = match &resumed {
                Some(FilesKey::Manifest { .. }) => true,
                Some(FilesKey::Diff { .. }) => false,
                None => from_indexed,
            };
            let mut files: Vec<(RepoPath, Value)>;
            if use_manifests {
                let page_key = match &resumed {
                    Some(FilesKey::Manifest { page }) => page.clone(),
                    _ => StoreKey::default(),
                };
                let (changes, next) = store_page(&page_key, input.limit, |page| {
                    let prefix = prefix.clone();
                    async move { self.knowledge.manifest_diff(org, &from_key, &key, prefix.as_ref(), &page).await }
                })
                .await?;
                files = changes.iter().map(|c| (c.path.clone(), file_json(&c.path, c.change, None))).collect();
                env.set("files", Value::Array(files.iter().map(|(_, v)| v.clone()).collect()));
                let kept = env.fit(ListField::Extra("files")).unwrap_or(files.len());
                files.truncate(kept);
                env.next_cursor = resume_key(&page_key, kept, changes.len(), next)
                    .map(|page| cursors.encode(&serde_json::to_string(&FilesKey::Manifest { page }).unwrap_or_default()));
            } else {
                let offset = match &resumed {
                    Some(FilesKey::Diff { o }) => *o as usize,
                    _ => 0,
                };
                let opts = TreeDiffOptions { paths: Vec::new(), hunks: false, context: 0, max_bytes_per_file: 0, renames: true };
                match self.indexer.diff(org, scope.repo, from, to, &opts).await {
                    Ok(diffs) => {
                        let all: Vec<&FileChange> =
                            diffs.iter().map(|d| &d.change).filter(|c| prefix.as_ref().is_none_or(|p| c.path.is_within(p.as_str()))).collect();
                        let page: Vec<&FileChange> = all.iter().skip(offset).take(input.limit as usize).copied().collect();
                        files = page.iter().map(|c| (c.path.clone(), file_json(&c.path, c.change, c.old_path.as_ref()))).collect();
                        env.set("files", Value::Array(files.iter().map(|(_, v)| v.clone()).collect()));
                        let kept = env.fit(ListField::Extra("files")).unwrap_or(files.len());
                        files.truncate(kept);
                        if offset + kept < all.len() {
                            let next = FilesKey::Diff { o: u32::try_from(offset + kept).unwrap_or(u32::MAX) };
                            env.next_cursor = Some(cursors.encode(&serde_json::to_string(&next).unwrap_or_default()));
                        }
                    }
                    Err(e @ (IndexerLinkError::Unavailable(_) | IndexerLinkError::Timeout)) => {
                        return Err(ToolError::unavailable(format!("{from} is not an indexed version and the indexer is unavailable: {e}")));
                    }
                    Err(e) => return Err(e.into()),
                }
            }
            page_files = files.into_iter().map(|(p, _)| p).collect();
            if !include.contains(&ChangeInclude::Files) {
                env.extra.remove("files");
            }
        }

        // Commits (first page only).
        if include.contains(&ChangeInclude::Commits) && resumed.is_none() {
            if ctx.ready(Layer::History) {
                let q = LogQuery { include: vec![to], exclude: vec![from], path_prefix: prefix.clone(), since: None, skip: 0, max: COMMITS_MAX, with_files: false };
                match self.indexer.log(org, scope.repo, &q).await {
                    Ok(commits) => {
                        env.set("commits", Value::Array(self.commit_items(&ctx, &commits).await?));
                        env.layer = Layer::History.into();
                    }
                    Err(e @ (IndexerLinkError::Unavailable(_) | IndexerLinkError::Timeout)) => indexer_down = Some(e),
                    Err(e) => return Err(e.into()),
                }
            } else {
                env.degraded = Some(Degraded::new(ctx.not_ready_reason(Layer::History), Layer::History));
            }
        } else if include.contains(&ChangeInclude::Commits) {
            env.note("commits are listed on the first page only");
        }

        // Hunks of this page's files.
        if include.contains(&ChangeInclude::Hunks) && !page_files.is_empty() && indexer_down.is_none() {
            let paths: Vec<RepoPath> = page_files.iter().take(MAX_DIFF_PATHS).cloned().collect();
            if page_files.len() > MAX_DIFF_PATHS {
                env.note(format!("hunks for the first {MAX_DIFF_PATHS} files of this page"));
            }
            let opts = TreeDiffOptions { paths, hunks: true, context: 3, max_bytes_per_file: MAX_DIFF_BYTES_PER_FILE, renames: true };
            match self.indexer.diff(org, scope.repo, from, to, &opts).await {
                Ok(diffs) => {
                    env.set(
                        "hunks",
                        Value::Array(
                            diffs.iter().map(|d| json!({ "path": d.change.path, "binary": d.binary, "truncated": d.truncated, "hunks": d.hunks })).collect(),
                        ),
                    );
                    env.fit(ListField::Extra("hunks"));
                }
                Err(e @ (IndexerLinkError::Unavailable(_) | IndexerLinkError::Timeout)) => indexer_down = Some(e),
                Err(e) => return Err(e.into()),
            }
        }
        if let Some(e) = indexer_down {
            env.degraded = Some(Degraded::new(format!("indexer unavailable: {e}"), Layer::History));
            env.layer = Layer::Files.into();
        }
        Ok(env)
    }

    /// Commits enriched with their PR and ticket links (History tables).
    async fn commit_items(&self, ctx: &Ctx<'_>, commits: &[CommitInfo]) -> Result<Vec<Value>, ToolError> {
        let shas: Vec<CommitSha> = commits.iter().map(|c| c.sha).collect();
        let (prs, tickets) = self.history_links(ctx, &shas).await?;
        Ok(commits
            .iter()
            .map(|c| {
                let mut item = json!({
                    "sha": c.sha,
                    "subject": c.subject,
                    "author_name": c.author_name,
                    "time": rfc3339(c.committer_time),
                    "tickets": tickets.get(&c.sha).cloned().unwrap_or_default(),
                });
                if let Some(pr) = prs.get(&c.sha) {
                    item["pr"] = pr_json(pr);
                }
                item
            })
            .collect())
    }

    /// PRs by merge commit and ticket identifiers by commit (or by their PR).
    async fn history_links(
        &self,
        ctx: &Ctx<'_>,
        shas: &[CommitSha],
    ) -> Result<(BTreeMap<CommitSha, PullRequestRecord>, BTreeMap<CommitSha, Vec<String>>), ToolError> {
        if shas.is_empty() || !ctx.ready(Layer::History) {
            return Ok((BTreeMap::new(), BTreeMap::new()));
        }
        let (org, repo) = (ctx.org(), ctx.scope.repo);
        let prs: BTreeMap<CommitSha, PullRequestRecord> = self.knowledge.pull_requests_for_commits(org, repo, shas).await?.into_iter().collect();
        let mut links: Vec<TicketLink> = self.knowledge.ticket_links(org, repo, &LinkLookup::Commits(shas.to_vec())).await?;
        let numbers: Vec<u32> = prs.values().map(|p| p.number).collect();
        if !numbers.is_empty() {
            links.extend(self.knowledge.ticket_links(org, repo, &LinkLookup::Prs(numbers)).await?);
        }
        let by_number: BTreeMap<u32, CommitSha> = prs.iter().map(|(sha, pr)| (pr.number, *sha)).collect();
        let mut tickets: BTreeMap<CommitSha, BTreeSet<String>> = BTreeMap::new();
        for link in links {
            let sha = link.commit.or_else(|| link.pr.and_then(|n| by_number.get(&n).copied()));
            if let Some(sha) = sha {
                tickets.entry(sha).or_default().insert(link.identifier);
            }
        }
        Ok((prs, tickets.into_iter().map(|(sha, ids)| (sha, ids.into_iter().collect())).collect()))
    }

    pub(crate) async fn blame(&self, scope: &KnowledgeScope, input: BlameInput) -> Result<Envelope, ToolError> {
        check_scope(scope, input.repo.as_deref(), input.sha.as_deref())?;
        if input.start_line == 0 || input.end_line < input.start_line {
            return Err(ToolError::invalid("need 1 <= start_line <= end_line"));
        }
        if input.end_line - input.start_line + 1 > MAX_BLAME_LINES {
            return Err(ToolError::invalid(format!("at most {MAX_BLAME_LINES} lines (end_line <= start_line + 199)")));
        }
        let path = parse_path(&input.path)?;
        let lines = LineRange::new(input.start_line, input.end_line).map_err(|e| ToolError::invalid(e.to_string()))?;
        let ctx = self.ctx(scope).await?;
        let hunks = match self.indexer.blame(ctx.org(), &ctx.key(), &path, lines).await {
            Ok(hunks) => hunks,
            Err(IndexerLinkError::NotFound) => return Err(ToolError::not_found(format!("{path} is not a file at this sha"))),
            Err(e) => return Err(e.into()),
        };
        let shas: Vec<CommitSha> = hunks.iter().map(|h| h.commit).collect::<BTreeSet<_>>().into_iter().collect();
        let (prs, tickets) = self.history_links(&ctx, &shas).await?;
        let mut env = ctx.envelope(Layer::History);
        if !ctx.ready(Layer::History) {
            env.note(format!("PR and ticket links omitted: {}", ctx.not_ready_reason(Layer::History)));
        }
        env.items = hunks
            .iter()
            .filter_map(|h| {
                let start = h.start.max(lines.start());
                let end = (h.start + h.lines.max(1) - 1).min(lines.end());
                (start <= end).then_some((h, start, end))
            })
            .map(|(h, start, end)| {
                let mut item = json!({
                    "lines": [start, end],
                    "commit": { "sha": h.commit, "subject": h.subject, "author_name": h.author_name, "time": rfc3339(h.author_time) },
                    "tickets": tickets.get(&h.commit).cloned().unwrap_or_default(),
                    "citation": ctx.cite(&path, start, end),
                });
                if let Some(pr) = prs.get(&h.commit) {
                    item["pr"] = pr_json(pr);
                }
                item
            })
            .collect();
        Ok(env)
    }
}
