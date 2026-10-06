//! Parsers for git CLI output: `ls-tree -r -z -l`, raw commit objects, `diff-tree -z --raw
//! --numstat`, unified hunks, `blame --porcelain`. Pure functions over bytes.

use std::collections::HashMap;

use mage_core::domain::{BlobOid, CommitSha, RepoPath, TreeOid};
use mage_core::wire::indexer::{BlameHunk, ChangeKind, CommitInfo, FileChange, Hunk};
use time::OffsetDateTime;

use super::bundle::{RawEntryKind, RawTreeEntry};
use crate::ports::git::GitError;

/// `CommitInfo.body` cap.
pub const BODY_MAX_BYTES: usize = 4096;

fn bad(what: &str) -> GitError {
    GitError::Unavailable(format!("unexpected git output: {what}"))
}

fn hex20<T: std::str::FromStr>(s: &[u8], what: &str) -> Result<T, GitError> {
    std::str::from_utf8(s).ok().and_then(|s| s.parse().ok()).ok_or_else(|| bad(what))
}

fn unix_time(s: &str) -> Result<OffsetDateTime, GitError> {
    let secs: i64 = s.trim().parse().map_err(|_| bad("timestamp"))?;
    OffsetDateTime::from_unix_timestamp(secs).map_err(|_| bad("timestamp"))
}

/// `git ls-tree -r -z -l --full-tree`: `<mode> SP <type> SP <oid> SP+ <size>\t<path>\0`.
pub fn ls_tree(out: &[u8]) -> Result<Vec<RawTreeEntry>, GitError> {
    let mut entries = Vec::new();
    for record in out.split(|b| *b == 0).filter(|r| !r.is_empty()) {
        let tab = record.iter().position(|b| *b == b'\t').ok_or_else(|| bad("ls-tree record"))?;
        let (meta, path) = (&record[..tab], &record[tab + 1..]);
        let mut fields = meta.split(|b| *b == b' ').filter(|f| !f.is_empty());
        let (Some(mode), Some(_kind), Some(oid), Some(size)) = (fields.next(), fields.next(), fields.next(), fields.next()) else {
            return Err(bad("ls-tree record"));
        };
        let size = std::str::from_utf8(size).ok().and_then(|s| s.parse::<u64>().ok());
        let kind = match mode {
            b"100644" | b"100664" => RawEntryKind::File { oid: hex20(oid, "blob oid")?, executable: false },
            b"100755" => RawEntryKind::File { oid: hex20(oid, "blob oid")?, executable: true },
            b"120000" => RawEntryKind::Symlink { oid: hex20(oid, "blob oid")? },
            b"160000" => RawEntryKind::Submodule { commit: hex20(oid, "gitlink")? },
            // `-r` never lists trees; anything else is not a checkout-able entry.
            _ => continue,
        };
        let size = if matches!(kind, RawEntryKind::Submodule { .. }) { None } else { Some(size.ok_or_else(|| bad("blob size"))?) };
        entries.push(RawTreeEntry { path: path.to_vec(), kind, size });
    }
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(entries)
}

/// A raw commit object (`git cat-file commit`).
pub fn commit_object(sha: CommitSha, raw: &[u8]) -> Result<CommitInfo, GitError> {
    let split = raw.windows(2).position(|w| w == b"\n\n");
    let (headers, message) = match split {
        Some(i) => (&raw[..i], &raw[i + 2..]),
        None => (raw, &b""[..]),
    };
    let headers = String::from_utf8_lossy(headers);
    let mut tree = None;
    let mut parents = Vec::new();
    let mut author = None;
    let mut committer_time = None;
    for line in headers.lines() {
        if line.starts_with(' ') {
            continue; // continuation of a multi-line header (gpgsig, mergetag)
        }
        let (key, value) = line.split_once(' ').unwrap_or((line, ""));
        match key {
            "tree" => tree = Some(hex20::<TreeOid>(value.as_bytes(), "tree")?),
            "parent" => parents.push(hex20::<CommitSha>(value.as_bytes(), "parent")?),
            "author" => author = Some(signature(value)?),
            "committer" => committer_time = Some(signature(value)?.1),
            _ => {}
        }
    }
    let tree = tree.ok_or_else(|| bad("commit without tree"))?;
    let (author_name, author_time) = author.ok_or_else(|| bad("commit without author"))?;
    let committer_time = committer_time.ok_or_else(|| bad("commit without committer"))?;
    let (subject, body) = split_message(&String::from_utf8_lossy(message));
    Ok(CommitInfo { sha, tree, parents, author_name, author_time, committer_time, subject, body, files: None })
}

