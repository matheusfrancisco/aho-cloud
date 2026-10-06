//! In-memory `IndexerLink` (knowledge.md "Testing": fakes for `IndexerLink` live in
//! `mage_server::testing`). Search runs over seeded file texts with the indexer's semantics: hits
//! ordered by (path bytes, line), keyset `after`, `max_hits` with `truncated = Limit`, literal or
//! regex with smart case, include/exclude globs, generated files dropped unless asked. Blame,
//! diffs and commit graphs are seeded; `log` walks parents like `git rev-list include --not
//! exclude`. A configured failure makes every call fail (indexer down).

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};

use async_trait::async_trait;
use mage_core::domain::fix::Glob;
use mage_core::domain::{CommitSha, LineRange, OrgId, RepoId, RepoPath, RepoVersionKey};
use mage_core::wire::indexer::{
    BlameHunk, CaseMode, CommitInfo, FileDiff, LogQuery, PatternKind, TextHit, TextHits, TextQuery, TreeDiffOptions, Truncation,
};
use parking_lot::Mutex;

use crate::ports::indexer_link::{IndexerLink, IndexerLinkError};

/// Which method a recorded call hit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IndexerCall {
    Search,
    Blame,
    Diff,
    Log,
}

#[derive(Default)]
struct State {
    /// (org, version) → path → (text, generated/vendored/binary).
    files: HashMap<(OrgId, RepoVersionKey), BTreeMap<RepoPath, (String, bool)>>,
    blame: HashMap<(OrgId, RepoVersionKey, RepoPath), Vec<BlameHunk>>,
    diffs: HashMap<(OrgId, RepoId, CommitSha, CommitSha), Vec<FileDiff>>,
    commits: HashMap<(OrgId, RepoId), BTreeMap<CommitSha, CommitInfo>>,
    failure: Option<IndexerLinkError>,
    calls: Vec<IndexerCall>,
}

#[derive(Default)]
pub struct FakeIndexerLink {
    state: Mutex<State>,
}

impl FakeIndexerLink {
    pub fn new() -> Self {
        Self::default()
    }

    /// A file of the checkout at `key`; `generated` marks Generated/Vendored/Binary classes.
    pub fn put_file(&self, org: OrgId, key: RepoVersionKey, path: RepoPath, text: impl Into<String>, generated: bool) {
        self.state.lock().files.entry((org, key)).or_default().insert(path, (text.into(), generated));
    }

    pub fn put_blame(&self, org: OrgId, key: RepoVersionKey, path: RepoPath, hunks: Vec<BlameHunk>) {
        self.state.lock().blame.insert((org, key, path), hunks);
    }

    pub fn put_diff(&self, org: OrgId, repo: RepoId, from: CommitSha, to: CommitSha, files: Vec<FileDiff>) {
        self.state.lock().diffs.insert((org, repo, from, to), files);
    }

    /// Commits of the repo's graph (parents link them).
    pub fn put_commits(&self, org: OrgId, repo: RepoId, commits: Vec<CommitInfo>) {
        let mut state = self.state.lock();
        let graph = state.commits.entry((org, repo)).or_default();
        for commit in commits {
            graph.insert(commit.sha, commit);
        }
    }

    /// Every following call fails with `error` until [`Self::heal`].
    pub fn fail_with(&self, error: IndexerLinkError) {
        self.state.lock().failure = Some(error);
    }

    pub fn heal(&self) {
        self.state.lock().failure = None;
    }

    pub fn calls(&self) -> Vec<IndexerCall> {
        self.state.lock().calls.clone()
    }

    fn enter(&self, call: IndexerCall) -> Result<parking_lot::MutexGuard<'_, State>, IndexerLinkError> {
        let mut state = self.state.lock();
        state.calls.push(call);
        match &state.failure {
            Some(error) => Err(error.clone()),
            None => Ok(state),
        }
    }
}

enum Matcher {
    Literal { needle: String, insensitive: bool },
    Regex(regex::Regex),
}

impl Matcher {
    fn new(q: &TextQuery) -> Result<Self, IndexerLinkError> {
        let insensitive = match q.case {
            CaseMode::Sensitive => false,
            CaseMode::Insensitive => true,
            CaseMode::Smart => !q.pattern.chars().any(char::is_uppercase),
        };
        match q.kind {
            PatternKind::Literal => {
                Ok(Matcher::Literal { needle: if insensitive { q.pattern.to_lowercase() } else { q.pattern.clone() }, insensitive })
            }
            PatternKind::Regex => regex::RegexBuilder::new(&q.pattern)
                .case_insensitive(insensitive)
                .size_limit(1 << 20)
                .build()
                .map(Matcher::Regex)
                .map_err(|e| IndexerLinkError::Invalid(format!("bad regex: {e}"))),
        }
    }

    /// 1-based byte column of the first match.
    fn find(&self, line: &str) -> Option<u32> {
        let at = match self {
            Matcher::Literal { needle, insensitive: true } => line.to_lowercase().find(needle.as_str()),
            Matcher::Literal { needle, insensitive: false } => line.find(needle.as_str()),
            Matcher::Regex(re) => re.find(line).map(|m| m.start()),
        }?;
        u32::try_from(at + 1).ok()
    }
}

fn globs(patterns: &[String]) -> Result<Vec<Glob>, IndexerLinkError> {
    patterns.iter().map(|p| Glob::new(p.as_str()).map_err(|e| IndexerLinkError::Invalid(format!("bad glob {p:?}: {e}")))).collect()
}

