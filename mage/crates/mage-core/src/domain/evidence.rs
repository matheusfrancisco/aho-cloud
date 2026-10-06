//! Evidence gate (triage.md "Evidence gate algorithm"; elaborates domain.md's `evidence::check`).
//!
//! Pure and deterministic: the caller loads the manifest entries and full-file texts of the cited
//! paths into a [`ManifestView`]; [`check`] verifies every citation of one cause against it and the
//! resulting [`EvidenceReport`] is the only way to mint the [`EvidenceChecked`] proof token.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::domain::citation::Citation;
use crate::domain::error::DomainError;
use crate::domain::ids::{BlobOid, RepoVersionKey};
use crate::domain::manifest::{EntryKind, ManifestEntry};
use crate::domain::path::{LineRange, RepoPath};
use crate::domain::triage::CitationDraft;

/// Maximum lines per citation (`end - start + 1`).
pub const MAX_CITATION_SPAN: u32 = 200;
/// Minimum non-whitespace characters of a normalized quote.
pub const MIN_QUOTE_CHARS: usize = 10;
/// Lines around the cited range searched for a misplaced quote.
pub const QUOTE_SEARCH_WINDOW: u32 = 20;

/// Manifest entries and full-file texts of the cited paths at one commit.
///
/// `texts` holds the lines of a blob exactly as returned by `SourceReader` (no terminators). A text
/// entry without loaded text (e.g. `SourceReader` said `TooLarge`) is treated as not stored.
#[derive(Debug, Clone)]
pub struct ManifestView {
    key: RepoVersionKey,
    entries: BTreeMap<RepoPath, ManifestEntry>,
    texts: BTreeMap<BlobOid, Arc<[String]>>,
}

impl ManifestView {
    pub fn new(key: RepoVersionKey, entries: Vec<ManifestEntry>, texts: Vec<(BlobOid, Vec<String>)>) -> Self {
        let entries = entries.into_iter().map(|e| (e.path.clone(), e)).collect();
        let texts = texts.into_iter().map(|(oid, lines)| (oid, Arc::from(lines))).collect();
        Self { key, entries, texts }
    }

    pub fn key(&self) -> RepoVersionKey {
        self.key
    }
}

/// Why a citation was dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, thiserror::Error)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CitationDefect {
    #[error("invalid path")]
    BadPath,
    #[error("path not found")]
    PathNotFound,
    #[error("not a regular file")]
    NotAFile,
    #[error("not a stored text file")]
    NotText,
    #[error("invalid line range")]
    BadRange,
    #[error("lines beyond end of file ({line_count} lines)")]
    LineOutOfRange { line_count: u32 },
    #[error("more than {MAX_CITATION_SPAN} lines cited")]
    SpanTooLarge,
    #[error("quote shorter than {MIN_QUOTE_CHARS} non-whitespace characters")]
    QuoteTooShort,
    #[error("quote contains a read_file truncation marker")]
    QuoteTruncated,
    #[error("quote not found in or near these lines")]
    QuoteMismatch,
    #[error("quote matches several places")]
    QuoteAmbiguous,
}

/// How a citation's quote was verified.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum QuoteCheck {
    /// No quote was given.
    Absent,
    /// The quote occurs within the cited lines.
    Exact,
    /// The quote was found elsewhere; the citation now covers the matched lines.
    Adjusted { from: LineRange },
}

impl QuoteCheck {
    /// `Exact` or `Adjusted`.
    pub const fn is_verified(&self) -> bool {
        matches!(self, Self::Exact | Self::Adjusted { .. })
    }
}

/// Outcome for one submitted citation, in submission order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum CitationCheck {
    /// Passed; `lines` is the final (possibly adjusted) range.
    Ok {
        lines: LineRange,
        quote: QuoteCheck,
    },
    Rejected(CitationDefect),
}

/// Verdict for one cause.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CauseEvidence {
    Valid,
    NoValidCitation,
    NoVerifiedQuote,
}

/// Result of [`check`] for one cause.
///
/// Fields are private so a report cannot be edited into a passing one; it is `Serialize` only,
/// because deserializing one would forge the input of [`EvidenceReport::into_checked`].
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EvidenceReport {
    key: RepoVersionKey,
    items: Vec<CitationCheck>,
    cause: CauseEvidence,
    citations: Vec<Citation>,
    sources: Vec<usize>,
}