/// `Name <email> <unix> <tz>` → name, time.
fn signature(value: &str) -> Result<(String, OffsetDateTime), GitError> {
    let lt = value.find(" <").or_else(|| value.find('<')).ok_or_else(|| bad("signature"))?;
    let gt = value.rfind('>').ok_or_else(|| bad("signature"))?;
    let name = value[..lt].trim().to_owned();
    let when = value[gt + 1..].split_whitespace().next().ok_or_else(|| bad("signature time"))?;
    Ok((name, unix_time(when)?))
}

/// Subject = first paragraph joined with spaces (git `%s`); body = the rest without leading
/// blank lines and trailing whitespace, truncated to [`BODY_MAX_BYTES`].
pub fn split_message(message: &str) -> (String, String) {
    let mut lines = message.lines().skip_while(|l| l.trim().is_empty()).peekable();
    let mut subject_lines = Vec::new();
    while let Some(line) = lines.next_if(|l| !l.trim().is_empty()) {
        subject_lines.push(line.trim());
    }
    let rest: Vec<&str> = lines.skip_while(|l| l.trim().is_empty()).collect();
    let mut body = rest.join("\n").trim_end().to_owned();
    truncate_utf8(&mut body, BODY_MAX_BYTES);
    (subject_lines.join(" "), body)
}

pub fn truncate_utf8(s: &mut String, max: usize) {
    if s.len() <= max {
        return;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
}

/// One changed file from `diff-tree --raw` merged with its `--numstat` counts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawChange {
    pub change: FileChange,
    pub old_oid: Option<BlobOid>,
    pub new_oid: Option<BlobOid>,
    pub binary: bool,
}

/// `git diff-tree -r -z --raw --numstat [-M]` output. Paths that are not valid `RepoPath`s are
/// dropped. Sorted by path.
pub fn raw_numstat(out: &[u8]) -> Result<Vec<RawChange>, GitError> {
    let mut tokens = out.split(|b| *b == 0).peekable();
    let mut changes: Vec<RawChange> = Vec::new();
    let mut stats: HashMap<Vec<u8>, (Option<u32>, Option<u32>)> = HashMap::new();
    while let Some(token) = tokens.next() {
        if token.is_empty() {
            continue;
        }
        if token[0] == b':' {
            let meta = String::from_utf8_lossy(&token[1..]).into_owned();
            let fields: Vec<&str> = meta.split(' ').collect();
            let [old_mode, new_mode, old_oid, new_oid, status] = fields[..] else {
                return Err(bad("raw diff record"));
            };
            let first = tokens.next().ok_or_else(|| bad("raw diff path"))?.to_vec();
            let letter = status.chars().next().ok_or_else(|| bad("raw diff status"))?;
            let (old_path, path) = if matches!(letter, 'R' | 'C') { (Some(first), tokens.next().ok_or_else(|| bad("rename path"))?.to_vec()) } else { (None, first) };
            let change = match letter {
                'A' | 'C' => ChangeKind::Added,
                'D' => ChangeKind::Deleted,
                'R' => ChangeKind::Renamed,
                'T' => ChangeKind::TypeChanged,
                _ => ChangeKind::Modified,
            };
            let oid = |mode: &str, oid: &str| -> Option<BlobOid> { if mode == "000000" || mode == "160000" { None } else { oid.parse().ok() } };
            let Ok(path_str) = String::from_utf8(path.clone()) else { continue };
            let Ok(repo_path) = RepoPath::new(path_str) else { continue };
            let old_repo_path = match (letter, old_path) {
                ('R', Some(old)) => match String::from_utf8(old).ok().and_then(|s| RepoPath::new(s).ok()) {
                    Some(p) => Some(p),
                    None => continue,
                },
                _ => None,
            };
            changes.push(RawChange {
                change: FileChange { path: repo_path, old_path: old_repo_path, change, adds: None, dels: None },
                old_oid: oid(old_mode, old_oid),
                new_oid: oid(new_mode, new_oid),
                binary: false,
            });
        } else {
            // numstat: `adds\tdels\tpath` or `adds\tdels\t` + old + new for renames.
            let text = String::from_utf8_lossy(token).into_owned();
            let mut parts = text.splitn(3, '\t');
            let (Some(adds), Some(dels), Some(rest)) = (parts.next(), parts.next(), parts.next()) else {
                return Err(bad("numstat record"));
            };
            let path = if rest.is_empty() {
                let _old = tokens.next().ok_or_else(|| bad("numstat rename"))?;
                tokens.next().ok_or_else(|| bad("numstat rename"))?.to_vec()
            } else {
                token[adds.len() + dels.len() + 2..].to_vec()
            };
            stats.insert(path, (adds.parse().ok(), dels.parse().ok()));
        }
    }
    for change in &mut changes {
        if let Some((adds, dels)) = stats.get(change.change.path.as_str().as_bytes()) {
            change.change.adds = *adds;
            change.change.dels = *dels;
            change.binary = adds.is_none() && dels.is_none();
        }
    }
    changes.sort_by(|a, b| a.change.path.cmp(&b.change.path));
    Ok(changes)
}