fn cut(line: &str, max: u16) -> String {
    let max = usize::from(max);
    if line.len() <= max {
        return line.to_owned();
    }
    let mut end = max;
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    line[..end].to_owned()
}

#[async_trait]
impl IndexerLink for FakeIndexerLink {
    async fn search(&self, org: OrgId, key: &RepoVersionKey, q: &TextQuery, include_generated: bool) -> Result<TextHits, IndexerLinkError> {
        let state = self.enter(IndexerCall::Search)?;
        q.check_limits().map_err(IndexerLinkError::Invalid)?;
        let matcher = Matcher::new(q)?;
        let (include, exclude) = (globs(&q.include)?, globs(&q.exclude)?);
        let files = state.files.get(&(org, *key)).ok_or(IndexerLinkError::NotFound)?;
        let mut hits = Vec::new();
        let mut files_scanned = 0;
        for (path, (text, generated)) in files {
            if (*generated && !include_generated)
                || (!include.is_empty() && !include.iter().any(|g| g.matches(path.as_str())))
                || exclude.iter().any(|g| g.matches(path.as_str()))
                || text.len() as u64 > q.max_file_bytes
            {
                continue;
            }
            if q.after.as_ref().is_some_and(|(after_path, _)| path < after_path) {
                continue;
            }
            files_scanned += 1;
            let lines: Vec<&str> = text.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l)).collect();
            for (i, line) in lines.iter().enumerate() {
                let line_no = u32::try_from(i + 1).unwrap_or(u32::MAX);
                if q.after.as_ref().is_some_and(|(after_path, after_line)| path == after_path && line_no <= *after_line) {
                    continue;
                }
                let Some(col) = matcher.find(line) else { continue };
                if hits.len() == q.max_hits as usize {
                    let next = hits.last().map(|h: &TextHit| (h.path.clone(), h.line));
                    return Ok(TextHits { hits, next, truncated: Some(Truncation::Limit), files_scanned });
                }
                let context = usize::from(q.context);
                let before = lines[i.saturating_sub(context)..i].iter().map(|l| cut(l, q.max_line_len)).collect();
                let after = lines[i + 1..(i + 1 + context).min(lines.len())].iter().map(|l| cut(l, q.max_line_len)).collect();
                hits.push(TextHit { path: path.clone(), line: line_no, col, text: cut(line, q.max_line_len), before, after });
            }
        }
        Ok(TextHits { hits, next: None, truncated: None, files_scanned })
    }

    async fn blame(&self, org: OrgId, key: &RepoVersionKey, path: &RepoPath, lines: LineRange) -> Result<Vec<BlameHunk>, IndexerLinkError> {
        let state = self.enter(IndexerCall::Blame)?;
        let hunks = state.blame.get(&(org, *key, path.clone())).ok_or(IndexerLinkError::NotFound)?;
        Ok(hunks
            .iter()
            .filter(|h| h.start <= lines.end() && lines.start() < h.start + h.lines)
            .cloned()
            .collect())
    }

    async fn diff(&self, org: OrgId, repo: RepoId, from: CommitSha, to: CommitSha, opts: &TreeDiffOptions) -> Result<Vec<FileDiff>, IndexerLinkError> {
        let state = self.enter(IndexerCall::Diff)?;
        let files = state.diffs.get(&(org, repo, from, to)).ok_or(IndexerLinkError::NotFound)?;
        Ok(files
            .iter()
            .filter(|f| opts.paths.is_empty() || opts.paths.contains(&f.change.path))
            .map(|f| {
                let mut f = f.clone();
                if !opts.hunks {
                    f.hunks.clear();
                }
                f
            })
            .collect())
    }

    async fn log(&self, org: OrgId, repo: RepoId, q: &LogQuery) -> Result<Vec<CommitInfo>, IndexerLinkError> {
        let state = self.enter(IndexerCall::Log)?;
        let graph = state.commits.get(&(org, repo)).ok_or(IndexerLinkError::NotFound)?;
        let reach = |starts: &[CommitSha]| {
            let mut seen = BTreeSet::new();
            let mut queue: VecDeque<CommitSha> = starts.iter().copied().collect();
            while let Some(sha) = queue.pop_front() {
                if !seen.insert(sha) {
                    continue;
                }
                if let Some(commit) = graph.get(&sha) {
                    queue.extend(commit.parents.iter().copied());
                }
            }
            seen
        };
        for sha in q.include.iter().chain(&q.exclude) {
            if !graph.contains_key(sha) {
                return Err(IndexerLinkError::NotFound);
            }
        }
        let excluded = reach(&q.exclude);
        let mut commits: Vec<&CommitInfo> =
            reach(&q.include).into_iter().filter(|sha| !excluded.contains(sha)).filter_map(|sha| graph.get(&sha)).collect();
        commits.sort_by(|a, b| b.committer_time.cmp(&a.committer_time).then(b.sha.cmp(&a.sha)));
        Ok(commits
            .into_iter()
            .filter(|c| q.since.is_none_or(|since| c.committer_time >= since))
            .filter(|c| match &q.path_prefix {
                None => true,
                Some(prefix) => c.files.as_ref().is_some_and(|files| files.iter().any(|f| f.path.is_within(prefix.as_str()))),
            })
            .skip(q.skip as usize)
            .take(q.max as usize)
            .map(|c| {
                let mut c = c.clone();
                if !q.with_files {
                    c.files = None;
                }
                c
            })
            .collect())
    }
}
