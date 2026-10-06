//! Indexer internal API `/internal/v1` and the git/search DTOs it shares with the indexer ports
//! (`GitRepoReader`, `TextSearch`) and the server's `IndexerLink` (knowledge.md "Text search on
//! Cloud Run"). Every call is authenticated with an `mgi_` token ([`crate::wire::token`]).

use std::time::Duration;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::domain::budget::duration_ms;
use crate::domain::{CommitSha, LineRange, OrgId, RepoId, RepoPath, TreeOid};

/// Paths of the indexer router (`indexer.internal_listen`).
pub mod routes {
    pub const PREFIX: &str = "/internal/v1";
    pub const SEARCH: &str = "/internal/v1/search";
    pub const BLAME: &str = "/internal/v1/blame";
    pub const DIFF: &str = "/internal/v1/diff";
    pub const LOG: &str = "/internal/v1/log";
    pub const HEALTH: &str = "/internal/v1/health";

    /// Every route with its constant name.
    pub const ALL: &[(&str, &str)] = &[("SEARCH", SEARCH), ("BLAME", BLAME), ("DIFF", DIFF), ("LOG", LOG), ("HEALTH", HEALTH)];
}

// ---------------------------------------------------------------------------------------------
// Text search
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PatternKind {
    Literal,
    Regex,
}

/// `Smart`: case-insensitive unless the pattern has an uppercase letter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaseMode {
    Sensitive,
    Insensitive,
    Smart,
}

/// One search over a materialized checkout. Hits are ordered by `(path bytes, line)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextQuery {
    pub pattern: String,
    pub kind: PatternKind,
    pub case: CaseMode,
    /// Globs, at most [`TextQuery::MAX_GLOBS`].
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    /// Lines of context before and after each hit.
    pub context: u8,
    pub max_hits: u32,
    /// Preview truncation per line.
    pub max_line_len: u16,
    /// Keyset cursor: hits strictly after `(path, line)`.
    pub after: Option<(RepoPath, u32)>,
    /// Larger files are skipped.
    pub max_file_bytes: u64,
    #[serde(rename = "timeout_ms", with = "duration_ms")]
    pub timeout: Duration,
}

impl TextQuery {
    pub const MAX_PATTERN_BYTES: usize = 512;
    pub const MAX_GLOBS: usize = 8;
    pub const MAX_CONTEXT: u8 = 3;
    pub const MAX_HITS: u32 = 200;
    pub const DEFAULT_MAX_LINE_LEN: u16 = 240;
    pub const DEFAULT_MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
    pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

    /// Query with the documented defaults: case-sensitive, no globs, no context,
    /// [`Self::MAX_HITS`] hits, from the start.
    pub fn new(pattern: impl Into<String>, kind: PatternKind) -> Self {
        TextQuery {
            pattern: pattern.into(),
            kind,
            case: CaseMode::Sensitive,
            include: Vec::new(),
            exclude: Vec::new(),
            context: 0,
            max_hits: Self::MAX_HITS,
            max_line_len: Self::DEFAULT_MAX_LINE_LEN,
            after: None,
            max_file_bytes: Self::DEFAULT_MAX_FILE_BYTES,
            timeout: Self::DEFAULT_TIMEOUT,
        }
    }