/// Unified diff output of one file → hunks, cut when the hunk text would exceed `max_bytes`.
/// Returns `(hunks, truncated)`.
pub fn hunks(patch: &[u8], max_bytes: u32) -> Result<(Vec<Hunk>, bool), GitError> {
    let text = String::from_utf8_lossy(patch);
    let mut hunks: Vec<Hunk> = Vec::new();
    let mut current: Option<Hunk> = None;
    for line in text.split_inclusive('\n') {
        if let Some(header) = line.strip_prefix("@@ ") {
            if let Some(h) = current.take() {
                hunks.push(h);
            }
            current = Some(hunk_header(header)?);
        } else if let Some(h) = current.as_mut() {
            if line.starts_with([' ', '-', '+', '\\']) {
                h.text.push_str(line);
                if !line.ends_with('\n') {
                    h.text.push('\n');
                }
            }
        }
    }
    if let Some(h) = current.take() {
        hunks.push(h);
    }
    Ok(cut_hunks(hunks, max_bytes))
}

/// Keeps whole hunks while their total text fits in `max_bytes`.
pub fn cut_hunks(hunks: Vec<Hunk>, max_bytes: u32) -> (Vec<Hunk>, bool) {
    let mut total = 0usize;
    let mut kept = Vec::with_capacity(hunks.len());
    let count = hunks.len();
    for hunk in hunks {
        total += hunk.text.len();
        if total > max_bytes as usize {
            break;
        }
        kept.push(hunk);
    }
    let truncated = kept.len() < count;
    (kept, truncated)
}

/// `-a,b +c,d @@ ...` (counts default to 1).
fn hunk_header(header: &str) -> Result<Hunk, GitError> {
    let mut parts = header.split(' ');
    let old = parts.next().and_then(|p| p.strip_prefix('-')).ok_or_else(|| bad("hunk header"))?;
    let new = parts.next().and_then(|p| p.strip_prefix('+')).ok_or_else(|| bad("hunk header"))?;
    let range = |r: &str| -> Result<(u32, u32), GitError> {
        let (start, count) = r.split_once(',').unwrap_or((r, "1"));
        Ok((start.parse().map_err(|_| bad("hunk start"))?, count.parse().map_err(|_| bad("hunk count"))?))
    };
    let (old_start, old_lines) = range(old)?;
    let (new_start, new_lines) = range(new)?;
    Ok(Hunk { old_start, old_lines, new_start, new_lines, text: String::new() })
}

#[derive(Default)]
struct BlameCommit {
    author_name: String,
    author_time: Option<OffsetDateTime>,
    subject: String,
    filename: Option<String>,
}

