//! `Patch`: the strict git-diff subset produced by `Env::diff_against_base`
//! (`git diff --no-renames --full-index -U3 --src-prefix=a/ --dst-prefix=b/`), and
//! `FilePatch::apply` with `git apply` semantics (zero fuzz, optional offsets).
//!
//! Accepted per file: `diff --git`, `old mode`/`new mode`, `new file mode`, `deleted file mode`,
//! `index <40 hex>..<40 hex>[ <mode>]`, `---`/`+++`, `@@` hunks, `\ No newline at end of file`.
//! Rejected: renames/copies, binary stanzas, symlinks, gitlinks, abbreviated object ids, anything
//! else. Hunk line content is kept byte-for-byte (CRLF included).

use std::borrow::Cow;
use std::collections::BTreeSet;
use std::ops::Range;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::domain::ids::BlobOid;
use crate::domain::path::RepoPath;
use crate::domain::verify::FixStats;

/// How hunks may be placed (executor.md `ApplyMode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApplyMode {
    /// Every hunk must apply at the position its header states.
    Exact,
    /// A hunk may apply at an offset (nearest match first, like `git apply`); fuzz stays zero.
    AllowOffset,
}

/// Mode of a regular file in a tree; symlinks and gitlinks are never produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileMode {
    /// `100644`
    Regular,
    /// `100755`
    Executable,
}

impl FileMode {
    /// Octal git tree mode (`100644` / `100755`).
    pub fn git_mode(self) -> &'static str {
        match self {
            FileMode::Regular => "100644",
            FileMode::Executable => "100755",
        }
    }
}

/// What a `FilePatch` does to its path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PatchChange {
    Add,
    /// Content change, possibly with a `±x` mode change.
    Modify,
    Delete,
    /// `±x` mode change without content change.
    ModeOnly,
}

/// Why patch bytes are outside the accepted git-diff subset. `line` is 1-based.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PatchError {
    #[error("empty patch")]
    Empty,
    #[error("patch does not end with a newline")]
    MissingFinalNewline,
    #[error("line {line}: expected `diff --git`")]
    ExpectedDiffHeader { line: usize },
    #[error("line {line}: malformed `diff --git` header")]
    MalformedDiffHeader { line: usize },
    #[error("line {line}: invalid path: {reason}")]
    InvalidPath { line: usize, reason: String },
    #[error("line {line}: renames and copies are not supported")]
    RenameOrCopy { line: usize },
    #[error("line {line}: binary patches are not supported")]
    Binary { line: usize },
    #[error("line {line}: `{path}` is a symlink")]
    Symlink { line: usize, path: RepoPath },
    #[error("line {line}: `{path}` is a submodule (gitlink)")]
    Gitlink { line: usize, path: RepoPath },
    #[error("line {line}: `{path}` has an unsupported file mode")]
    UnsupportedMode { line: usize, path: RepoPath },
    #[error("line {line}: malformed `index` line (full object ids required)")]
    MalformedIndex { line: usize },
    #[error("line {line}: unexpected line")]
    UnexpectedLine { line: usize },
    #[error("line {line}: inconsistent file header")]
    InconsistentHeader { line: usize },
    #[error("line {line}: content change without hunks")]
    MissingHunks { line: usize },
    #[error("line {line}: malformed hunk header")]
    MalformedHunkHeader { line: usize },
    #[error("line {line}: hunk body does not match the header's line counts")]
    HunkCountMismatch { line: usize },
    #[error("line {line}: misplaced `\\ No newline at end of file`")]
    MisplacedNoNewline { line: usize },
    #[error("line {line}: hunks overlap or are out of order")]
    HunkOrder { line: usize },
    #[error("`{path}` appears more than once")]
    DuplicatePath { path: RepoPath },
}

/// Why a `FilePatch` does not apply to the given base.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ApplyError {
    #[error("{path}: already exists")]
    AlreadyExists { path: RepoPath },
    #[error("{path}: does not exist")]
    Missing { path: RepoPath },
    /// `hunk` is 1-based within the file.
    #[error("{path}: hunk {hunk} (old line {old_start}) does not apply")]
    HunkDoesNotApply { path: RepoPath, hunk: usize, old_start: u32 },
    #[error("{path}: removal patch leaves file contents")]
    DeleteLeavesContent { path: RepoPath },
}

/// A parsed patch. Construct with `Patch::parse`; the raw bytes themselves are the artifact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Patch {
    sha256: [u8; 32],
    len: u64,
    files: Vec<FilePatch>,
}

/// One `diff --git` section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilePatch {
    path: RepoPath,
    change: PatchChange,
    old_mode: Option<FileMode>,
    new_mode: Option<FileMode>,
    old_oid: Option<BlobOid>,
    new_oid: Option<BlobOid>,
    hunks: Vec<Hunk>,
    lines: Vec<HunkLine>,
    /// Concatenated line contents (op character stripped, terminator kept unless the line was
    /// followed by `\ No newline at end of file`).
    text: Vec<u8>,
    added: u32,
    removed: u32,
    new_bytes: u64,
}