    /// Bounds of knowledge.md `TextQuery` (the indexer answers 400 `invalid` otherwise). Regex
    /// syntax and glob syntax are checked by the search adapter.
    pub fn check_limits(&self) -> Result<(), String> {
        if self.pattern.is_empty() {
            return Err("pattern is empty".into());
        }
        if self.pattern.len() > Self::MAX_PATTERN_BYTES {
            return Err(format!("pattern is {} bytes (max {})", self.pattern.len(), Self::MAX_PATTERN_BYTES));
        }
        if self.include.len() > Self::MAX_GLOBS || self.exclude.len() > Self::MAX_GLOBS {
            return Err(format!("at most {} include and {} exclude globs", Self::MAX_GLOBS, Self::MAX_GLOBS));
        }
        if self.context > Self::MAX_CONTEXT {
            return Err(format!("context {} (max {})", self.context, Self::MAX_CONTEXT));
        }
        if self.max_hits == 0 || self.max_hits > Self::MAX_HITS {
            return Err(format!("max_hits {} (1..={})", self.max_hits, Self::MAX_HITS));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextHit {
    pub path: RepoPath,
    /// 1-based.
    pub line: u32,
    /// 1-based, in bytes.
    pub col: u32,
    pub text: String,
    pub before: Vec<String>,
    pub after: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Truncation {
    Limit,
    Timeout,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextHits {
    pub hits: Vec<TextHit>,
    /// Cursor for the next page (`TextQuery.after`).
    pub next: Option<(RepoPath, u32)>,
    pub truncated: Option<Truncation>,
    pub files_scanned: u32,
}

// ---------------------------------------------------------------------------------------------
// Git history
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitInfo {
    pub sha: CommitSha,
    pub tree: TreeOid,
    pub parents: Vec<CommitSha>,
    pub author_name: String,
    #[serde(with = "time::serde::rfc3339")]
    pub author_time: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub committer_time: OffsetDateTime,
    pub subject: String,
    /// Truncated to 4 KiB.
    pub body: String,
    /// First-parent diff when `LogQuery.with_files`.
    pub files: Option<Vec<FileChange>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
    Renamed,
    TypeChanged,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileChange {
    pub path: RepoPath,
    /// Set for `Renamed`.
    pub old_path: Option<RepoPath>,
    pub change: ChangeKind,
    /// `None` for binary files.
    pub adds: Option<u32>,
    pub dels: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TreeDiffOptions {
    /// Restrict to these paths (empty = all).
    pub paths: Vec<RepoPath>,
    /// Include hunks (else file list only).
    pub hunks: bool,
    pub context: u8,
    pub max_bytes_per_file: u32,
    pub renames: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hunk {
    pub old_start: u32,
    pub old_lines: u32,
    pub new_start: u32,
    pub new_lines: u32,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileDiff {
    pub change: FileChange,
    pub hunks: Vec<Hunk>,
    pub binary: bool,
    /// Hunks cut at `max_bytes_per_file`.
    pub truncated: bool,
}

/// `git rev-list include --not exclude`, optionally restricted to a path prefix.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogQuery {
    pub include: Vec<CommitSha>,
    pub exclude: Vec<CommitSha>,
    pub path_prefix: Option<RepoPath>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub since: Option<OffsetDateTime>,
    pub skip: u32,
    pub max: u32,
    pub with_files: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlameHunk {
    /// First line of the hunk at the blamed commit (1-based).
    pub start: u32,
    pub lines: u32,
    pub commit: CommitSha,
    pub author_name: String,
    #[serde(with = "time::serde::rfc3339")]
    pub author_time: OffsetDateTime,
    pub subject: String,
    pub orig_path: RepoPath,
    pub orig_start: u32,
}

// ---------------------------------------------------------------------------------------------
// Endpoint bodies
// ---------------------------------------------------------------------------------------------

/// Request caps enforced by the indexer router.
pub const MAX_BLAME_LINES: u32 = 200;
pub const MAX_DIFF_PATHS: usize = 20;
pub const MAX_DIFF_BYTES_PER_FILE: u32 = 8 * 1024;
pub const MAX_LOG_COMMITS: u32 = 200;

/// `POST /internal/v1/search`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchRequest {
    pub org_id: OrgId,
    pub repo_id: RepoId,
    pub sha: CommitSha,
    pub query: TextQuery,
    /// Keep hits in files classed Generated, Vendored or Binary.
    pub include_generated: bool,
}

/// `POST /internal/v1/blame`. Response [`BlameResponse`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlameRequest {
    pub org_id: OrgId,
    pub repo_id: RepoId,
    pub sha: CommitSha,
    pub path: RepoPath,
    pub start_line: u32,
    pub end_line: u32,
}

impl BlameRequest {
    pub fn new(org_id: OrgId, repo_id: RepoId, sha: CommitSha, path: RepoPath, lines: LineRange) -> Self {
        BlameRequest { org_id, repo_id, sha, path, start_line: lines.start(), end_line: lines.end() }
    }

    /// The requested range, at most [`MAX_BLAME_LINES`] lines.
    pub fn lines(&self) -> Result<LineRange, String> {
        let lines = LineRange::new(self.start_line, self.end_line).map_err(|e| e.to_string())?;
        if lines.len() > MAX_BLAME_LINES {
            return Err(format!("{} lines (max {MAX_BLAME_LINES})", lines.len()));
        }
        Ok(lines)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlameResponse {
    pub hunks: Vec<BlameHunk>,
}

/// `POST /internal/v1/diff`. Response [`DiffResponse`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffRequest {
    pub org_id: OrgId,
    pub repo_id: RepoId,
    pub from: CommitSha,
    pub to: CommitSha,
    pub opts: TreeDiffOptions,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffResponse {
    pub files: Vec<FileDiff>,
}

/// `POST /internal/v1/log`. Response [`LogResponse`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogRequest {
    pub org_id: OrgId,
    pub repo_id: RepoId,
    pub query: LogQuery,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogResponse {
    pub commits: Vec<CommitInfo>,
}

/// `GET /internal/v1/health`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthResponse {
    pub ok: bool,
    pub mirrors: u32,
    pub checkouts_bytes: u64,
}

/// Error `code`; the server's `IndexerLink` maps 400 → `Invalid`, 404 → `NotFound`,
/// 503 → `Unavailable`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IndexerErrorCode {
    Invalid,
    Unauthorized,
    /// Body `org_id`/`repo_id` differ from the token claims.
    Forbidden,
    NotFound,
    Internal,
    /// Search concurrency exhausted or the indexer is shutting down.
    Unavailable,
}

impl IndexerErrorCode {
    pub const fn http_status(self) -> u16 {
        match self {
            IndexerErrorCode::Invalid => 400,
            IndexerErrorCode::Unauthorized => 401,
            IndexerErrorCode::Forbidden => 403,
            IndexerErrorCode::NotFound => 404,
            IndexerErrorCode::Internal => 500,
            IndexerErrorCode::Unavailable => 503,
        }
    }
}

/// Error body of every `/internal/v1` endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexerError {
    pub code: IndexerErrorCode,
    pub message: String,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn path(s: &str) -> RepoPath {
        s.parse().unwrap_or_else(|e| panic!("{s}: {e:?}"))
    }

    #[test]
    fn search_request_wire_shape() {
        let mut query = TextQuery::new("Cannot read properties", PatternKind::Literal);
        query.case = CaseMode::Smart;
        query.include = vec!["src/**".into()];
        query.after = Some((path("src/a.ts"), 12));
        let request = SearchRequest {
            org_id: "org_01j9z3k4m5n6p7q8r9s0t1v2w3".parse().unwrap_or_else(|e| panic!("{e:?}")),
            repo_id: "repo_01j9z3k4m5n6p7q8r9s0t1v2z6".parse().unwrap_or_else(|e| panic!("{e:?}")),
            sha: "3f2a1c9e5b7d4f6a8c0e2b4d6f8a0c2e4b6d8f0a".parse().unwrap_or_else(|e| panic!("{e:?}")),
            query,
            include_generated: false,
        };
        let want = json!({
            "org_id": "org_01j9z3k4m5n6p7q8r9s0t1v2w3",
            "repo_id": "repo_01j9z3k4m5n6p7q8r9s0t1v2z6",
            "sha": "3f2a1c9e5b7d4f6a8c0e2b4d6f8a0c2e4b6d8f0a",
            "query": {
                "pattern": "Cannot read properties",
                "kind": "literal",
                "case": "smart",
                "include": ["src/**"],
                "exclude": [],
                "context": 0,
                "max_hits": 200,
                "max_line_len": 240,
                "after": ["src/a.ts", 12],
                "max_file_bytes": 2097152,
                "timeout_ms": 5000
            },
            "include_generated": false
        });
        assert_eq!(serde_json::to_value(&request).unwrap_or_else(|e| panic!("{e}")), want);
        let back: SearchRequest = serde_json::from_value(want).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(back, request);
    }

    #[test]
    fn hits_and_history_round_trip() {
        let hits = json!({
            "hits": [{ "path": "src/a.ts", "line": 3, "col": 7, "text": "throw new Error(x)", "before": ["// a"], "after": [] }],
            "next": ["src/a.ts", 3],
            "truncated": "limit",
            "files_scanned": 41
        });
        let parsed: TextHits = serde_json::from_value(hits.clone()).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(serde_json::to_value(&parsed).unwrap_or_else(|e| panic!("{e}")), hits);

        let log = json!({
            "commits": [{
                "sha": "3f2a1c9e5b7d4f6a8c0e2b4d6f8a0c2e4b6d8f0a",
                "tree": "aa2a1c9e5b7d4f6a8c0e2b4d6f8a0c2e4b6d8f0a",
                "parents": [],
                "author_name": "Ada",
                "author_time": "2026-10-01T09:00:00Z",
                "committer_time": "2026-10-01T09:05:00Z",
                "subject": "fix: guard billing",
                "body": "",
                "files": [{ "path": "src/b.ts", "old_path": "src/a.ts", "change": "renamed", "adds": 1, "dels": 0 }]
            }]
        });
        let parsed: LogResponse = serde_json::from_value(log.clone()).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(serde_json::to_value(&parsed).unwrap_or_else(|e| panic!("{e}")), log);

        let diff = json!({
            "files": [{
                "change": { "path": "src/b.ts", "old_path": null, "change": "type_changed", "adds": null, "dels": null },
                "hunks": [{ "old_start": 1, "old_lines": 2, "new_start": 1, "new_lines": 3, "text": "@@ -1,2 +1,3 @@\n" }],
                "binary": false,
                "truncated": true
            }]
        });
        let parsed: DiffResponse = serde_json::from_value(diff.clone()).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(serde_json::to_value(&parsed).unwrap_or_else(|e| panic!("{e}")), diff);

        let blame = json!({
            "hunks": [{
                "start": 10, "lines": 4, "commit": "3f2a1c9e5b7d4f6a8c0e2b4d6f8a0c2e4b6d8f0a", "author_name": "Ada",
                "author_time": "2026-10-01T09:00:00Z", "subject": "feat: export", "orig_path": "src/a.ts", "orig_start": 8
            }]
        });
        let parsed: BlameResponse = serde_json::from_value(blame.clone()).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(serde_json::to_value(&parsed).unwrap_or_else(|e| panic!("{e}")), blame);

        let query = json!({
            "org_id": "org_01j9z3k4m5n6p7q8r9s0t1v2w3", "repo_id": "repo_01j9z3k4m5n6p7q8r9s0t1v2z6",
            "query": { "include": ["3f2a1c9e5b7d4f6a8c0e2b4d6f8a0c2e4b6d8f0a"], "exclude": [], "path_prefix": "src", "since": null,
                       "skip": 0, "max": 50, "with_files": true }
        });
        let parsed: LogRequest = serde_json::from_value(query.clone()).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(serde_json::to_value(&parsed).unwrap_or_else(|e| panic!("{e}")), query);
    }

    #[test]
    fn repo_paths_are_validated_on_the_way_in() {
        let hit = json!({ "path": "../etc/passwd", "line": 1, "col": 1, "text": "", "before": [], "after": [] });
        assert!(serde_json::from_value::<TextHit>(hit).is_err());
    }

    #[test]
    fn text_query_limits() {
        let ok = TextQuery::new("x", PatternKind::Regex);
        assert_eq!(ok.check_limits(), Ok(()));
        let long = TextQuery::new("a".repeat(TextQuery::MAX_PATTERN_BYTES + 1), PatternKind::Literal);
        assert!(long.check_limits().is_err());
        let exact = TextQuery::new("a".repeat(TextQuery::MAX_PATTERN_BYTES), PatternKind::Literal);
        assert_eq!(exact.check_limits(), Ok(()));
        assert!(TextQuery::new("", PatternKind::Literal).check_limits().is_err());
        let mut q = TextQuery::new("x", PatternKind::Literal);
        q.context = TextQuery::MAX_CONTEXT + 1;
        assert!(q.check_limits().is_err());
        let mut q = TextQuery::new("x", PatternKind::Literal);
        q.include = vec!["*".into(); TextQuery::MAX_GLOBS + 1];
        assert!(q.check_limits().is_err());
        let mut q = TextQuery::new("x", PatternKind::Literal);
        q.max_hits = 0;
        assert!(q.check_limits().is_err());
    }

    #[test]
    fn blame_range_is_capped_at_200_lines() {
        let org = "org_01j9z3k4m5n6p7q8r9s0t1v2w3".parse().unwrap_or_else(|e| panic!("{e:?}"));
        let repo = "repo_01j9z3k4m5n6p7q8r9s0t1v2z6".parse().unwrap_or_else(|e| panic!("{e:?}"));
        let sha = "3f2a1c9e5b7d4f6a8c0e2b4d6f8a0c2e4b6d8f0a".parse().unwrap_or_else(|e| panic!("{e:?}"));
        let at = |start, end| BlameRequest { org_id: org, repo_id: repo, sha, path: path("src/a.ts"), start_line: start, end_line: end };
        assert!(at(1, 200).lines().is_ok());
        assert!(at(1, 201).lines().is_err());
        assert!(at(5, 4).lines().is_err());
        assert!(at(0, 4).lines().is_err());
        let range = LineRange::new(10, 12).unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(BlameRequest::new(org, repo, sha, path("src/a.ts"), range).lines(), Ok(range));
    }
}