/// `git blame --porcelain` → hunks in final line order (one per porcelain group).
pub fn blame_porcelain(out: &[u8]) -> Result<Vec<BlameHunk>, GitError> {
    let text = String::from_utf8_lossy(out);
    let mut commits: HashMap<CommitSha, BlameCommit> = HashMap::new();
    // (commit, orig_start, final_start, lines)
    let mut groups: Vec<(CommitSha, u32, u32, u32, Option<String>)> = Vec::new();
    let mut current: Option<CommitSha> = None;
    for line in text.lines() {
        if line.starts_with('\t') {
            current = None;
            continue;
        }
        if current.is_none() {
            let fields: Vec<&str> = line.split(' ').collect();
            if fields.len() < 3 {
                return Err(bad("blame header"));
            }
            let sha: CommitSha = hex20(fields[0].as_bytes(), "blame sha")?;
            commits.entry(sha).or_default();
            if fields.len() >= 4 {
                let num = |s: &str| s.parse::<u32>().map_err(|_| bad("blame line numbers"));
                groups.push((sha, num(fields[1])?, num(fields[2])?, num(fields[3])?, None));
            }
            current = Some(sha);
            continue;
        }
        let Some(sha) = current else { continue };
        let (key, value) = line.split_once(' ').unwrap_or((line, ""));
        let entry = commits.entry(sha).or_default();
        match key {
            "author" => entry.author_name = value.to_owned(),
            "author-time" => entry.author_time = Some(unix_time(value)?),
            "summary" => entry.subject = value.to_owned(),
            "filename" => {
                entry.filename = Some(value.to_owned());
                if let Some(group) = groups.last_mut().filter(|g| g.0 == sha) {
                    group.4 = Some(value.to_owned());
                }
            }
            _ => {}
        }
    }
    let mut hunks = Vec::with_capacity(groups.len());
    for (sha, orig_start, start, lines, filename) in groups {
        let info = commits.get(&sha).ok_or_else(|| bad("blame commit"))?;
        let path = filename.or_else(|| info.filename.clone()).ok_or_else(|| bad("blame filename"))?;
        hunks.push(BlameHunk {
            start,
            lines,
            commit: sha,
            author_name: info.author_name.clone(),
            author_time: info.author_time.ok_or_else(|| bad("blame author-time"))?,
            subject: info.subject.clone(),
            orig_path: RepoPath::new(path).map_err(|e| bad(&format!("blame filename: {e}")))?,
            orig_start,
        });
    }
    Ok(hunks)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ls_tree_parses_modes_and_sorts() {
        let oid = "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391";
        let sub = "1111111111111111111111111111111111111111";
        let out = format!(
            "100755 blob {oid}      12\tz/run\0120000 blob {oid}       3\tlink\0160000 commit {sub}       -\tvendor\0100644 blob {oid}       0\ta.txt\0"
        );
        let entries = ls_tree(out.as_bytes()).unwrap();
        let paths: Vec<_> = entries.iter().map(|e| String::from_utf8(e.path.clone()).unwrap()).collect();
        assert_eq!(paths, ["a.txt", "link", "vendor", "z/run"]);
        assert!(matches!(entries[3].kind, RawEntryKind::File { executable: true, .. }));
        assert_eq!(entries[3].size, Some(12));
        assert!(matches!(entries[1].kind, RawEntryKind::Symlink { .. }));
        assert_eq!(entries[2].size, None);
    }

    #[test]
    fn message_split_and_truncation() {
        assert_eq!(split_message("Fix it\n\nBody line\n\nmore\n"), ("Fix it".into(), "Body line\n\nmore".into()));
        assert_eq!(split_message("one\ntwo\n"), ("one two".into(), String::new()));
        let (_, body) = split_message(&format!("s\n\n{}", "é".repeat(3000)));
        assert!(body.len() <= BODY_MAX_BYTES && body.len() > BODY_MAX_BYTES - 2);
    }

    #[test]
    fn raw_numstat_merges_renames_and_binary() {
        let a = "1".repeat(40);
        let b = "2".repeat(40);
        let z = "0".repeat(40);
        let out = format!(
            ":100644 100644 {a} {b} M\0src/x.rs\0:100644 100644 {a} {a} R100\0old.txt\0new.txt\0:000000 100644 {z} {b} A\0img.png\0\
             3\t1\tsrc/x.rs\00\t0\t\0old.txt\0new.txt\0-\t-\timg.png\0"
        );
        let changes = raw_numstat(out.as_bytes()).unwrap();
        assert_eq!(changes.len(), 3);
        assert_eq!(changes[0].change.path.as_str(), "img.png");
        assert!(changes[0].binary && changes[0].change.adds.is_none());
        assert_eq!(changes[1].change.change, ChangeKind::Renamed);
        assert_eq!(changes[1].change.old_path.as_ref().map(RepoPath::as_str), Some("old.txt"));
        assert_eq!((changes[2].change.adds, changes[2].change.dels), (Some(3), Some(1)));
    }

    #[test]
    fn hunks_parse_and_cut() {
        let patch = b"diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1,3 +1,3 @@ fn x\n a\n-b\n+B\n c\n@@ -10 +10 @@\n-y\n+Y\n";
        let (hunks, truncated) = hunks(patch, 1000).unwrap();
        assert!(!truncated);
        assert_eq!(hunks[0], Hunk { old_start: 1, old_lines: 3, new_start: 1, new_lines: 3, text: " a\n-b\n+B\n c\n".into() });
        assert_eq!((hunks[1].old_start, hunks[1].old_lines), (10, 1));
        let (cut, truncated) = super::hunks(patch, 14).unwrap();
        assert!(truncated);
        assert_eq!(cut.len(), 1);
    }
}