/// One `@@` hunk; positions are 1-based as in the header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    pub old_start: u32,
    pub old_lines: u32,
    pub new_start: u32,
    pub new_lines: u32,
    /// Indices into the owning `FilePatch::lines`.
    lines: Range<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineOp {
    Context,
    Remove,
    Add,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HunkLine {
    op: LineOp,
    start: usize,
    end: usize,
}

/// Blob id of the empty file.
const EMPTY_BLOB: [u8; 20] =
    [0xe6, 0x9d, 0xe2, 0x9b, 0xb2, 0xd1, 0xd6, 0x43, 0x4b, 0x8b, 0x29, 0xae, 0x77, 0x5a, 0xd8, 0xc2, 0xe4, 0x8c, 0x53, 0x91];

impl Patch {
    /// Parse and validate `raw` (see module docs for the accepted subset).
    pub fn parse(raw: &[u8]) -> Result<Patch, PatchError> {
        if raw.is_empty() {
            return Err(PatchError::Empty);
        }
        if raw.last() != Some(&b'\n') {
            return Err(PatchError::MissingFinalNewline);
        }
        let mut lines = Lines { src: raw, pos: 0, line_no: 1 };
        let mut files = Vec::new();
        while lines.peek().is_some() {
            files.push(parse_file(&mut lines)?);
        }
        let mut seen = BTreeSet::new();
        for file in &files {
            if !seen.insert(&file.path) {
                return Err(PatchError::DuplicatePath { path: file.path.clone() });
            }
        }
        Ok(Patch { sha256: Sha256::digest(raw).into(), len: raw.len() as u64, files })
    }

    /// SHA-256 of the exact bytes parsed.
    pub fn sha256(&self) -> [u8; 32] {
        self.sha256
    }

    /// Size of the raw patch in bytes.
    pub fn len_bytes(&self) -> u64 {
        self.len
    }

    pub fn files(&self) -> &[FilePatch] {
        &self.files
    }

    pub fn paths(&self) -> impl Iterator<Item = &RepoPath> {
        self.files.iter().map(|f| &f.path)
    }

    /// Files touched and lines added / removed.
    pub fn stats(&self) -> FixStats {
        FixStats {
            files: u32::try_from(self.files.len()).unwrap_or(u32::MAX),
            added: self.files.iter().fold(0u32, |n, f| n.saturating_add(f.added)),
            removed: self.files.iter().fold(0u32, |n, f| n.saturating_add(f.removed)),
        }
    }
}

impl FilePatch {
    pub fn path(&self) -> &RepoPath {
        &self.path
    }

    pub fn change(&self) -> PatchChange {
        self.change
    }

    /// `None` for `Add`.
    pub fn old_mode(&self) -> Option<FileMode> {
        self.old_mode
    }

    /// `None` for `Delete`.
    pub fn new_mode(&self) -> Option<FileMode> {
        self.new_mode
    }

    /// Pre-image blob id from the `index` line (`None` for `Add` and `ModeOnly`).
    pub fn old_oid(&self) -> Option<BlobOid> {
        self.old_oid
    }

    /// Post-image blob id from the `index` line (`None` for `Delete` and `ModeOnly`).
    pub fn new_oid(&self) -> Option<BlobOid> {
        self.new_oid
    }

    pub fn hunks(&self) -> &[Hunk] {
        &self.hunks
    }

    pub fn added(&self) -> u32 {
        self.added
    }

    pub fn removed(&self) -> u32 {
        self.removed
    }

    /// Bytes of post-image content carried by the hunks (context + added lines). Exact
    /// post-image size for `Add`; a lower bound for `Modify`.
    pub fn post_image_bytes(&self) -> u64 {
        self.new_bytes
    }

    /// Added lines as `(post-image line number, content incl. its terminator if any)`.
    pub fn added_lines(&self) -> impl Iterator<Item = (u32, &[u8])> + '_ {
        self.hunks.iter().flat_map(move |hunk| {
            let mut line_no = hunk.new_start;
            self.lines[hunk.lines.clone()].iter().filter_map(move |line| match line.op {
                LineOp::Remove => None,
                LineOp::Context => {
                    line_no += 1;
                    None
                }
                LineOp::Add => {
                    let at = line_no;
                    line_no += 1;
                    Some((at, &self.text[line.start..line.end]))
                }
            })
        })
    }

    /// Apply to `base` (`None` = path absent) with `git apply` semantics and zero fuzz.
    /// Returns the post-image, or `None` when the file is deleted.
    ///
    /// One deliberate divergence: a pre-image line marked `\ No newline at end of file` only
    /// matches a base line that also lacks the newline. `git apply` compares the pre-image as a
    /// byte prefix there, so it accepts a base with more content after that line and splices it
    /// onto the post-image's last line (`c` + `d` → `cd`); that result would corrupt a commit.
    pub fn apply(&self, base: Option<&[u8]>, mode: ApplyMode) -> Result<Option<Vec<u8>>, ApplyError> {
        let base = match (self.change, base) {
            (PatchChange::Add, Some(_)) => {
                return Err(ApplyError::AlreadyExists { path: self.path.clone() });
            }
            (PatchChange::Add, None) => &[][..],
            (_, None) => return Err(ApplyError::Missing { path: self.path.clone() }),
            (_, Some(base)) => base,
        };
        let mut image: Vec<ImageLine<'_>> =
            base.split_inclusive(|&b| b == b'\n').map(|bytes| ImageLine { bytes, patched: false }).collect();
        for (index, hunk) in self.hunks.iter().enumerate() {
            let Some(at) = self.locate(hunk, &image, mode) else {
                return Err(ApplyError::HunkDoesNotApply { path: self.path.clone(), hunk: index + 1, old_start: hunk.old_start });
            };
            let post = self.lines[hunk.lines.clone()]
                .iter()
                .filter(|line| line.op != LineOp::Remove)
                .map(|line| ImageLine { bytes: &self.text[line.start..line.end], patched: true });
            image.splice(at..at + hunk.old_lines as usize, post);
        }
        let len: usize = image.iter().map(|line| line.bytes.len()).sum();
        if self.change == PatchChange::Delete {
            if len != 0 {
                return Err(ApplyError::DeleteLeavesContent { path: self.path.clone() });
            }
            return Ok(None);
        }
        let mut out = Vec::with_capacity(len);
        for line in &image {
            out.extend_from_slice(line.bytes);
        }
        Ok(Some(out))
    }

    /// Image line where `hunk`'s pre-image goes. Mirrors git's `apply_one_fragment`/`find_pos`
    /// with the default (unlimited) context: hunks starting at old line 0/1 must match at the
    /// beginning, hunks without trailing context must match at the end, lines already written by
    /// an earlier hunk never match again, and the search goes nearest-first (+1, -1, +2, …).
    fn locate(&self, hunk: &Hunk, image: &[ImageLine<'_>], mode: ApplyMode) -> Option<usize> {
        let lines = &self.lines[hunk.lines.clone()];
        let pre_len = hunk.old_lines as usize;
        let trailing = lines.iter().rev().take_while(|line| line.op == LineOp::Context).count();
        let match_beginning = hunk.old_start <= 1;
        let match_end = trailing == 0;
        let stated = (hunk.new_start as usize).saturating_sub(1);
        let n = image.len();
        let line = if match_beginning {
            0
        } else if match_end {
            n.checked_sub(pre_len).unwrap_or(n)
        } else {
            stated.min(n)
        };
        let fits = |at: usize| -> bool {
            if at + pre_len > n || (match_end && at + pre_len != n) || (match_beginning && at != 0) {
                return false;
            }
            let pre = lines.iter().filter(|l| l.op != LineOp::Add).map(|l| &self.text[l.start..l.end]);
            image[at..at + pre_len].iter().zip(pre).all(|(img, want)| !img.patched && img.bytes == want)
        };
        match mode {
            ApplyMode::Exact => (line == stated && fits(line)).then_some(line),
            // Anchored hunks have exactly one admissible position.
            ApplyMode::AllowOffset if match_beginning || match_end => fits(line).then_some(line),
            ApplyMode::AllowOffset => {
                let (mut back, mut fwd, mut cur) = (line, line, line);
                let mut i = 0usize;
                loop {
                    if fits(cur) {
                        return Some(cur);
                    }
                    loop {
                        if back == 0 && fwd == n {
                            return None;
                        }
                        if i & 1 == 1 {
                            if back == 0 {
                                i += 1;
                                continue;
                            }
                            back -= 1;
                            cur = back;
                        } else {
                            if fwd == n {
                                i += 1;
                                continue;
                            }
                            fwd += 1;
                            cur = fwd;
                        }
                        break;
                    }
                    i += 1;
                }
            }
        }
    }
}

#[derive(Clone, Copy)]
struct ImageLine<'a> {
    bytes: &'a [u8],
    /// Written by an earlier hunk (git's `LINE_PATCHED`): never part of a later match.
    patched: bool,
}

/// Line cursor over the raw patch; every line ends with `\n` (checked up front).
struct Lines<'a> {
    src: &'a [u8],
    pos: usize,
    line_no: usize,
}