impl EvidenceReport {
    pub fn key(&self) -> RepoVersionKey {
        self.key
    }

    /// One entry per checked citation, in input order.
    pub fn items(&self) -> &[CitationCheck] {
        &self.items
    }

    pub fn cause(&self) -> CauseEvidence {
        self.cause
    }

    /// Passing citations after dedupe, with final line ranges and snippet hashes.
    pub fn citations(&self) -> &[Citation] {
        &self.citations
    }

    /// For each entry of [`Self::citations`], the index of the input citation it came from.
    pub fn sources(&self) -> &[usize] {
        &self.sources
    }

    /// Only way to obtain `EvidenceChecked`; the token owns the resulting citations.
    pub fn into_checked(self) -> Result<EvidenceChecked, DomainError> {
        match self.cause {
            CauseEvidence::Valid => Ok(EvidenceChecked { key: self.key, citations: self.citations }),
            CauseEvidence::NoValidCitation | CauseEvidence::NoVerifiedQuote => Err(DomainError::MissingEvidence),
        }
    }
}

/// Proof that a cause's citations passed the evidence gate at `key`. Consumed by `Cause::new`.
#[derive(Debug, Clone)]
pub struct EvidenceChecked {
    key: RepoVersionKey,
    citations: Vec<Citation>,
}

impl EvidenceChecked {
    pub fn key(&self) -> RepoVersionKey {
        self.key
    }

    pub fn citations(&self) -> &[Citation] {
        &self.citations
    }

    pub fn into_citations(self) -> Vec<Citation> {
        self.citations
    }
}

/// Gate step 1 on the raw agent path: strip one leading `./` or `/`, then reject empty paths,
/// `\`, NUL, `.`/`..` segments, `//` and a trailing `/`. Case-sensitive.
pub fn normalize_cited_path(raw: &str) -> Result<RepoPath, CitationDefect> {
    let path = raw.strip_prefix("./").or_else(|| raw.strip_prefix('/')).unwrap_or(raw);
    if path.is_empty()
        || path.contains(['\\', '\0'])
        || path.contains("//")
        || path.ends_with('/')
        || path.split('/').any(|s| s == "." || s == "..")
    {
        return Err(CitationDefect::BadPath);
    }
    RepoPath::new(path).map_err(|_| CitationDefect::BadPath)
}

/// Raw agent `start_line`/`end_line` → range; `start == 0` or `end < start` is `BadRange`.
pub fn cited_range(start_line: u32, end_line: u32) -> Result<LineRange, CitationDefect> {
    LineRange::new(start_line, end_line).map_err(|_| CitationDefect::BadRange)
}

/// SHA-256 over `lines` (no terminators) joined with `\n`.
pub fn snippet_hash<S: AsRef<str>>(lines: &[S]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            hasher.update(b"\n");
        }
        hasher.update(line.as_ref().as_bytes());
    }
    hasher.finalize().into()
}

/// Checks the citations of one cause (gate steps 2-6 and the cause verdict).
pub fn check(citations: &[CitationDraft], view: &ManifestView) -> EvidenceReport {
    let mut normalized: BTreeMap<BlobOid, NormalizedText> = BTreeMap::new();
    let mut items = Vec::with_capacity(citations.len());
    let mut kept: Vec<Citation> = Vec::with_capacity(citations.len());
    let mut sources: Vec<usize> = Vec::with_capacity(citations.len());
    let mut verified: Vec<bool> = Vec::with_capacity(citations.len());

    for (index, draft) in citations.iter().enumerate() {
        let passed = match check_one(draft, view, &mut normalized) {
            Ok(passed) => passed,
            Err(defect) => {
                items.push(CitationCheck::Rejected(defect));
                continue;
            }
        };
        items.push(CitationCheck::Ok { lines: passed.lines, quote: passed.quote });
        let is_verified = passed.quote.is_verified();
        match kept.iter().position(|c| c.path == draft.path && c.lines == passed.lines) {
            // A later verified duplicate replaces an unverified one in place.
            Some(at) => {
                if is_verified && !verified[at] {
                    kept[at].note.clone_from(&draft.note);
                    sources[at] = index;
                    verified[at] = true;
                }
            }
            None => {
                kept.push(Citation {
                    repo_id: view.key.repo_id,
                    sha: view.key.sha,
                    path: draft.path.clone(),
                    lines: passed.lines,
                    snippet_hash: Some(passed.snippet_hash),
                    note: draft.note.clone(),
                });
                sources.push(index);
                verified.push(is_verified);
            }
        }
    }

    let cause = if kept.is_empty() {
        CauseEvidence::NoValidCitation
    } else if !verified.contains(&true) {
        CauseEvidence::NoVerifiedQuote
    } else {
        CauseEvidence::Valid
    };
    EvidenceReport { key: view.key, items, cause, citations: kept, sources }
}

