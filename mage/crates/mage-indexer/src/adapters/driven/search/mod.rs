//! `TextSearch` over a materialized checkout with the ripgrep libraries (knowledge.md
//! "`mage_indexer::adapters::driven::search`"): `grep-regex` matcher (literal via
//! `fixed_strings`, smart/insensitive case, 1 MiB regex size limits), `grep-searcher` line search
//! with `BinaryDetection::quit(b'\0')`, `ignore::WalkBuilder` with the standard filters off, hidden
//! files on and symlinks not followed, `globset` include/exclude.
//!
//! Files are collected first and sorted by path bytes (a directory walk sorted per directory puts
//! `a/b` before `a.txt`, which would break the keyset order). The search runs on a blocking
//! thread; a deadline checked per file and per match yields a partial `Timeout` result.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use async_trait::async_trait;
use globset::{Glob, GlobSet, GlobSetBuilder};
use grep_matcher::Matcher;
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder, Sink, SinkMatch};
use ignore::WalkBuilder;
use mage_core::domain::RepoPath;
use mage_core::wire::indexer::{CaseMode, PatternKind, TextHit, TextHits, TextQuery, Truncation};

use crate::ports::search::{Checkout, SearchError, TextSearch};

/// `size_limit` / `dfa_size_limit` of compiled user regexes.
const REGEX_SIZE_LIMIT: usize = 1 << 20;

/// The ripgrep-library `TextSearch` adapter. Stateless; cheap to share.
#[derive(Debug, Clone, Copy, Default)]
pub struct RipgrepSearch;

impl RipgrepSearch {
    pub fn new() -> Self {
        RipgrepSearch
    }
}

#[async_trait]
impl TextSearch for RipgrepSearch {
    async fn search(&self, checkout: &Checkout, q: &TextQuery) -> Result<TextHits, SearchError> {
        let root = checkout.root.clone();
        let q = q.clone();
        let deadline = Instant::now() + q.timeout;
        tokio::task::spawn_blocking(move || search_blocking(&root, &q, deadline))
            .await
            .map_err(|e| SearchError::Unavailable(format!("search task: {e}")))?
    }
}

fn build_matcher(q: &TextQuery) -> Result<RegexMatcher, SearchError> {
    let mut builder = RegexMatcherBuilder::new();
    builder
        .fixed_strings(q.kind == PatternKind::Literal)
        .case_insensitive(q.case == CaseMode::Insensitive)
        .case_smart(q.case == CaseMode::Smart)
        .size_limit(REGEX_SIZE_LIMIT)
        .dfa_size_limit(REGEX_SIZE_LIMIT)
        .line_terminator(Some(b'\n'));
    builder.build(&q.pattern).map_err(|e| SearchError::Invalid(format!("pattern: {e}")))
}

fn build_globs(globs: &[String]) -> Result<Option<GlobSet>, SearchError> {
    if globs.is_empty() {
        return Ok(None);
    }
    let mut set = GlobSetBuilder::new();
    for glob in globs {
        set.add(Glob::new(glob).map_err(|e| SearchError::Invalid(format!("glob {glob:?}: {e}")))?);
    }
    set.build().map(Some).map_err(|e| SearchError::Invalid(format!("globs: {e}")))
}