impl<'a> Lines<'a> {
    /// Next line without its `\n`.
    fn peek(&self) -> Option<&'a [u8]> {
        let rest = self.src.get(self.pos..).filter(|r| !r.is_empty())?;
        let end = rest.iter().position(|&b| b == b'\n').unwrap_or(rest.len());
        Some(&rest[..end])
    }

    fn next(&mut self) -> Option<&'a [u8]> {
        let line = self.peek()?;
        self.pos += line.len() + 1;
        self.line_no += 1;
        Some(line)
    }

    fn peek_starts_with(&self, prefix: &[u8]) -> bool {
        self.peek().is_some_and(|l| l.starts_with(prefix))
    }
}

/// Raw extended header value and its line number.
type Raw<'a> = Option<(&'a [u8], usize)>;

#[derive(Default)]
struct Extended<'a> {
    old_mode: Raw<'a>,
    new_mode: Raw<'a>,
    new_file: Raw<'a>,
    deleted: Raw<'a>,
    index: Raw<'a>,
}

fn set_once<'a>(slot: &mut Raw<'a>, value: &'a [u8], line: usize) -> Result<(), PatchError> {
    if slot.is_some() {
        return Err(PatchError::UnexpectedLine { line });
    }
    *slot = Some((value, line));
    Ok(())
}

fn parse_extended<'a>(ext: &mut Extended<'a>, line: &'a [u8], no: usize) -> Result<(), PatchError> {
    const RENAME_OR_COPY: [&[u8]; 6] =
        [b"similarity index ", b"dissimilarity index ", b"rename from ", b"rename to ", b"copy from ", b"copy to "];
    if let Some(v) = line.strip_prefix(b"old mode ") {
        set_once(&mut ext.old_mode, v, no)
    } else if let Some(v) = line.strip_prefix(b"new mode ") {
        set_once(&mut ext.new_mode, v, no)
    } else if let Some(v) = line.strip_prefix(b"new file mode ") {
        set_once(&mut ext.new_file, v, no)
    } else if let Some(v) = line.strip_prefix(b"deleted file mode ") {
        set_once(&mut ext.deleted, v, no)
    } else if let Some(v) = line.strip_prefix(b"index ") {
        set_once(&mut ext.index, v, no)
    } else if RENAME_OR_COPY.iter().any(|p| line.starts_with(p)) {
        Err(PatchError::RenameOrCopy { line: no })
    } else if line.starts_with(b"Binary files ") || line == b"GIT binary patch" {
        Err(PatchError::Binary { line: no })
    } else {
        Err(PatchError::UnexpectedLine { line: no })
    }
}

fn parse_mode(raw: (&[u8], usize), path: &RepoPath) -> Result<FileMode, PatchError> {
    let (value, line) = raw;
    match value {
        b"100644" => Ok(FileMode::Regular),
        b"100755" => Ok(FileMode::Executable),
        b"120000" => Err(PatchError::Symlink { line, path: path.clone() }),
        b"160000" => Err(PatchError::Gitlink { line, path: path.clone() }),
        _ => Err(PatchError::UnsupportedMode { line, path: path.clone() }),
    }
}

struct Index<'a> {
    old: Option<BlobOid>,
    new: Option<BlobOid>,
    mode: Raw<'a>,
    line: usize,
}

/// `<40 hex>..<40 hex>[ <mode>]`; the all-zero id means "absent".
fn parse_index(raw: (&[u8], usize)) -> Result<Index<'_>, PatchError> {
    let (value, line) = raw;
    let bad = || PatchError::MalformedIndex { line };
    let oid = |hex: &[u8]| -> Result<Option<BlobOid>, PatchError> {
        let mut out = [0u8; 20];
        if hex.len() != 40 {
            return Err(bad());
        }
        hex::decode_to_slice(hex, &mut out).map_err(|_| bad())?;
        Ok((out != [0u8; 20]).then_some(BlobOid(out)))
    };
    let (ids, mode) = match value.iter().position(|&b| b == b' ') {
        Some(i) => (&value[..i], Some((&value[i + 1..], line))),
        None => (value, None),
    };
    if ids.len() != 82 || &ids[40..42] != b".." {
        return Err(bad());
    }
    Ok(Index { old: oid(&ids[..40])?, new: oid(&ids[42..])?, mode, line })
}