struct Passed {
    lines: LineRange,
    quote: QuoteCheck,
    snippet_hash: [u8; 32],
}

fn check_one(
    draft: &CitationDraft,
    view: &ManifestView,
    normalized: &mut BTreeMap<BlobOid, NormalizedText>,
) -> Result<Passed, CitationDefect> {
    let entry = view.entries.get(&draft.path).ok_or(CitationDefect::PathNotFound)?;
    if entry.kind != EntryKind::File {
        return Err(CitationDefect::NotAFile);
    }
    let text = match view.texts.get(&entry.blob) {
        Some(text) if entry.is_text && entry.line_count > 0 && !text.is_empty() => text,
        _ => return Err(CitationDefect::NotText),
    };
    // The loaded text is what `read_file` showed the agent, so its length is authoritative.
    let line_count = u32::try_from(text.len()).unwrap_or(u32::MAX);
    check_range(draft.lines, line_count)?;

    let (lines, quote) = match draft.quote.as_deref() {
        None => (draft.lines, QuoteCheck::Absent),
        Some(raw) => {
            let quote = normalize_quote(raw)?;
            let file = normalized.entry(entry.blob).or_insert_with(|| NormalizedText::build(text));
            match locate(&quote, file, draft.lines)? {
                Located::Exact => (draft.lines, QuoteCheck::Exact),
                Located::Moved(span) => {
                    check_range(span, line_count)?;
                    (span, QuoteCheck::Adjusted { from: draft.lines })
                }
            }
        }
    };
    let cited = &text[lines.start() as usize - 1..lines.end() as usize];
    Ok(Passed { lines, quote, snippet_hash: snippet_hash(cited) })
}

fn check_range(lines: LineRange, line_count: u32) -> Result<(), CitationDefect> {
    if lines.end() > line_count {
        return Err(CitationDefect::LineOutOfRange { line_count });
    }
    if lines.len() > MAX_CITATION_SPAN {
        return Err(CitationDefect::SpanTooLarge);
    }
    Ok(())
}

/// `read_file`'s long-line cut marker: `…[+N chars]`.
fn contains_cut_marker(s: &str) -> bool {
    const HEAD: &str = "…[+";
    const TAIL: &str = " chars]";
    s.match_indices(HEAD).any(|(i, _)| {
        let rest = &s[i + HEAD.len()..];
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        digits > 0 && rest[digits..].starts_with(TAIL)
    })
}

/// Strips `read_file`'s line prefix `^ *\d+\t`.
fn strip_line_prefix(line: &str) -> Option<&str> {
    let rest = line.trim_start_matches(' ');
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return None;
    }
    rest[digits..].strip_prefix('\t')
}

/// Appends `line` with whitespace runs collapsed and trimmed, preceded by `\n` unless `out` is
/// empty. Returns false (appending nothing) for a blank line.
fn push_collapsed(out: &mut String, line: &str) -> bool {
    let mut words = line.split_whitespace();
    let Some(first) = words.next() else {
        return false;
    };
    if !out.is_empty() {
        out.push('\n');
    }
    out.push_str(first);
    for word in words {
        out.push(' ');
        out.push_str(word);
    }
    true
}

