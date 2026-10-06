//! `TextSearch` fake ([`MemTextSearch`]: in-memory files per checkout root, Rust `regex`) and the
//! port contract suite [`contract_text_search`] (knowledge.md "Testing": ordering, keyset
//! pagination equals one unpaged run, literal vs regex, smart case, binary skip, timeout partial
//! result), run against the fake and `adapters::driven::search::RipgrepSearch`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use globset::{Glob, GlobSet, GlobSetBuilder};
use mage_core::domain::{CommitSha, RepoPath};
use mage_core::wire::indexer::{CaseMode, PatternKind, TextHit, TextHits, TextQuery, Truncation};

use crate::ports::search::{Checkout, SearchError, TextSearch};

/// Files per checkout root, searched with the same order, keyset, limits and skips as the real
/// adapter.
#[derive(Default)]
pub struct MemTextSearch {
    checkouts: Mutex<HashMap<PathBuf, Vec<(RepoPath, Vec<u8>)>>>,
}

impl MemTextSearch {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers (or replaces) the files of the checkout at `root`.
    pub fn put_checkout(&self, root: impl Into<PathBuf>, files: Vec<(RepoPath, Vec<u8>)>) {
        let mut files = files;
        files.sort_by(|a, b| a.0.cmp(&b.0));
        files.dedup_by(|a, b| a.0 == b.0);
        self.lock().insert(root.into(), files);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<PathBuf, Vec<(RepoPath, Vec<u8>)>>> {
        self.checkouts.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

fn globs(list: &[String]) -> Result<Option<GlobSet>, SearchError> {
    if list.is_empty() {
        return Ok(None);
    }
    let mut set = GlobSetBuilder::new();
    for glob in list {
        set.add(Glob::new(glob).map_err(|e| SearchError::Invalid(format!("glob {glob:?}: {e}")))?);
    }
    set.build().map(Some).map_err(|e| SearchError::Invalid(e.to_string()))
}

fn cut(bytes: &[u8], max: u16) -> String {
    let mut text = String::from_utf8_lossy(bytes).into_owned();
    let mut end = usize::from(max).min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text
}

#[async_trait]
impl TextSearch for MemTextSearch {
    async fn search(&self, checkout: &Checkout, q: &TextQuery) -> Result<TextHits, SearchError> {
        let deadline = Instant::now() + q.timeout;
        q.check_limits().map_err(SearchError::Invalid)?;
        let source = match q.kind {
            PatternKind::Literal => regex::escape(&q.pattern),
            PatternKind::Regex => q.pattern.clone(),
        };
        let insensitive = match q.case {
            CaseMode::Sensitive => false,
            CaseMode::Insensitive => true,
            CaseMode::Smart => !q.pattern.chars().any(char::is_uppercase),
        };
        if q.kind == PatternKind::Regex && q.pattern.contains('\n') {
            return Err(SearchError::Invalid("pattern matches a line terminator".into()));
        }
        let re = regex::bytes::RegexBuilder::new(&source)
            .case_insensitive(insensitive)
            .size_limit(1 << 20)
            .build()
            .map_err(|e| SearchError::Invalid(format!("pattern: {e}")))?;
        let (include, exclude) = (globs(&q.include)?, globs(&q.exclude)?);
        let files = self.lock().get(&checkout.root).cloned().ok_or(SearchError::NotFound)?;
        let max_hits = q.max_hits as usize;
        let mut hits: Vec<TextHit> = Vec::new();
        let mut files_scanned = 0;
        let mut resume = q.after.clone();
        for (path, bytes) in files {
            if q.after.as_ref().is_some_and(|(after, _)| &path < after) {
                continue;
            }
            if include.as_ref().is_some_and(|s| !s.is_match(path.as_str())) || exclude.as_ref().is_some_and(|s| s.is_match(path.as_str())) {
                continue;
            }
            if Instant::now() >= deadline {
                return Ok(TextHits { hits, next: resume, truncated: Some(Truncation::Timeout), files_scanned });
            }
            if bytes.len() as u64 > q.max_file_bytes || bytes.contains(&0) {
                continue;
            }
            files_scanned += 1;
            let min_line = q.after.as_ref().filter(|(after, _)| after == &path).map_or(0, |(_, line)| *line);
            let body = bytes.strip_suffix(b"\n").unwrap_or(&bytes);
            let lines: Vec<&[u8]> =
                if bytes.is_empty() { Vec::new() } else { body.split(|&b| b == b'\n').map(|l| l.strip_suffix(b"\r").unwrap_or(l)).collect() };
            for (idx, line) in lines.iter().enumerate() {
                let line_no = idx as u32 + 1;
                if line_no <= min_line {
                    continue;
                }
                let Some(m) = re.find(line) else { continue };
                if hits.len() == max_hits {
                    let next = hits.last().map(|h| (h.path.clone(), h.line));
                    return Ok(TextHits { hits, next, truncated: Some(Truncation::Limit), files_scanned });
                }
                let n = usize::from(q.context);
                let before = lines[idx.saturating_sub(n)..idx].iter().map(|l| cut(l, q.max_line_len)).collect();
                let after = lines[idx + 1..(idx + 1 + n).min(lines.len())].iter().map(|l| cut(l, q.max_line_len)).collect();
                hits.push(TextHit { path: path.clone(), line: line_no, col: m.start() as u32 + 1, text: cut(line, q.max_line_len), before, after });
            }
            resume = Some((path.clone(), u32::MAX));
        }
        Ok(TextHits { hits, next: None, truncated: None, files_scanned })
    }
}

// ---------------------------------------------------------------------------------------------
// Contract suite
// ---------------------------------------------------------------------------------------------

/// Generic `TextSearch` contract. For every scenario the suite writes the files under a fresh
/// temp directory and calls `make(root, files)`: a disk-backed adapter searches `root`, the fake
/// registers `files` under `root`. Panics on the first violated expectation.
pub async fn contract_text_search<F, Fut>(make: F)
where
    F: Fn(&Path, &[(RepoPath, Vec<u8>)]) -> Fut,
    Fut: Future<Output = Arc<dyn TextSearch>>,
{
    let suite = Suite { make };
    suite.ordering_and_columns().await;
    suite.keyset_pages_equal_one_run().await;
    suite.literal_vs_regex().await;
    suite.case_modes().await;
    suite.skips_binary_and_large_files().await;
    suite.globs_context_and_line_cut().await;
    suite.timeout_returns_partial_result().await;
    suite.invalid_queries().await;
}

fn rp(s: &str) -> RepoPath {
    RepoPath::new(s).unwrap_or_else(|e| panic!("{s}: {e}"))
}

struct Suite<F> {
    make: F,
}

struct Scenario {
    search: Arc<dyn TextSearch>,
    checkout: Checkout,
    _dir: tempfile::TempDir,
}

impl Scenario {
    async fn run(&self, q: &TextQuery) -> TextHits {
        self.search.search(&self.checkout, q).await.unwrap_or_else(|e| panic!("search {q:?}: {e}"))
    }

    async fn positions(&self, q: &TextQuery) -> Vec<(String, u32)> {
        self.run(q).await.hits.into_iter().map(|h| (h.path.into_string(), h.line)).collect()
    }
}

fn query(pattern: &str, kind: PatternKind) -> TextQuery {
    let mut q = TextQuery::new(pattern, kind);
    q.timeout = Duration::from_secs(10);
    q
}

impl<F, Fut> Suite<F>
where
    F: Fn(&Path, &[(RepoPath, Vec<u8>)]) -> Fut,
    Fut: Future<Output = Arc<dyn TextSearch>>,
{
    async fn scenario(&self, files: &[(&str, &[u8])]) -> Scenario {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let mut owned = Vec::new();
        for (path, bytes) in files {
            let abs = dir.path().join(path);
            if let Some(parent) = abs.parent() {
                std::fs::create_dir_all(parent).unwrap_or_else(|e| panic!("mkdir: {e}"));
            }
            std::fs::write(&abs, bytes).unwrap_or_else(|e| panic!("write {path}: {e}"));
            owned.push((rp(path), bytes.to_vec()));
        }
        let search = (self.make)(dir.path(), &owned).await;
        Scenario { search, checkout: Checkout { root: dir.path().to_path_buf(), sha: CommitSha::from_bytes([9; 20]) }, _dir: dir }
    }

    async fn ordering_and_columns(&self) {
        let s = self
            .scenario(&[
                ("b.txt", b"needle\n"),
                ("a/x.txt", b"x\n  needle here\n"),
                ("a.txt", b"one needle\ntwo\nneedle needle\n"),
                ("a/b/c.txt", b"needle"),
                (".hidden/h.txt", b"needle\n"),
            ])
            .await;
        let hits = s.run(&query("needle", PatternKind::Literal)).await;
        let got: Vec<(&str, u32, u32)> = hits.hits.iter().map(|h| (h.path.as_str(), h.line, h.col)).collect();
        assert_eq!(
            got,
            [(".hidden/h.txt", 1, 1), ("a.txt", 1, 5), ("a.txt", 3, 1), ("a/b/c.txt", 1, 1), ("a/x.txt", 2, 3), ("b.txt", 1, 1)],
            "hits ordered by path bytes then line, one per line, first-match column"
        );
        assert_eq!(hits.hits[1].text, "one needle");
        assert_eq!((hits.next.clone(), hits.truncated, hits.files_scanned), (None, None, 5));
    }

    async fn keyset_pages_equal_one_run(&self) {
        let s = self
            .scenario(&[
                ("a.go", b"hit 1\nmiss\nhit 3\nhit 4\n"),
                ("b/c.go", b"hit 1\n"),
                ("b/d.go", b"nothing\n"),
                ("e.go", b"miss\nhit 2\nhit 3\n"),
            ])
            .await;
        let all = s.run(&query("hit", PatternKind::Literal)).await;
        assert_eq!(all.hits.len(), 6);
        assert_eq!((all.next.clone(), all.truncated), (None, None));
        for page_size in [1u32, 2, 3, 5, 6] {
            let mut q = query("hit", PatternKind::Literal);
            q.max_hits = page_size;
            let mut paged = Vec::new();
            let mut pages = 0;
            loop {
                let page = s.run(&q).await;
                pages += 1;
                assert!(page.hits.len() <= page_size as usize);
                paged.extend(page.hits.clone());
                match page.next {
                    Some(next) => {
                        assert_eq!(page.truncated, Some(Truncation::Limit), "page size {page_size}");
                        let last = page.hits.last().map(|h| (h.path.clone(), h.line));
                        assert_eq!(Some(next.clone()), last, "next is the last returned hit");
                        q.after = Some(next);
                    }
                    None => {
                        assert_eq!(page.truncated, None);
                        break;
                    }
                }
                assert!(pages <= 10, "pagination does not terminate");
            }
            assert_eq!(paged, all.hits, "page size {page_size}");
        }
        // Exactly max_hits hits left: no next page.
        let mut q = query("hit", PatternKind::Literal);
        q.max_hits = 6;
        let exact = s.run(&q).await;
        assert_eq!((exact.hits.len(), exact.next, exact.truncated), (6, None, None));
        // A cursor inside a file skips its earlier lines and every earlier path.
        let mut q = query("hit", PatternKind::Literal);
        q.after = Some((rp("a.go"), 3));
        assert_eq!(s.positions(&q).await, [("a.go".to_owned(), 4), ("b/c.go".to_owned(), 1), ("e.go".to_owned(), 2), ("e.go".to_owned(), 3)]);
        q.after = Some((rp("b/cz.go"), 0));
        assert_eq!(s.positions(&q).await, [("e.go".to_owned(), 2), ("e.go".to_owned(), 3)], "cursor path need not exist");
    }

    async fn literal_vs_regex(&self) {
        let s = self.scenario(&[("f.ts", b"a.c\nabc\nfoo(bar)\nfoo bar\n")]).await;
        let lines = |hits: Vec<(String, u32)>| hits.into_iter().map(|(_, l)| l).collect::<Vec<_>>();
        assert_eq!(lines(s.positions(&query("a.c", PatternKind::Literal)).await), [1]);
        assert_eq!(lines(s.positions(&query("a.c", PatternKind::Regex)).await), [1, 2]);
        assert_eq!(lines(s.positions(&query("foo(bar)", PatternKind::Literal)).await), [3]);
        assert_eq!(lines(s.positions(&query(r"foo\s+bar", PatternKind::Regex)).await), [4]);
        assert_eq!(lines(s.positions(&query(r"^a\w+$", PatternKind::Regex)).await), [2], "anchors are per line");
    }

    async fn case_modes(&self) {
        let s = self.scenario(&[("f.txt", b"needle\nNEEDLE\nNeedle\n")]).await;
        let run = |pattern: &str, case: CaseMode| {
            let mut q = query(pattern, PatternKind::Literal);
            q.case = case;
            q
        };
        let lines = |hits: Vec<(String, u32)>| hits.into_iter().map(|(_, l)| l).collect::<Vec<_>>();
        assert_eq!(lines(s.positions(&run("needle", CaseMode::Sensitive)).await), [1]);
        assert_eq!(lines(s.positions(&run("needle", CaseMode::Insensitive)).await), [1, 2, 3]);
        assert_eq!(lines(s.positions(&run("needle", CaseMode::Smart)).await), [1, 2, 3], "lowercase pattern: insensitive");
        assert_eq!(lines(s.positions(&run("Needle", CaseMode::Smart)).await), [3], "uppercase letter: sensitive");
    }

    async fn skips_binary_and_large_files(&self) {
        let s = self.scenario(&[("bin.dat", b"needle\0\x01\x02"), ("big.txt", &[b'n'; 300]), ("ok.txt", b"needle\nnnn\n")]).await;
        let mut q = query("n", PatternKind::Literal);
        q.max_file_bytes = 200;
        let hits = s.run(&q).await;
        let paths: Vec<&str> = hits.hits.iter().map(|h| h.path.as_str()).collect();
        assert_eq!(paths, ["ok.txt", "ok.txt"], "binary and oversized files are skipped");
        assert_eq!(hits.files_scanned, 1);
    }

    async fn globs_context_and_line_cut(&self) {
        let s = self
            .scenario(&[
                ("src/a.go", b"l1\nl2\nTARGET line here\nl4\nl5\n"),
                ("src/a_test.go", b"TARGET\n"),
                ("web/b.ts", "é TARGET ééé\n".as_bytes()),
            ])
            .await;
        let mut q = query("TARGET", PatternKind::Literal);
        q.include = vec!["*.go".into()];
        q.exclude = vec!["**/*_test.go".into()];
        q.context = 2;
        let hits = s.run(&q).await;
        assert_eq!(hits.hits.len(), 1);
        let hit = &hits.hits[0];
        assert_eq!((hit.path.as_str(), hit.line, hit.col), ("src/a.go", 3, 1));
        assert_eq!(hit.before, ["l1", "l2"]);
        assert_eq!(hit.after, ["l4", "l5"]);
        let mut q = query("TARGET", PatternKind::Literal);
        q.include = vec!["web/**".into()];
        q.max_line_len = 4;
        let hits = s.run(&q).await;
        assert_eq!(hits.hits.len(), 1);
        assert_eq!(hits.hits[0].col, 4, "byte column");
        assert_eq!(hits.hits[0].text, "é T", "cut to 4 bytes on a char boundary");
    }

    async fn timeout_returns_partial_result(&self) {
        let s = self.scenario(&[("a.txt", b"needle\n"), ("b.txt", b"needle\n")]).await;
        let mut q = query("needle", PatternKind::Literal);
        q.timeout = Duration::ZERO;
        let partial = s.run(&q).await;
        assert_eq!(partial.truncated, Some(Truncation::Timeout));
        assert_eq!((partial.hits.len(), partial.next.clone()), (0, None), "nothing scanned: no resume point");
        q.after = Some((rp("a.txt"), 1));
        let partial = s.run(&q).await;
        assert_eq!(partial.truncated, Some(Truncation::Timeout));
        assert_eq!(partial.next, Some((rp("a.txt"), 1)), "resume point is kept");
        q.timeout = Duration::from_secs(10);
        assert_eq!(s.positions(&q).await, [("b.txt".to_owned(), 1)]);
    }

    async fn invalid_queries(&self) {
        let s = self.scenario(&[("a.txt", b"x\n")]).await;
        let invalid = |q: TextQuery| {
            let s = &s;
            async move {
                let result = s.search.search(&s.checkout, &q).await;
                assert!(matches!(result, Err(SearchError::Invalid(_))), "{q:?} → {result:?}");
            }
        };
        invalid(query("(", PatternKind::Regex)).await;
        invalid(query("", PatternKind::Literal)).await;
        let mut q = query("x", PatternKind::Literal);
        q.include = vec!["a[".into()];
        invalid(q).await;
        let mut q = query("x", PatternKind::Literal);
        q.max_hits = 0;
        invalid(q).await;
        let missing = Checkout { root: s.checkout.root.join("missing"), sha: s.checkout.sha };
        assert_eq!(s.search.search(&missing, &query("x", PatternKind::Literal)).await, Err(SearchError::NotFound));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mem_text_search_passes_the_contract() {
        contract_text_search(|root, files| {
            let fake = MemTextSearch::new();
            fake.put_checkout(root, files.to_vec());
            async move { Arc::new(fake) as Arc<dyn TextSearch> }
        })
        .await;
    }
}