/// C-style quoted git path (`"a/\303\251\tb"`): unquoted bytes and the rest after the quote.
fn unquote(s: &[u8]) -> Option<(Vec<u8>, &[u8])> {
    let mut out = Vec::with_capacity(s.len());
    let mut i = 1;
    while i < s.len() {
        match s[i] {
            b'"' => return Some((out, &s[i + 1..])),
            b'\\' => {
                let c = *s.get(i + 1)?;
                i += 2;
                match c {
                    b'a' => out.push(0x07),
                    b'b' => out.push(0x08),
                    b't' => out.push(b'\t'),
                    b'n' => out.push(b'\n'),
                    b'v' => out.push(0x0b),
                    b'f' => out.push(0x0c),
                    b'r' => out.push(b'\r'),
                    b'"' | b'\\' => out.push(c),
                    b'0'..=b'3' => {
                        let d1 = *s.get(i)?;
                        let d2 = *s.get(i + 1)?;
                        if !(b'0'..=b'7').contains(&d1) || !(b'0'..=b'7').contains(&d2) {
                            return None;
                        }
                        out.push(((c - b'0') << 6) | ((d1 - b'0') << 3) | (d2 - b'0'));
                        i += 2;
                    }
                    _ => return None,
                }
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    None
}

fn to_repo_path(bytes: &[u8], line: usize) -> Result<RepoPath, PatchError> {
    let s = std::str::from_utf8(bytes).map_err(|_| PatchError::InvalidPath { line, reason: "path is not UTF-8".into() })?;
    RepoPath::new(s).map_err(|e| PatchError::InvalidPath { line, reason: e.to_string() })
}

/// Path from `diff --git a/<p> b/<p>` (both names equal: no renames).
fn parse_header_path(rest: &[u8], line: usize) -> Result<RepoPath, PatchError> {
    let malformed = || PatchError::MalformedDiffHeader { line };
    if rest.first() == Some(&b'"') {
        let (a, tail) = unquote(rest).ok_or_else(malformed)?;
        let tail = tail.strip_prefix(b" ").filter(|t| t.first() == Some(&b'"')).ok_or_else(malformed)?;
        let (b, tail) = unquote(tail).ok_or_else(malformed)?;
        if !tail.is_empty() {
            return Err(malformed());
        }
        return same_name(&a, &b, line);
    }
    // Unquoted names may contain spaces: `a/<n bytes> b/<n bytes>`, total 2n + 5.
    let len = rest.len();
    if len < 7 || (len - 5) % 2 != 0 {
        return Err(malformed());
    }
    let n = (len - 5) / 2;
    if rest[2 + n] != b' ' {
        return Err(malformed());
    }
    same_name(&rest[..2 + n], &rest[3 + n..], line)
}

fn same_name(a: &[u8], b: &[u8], line: usize) -> Result<RepoPath, PatchError> {
    match (a.strip_prefix(b"a/"), b.strip_prefix(b"b/")) {
        (Some(a), Some(b)) if a == b => to_repo_path(a, line),
        _ => Err(PatchError::MalformedDiffHeader { line }),
    }
}

enum Side {
    DevNull,
    Path(RepoPath),
}

/// Name after `--- ` / `+++ `: `/dev/null` or `<prefix><path>`, optionally quoted, optionally
/// followed by the tab git appends to names containing a space.
fn parse_side(rest: &[u8], prefix: &[u8], line: usize) -> Result<Side, PatchError> {
    let inconsistent = || PatchError::InconsistentHeader { line };
    let name: Cow<'_, [u8]> = if rest.first() == Some(&b'"') {
        let (name, tail) = unquote(rest).ok_or_else(inconsistent)?;
        if !(tail.is_empty() || tail == b"\t") {
            return Err(inconsistent());
        }
        Cow::Owned(name)
    } else {
        Cow::Borrowed(rest.strip_suffix(b"\t").unwrap_or(rest))
    };
    if &*name == b"/dev/null" {
        return Ok(Side::DevNull);
    }
    let path = name.strip_prefix(prefix).ok_or_else(inconsistent)?;
    Ok(Side::Path(to_repo_path(path, line)?))
}

fn parse_file(lines: &mut Lines<'_>) -> Result<FilePatch, PatchError> {
    let header_no = lines.line_no;
    let header = lines.next().unwrap_or_default();
    let rest = header.strip_prefix(b"diff --git ").ok_or(PatchError::ExpectedDiffHeader { line: header_no })?;

    // Extended headers first, so renames/copies and binary stanzas are reported as such even
    // when their `diff --git` names differ.
    let mut ext = Extended::default();
    while let Some(line) = lines.peek() {
        if line.starts_with(b"diff --git ") || line.starts_with(b"--- ") || line.starts_with(b"@@") {
            break;
        }
        let no = lines.line_no;
        lines.next();
        parse_extended(&mut ext, line, no)?;
    }
    let path = parse_header_path(rest, header_no)?;
    let index = ext.index.map(parse_index).transpose()?;
    let inconsistent = || PatchError::InconsistentHeader { line: header_no };

    let (change, old_mode, new_mode) = match (ext.new_file, ext.deleted, ext.old_mode, ext.new_mode) {
        (Some(m), None, None, None) => (PatchChange::Add, None, Some(parse_mode(m, &path)?)),
        (None, Some(m), None, None) => (PatchChange::Delete, Some(parse_mode(m, &path)?), None),
        (None, None, Some(old), Some(new)) => {
            let (old, new) = (parse_mode(old, &path)?, parse_mode(new, &path)?);
            if old == new {
                return Err(inconsistent());
            }
            let change = if index.is_some() { PatchChange::Modify } else { PatchChange::ModeOnly };
            (change, Some(old), Some(new))
        }
        (None, None, None, None) => {
            // Plain content change: git prints the unchanged mode on the `index` line.
            let mode = index.as_ref().and_then(|i| i.mode).ok_or_else(inconsistent)?;
            let mode = parse_mode(mode, &path)?;
            (PatchChange::Modify, Some(mode), Some(mode))
        }
        _ => return Err(inconsistent()),
    };
    let plain_modify = change == PatchChange::Modify && ext.old_mode.is_none();
    let (old_oid, new_oid) = match (&index, change) {
        (None, PatchChange::ModeOnly) => (None, None),
        (None, _) => return Err(inconsistent()),
        (Some(index), _) => {
            if index.mode.is_some() != plain_modify {
                return Err(PatchError::InconsistentHeader { line: index.line });
            }
            let ok = match change {
                PatchChange::Add => index.old.is_none() && index.new.is_some(),
                PatchChange::Delete => index.old.is_some() && index.new.is_none(),
                PatchChange::Modify => index.old.is_some() && index.new.is_some() && index.old != index.new,
                PatchChange::ModeOnly => false,
            };
            if !ok {
                return Err(PatchError::InconsistentHeader { line: index.line });
            }
            (index.old, index.new)
        }
    };

    let mut file = FilePatch {
        path,
        change,
        old_mode,
        new_mode,
        old_oid,
        new_oid,
        hunks: Vec::new(),
        lines: Vec::new(),
        text: Vec::new(),
        added: 0,
        removed: 0,
        new_bytes: 0,
    };

    if lines.peek_starts_with(b"--- ") {
        if change == PatchChange::ModeOnly {
            return Err(PatchError::InconsistentHeader { line: lines.line_no });
        }
        let minus_no = lines.line_no;
        let minus = parse_side(&lines.next().unwrap_or_default()[4..], b"a/", minus_no)?;
        let plus_no = lines.line_no;
        if !lines.peek_starts_with(b"+++ ") {
            return Err(PatchError::InconsistentHeader { line: plus_no });
        }
        let plus = parse_side(&lines.next().unwrap_or_default()[4..], b"b/", plus_no)?;
        let same = |side: &Side| matches!(side, Side::Path(p) if *p == file.path);
        let sides_ok = match change {
            PatchChange::Add => matches!(minus, Side::DevNull) && same(&plus),
            PatchChange::Delete => same(&minus) && matches!(plus, Side::DevNull),
            PatchChange::Modify | PatchChange::ModeOnly => same(&minus) && same(&plus),
        };
        if !sides_ok {
            return Err(PatchError::InconsistentHeader { line: minus_no });
        }
        if !lines.peek_starts_with(b"@@") {
            return Err(PatchError::MissingHunks { line: lines.line_no });
        }
        let mut closed = Closed::default();
        while lines.peek_starts_with(b"@@") {
            parse_hunk(lines, &mut file, &mut closed)?;
        }
    } else {
        if lines.peek_starts_with(b"@@") {
            return Err(PatchError::InconsistentHeader { line: lines.line_no });
        }
        // Only empty-file additions/deletions and pure mode changes carry no hunks.
        let empty = |oid: Option<BlobOid>| oid == Some(BlobOid(EMPTY_BLOB));
        let ok = match change {
            PatchChange::Add => empty(file.new_oid),
            PatchChange::Delete => empty(file.old_oid),
            PatchChange::Modify => false,
            PatchChange::ModeOnly => true,
        };
        if !ok {
            return Err(PatchError::MissingHunks { line: header_no });
        }
    }
    if lines.peek().is_some() && !lines.peek_starts_with(b"diff --git ") {
        return Err(PatchError::UnexpectedLine { line: lines.line_no });
    }
    Ok(file)
}

/// Sides that already ended without a trailing newline: nothing may follow on them.
#[derive(Default)]
struct Closed {
    old: bool,
    new: bool,
}

/// `-<start>[,<count>]` / `+<start>[,<count>]` (count defaults to 1).
fn parse_range(s: &[u8], sign: u8) -> Option<(u32, u32)> {
    let s = s.strip_prefix(&[sign])?;
    let number = |d: &[u8]| -> Option<u32> {
        if d.is_empty() || !d.iter().all(u8::is_ascii_digit) {
            return None;
        }
        std::str::from_utf8(d).ok()?.parse().ok()
    };
    let (start, count) = match s.iter().position(|&b| b == b',') {
        Some(i) => (number(&s[..i])?, number(&s[i + 1..])?),
        None => (number(s)?, 1),
    };
    (count == 0 || start > 0).then_some((start, count))
}

fn parse_hunk(lines: &mut Lines<'_>, file: &mut FilePatch, closed: &mut Closed) -> Result<(), PatchError> {
    let header_no = lines.line_no;
    let header = lines.next().unwrap_or_default();
    let bad_header = || PatchError::MalformedHunkHeader { line: header_no };
    let rest = header.strip_prefix(b"@@ ").ok_or_else(bad_header)?;
    let mut parts = rest.splitn(4, |&b| b == b' ');
    let old = parts.next().and_then(|s| parse_range(s, b'-')).ok_or_else(bad_header)?;
    let new = parts.next().and_then(|s| parse_range(s, b'+')).ok_or_else(bad_header)?;
    if parts.next() != Some(b"@@".as_slice()) {
        return Err(bad_header());
    }
    if closed.old || closed.new {
        return Err(PatchError::MisplacedNoNewline { line: header_no });
    }
    let (old_start, old_lines) = old;
    let (new_start, new_lines) = new;
    let anchored = match file.change {
        PatchChange::Add => old_start == 0 && old_lines == 0,
        PatchChange::Delete => new_start == 0 && new_lines == 0,
        PatchChange::Modify | PatchChange::ModeOnly => true,
    };
    if !anchored {
        return Err(PatchError::InconsistentHeader { line: header_no });
    }
    if let Some(prev) = file.hunks.last() {
        if old_start < prev.old_start.saturating_add(prev.old_lines.max(1)) {
            return Err(PatchError::HunkOrder { line: header_no });
        }
    }

    let first = file.lines.len();
    let (mut old_left, mut new_left) = (old_lines, new_lines);
    loop {
        // The marker may follow the hunk's last line, so look for it before checking counts.
        if lines.peek_starts_with(b"\\") {
            let no = lines.line_no;
            lines.next();
            let last = file.lines[first..].last_mut().ok_or(PatchError::MisplacedNoNewline { line: no })?;
            if last.end == last.start || file.text.get(last.end - 1) != Some(&b'\n') {
                return Err(PatchError::MisplacedNoNewline { line: no });
            }
            file.text.pop();
            last.end -= 1;
            match last.op {
                LineOp::Context => {
                    closed.old = true;
                    closed.new = true;
                    file.new_bytes -= 1;
                }
                LineOp::Remove => closed.old = true,
                LineOp::Add => {
                    closed.new = true;
                    file.new_bytes -= 1;
                }
            }
            continue;
        }
        if old_left == 0 && new_left == 0 {
            break;
        }
        let no = lines.line_no;
        let line = lines.peek().ok_or(PatchError::HunkCountMismatch { line: header_no })?;
        let op = match line.first() {
            Some(b' ') if old_left > 0 && new_left > 0 => LineOp::Context,
            Some(b'-') if old_left > 0 => LineOp::Remove,
            Some(b'+') if new_left > 0 => LineOp::Add,
            _ => return Err(PatchError::HunkCountMismatch { line: no }),
        };
        let touches_old = op != LineOp::Add;
        let touches_new = op != LineOp::Remove;
        if (touches_old && closed.old) || (touches_new && closed.new) {
            return Err(PatchError::MisplacedNoNewline { line: no });
        }
        lines.next();
        let start = file.text.len();
        file.text.extend_from_slice(&line[1..]);
        file.text.push(b'\n');
        file.lines.push(HunkLine { op, start, end: file.text.len() });
        if touches_old {
            old_left -= 1;
        }
        if touches_new {
            new_left -= 1;
            file.new_bytes += line.len() as u64;
        }
        match op {
            LineOp::Add => file.added += 1,
            LineOp::Remove => file.removed += 1,
            LineOp::Context => {}
        }
    }
    file.hunks.push(Hunk { old_start, old_lines, new_start, new_lines, lines: first..file.lines.len() });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const Z: &str = "0000000000000000000000000000000000000000";
    const A: &str = "1111111111111111111111111111111111111111";
    const B: &str = "2222222222222222222222222222222222222222";
    const EMPTY: &str = "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391";

    fn modify(path: &str, hunks: &str) -> String {
        format!("diff --git a/{path} b/{path}\nindex {A}..{B} 100644\n--- a/{path}\n+++ b/{path}\n{hunks}")
    }

    fn numbered(range: std::ops::RangeInclusive<u32>) -> String {
        range.map(|i| format!("line {i}\n")).collect()
    }

    fn apply(patch: &str, base: Option<&[u8]>, mode: ApplyMode) -> Result<Option<Vec<u8>>, ApplyError> {
        let patch = Patch::parse(patch.as_bytes()).unwrap();
        assert_eq!(patch.files().len(), 1);
        patch.files()[0].apply(base, mode)
    }

    fn parse_err(patch: &str) -> PatchError {
        Patch::parse(patch.as_bytes()).unwrap_err()
    }

    #[test]
    fn add_file() {
        let p = format!(
            "diff --git a/src/new.ts b/src/new.ts\nnew file mode 100644\nindex {Z}..{A}\n--- /dev/null\n+++ b/src/new.ts\n@@ -0,0 +1,2 @@\n+export const a = 1;\n+export const b = 2;\n"
        );
        let patch = Patch::parse(p.as_bytes()).unwrap();
        let file = &patch.files()[0];
        assert_eq!(file.change(), PatchChange::Add);
        assert_eq!((file.old_mode(), file.new_mode()), (None, Some(FileMode::Regular)));
        assert_eq!((file.old_oid(), file.new_oid()), (None, Some(BlobOid([0x11; 20]))));
        assert_eq!(patch.stats(), FixStats { files: 1, added: 2, removed: 0 });
        let added: Vec<_> = file.added_lines().collect();
        assert_eq!(added, vec![(1, b"export const a = 1;\n".as_slice()), (2, b"export const b = 2;\n".as_slice())]);
        assert_eq!(file.post_image_bytes(), 40);
        assert_eq!(file.apply(None, ApplyMode::Exact).unwrap().unwrap(), b"export const a = 1;\nexport const b = 2;\n");
        // An add requires the path to be absent, even when the existing file is empty.
        assert_eq!(
            file.apply(Some(b""), ApplyMode::AllowOffset),
            Err(ApplyError::AlreadyExists { path: RepoPath::new("src/new.ts").unwrap() })
        );
    }

    #[test]
    fn add_and_delete_empty_files_have_no_hunks() {
        let add = format!("diff --git a/e b/e\nnew file mode 100644\nindex {Z}..{EMPTY}\n");
        assert_eq!(apply(&add, None, ApplyMode::Exact).unwrap().unwrap(), b"");
        let del = format!("diff --git a/e b/e\ndeleted file mode 100644\nindex {EMPTY}..{Z}\n");
        assert_eq!(apply(&del, Some(b""), ApplyMode::Exact).unwrap(), None);
        assert!(matches!(apply(&del, Some(b"x\n"), ApplyMode::Exact), Err(ApplyError::DeleteLeavesContent { .. })));
        // A non-empty blob without hunks is a truncated patch.
        let truncated = format!("diff --git a/e b/e\nnew file mode 100644\nindex {Z}..{A}\n");
        assert_eq!(parse_err(&truncated), PatchError::MissingHunks { line: 1 });
    }

    #[test]
    fn delete_file() {
        let p = format!(
            "diff --git a/old.txt b/old.txt\ndeleted file mode 100755\nindex {A}..{Z}\n--- a/old.txt\n+++ /dev/null\n@@ -1,2 +0,0 @@\n-a\n-b\n"
        );
        let patch = Patch::parse(p.as_bytes()).unwrap();
        let file = &patch.files()[0];
        assert_eq!(file.change(), PatchChange::Delete);
        assert_eq!((file.old_mode(), file.new_mode()), (Some(FileMode::Executable), None));
        assert_eq!(file.apply(Some(b"a\nb\n"), ApplyMode::Exact).unwrap(), None);
        // Content differs from what the patch removes: rejected, no fuzz.
        assert!(matches!(file.apply(Some(b"a\nc\n"), ApplyMode::AllowOffset), Err(ApplyError::HunkDoesNotApply { .. })));
        // Base has more than the patch removes.
        assert!(matches!(file.apply(Some(b"a\nb\nc\n"), ApplyMode::AllowOffset), Err(ApplyError::HunkDoesNotApply { .. })));
        assert!(matches!(file.apply(None, ApplyMode::Exact), Err(ApplyError::Missing { .. })));
    }

    #[test]
    fn modify_single_hunk() {
        let base = numbered(1..=10);
        let p = modify("f.txt", "@@ -4,5 +4,5 @@\n line 4\n line 5\n-line 6\n+line six\n line 7\n line 8\n");
        let out = apply(&p, Some(base.as_bytes()), ApplyMode::Exact).unwrap().unwrap();
        assert_eq!(out, base.replace("line 6\n", "line six\n").as_bytes());
        let patch = Patch::parse(p.as_bytes()).unwrap();
        assert_eq!(patch.files()[0].change(), PatchChange::Modify);
        assert_eq!(patch.files()[0].old_mode(), Some(FileMode::Regular));
        assert_eq!(patch.files()[0].added_lines().collect::<Vec<_>>(), vec![(6, b"line six\n".as_slice())]);
        assert!(matches!(apply(&p, None, ApplyMode::Exact), Err(ApplyError::Missing { .. })));
    }

    #[test]
    fn multi_hunk() {
        let base = numbered(1..=30);
        let p = modify(
            "f.txt",
            "@@ -2,7 +2,8 @@\n line 2\n line 3\n line 4\n-line 5\n+line 5a\n+line 5b\n line 6\n line 7\n line 8\n@@ -20,7 +21,6 @@\n line 20\n line 21\n line 22\n-line 23\n line 24\n line 25\n line 26\n",
        );
        let want = base.replace("line 5\n", "line 5a\nline 5b\n").replace("line 23\n", "");
        let patch = Patch::parse(p.as_bytes()).unwrap();
        assert_eq!(patch.stats(), FixStats { files: 1, added: 2, removed: 2 });
        assert_eq!(patch.files()[0].hunks().len(), 2);
        assert_eq!(apply(&p, Some(base.as_bytes()), ApplyMode::Exact).unwrap().unwrap(), want.as_bytes());
        assert_eq!(apply(&p, Some(base.as_bytes()), ApplyMode::AllowOffset).unwrap().unwrap(), want.as_bytes());
    }

    #[test]
    fn shifted_hunk_needs_allow_offset() {
        let hunk = "@@ -8,7 +8,7 @@\n line 8\n line 9\n line 10\n-line 11\n+line eleven\n line 12\n line 13\n line 14\n";
        let p = modify("f.txt", hunk);
        // Two lines inserted above the hunk: the context now sits at +2.
        let shifted = format!("extra 1\nextra 2\n{}", numbered(1..=20));
        let want = shifted.replace("line 11\n", "line eleven\n");
        assert_eq!(apply(&p, Some(shifted.as_bytes()), ApplyMode::AllowOffset).unwrap().unwrap(), want.as_bytes());
        assert_eq!(
            apply(&p, Some(shifted.as_bytes()), ApplyMode::Exact),
            Err(ApplyError::HunkDoesNotApply { path: RepoPath::new("f.txt").unwrap(), hunk: 1, old_start: 8 })
        );
        // Lines removed above: negative offset.
        let up = numbered(3..=20);
        let want = up.replace("line 11\n", "line eleven\n");
        assert_eq!(apply(&p, Some(up.as_bytes()), ApplyMode::AllowOffset).unwrap().unwrap(), want.as_bytes());
    }

    #[test]
    fn offset_search_prefers_the_nearest_match() {
        // Identical blocks at old lines 2-4 and 8-10; the hunk states line 8 → the second block.
        let base = "a\nx\ny\nz\nb\nc\nd\nx\ny\nz\ne\nf\ng\n";
        let p = modify("f.txt", "@@ -8,3 +8,3 @@\n x\n-y\n+Y\n z\n");
        let out = apply(&p, Some(base.as_bytes()), ApplyMode::AllowOffset).unwrap().unwrap();
        assert_eq!(out, b"a\nx\ny\nz\nb\nc\nd\nx\nY\nz\ne\nf\ng\n");
        // One line inserted at the top: the block at 9 (+1) beats the block at 3 (-5).
        let shifted = format!("top\n{base}");
        let out = apply(&p, Some(shifted.as_bytes()), ApplyMode::AllowOffset).unwrap().unwrap();
        assert_eq!(out, format!("top\n{}", "a\nx\ny\nz\nb\nc\nd\nx\nY\nz\ne\nf\ng\n").as_bytes());
        // Equidistant candidates (stated line 8 has no match, lines 7 and 9 do): like git, the
        // forward one (+1) is tried before the backward one (-1).
        let p = modify("f.txt", "@@ -8,2 +8,2 @@\n-y\n+Y\n z\n");
        let base = "a\nb\nc\nd\ne\nf\ny\nz\ny\nz\ng\nh\n";
        let out = apply(&p, Some(base.as_bytes()), ApplyMode::AllowOffset).unwrap().unwrap();
        assert_eq!(out, b"a\nb\nc\nd\ne\nf\ny\nz\nY\nz\ng\nh\n");
    }

    #[test]
    fn hunks_at_file_start_and_end_are_anchored() {
        // Starts at old line 1: must match at the beginning even with AllowOffset.
        let p = modify("f.txt", "@@ -1,3 +1,3 @@\n-line 1\n+LINE 1\n line 2\n line 3\n");
        let base = numbered(1..=10);
        assert!(apply(&p, Some(base.as_bytes()), ApplyMode::AllowOffset).is_ok());
        let shifted = format!("new\n{base}");
        assert!(matches!(apply(&p, Some(shifted.as_bytes()), ApplyMode::AllowOffset), Err(ApplyError::HunkDoesNotApply { .. })));
        // No trailing context: must match at the end.
        let p = modify("f.txt", "@@ -8,3 +8,4 @@\n line 8\n line 9\n line 10\n+line 11\n");
        assert_eq!(apply(&p, Some(base.as_bytes()), ApplyMode::AllowOffset).unwrap().unwrap(), numbered(1..=11).as_bytes());
        let longer = numbered(1..=12);
        assert!(matches!(apply(&p, Some(longer.as_bytes()), ApplyMode::AllowOffset), Err(ApplyError::HunkDoesNotApply { .. })));
    }

    #[test]
    fn changed_context_is_rejected_without_fuzz() {
        let p = modify("f.txt", "@@ -4,5 +4,5 @@\n line 4\n line 5\n-line 6\n+line six\n line 7\n line 8\n");
        let base = numbered(1..=10).replace("line 8\n", "line 8 changed\n");
        for mode in [ApplyMode::Exact, ApplyMode::AllowOffset] {
            assert_eq!(
                apply(&p, Some(base.as_bytes()), mode),
                Err(ApplyError::HunkDoesNotApply { path: RepoPath::new("f.txt").unwrap(), hunk: 1, old_start: 4 })
            );
        }
    }

    #[test]
    fn crlf_is_preserved_byte_for_byte() {
        let base = b"one\r\ntwo\r\nthree\r\n";
        let p = modify("w.txt", "@@ -1,3 +1,3 @@\n one\r\n-two\r\n+TWO\r\n three\r\n");
        assert_eq!(apply(&p, Some(base), ApplyMode::Exact).unwrap().unwrap(), b"one\r\nTWO\r\nthree\r\n");
        // An LF-only base does not match CRLF context.
        assert!(apply(&p, Some(b"one\ntwo\nthree\n"), ApplyMode::AllowOffset).is_err());
    }

    #[test]
    fn missing_trailing_newline_on_each_side() {
        let marker = "\\ No newline at end of file\n";
        // Old side only: the patch adds the final newline.
        let p = modify("f", &format!("@@ -1,2 +1,2 @@\n a\n-b\n{marker}+b\n"));
        assert_eq!(apply(&p, Some(b"a\nb"), ApplyMode::Exact).unwrap().unwrap(), b"a\nb\n");
        assert!(apply(&p, Some(b"a\nb\n"), ApplyMode::AllowOffset).is_err());
        // New side only: the patch removes the final newline.
        let p = modify("f", &format!("@@ -1,2 +1,2 @@\n a\n-b\n+b\n{marker}"));
        assert_eq!(apply(&p, Some(b"a\nb\n"), ApplyMode::Exact).unwrap().unwrap(), b"a\nb");
        // Both sides.
        let p = modify("f", &format!("@@ -1,2 +1,2 @@\n a\n-b\n{marker}+c\n{marker}"));
        assert_eq!(apply(&p, Some(b"a\nb"), ApplyMode::Exact).unwrap().unwrap(), b"a\nc");
        // Unchanged last line without newline as context.
        let p = modify("f", &format!("@@ -1,2 +1,2 @@\n-a\n+A\n b\n{marker}"));
        assert_eq!(apply(&p, Some(b"a\nb"), ApplyMode::Exact).unwrap().unwrap(), b"A\nb");
        let patch = Patch::parse(p.as_bytes()).unwrap();
        assert_eq!(patch.files()[0].post_image_bytes(), 3);
        // Divergence from git: a no-newline pre-image line followed by more base content is a
        // mismatch here (git prefix-matches it and yields `A\nb\ncd`).
        let p = modify("f", &format!("@@ -1,3 +1,3 @@\n-a\n+A\n b\n c\n{marker}"));
        assert_eq!(apply(&p, Some(b"a\nb\nc"), ApplyMode::Exact).unwrap().unwrap(), b"A\nb\nc");
        for mode in [ApplyMode::Exact, ApplyMode::AllowOffset] {
            assert!(matches!(apply(&p, Some(b"a\nb\nc\nd"), mode), Err(ApplyError::HunkDoesNotApply { .. })));
        }
    }

    #[test]
    fn executable_mode_change() {
        let p = "diff --git a/run.sh b/run.sh\nold mode 100644\nnew mode 100755\n";
        let patch = Patch::parse(p.as_bytes()).unwrap();
        let file = &patch.files()[0];
        assert_eq!(file.change(), PatchChange::ModeOnly);
        assert_eq!((file.old_mode(), file.new_mode()), (Some(FileMode::Regular), Some(FileMode::Executable)));
        assert_eq!(file.apply(Some(b"echo\n"), ApplyMode::Exact).unwrap().unwrap(), b"echo\n");
        assert!(matches!(file.apply(None, ApplyMode::Exact), Err(ApplyError::Missing { .. })));
        assert_eq!(patch.stats(), FixStats { files: 1, added: 0, removed: 0 });

        // Mode change together with a content change.
        let p = format!(
            "diff --git a/run.sh b/run.sh\nold mode 100755\nnew mode 100644\nindex {A}..{B}\n--- a/run.sh\n+++ b/run.sh\n@@ -1 +1 @@\n-echo\n+echo hi\n"
        );
        let patch = Patch::parse(p.as_bytes()).unwrap();
        let file = &patch.files()[0];
        assert_eq!(file.change(), PatchChange::Modify);
        assert_eq!((file.old_mode(), file.new_mode()), (Some(FileMode::Executable), Some(FileMode::Regular)));
        assert_eq!(file.apply(Some(b"echo\n"), ApplyMode::Exact).unwrap().unwrap(), b"echo hi\n");
    }

    #[test]
    fn multiple_files_quoted_and_spaced_paths() {
        let p = format!(
            "diff --git a/dir with space/x.txt b/dir with space/x.txt\nindex {A}..{B} 100644\n--- a/dir with space/x.txt\t\n+++ b/dir with space/x.txt\t\n@@ -1 +1 @@\n-a\n+b\ndiff --git \"a/caf\\303\\251.txt\" \"b/caf\\303\\251.txt\"\nindex {A}..{B} 100644\n--- \"a/caf\\303\\251.txt\"\n+++ \"b/caf\\303\\251.txt\"\n@@ -1 +1 @@\n-a\n+b\n"
        );
        let patch = Patch::parse(p.as_bytes()).unwrap();
        let paths: Vec<&str> = patch.paths().map(RepoPath::as_str).collect();
        assert_eq!(paths, vec!["dir with space/x.txt", "café.txt"]);
        assert_eq!(patch.stats(), FixStats { files: 2, added: 2, removed: 2 });
    }

    #[test]
    fn sha256_and_size_are_of_the_raw_bytes() {
        let p = modify("f", "@@ -1 +1 @@\n-a\n+b\n");
        let patch = Patch::parse(p.as_bytes()).unwrap();
        let want: [u8; 32] = Sha256::digest(p.as_bytes()).into();
        assert_eq!(patch.sha256(), want);
        assert_eq!(patch.len_bytes(), p.len() as u64);
    }

    #[test]
    fn hunks_cannot_rematch_patched_lines() {
        // Second hunk's pre-image only exists inside what the first hunk wrote.
        let p = modify("f", "@@ -2,3 +2,3 @@\n a\n-b\n+x\n c\n@@ -9,3 +9,3 @@\n a\n-x\n+y\n c\n");
        let base = b"0\na\nb\nc\n4\n5\n6\n7\n8\n9\n10\n11\n12\n";
        assert!(matches!(apply(&p, Some(base), ApplyMode::AllowOffset), Err(ApplyError::HunkDoesNotApply { hunk: 2, .. })));
    }

    #[test]
    fn malformed_inputs() {
        let ok_head = format!("diff --git a/f b/f\nindex {A}..{B} 100644\n--- a/f\n+++ b/f\n");
        let cases: Vec<(String, PatchError)> = vec![
            (String::new(), PatchError::Empty),
            ("diff --git a/f b/f".into(), PatchError::MissingFinalNewline),
            ("hello\n".into(), PatchError::ExpectedDiffHeader { line: 1 }),
            ("diff --git a/f c/f\n".into(), PatchError::MalformedDiffHeader { line: 1 }),
            ("diff --git a/f b/g\n".into(), PatchError::MalformedDiffHeader { line: 1 }),
            (
                "diff --git a/../x b/../x\n".into(),
                PatchError::InvalidPath { line: 1, reason: "invalid: repo path has a `.` or `..` segment".into() },
            ),
            (
                "diff --git a//etc/passwd b//etc/passwd\n".into(),
                PatchError::InvalidPath { line: 1, reason: "invalid: repo path is absolute".into() },
            ),
            ("diff --git a/old b/new\nsimilarity index 90%\nrename from old\nrename to new\n".into(), PatchError::RenameOrCopy { line: 2 }),
            ("diff --git a/a b/b\ncopy from a\ncopy to b\n".into(), PatchError::RenameOrCopy { line: 2 }),
            (
                format!("diff --git a/i.png b/i.png\nindex {A}..{B} 100644\nBinary files a/i.png and b/i.png differ\n"),
                PatchError::Binary { line: 3 },
            ),
            (format!("diff --git a/i.png b/i.png\nindex {A}..{B} 100644\nGIT binary patch\n"), PatchError::Binary { line: 3 }),
            (
                format!("diff --git a/l b/l\nnew file mode 120000\nindex {Z}..{A}\n--- /dev/null\n+++ b/l\n@@ -0,0 +1 @@\n+target\n"),
                PatchError::Symlink { line: 2, path: RepoPath::new("l").unwrap() },
            ),
            (
                format!(
                    "diff --git a/sub b/sub\nindex {A}..{B} 160000\n--- a/sub\n+++ b/sub\n@@ -1 +1 @@\n-Subproject commit {A}\n+Subproject commit {B}\n"
                ),
                PatchError::Gitlink { line: 2, path: RepoPath::new("sub").unwrap() },
            ),
            (
                "diff --git a/f b/f\nold mode 100644\nnew mode 100664\n".into(),
                PatchError::UnsupportedMode { line: 3, path: RepoPath::new("f").unwrap() },
            ),
            (
                "diff --git a/f b/f\nindex 1111111..2222222 100644\n--- a/f\n+++ b/f\n@@ -1 +1 @@\n-a\n+b\n".into(),
                PatchError::MalformedIndex { line: 2 },
            ),
            ("diff --git a/f b/f\nmystery header\n".into(), PatchError::UnexpectedLine { line: 2 }),
            (format!("{ok_head}@@ -1 +1 @@\n-a\n+b\ntrailing junk\n"), PatchError::UnexpectedLine { line: 8 }),
            (
                format!("diff --git a/f b/f\nnew file mode 100644\ndeleted file mode 100644\nindex {Z}..{A}\n"),
                PatchError::InconsistentHeader { line: 1 },
            ),
            ("diff --git a/f b/f\nold mode 100644\nnew mode 100644\n".into(), PatchError::InconsistentHeader { line: 1 }),
            (
                format!("diff --git a/f b/f\nindex {A}..{B} 100644\n--- a/f\n+++ b/g\n@@ -1 +1 @@\n-a\n+b\n"),
                PatchError::InconsistentHeader { line: 3 },
            ),
            (
                format!("diff --git a/f b/f\nnew file mode 100644\nindex {Z}..{A}\n--- a/f\n+++ b/f\n@@ -0,0 +1 @@\n+a\n"),
                PatchError::InconsistentHeader { line: 4 },
            ),
            (ok_head.to_string(), PatchError::MissingHunks { line: 5 }),
            (format!("diff --git a/f b/f\nindex {A}..{B} 100644\n"), PatchError::MissingHunks { line: 1 }),
            (format!("{ok_head}@@ -1 +1\n-a\n+b\n"), PatchError::MalformedHunkHeader { line: 5 }),
            (format!("{ok_head}@@ -0,1 +1 @@\n-a\n+b\n"), PatchError::MalformedHunkHeader { line: 5 }),
            (format!("{ok_head}@@ -1,2 +1,2 @@\n-a\n+b\n"), PatchError::HunkCountMismatch { line: 5 }),
            (format!("{ok_head}@@ -1 +1 @@\n-a\n-b\n+b\n"), PatchError::HunkCountMismatch { line: 7 }),
            (format!("{ok_head}@@ -1,2 +1,2 @@\n\n-a\n+b\n"), PatchError::HunkCountMismatch { line: 6 }),
            (
                format!("{ok_head}@@ -1,2 +1,2 @@\n-a\n\\ No newline at end of file\n-b\n+a\n+b\n"),
                PatchError::MisplacedNoNewline { line: 8 },
            ),
            (format!("{ok_head}@@ -1 +1 @@\n\\ No newline at end of file\n-a\n+b\n"), PatchError::MisplacedNoNewline { line: 6 }),
            (
                format!("{ok_head}@@ -1 +1 @@\n-a\n+b\n\\ No newline at end of file\n\\ No newline at end of file\n"),
                PatchError::MisplacedNoNewline { line: 9 },
            ),
            (
                format!("{ok_head}@@ -1 +1 @@\n-a\n+b\n\\ No newline at end of file\n@@ -9 +9 @@\n-c\n+d\n"),
                PatchError::MisplacedNoNewline { line: 9 },
            ),
            (format!("{ok_head}@@ -9 +9 @@\n-c\n+d\n@@ -1 +1 @@\n-a\n+b\n"), PatchError::HunkOrder { line: 8 }),
            (format!("{ok_head}@@ -1,3 +1,3 @@\n a\n-b\n+B\n c\n@@ -3 +3 @@\n-c\n+C\n"), PatchError::HunkOrder { line: 10 }),
            (
                format!("{ok_head}@@ -1 +1 @@\n-a\n+b\n{ok_head}@@ -1 +1 @@\n-a\n+b\n"),
                PatchError::DuplicatePath { path: RepoPath::new("f").unwrap() },
            ),
        ];
        for (input, want) in cases {
            assert_eq!(parse_err(&input), want, "input:\n{input}");
        }
    }
}