fn normalize_quote(raw: &str) -> Result<String, CitationDefect> {
    if contains_cut_marker(raw) {
        return Err(CitationDefect::QuoteTruncated);
    }
    let mut non_blank = raw.split(['\r', '\n']).filter(|l| !l.trim().is_empty()).peekable();
    let strip = non_blank.peek().is_some() && non_blank.all(|l| strip_line_prefix(l).is_some());
    let mut out = String::with_capacity(raw.len());
    for line in raw.split(['\r', '\n']) {
        let line = if strip { strip_line_prefix(line).unwrap_or(line) } else { line };
        push_collapsed(&mut out, line);
    }
    if out.chars().filter(|c| !c.is_whitespace()).count() < MIN_QUOTE_CHARS {
        return Err(CitationDefect::QuoteTooShort);
    }
    Ok(out)
}

/// A file normalized like a quote, with the source line of every normalized segment.
struct NormalizedText {
    text: String,
    /// `(byte offset of the segment in text, 1-based source line)`, ascending.
    segments: Vec<(usize, u32)>,
}

impl NormalizedText {
    fn build(lines: &[String]) -> Self {
        let mut text = String::new();
        let mut segments = Vec::with_capacity(lines.len());
        for (i, line) in lines.iter().enumerate() {
            let line_no = u32::try_from(i + 1).unwrap_or(u32::MAX);
            for part in line.split(['\r', '\n']) {
                let start = if text.is_empty() { 0 } else { text.len() + 1 };
                if push_collapsed(&mut text, part) {
                    segments.push((start, line_no));
                }
            }
        }
        Self { text, segments }
    }

    fn line_at(&self, offset: usize) -> u32 {
        let after = self.segments.partition_point(|&(start, _)| start <= offset);
        match after.checked_sub(1).and_then(|i| self.segments.get(i)) {
            Some(&(_, line)) => line,
            None => 1,
        }
    }

    /// Distinct source line spans of every (possibly overlapping) occurrence of `quote`, ascending.
    fn spans(&self, quote: &str) -> Vec<LineRange> {
        let mut spans: Vec<LineRange> = Vec::new();
        let mut from = 0;
        while let Some(pos) = self.text[from..].find(quote) {
            let start = from + pos;
            let last = start + quote.len() - 1;
            match LineRange::new(self.line_at(start), self.line_at(last)) {
                Ok(span) if spans.last() != Some(&span) => spans.push(span),
                _ => {}
            }
            from = start + self.text[start..].chars().next().map_or(1, char::len_utf8);
        }
        spans
    }
}

enum Located {
    Exact,
    Moved(LineRange),
}

/// Lines between `span` and `cited`; 0 when they overlap.
fn distance(span: LineRange, cited: LineRange) -> u32 {
    if span.end() < cited.start() {
        cited.start() - span.end()
    } else if span.start() > cited.end() {
        span.start() - cited.end()
    } else {
        0
    }
}