/// Regular files under `root` as `(repo path, absolute path)`, filtered by the globs, sorted by
/// repo path bytes.
fn collect_files(root: &Path, include: Option<&GlobSet>, exclude: Option<&GlobSet>) -> Vec<(RepoPath, PathBuf)> {
    let mut files = Vec::new();
    let walk = WalkBuilder::new(root).standard_filters(false).hidden(false).follow_links(false).build();
    for entry in walk.flatten() {
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let Ok(rel) = entry.path().strip_prefix(root) else { continue };
        let Some(rel) = rel.to_str() else { continue };
        let rel = if std::path::MAIN_SEPARATOR == '/' { rel.to_owned() } else { rel.replace(std::path::MAIN_SEPARATOR, "/") };
        let Ok(path) = RepoPath::new(rel) else { continue };
        if include.is_some_and(|set| !set.is_match(path.as_str())) || exclude.is_some_and(|set| set.is_match(path.as_str())) {
            continue;
        }
        files.push((path, entry.into_path()));
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    files
}

fn search_blocking(root: &Path, q: &TextQuery, deadline: Instant) -> Result<TextHits, SearchError> {
    q.check_limits().map_err(SearchError::Invalid)?;
    let matcher = build_matcher(q)?;
    let include = build_globs(&q.include)?;
    let exclude = build_globs(&q.exclude)?;
    if !root.is_dir() {
        return Err(SearchError::NotFound);
    }
    let files = collect_files(root, include.as_ref(), exclude.as_ref());
    let start = match &q.after {
        Some((path, _)) => files.partition_point(|(p, _)| p < path),
        None => 0,
    };
    let max_hits = q.max_hits as usize;
    let mut searcher = SearcherBuilder::new().line_number(true).binary_detection(BinaryDetection::quit(b'\0')).build();
    let mut hits: Vec<TextHit> = Vec::new();
    let mut files_scanned = 0u32;
    // Everything up to here has been returned: the resume point of a timed-out search.
    let mut resume = q.after.clone();

    for (path, abs) in &files[start..] {
        if Instant::now() >= deadline {
            return Ok(TextHits { hits, next: resume, truncated: Some(Truncation::Timeout), files_scanned });
        }
        let Ok(meta) = fs::symlink_metadata(abs) else { continue };
        if !meta.is_file() || meta.len() > q.max_file_bytes {
            continue;
        }
        let Ok(bytes) = fs::read(abs) else { continue };
        if bytes.contains(&0) {
            continue;
        }
        files_scanned += 1;
        let min_line = match &q.after {
            Some((after_path, line)) if after_path == path => *line,
            _ => 0,
        };
        // One extra match tells "more exist" apart from "exactly max_hits".
        let mut sink = LineSink { matcher: &matcher, min_line, want: max_hits + 1 - hits.len(), deadline, found: Vec::new(), timed_out: false };
        searcher.search_slice(&matcher, &bytes, &mut sink).map_err(|e| SearchError::Unavailable(format!("{path}: {e}")))?;
        let LineSink { found, timed_out, .. } = sink;
        let lines = if q.context > 0 && !found.is_empty() { Some(split_lines(&bytes)) } else { None };
        for m in found {
            if hits.len() == max_hits {
                let next = hits.last().map(|h| (h.path.clone(), h.line));
                return Ok(TextHits { hits, next, truncated: Some(Truncation::Limit), files_scanned });
            }
            let (before, after) = match &lines {
                Some(lines) => context(lines, m.line, q.context, q.max_line_len),
                None => (Vec::new(), Vec::new()),
            };
            hits.push(TextHit { path: path.clone(), line: m.line, col: m.col, text: cut(&m.text, q.max_line_len), before, after });
            resume = Some((path.clone(), m.line));
        }
        if timed_out {
            return Ok(TextHits { hits, next: resume, truncated: Some(Truncation::Timeout), files_scanned });
        }
        resume = Some((path.clone(), u32::MAX));
    }
    Ok(TextHits { hits, next: None, truncated: None, files_scanned })
}

struct Found {
    line: u32,
    col: u32,
    text: Vec<u8>,
}

/// Collects matching lines after `min_line`, at most `want`, stopping at the deadline.
struct LineSink<'m> {
    matcher: &'m RegexMatcher,
    min_line: u32,
    want: usize,
    deadline: Instant,
    found: Vec<Found>,
    timed_out: bool,
}

impl Sink for LineSink<'_> {
    type Error = std::io::Error;

    fn matched(&mut self, _searcher: &Searcher, mat: &SinkMatch<'_>) -> Result<bool, Self::Error> {
        if Instant::now() >= self.deadline {
            self.timed_out = true;
            return Ok(false);
        }
        let line = mat.line_number().and_then(|n| u32::try_from(n).ok()).unwrap_or(0);
        if line <= self.min_line {
            return Ok(true);
        }
        let text = strip_terminator(mat.bytes());
        let col = match self.matcher.find(text) {
            Ok(Some(m)) => u32::try_from(m.start()).unwrap_or(u32::MAX - 1) + 1,
            _ => 1,
        };
        self.found.push(Found { line, col, text: text.to_vec() });
        Ok(self.found.len() < self.want)
    }
}