fn locate(quote: &str, file: &NormalizedText, cited: LineRange) -> Result<Located, CitationDefect> {
    let spans = file.spans(quote);
    if spans.iter().any(|s| cited.start() <= s.start() && s.end() <= cited.end()) {
        return Ok(Located::Exact);
    }
    let mut nearest: Option<(u32, LineRange)> = None;
    let mut tied = false;
    for &span in &spans {
        let d = distance(span, cited);
        if d > QUOTE_SEARCH_WINDOW {
            continue;
        }
        match nearest {
            Some((best, _)) if d > best => {}
            Some((best, _)) if d == best => tied = true,
            _ => {
                nearest = Some((d, span));
                tied = false;
            }
        }
    }
    match (nearest, spans.as_slice()) {
        (Some(_), _) if tied => Err(CitationDefect::QuoteAmbiguous),
        (Some((_, span)), _) => Ok(Located::Moved(span)),
        (None, []) => Err(CitationDefect::QuoteMismatch),
        (None, [only]) => Ok(Located::Moved(*only)),
        (None, _) => Err(CitationDefect::QuoteAmbiguous),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ids::{CommitSha, RepoId};
    use crate::domain::path::{FileClass, Language};
    use uuid::Uuid;

    fn key() -> RepoVersionKey {
        RepoVersionKey::new(RepoId::from_uuid(Uuid::from_u128(7)), CommitSha([0xab; 20]))
    }

    fn entry(path: &str, blob: u8, kind: EntryKind, is_text: bool, line_count: u32) -> ManifestEntry {
        ManifestEntry {
            path: RepoPath::new(path).unwrap(),
            kind,
            executable: false,
            blob: BlobOid([blob; 20]),
            lang: Some(Language::Rust),
            class: FileClass::Source,
            size: 1,
            is_text,
            line_count,
        }
    }

    fn numbered(n: u32) -> Vec<String> {
        (1..=n).map(|i| format!("let value_{i} = compute({i});")).collect()
    }

    /// Text files `(path, lines)`; blob ids follow the slice order.
    fn view(files: Vec<(&str, Vec<String>)>) -> ManifestView {
        let mut entries = Vec::new();
        let mut texts = Vec::new();
        for (i, (path, lines)) in files.into_iter().enumerate() {
            let blob = u8::try_from(i + 1).unwrap();
            entries.push(entry(path, blob, EntryKind::File, true, lines.len() as u32));
            texts.push((BlobOid([blob; 20]), lines));
        }
        ManifestView::new(key(), entries, texts)
    }

    fn cite(path: &str, start: u32, end: u32, quote: Option<&str>) -> CitationDraft {
        CitationDraft {
            path: RepoPath::new(path).unwrap(),
            lines: LineRange::new(start, end).unwrap(),
            quote: quote.map(str::to_owned),
            note: None,
        }
    }

    fn range(start: u32, end: u32) -> LineRange {
        LineRange::new(start, end).unwrap()
    }

    fn one(citation: CitationDraft, view: &ManifestView) -> CitationCheck {
        check(&[citation], view).items()[0]
    }

    const BOOM: &str = "throw new Error(\"boom\");";

    /// 100 numbered lines with `BOOM` on the given lines.
    fn boom_file(at: &[u32]) -> ManifestView {
        let mut lines = numbered(100);
        for &l in at {
            lines[l as usize - 1] = format!("    {BOOM}");
        }
        view(vec![("src/a.ts", lines)])
    }

    #[test]
    fn normalize_cited_path_table() {
        let ok = [
            ("src/a.rs", "src/a.rs"),
            ("./src/a.rs", "src/a.rs"),
            ("/src/a.rs", "src/a.rs"),
            ("Src/A.rs", "Src/A.rs"),
            (".github/ci.yml", ".github/ci.yml"),
        ];
        for (raw, want) in ok {
            assert_eq!(normalize_cited_path(raw).unwrap().as_str(), want, "{raw}");
        }
        let bad = ["", "./", "/", "././a.rs", "//a.rs", "a//b.rs", "a/", "src\\a.rs", "a\0b", "a/./b", "a/../b", "..", "../a", "a/.."];
        for raw in bad {
            assert_eq!(normalize_cited_path(raw), Err(CitationDefect::BadPath), "{raw:?}");
        }
    }

    #[test]
    fn cited_range_rejects_zero_and_reversed() {
        assert_eq!(cited_range(0, 3), Err(CitationDefect::BadRange));
        assert_eq!(cited_range(5, 4), Err(CitationDefect::BadRange));
        assert_eq!(cited_range(4, 4), Ok(range(4, 4)));
    }

    #[test]
    fn manifest_defects() {
        let entries = vec![
            entry("link", 1, EntryKind::Symlink, true, 1),
            entry("vendor/sub", 2, EntryKind::Submodule, false, 0),
            entry("logo.png", 3, EntryKind::File, false, 0),
            entry("empty.txt", 4, EntryKind::File, true, 0),
            entry("huge.sql", 5, EntryKind::File, true, 90_000),
            entry("ok.rs", 6, EntryKind::File, true, 3),
        ];
        let texts = vec![(BlobOid([1; 20]), vec!["target".to_owned()]), (BlobOid([6; 20]), numbered(3))];
        let view = ManifestView::new(key(), entries, texts);
        let cases = [
            ("missing.rs", CitationDefect::PathNotFound),
            ("link", CitationDefect::NotAFile),
            ("vendor/sub", CitationDefect::NotAFile),
            ("logo.png", CitationDefect::NotText),
            ("empty.txt", CitationDefect::NotText),
            ("huge.sql", CitationDefect::NotText),
        ];
        for (path, defect) in cases {
            assert_eq!(one(cite(path, 1, 1, None), &view), CitationCheck::Rejected(defect), "{path}");
        }
        assert_eq!(one(cite("ok.rs", 1, 3, None), &view), CitationCheck::Ok { lines: range(1, 3), quote: QuoteCheck::Absent });
    }

    #[test]
    fn range_ends_at_line_count() {
        let view = view(vec![("a.rs", numbered(100))]);
        assert!(matches!(one(cite("a.rs", 90, 100, None), &view), CitationCheck::Ok { .. }));
        assert_eq!(one(cite("a.rs", 90, 101, None), &view), CitationCheck::Rejected(CitationDefect::LineOutOfRange { line_count: 100 }));
        assert_eq!(one(cite("a.rs", 101, 101, None), &view), CitationCheck::Rejected(CitationDefect::LineOutOfRange { line_count: 100 }));
    }

    #[test]
    fn span_limit_is_200_lines() {
        let view = view(vec![("a.rs", numbered(300))]);
        assert!(matches!(one(cite("a.rs", 1, 200, None), &view), CitationCheck::Ok { .. }));
        assert_eq!(one(cite("a.rs", 1, 201, None), &view), CitationCheck::Rejected(CitationDefect::SpanTooLarge));
    }

    #[test]
    fn exact_quote_keeps_cited_range() {
        let view = boom_file(&[30, 50]);
        assert_eq!(one(cite("src/a.ts", 28, 31, Some(BOOM)), &view), CitationCheck::Ok { lines: range(28, 31), quote: QuoteCheck::Exact });
    }

    #[test]
    fn quote_adjusted_to_nearest_occurrence_within_window() {
        let view = boom_file(&[30, 50]);
        // 9 lines to 50, 11 lines to 30.
        assert_eq!(
            one(cite("src/a.ts", 41, 41, Some(BOOM)), &view),
            CitationCheck::Ok { lines: range(50, 50), quote: QuoteCheck::Adjusted { from: range(41, 41) } }
        );
        // A nearby occurrence wins over a far one.
        let view = boom_file(&[30, 90]);
        assert_eq!(
            one(cite("src/a.ts", 45, 46, Some(BOOM)), &view),
            CitationCheck::Ok { lines: range(30, 30), quote: QuoteCheck::Adjusted { from: range(45, 46) } }
        );
    }

    #[test]
    fn equidistant_nearby_occurrences_are_ambiguous() {
        let view = boom_file(&[30, 50]);
        assert_eq!(one(cite("src/a.ts", 40, 40, Some(BOOM)), &view), CitationCheck::Rejected(CitationDefect::QuoteAmbiguous));
    }

    #[test]
    fn search_window_is_20_lines() {
        let view = boom_file(&[30, 80]);
        // 20 lines from 30: nearby.
        assert_eq!(
            one(cite("src/a.ts", 50, 50, Some(BOOM)), &view),
            CitationCheck::Ok { lines: range(30, 30), quote: QuoteCheck::Adjusted { from: range(50, 50) } }
        );
        // 21 lines from 30 and 29 from 80: both far, two in the file.
        assert_eq!(one(cite("src/a.ts", 51, 51, Some(BOOM)), &view), CitationCheck::Rejected(CitationDefect::QuoteAmbiguous));
    }

    #[test]
    fn unique_far_occurrence_is_used() {
        let view = boom_file(&[90]);
        assert_eq!(
            one(cite("src/a.ts", 10, 12, Some(BOOM)), &view),
            CitationCheck::Ok { lines: range(90, 90), quote: QuoteCheck::Adjusted { from: range(10, 12) } }
        );
    }

    #[test]
    fn several_far_occurrences_are_ambiguous() {
        let view = boom_file(&[80, 90]);
        assert_eq!(one(cite("src/a.ts", 10, 10, Some(BOOM)), &view), CitationCheck::Rejected(CitationDefect::QuoteAmbiguous));
    }

    #[test]
    fn missing_quote_is_mismatch() {
        let view = boom_file(&[30]);
        assert_eq!(
            one(cite("src/a.ts", 30, 30, Some("throw new Error(\"bang\");")), &view),
            CitationCheck::Rejected(CitationDefect::QuoteMismatch)
        );
    }

    #[test]
    fn multi_line_adjustment_covers_matched_span() {
        let view = view(vec![("a.rs", numbered(100))]);
        let quote = "let value_70 = compute(70);\nlet value_71 = compute(71);";
        assert_eq!(
            one(cite("a.rs", 60, 65, Some(quote)), &view),
            CitationCheck::Ok { lines: range(70, 71), quote: QuoteCheck::Adjusted { from: range(60, 65) } }
        );
    }

    #[test]
    fn quote_normalization_crlf_whitespace_blank_lines() {
        let lines =
            vec!["    if (total  > limit) {".to_owned(), "\t\treturn   reject(total);".to_owned(), String::new(), "    }".to_owned()];
        let view = view(vec![("a.ts", lines)]);
        let quote = "if (total > limit) {\r\n\r\n   return reject(total);  \r\n  \r\n}";
        assert_eq!(one(cite("a.ts", 1, 4, Some(quote)), &view), CitationCheck::Ok { lines: range(1, 4), quote: QuoteCheck::Exact });
        // Old-Mac CR separators in the quote.
        let quote = "if (total > limit) {\rreturn reject(total);";
        assert_eq!(one(cite("a.ts", 1, 2, Some(quote)), &view), CitationCheck::Ok { lines: range(1, 2), quote: QuoteCheck::Exact });
    }

    #[test]
    fn crlf_file_lines_match() {
        let lines = vec!["fn total() {\r".to_owned(), "    sum_all(items)\r".to_owned(), "}\r".to_owned()];
        let view = view(vec![("a.rs", lines)]);
        assert_eq!(
            one(cite("a.rs", 1, 3, Some("fn total() {\nsum_all(items)\n}")), &view),
            CitationCheck::Ok { lines: range(1, 3), quote: QuoteCheck::Exact }
        );
    }

    #[test]
    fn read_file_prefix_stripped_only_when_every_line_has_it() {
        let full = view(vec![("a.rs", numbered(100))]);
        let prefixed = "    10\tlet value_10 = compute(10);\n    11\t\n    12\tlet value_12 = compute(12);";
        let mut file = numbered(100);
        file[10] = String::new();
        let view_blank = view(vec![("a.rs", file)]);
        assert_eq!(
            one(cite("a.rs", 10, 12, Some(prefixed)), &view_blank),
            CitationCheck::Ok { lines: range(10, 12), quote: QuoteCheck::Exact }
        );
        let mixed = "    10\tlet value_10 = compute(10);\nlet value_11 = compute(11);";
        assert_eq!(one(cite("a.rs", 10, 11, Some(mixed)), &full), CitationCheck::Rejected(CitationDefect::QuoteMismatch));
    }

    #[test]
    fn short_quotes_rejected() {
        let view = view(vec![("a.rs", vec!["0123456789".to_owned()])]);
        assert_eq!(one(cite("a.rs", 1, 1, Some("0123 45678")), &view), CitationCheck::Rejected(CitationDefect::QuoteTooShort));
        assert_eq!(one(cite("a.rs", 1, 1, Some("0123456789")), &view), CitationCheck::Ok { lines: range(1, 1), quote: QuoteCheck::Exact });
        // A quote of read_file prefixes only is empty after stripping.
        assert_eq!(one(cite("a.rs", 1, 1, Some("     1\t\n     2\t")), &view), CitationCheck::Rejected(CitationDefect::QuoteTooShort));
    }

    #[test]
    fn truncation_marker_rejected() {
        let view = view(vec![("a.rs", vec!["let long_line = \"abcdefghijkl\";".to_owned()])]);
        assert_eq!(
            one(cite("a.rs", 1, 1, Some("let long_line = \"abc…[+1520 chars]")), &view),
            CitationCheck::Rejected(CitationDefect::QuoteTruncated)
        );
        // Without digits it is not the marker.
        assert_eq!(
            one(cite("a.rs", 1, 1, Some("let long_line = \"abc…[+ chars]")), &view),
            CitationCheck::Rejected(CitationDefect::QuoteMismatch)
        );
    }

    #[test]
    fn snippet_hash_known_vector() {
        assert_eq!(hex::encode(snippet_hash(&["fn main() {", "}"])), "b6fa7f0cc8a512b7b045f23ac7283e7a19a0b188bc5f521cc74ec526e1754cfd");
        let view = view(vec![("a.rs", vec!["// head".to_owned(), "fn main() {".to_owned(), "}".to_owned()])]);
        let report = check(&[cite("a.rs", 2, 3, None)], &view);
        assert_eq!(report.citations()[0].snippet_hash, Some(snippet_hash(&["fn main() {", "}"])));
    }

    #[test]
    fn snippet_hash_covers_adjusted_lines() {
        let view = boom_file(&[90]);
        let report = check(&[cite("src/a.ts", 10, 10, Some(BOOM))], &view);
        let want = format!("    {BOOM}");
        assert_eq!(report.citations()[0].lines, range(90, 90));
        assert_eq!(report.citations()[0].snippet_hash, Some(snippet_hash(&[want])));
    }

    #[test]
    fn cause_verdicts() {
        let view = boom_file(&[30]);
        let report = check(&[cite("nope.ts", 1, 1, Some(BOOM)), cite("src/a.ts", 200, 200, None)], &view);
        assert_eq!(report.cause(), CauseEvidence::NoValidCitation);
        assert!(report.citations().is_empty());

        let report = check(&[cite("src/a.ts", 1, 5, None), cite("src/a.ts", 30, 30, Some("not in the file at all"))], &view);
        assert_eq!(report.cause(), CauseEvidence::NoVerifiedQuote);
        assert_eq!(report.citations().len(), 1);

        let report = check(&[cite("nope.ts", 1, 1, None), cite("src/a.ts", 30, 30, Some(BOOM)), cite("src/a.ts", 1, 2, None)], &view);
        assert_eq!(report.cause(), CauseEvidence::Valid);
        assert_eq!(report.sources(), &[1, 2]);
        let c = &report.citations()[0];
        assert_eq!((c.repo_id, c.sha), (key().repo_id, key().sha));
    }

    #[test]
    fn identical_citations_deduped() {
        let view = boom_file(&[30]);
        let mut first = cite("src/a.ts", 30, 30, None);
        first.note = Some("unquoted".to_owned());
        let mut second = cite("src/a.ts", 30, 30, Some(BOOM));
        second.note = Some("quoted".to_owned());
        let mut third = cite("src/a.ts", 30, 30, Some(BOOM));
        third.note = Some("again".to_owned());
        let report = check(&[first, second, third], &view);
        assert_eq!(report.items().len(), 3);
        assert_eq!(report.citations().len(), 1);
        assert_eq!(report.citations()[0].note.as_deref(), Some("quoted"));
        assert_eq!(report.sources(), &[1]);
        assert_eq!(report.cause(), CauseEvidence::Valid);

        // An adjusted citation that lands on an existing range is a duplicate too.
        let report = check(&[cite("src/a.ts", 30, 30, Some(BOOM)), cite("src/a.ts", 35, 35, Some(BOOM))], &view);
        assert_eq!(report.citations().len(), 1);
    }

    #[test]
    fn into_checked_requires_valid_cause() {
        let view = boom_file(&[30]);
        let report = check(&[cite("src/a.ts", 25, 26, Some(BOOM)), cite("src/a.ts", 1, 3, None)], &view);
        let token = report.into_checked().unwrap();
        assert_eq!(token.key(), key());
        let lines: Vec<LineRange> = token.citations().iter().map(|c| c.lines).collect();
        assert_eq!(lines, vec![range(30, 30), range(1, 3)]);
        assert!(token.citations().iter().all(|c| c.snippet_hash.is_some()));

        let unverified = check(&[cite("src/a.ts", 1, 3, None)], &view);
        assert_eq!(unverified.into_checked().unwrap_err(), DomainError::MissingEvidence);
        let none = check(&[], &view);
        assert_eq!(none.cause(), CauseEvidence::NoValidCitation);
        assert_eq!(none.into_checked().unwrap_err(), DomainError::MissingEvidence);
    }
}