fn strip_terminator(line: &[u8]) -> &[u8] {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    line.strip_suffix(b"\r").unwrap_or(line)
}

/// Lines of a file without terminators; index 0 = line 1.
fn split_lines(bytes: &[u8]) -> Vec<&[u8]> {
    let body = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    if body.is_empty() && bytes.is_empty() {
        return Vec::new();
    }
    body.split(|&b| b == b'\n').map(|l| l.strip_suffix(b"\r").unwrap_or(l)).collect()
}

fn context(lines: &[&[u8]], line: u32, n: u8, max_len: u16) -> (Vec<String>, Vec<String>) {
    let idx = line as usize - 1;
    let n = usize::from(n);
    let before = lines[idx.saturating_sub(n)..idx.min(lines.len())].iter().map(|l| cut(l, max_len)).collect();
    let after_end = (idx + 1 + n).min(lines.len());
    let after = lines.get(idx + 1..after_end).unwrap_or(&[]).iter().map(|l| cut(l, max_len)).collect();
    (before, after)
}

/// Lossy UTF-8, cut to at most `max` bytes on a char boundary.
fn cut(bytes: &[u8], max: u16) -> String {
    let mut text = String::from_utf8_lossy(bytes).into_owned();
    let max = usize::from(max);
    if text.len() > max {
        let mut end = max;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::testing::contract_text_search;

    #[tokio::test]
    async fn ripgrep_search_passes_the_text_search_contract() {
        // The suite writes the files under `root`; the adapter searches the disk.
        contract_text_search(|_root, _files| async { std::sync::Arc::new(RipgrepSearch::new()) as std::sync::Arc<dyn TextSearch> }).await;
    }

    fn checkout(dir: &tempfile::TempDir) -> Checkout {
        Checkout { root: dir.path().to_path_buf(), sha: mage_core::domain::CommitSha::from_bytes([7; 20]) }
    }

    #[tokio::test]
    async fn symlinks_are_never_followed() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let outside = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        fs::write(outside.path().join("secret.txt"), "needle outside\n").unwrap_or_else(|e| panic!("{e}"));
        fs::write(dir.path().join("a.txt"), "needle inside\n").unwrap_or_else(|e| panic!("{e}"));
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(outside.path().join("secret.txt"), dir.path().join("link.txt")).unwrap_or_else(|e| panic!("{e}"));
            std::os::unix::fs::symlink(outside.path(), dir.path().join("linkdir")).unwrap_or_else(|e| panic!("{e}"));
        }
        let hits = RipgrepSearch::new().search(&checkout(&dir), &TextQuery::new("needle", PatternKind::Literal)).await.unwrap_or_else(|e| panic!("{e}"));
        let paths: Vec<&str> = hits.hits.iter().map(|h| h.path.as_str()).collect();
        assert_eq!(paths, ["a.txt"]);
    }

    #[tokio::test]
    async fn oversized_files_and_regex_newlines() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        fs::write(dir.path().join("big.txt"), format!("needle\n{}", "x".repeat(4096))).unwrap_or_else(|e| panic!("{e}"));
        fs::write(dir.path().join("small.txt"), "needle\n").unwrap_or_else(|e| panic!("{e}"));
        let mut q = TextQuery::new("needle", PatternKind::Literal);
        q.max_file_bytes = 1024;
        let hits = RipgrepSearch::new().search(&checkout(&dir), &q).await.unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(hits.hits.iter().map(|h| h.path.as_str()).collect::<Vec<_>>(), ["small.txt"]);
        assert_eq!(hits.files_scanned, 1);
        // A pattern that can only match across lines is refused, not silently empty.
        let err = RipgrepSearch::new().search(&checkout(&dir), &TextQuery::new("a\nb", PatternKind::Regex)).await;
        assert!(matches!(err, Err(SearchError::Invalid(_))), "{err:?}");
    }

    #[tokio::test]
    async fn missing_checkout_is_not_found() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let gone = Checkout { root: dir.path().join("absent"), sha: mage_core::domain::CommitSha::from_bytes([1; 20]) };
        let mut q = TextQuery::new("x", PatternKind::Literal);
        q.timeout = Duration::from_secs(1);
        assert_eq!(RipgrepSearch::new().search(&gone, &q).await, Err(SearchError::NotFound));
    }
}
