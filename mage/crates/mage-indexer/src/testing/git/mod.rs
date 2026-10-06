//! `GitRepoReader` fake ([`MemGitRepo`]: in-memory remotes and mirrors with real git object ids),
//! fixture repos ([`FixtureRepo`]) and the port contract suite [`contract_git_repo_reader`]
//! (`contract.rs`; knowledge.md "Testing"), run against the fake and
//! `adapters::driven::git::GitCli`.
//!
//! Fake approximations (fixtures are small): line diffs use an LCS (git uses Myers; both agree on
//! unambiguous edits), rename detection is exact-content only, `blame` walks first parents and
//! does not follow renames, `log` path limiting keeps commits not identical under the prefix to
//! every parent (git's default simplification, exact for linear history).

mod contract;

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use mage_core::domain::{BlobOid, CommitSha, LineRange, RepoPath, TreeOid};
use mage_core::ports::source_host::{AccessToken, FETCH_USERNAME, FetchSpec, InstallationToken};
use mage_core::wire::indexer::{BlameHunk, ChangeKind, CommitInfo, FileChange, FileDiff, Hunk, LogQuery, TreeDiffOptions};
use parking_lot::{Mutex, MutexGuard};
use sha1::{Digest as _, Sha1};
use time::OffsetDateTime;

pub use self::contract::{BundleFile, GitContractSubject, contract_git_repo_reader, read_bundle};
use crate::adapters::driven::git::bundle::{BundleWriter, Checkout, RawEntryKind, RawTreeEntry, TreePlan};
use crate::ports::git::{
    ArchiveInfo, BlobRead, FetchOutcome, FetchWants, GitError, GitRepoReader, MirrorRef, MirrorState, TreeEntry, TreeEntryKind, TreeListing,
};

// ---------------------------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FixtureRepo {
    pub default_branch: String,
    pub commits: Vec<FixtureCommit>,
    /// Full ref name (`refs/heads/main`, `refs/tags/v1`) → commit index. Commits no ref reaches
    /// exist on the remote but are unreachable (a force-pushed branch).
    pub refs: Vec<(String, usize)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FixtureCommit {
    /// Indices of earlier commits.
    pub parents: Vec<usize>,
    /// Full tree.
    pub tree: Vec<FixtureFile>,
    pub subject: String,
    pub body: String,
    pub author_name: String,
    /// Author = committer time.
    pub time: OffsetDateTime,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FixtureFile {
    pub path: String,
    pub entry: FixtureEntry,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FixtureEntry {
    File { content: Vec<u8>, executable: bool },
    Symlink { target: String },
}

impl FixtureRepo {
    /// Linear history on `branch`: each commit's parent is the previous one,
    /// `refs/heads/<branch>` → last.
    pub fn linear(branch: &str, commits: Vec<FixtureCommit>) -> Self {
        let mut commits = commits;
        for (i, commit) in commits.iter_mut().enumerate() {
            commit.parents = if i == 0 { Vec::new() } else { vec![i - 1] };
        }
        let refs = if commits.is_empty() { Vec::new() } else { vec![(format!("refs/heads/{branch}"), commits.len() - 1)] };
        FixtureRepo { default_branch: branch.to_owned(), commits, refs }
    }

    pub fn with_ref(mut self, name: &str, commit: usize) -> Self {
        self.refs.push((name.to_owned(), commit));
        self
    }
}

impl FixtureCommit {
    /// Text files, not executable, author "Ada", no parents.
    pub fn new(subject: &str, time: OffsetDateTime, files: &[(&str, &str)]) -> Self {
        FixtureCommit {
            parents: Vec::new(),
            tree: files.iter().map(|(p, c)| FixtureFile { path: (*p).to_owned(), entry: FixtureEntry::File { content: c.as_bytes().to_vec(), executable: false } }).collect(),
            subject: subject.to_owned(),
            body: String::new(),
            author_name: "Ada".to_owned(),
            time,
        }
    }

    pub fn with_body(mut self, body: &str) -> Self {
        self.body = body.to_owned();
        self
    }

    pub fn with_parents(mut self, parents: &[usize]) -> Self {
        self.parents = parents.to_vec();
        self
    }

    pub fn with_executable(mut self, path: &str, content: &str) -> Self {
        self.tree.push(FixtureFile { path: path.to_owned(), entry: FixtureEntry::File { content: content.as_bytes().to_vec(), executable: true } });
        self
    }

    pub fn with_symlink(mut self, path: &str, target: &str) -> Self {
        self.tree.push(FixtureFile { path: path.to_owned(), entry: FixtureEntry::Symlink { target: target.to_owned() } });
        self
    }
}

/// Email of fixture authors: `<lowercased name, spaces as dots>@example.invalid`.
pub fn fixture_email(name: &str) -> String {
    format!("{}@example.invalid", name.to_lowercase().replace(' ', "."))
}

/// Commit message of a fixture commit: `subject\n` or `subject\n\nbody\n`.
pub fn fixture_message(subject: &str, body: &str) -> String {
    if body.is_empty() { format!("{subject}\n") } else { format!("{subject}\n\n{body}\n") }
}

/// Git blob id of `content`.
pub fn git_blob_oid(content: &[u8]) -> BlobOid {
    BlobOid(git_object_id("blob", content))
}

fn git_object_id(kind: &str, body: &[u8]) -> [u8; 20] {
    let mut hasher = Sha1::new();
    hasher.update(format!("{kind} {}\0", body.len()).as_bytes());
    hasher.update(body);
    hasher.finalize().into()
}

/// A `FetchSpec` for `url` with a dummy token (fixture remotes ignore credentials).
pub fn test_fetch_spec(url: &str) -> FetchSpec {
    let url = match url.parse() {
        Ok(url) => url,
        Err(e) => panic!("test fetch url {url:?}: {e}"),
    };
    FetchSpec {
        url,
        username: FETCH_USERNAME.to_owned(),
        token: InstallationToken {
            token: AccessToken::new("ghs_contract_token"),
            expires_at: OffsetDateTime::UNIX_EPOCH,
            permissions: BTreeMap::new(),
            repositories: None,
        },
    }
}

// ---------------------------------------------------------------------------------------------
// In-memory object model
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MemKind {
    File { executable: bool },
    Symlink,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MemEntry {
    oid: BlobOid,
    kind: MemKind,
}

impl MemEntry {
    fn mode(&self) -> &'static str {
        match self.kind {
            MemKind::File { executable: false } => "100644",
            MemKind::File { executable: true } => "100755",
            MemKind::Symlink => "120000",
        }
    }
}

#[derive(Debug, Clone)]
struct MemCommit {
    info: CommitInfo,
    files: Arc<BTreeMap<String, MemEntry>>,
}

#[derive(Debug, Clone, Default)]
struct ObjectDb {
    commits: HashMap<CommitSha, MemCommit>,
    blobs: HashMap<BlobOid, Arc<Vec<u8>>>,
}

impl ObjectDb {
    /// Copies `tip` and every ancestor (with their blobs) from `src`.
    fn copy_reachable(&mut self, src: &ObjectDb, tip: CommitSha) {
        let mut stack = vec![tip];
        while let Some(sha) = stack.pop() {
            if self.commits.contains_key(&sha) {
                continue;
            }
            let Some(commit) = src.commits.get(&sha) else { continue };
            for entry in commit.files.values() {
                if let Some(blob) = src.blobs.get(&entry.oid) {
                    self.blobs.entry(entry.oid).or_insert_with(|| blob.clone());
                }
            }
            stack.extend(commit.info.parents.iter().copied());
            self.commits.insert(sha, commit.clone());
        }
    }

    fn reachable(&self, tips: &[CommitSha]) -> HashSet<CommitSha> {
        let mut seen = HashSet::new();
        let mut stack: Vec<CommitSha> = tips.to_vec();
        while let Some(sha) = stack.pop() {
            if !seen.insert(sha) {
                continue;
            }
            if let Some(commit) = self.commits.get(&sha) {
                stack.extend(commit.info.parents.iter().copied());
            }
        }
        seen
    }
}

/// Git tree object id of a flat path → entry map.
fn tree_id(files: &BTreeMap<String, MemEntry>) -> TreeOid {
    let entries: Vec<(&str, MemEntry)> = files.iter().map(|(p, e)| (p.as_str(), *e)).collect();
    TreeOid(write_tree(&entries))
}

fn write_tree(entries: &[(&str, MemEntry)]) -> [u8; 20] {
    // (sort key, mode, name, id)
    let mut items: Vec<(Vec<u8>, &str, String, [u8; 20])> = Vec::new();
    let mut dirs: BTreeMap<&str, Vec<(&str, MemEntry)>> = BTreeMap::new();
    for (path, entry) in entries {
        match path.split_once('/') {
            Some((dir, rest)) => dirs.entry(dir).or_default().push((rest, *entry)),
            None => items.push((path.as_bytes().to_vec(), entry.mode(), (*path).to_owned(), entry.oid.0)),
        }
    }
    for (dir, children) in dirs {
        let mut key = dir.as_bytes().to_vec();
        key.push(b'/');
        items.push((key, "40000", dir.to_owned(), write_tree(&children)));
    }
    items.sort_by(|a, b| a.0.cmp(&b.0));
    let mut body = Vec::new();
    for (_, mode, name, id) in items {
        body.extend_from_slice(mode.as_bytes());
        body.push(b' ');
        body.extend_from_slice(name.as_bytes());
        body.push(0);
        body.extend_from_slice(&id);
    }
    git_object_id("tree", &body)
}

fn build_remote(fixture: &FixtureRepo) -> (ObjectDb, Vec<CommitSha>) {
    let mut db = ObjectDb::default();
    let mut shas: Vec<CommitSha> = Vec::with_capacity(fixture.commits.len());
    for (index, commit) in fixture.commits.iter().enumerate() {
        let mut files = BTreeMap::new();
        for file in &commit.tree {
            let (content, kind) = match &file.entry {
                FixtureEntry::File { content, executable } => (content.clone(), MemKind::File { executable: *executable }),
                FixtureEntry::Symlink { target } => (target.as_bytes().to_vec(), MemKind::Symlink),
            };
            let oid = git_blob_oid(&content);
            db.blobs.insert(oid, Arc::new(content));
            files.insert(file.path.clone(), MemEntry { oid, kind });
        }
        let tree = tree_id(&files);
        let parents: Vec<CommitSha> = commit
            .parents
            .iter()
            .map(|p| match shas.get(*p) {
                Some(sha) if *p < index => *sha,
                _ => panic!("fixture commit {index}: parent {p} is not an earlier commit"),
            })
            .collect();
        let when = commit.time.unix_timestamp();
        let signature = format!("{} <{}> {when} +0000", commit.author_name, fixture_email(&commit.author_name));
        let mut object = format!("tree {tree}\n");
        for parent in &parents {
            object.push_str(&format!("parent {parent}\n"));
        }
        object.push_str(&format!("author {signature}\ncommitter {signature}\n\n{}", fixture_message(&commit.subject, &commit.body)));
        let sha = CommitSha(git_object_id("commit", object.as_bytes()));
        let time = OffsetDateTime::from_unix_timestamp(when).unwrap_or(OffsetDateTime::UNIX_EPOCH);
        let mut body = commit.body.trim_end().to_owned();
        truncate_utf8(&mut body, 4096);
        let info = CommitInfo {
            sha,
            tree,
            parents,
            author_name: commit.author_name.clone(),
            author_time: time,
            committer_time: time,
            subject: commit.subject.clone(),
            body,
            files: None,
        };
        db.commits.insert(sha, MemCommit { info, files: Arc::new(files) });
        shas.push(sha);
    }
    (db, shas)
}

fn truncate_utf8(s: &mut String, max: usize) {
    if s.len() > max {
        let mut end = max;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
    }
}

// ---------------------------------------------------------------------------------------------
// Line diff, hunks, blame
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Equal(usize, usize),
    Delete(usize),
    Insert(usize),
}

fn split_lines(content: &[u8]) -> Vec<&[u8]> {
    if content.is_empty() {
        return Vec::new();
    }
    content.split_inclusive(|b| *b == b'\n').collect()
}

fn is_binary(content: &[u8]) -> bool {
    content[..content.len().min(8000)].contains(&0)
}

fn line_ops(a: &[&[u8]], b: &[&[u8]]) -> Vec<Op> {
    let (n, m) = (a.len(), b.len());
    let mut lcs = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[i][j] = if a[i] == b[j] { lcs[i + 1][j + 1] + 1 } else { lcs[i + 1][j].max(lcs[i][j + 1]) };
        }
    }
    let (mut i, mut j, mut ops) = (0, 0, Vec::with_capacity(n + m));
    while i < n || j < m {
        if i < n && j < m && a[i] == b[j] {
            ops.push(Op::Equal(i, j));
            i += 1;
            j += 1;
        } else if i < n && (j == m || lcs[i + 1][j] >= lcs[i][j + 1]) {
            ops.push(Op::Delete(i));
            i += 1;
        } else {
            ops.push(Op::Insert(j));
            j += 1;
        }
    }
    ops
}

fn push_line(text: &mut String, prefix: char, line: &[u8]) {
    text.push(prefix);
    text.push_str(&String::from_utf8_lossy(line));
    if !line.ends_with(b"\n") {
        text.push_str("\n\\ No newline at end of file\n");
    }
}

fn unified_hunks(old: &[u8], new: &[u8], context: usize) -> Vec<Hunk> {
    let (a, b) = (split_lines(old), split_lines(new));
    let ops = line_ops(&a, &b);
    let changes: Vec<usize> = ops.iter().enumerate().filter(|(_, op)| !matches!(op, Op::Equal(..))).map(|(i, _)| i).collect();
    let mut groups: Vec<(usize, usize)> = Vec::new();
    for idx in changes {
        match groups.last_mut() {
            Some((_, end)) if idx - *end <= 2 * context + 1 => *end = idx,
            _ => groups.push((idx, idx)),
        }
    }
    // Old/new line counts before op index k.
    let mut before = Vec::with_capacity(ops.len() + 1);
    let (mut oi, mut ni) = (0u32, 0u32);
    for op in &ops {
        before.push((oi, ni));
        match op {
            Op::Equal(..) => {
                oi += 1;
                ni += 1;
            }
            Op::Delete(_) => oi += 1,
            Op::Insert(_) => ni += 1,
        }
    }
    groups
        .into_iter()
        .map(|(first, last)| {
            let start = first.saturating_sub(context);
            let end = (last + context).min(ops.len() - 1);
            let mut hunk = Hunk { old_start: 0, old_lines: 0, new_start: 0, new_lines: 0, text: String::new() };
            for op in &ops[start..=end] {
                match *op {
                    Op::Equal(i, _) => {
                        push_line(&mut hunk.text, ' ', a[i]);
                        hunk.old_lines += 1;
                        hunk.new_lines += 1;
                    }
                    Op::Delete(i) => {
                        push_line(&mut hunk.text, '-', a[i]);
                        hunk.old_lines += 1;
                    }
                    Op::Insert(j) => {
                        push_line(&mut hunk.text, '+', b[j]);
                        hunk.new_lines += 1;
                    }
                }
            }
            let (o, n) = before[start];
            hunk.old_start = if hunk.old_lines == 0 { o } else { o + 1 };
            hunk.new_start = if hunk.new_lines == 0 { n } else { n + 1 };
            hunk
        })
        .collect()
}

fn count_changes(old: &[u8], new: &[u8]) -> (u32, u32) {
    let ops = line_ops(&split_lines(old), &split_lines(new));
    let adds = ops.iter().filter(|o| matches!(o, Op::Insert(_))).count();
    let dels = ops.iter().filter(|o| matches!(o, Op::Delete(_))).count();
    (u32::try_from(adds).unwrap_or(u32::MAX), u32::try_from(dels).unwrap_or(u32::MAX))
}

fn within(path: &str, prefixes: &[RepoPath]) -> bool {
    prefixes.is_empty() || prefixes.iter().any(|p| path == p.as_str() || path.strip_prefix(p.as_str()).is_some_and(|r| r.starts_with('/')))
}

struct Change {
    change: FileChange,
    old: Option<MemEntry>,
    new: Option<MemEntry>,
}

/// Changes from `old` to `new` restricted to `paths`, sorted by path.
fn tree_changes(old: &BTreeMap<String, MemEntry>, new: &BTreeMap<String, MemEntry>, paths: &[RepoPath], renames: bool) -> Vec<Change> {
    let mut added = Vec::new();
    let mut deleted = Vec::new();
    let mut out = Vec::new();
    let all: BTreeSet<&String> = old.keys().chain(new.keys()).filter(|p| within(p, paths)).collect();
    for path in all {
        let Ok(repo_path) = RepoPath::new(path.clone()) else { continue };
        match (old.get(path), new.get(path)) {
            (Some(o), Some(n)) if o == n => {}
            (Some(o), Some(n)) => {
                let kind = if (o.kind == MemKind::Symlink) != (n.kind == MemKind::Symlink) { ChangeKind::TypeChanged } else { ChangeKind::Modified };
                out.push(Change { change: FileChange { path: repo_path, old_path: None, change: kind, adds: None, dels: None }, old: Some(*o), new: Some(*n) });
            }
            (None, Some(n)) => added.push((repo_path, *n)),
            (Some(o), None) => deleted.push((repo_path, *o)),
            (None, None) => {}
        }
    }
    let mut used = vec![false; deleted.len()];
    for (path, n) in added {
        let renamed = if renames { deleted.iter().enumerate().position(|(i, (_, o))| !used[i] && o.oid == n.oid) } else { None };
        match renamed {
            Some(i) => {
                used[i] = true;
                let (old_path, o) = deleted[i].clone();
                out.push(Change { change: FileChange { path, old_path: Some(old_path), change: ChangeKind::Renamed, adds: None, dels: None }, old: Some(o), new: Some(n) });
            }
            None => out.push(Change { change: FileChange { path, old_path: None, change: ChangeKind::Added, adds: None, dels: None }, old: None, new: Some(n) }),
        }
    }
    for (i, (path, o)) in deleted.into_iter().enumerate() {
        if !used[i] {
            out.push(Change { change: FileChange { path, old_path: None, change: ChangeKind::Deleted, adds: None, dels: None }, old: Some(o), new: None });
        }
    }
    out.sort_by(|a, b| a.change.path.cmp(&b.change.path));
    out
}

// ---------------------------------------------------------------------------------------------
// MemGitRepo
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GitMethod {
    MirrorState,
    Fetch,
    Pin,
    Unpin,
    Resolve,
    HasCommit,
    Commit,
    ListTree,
    ReadBlobs,
    Diff,
    Log,
    Blame,
    Archive,
    Materialize,
    Maintain,
    RemoveMirror,
    /// Not recorded in [`MemGitRepo::calls`] (no mirror); `fail_next` applies.
    ListMirrors,
}

struct MemRemote {
    db: ObjectDb,
    refs: BTreeMap<String, CommitSha>,
    head: String,
}

#[derive(Default)]
struct MemMirror {
    db: ObjectDb,
    /// Heads, tags and `refs/mage/keep/*`.
    refs: BTreeMap<String, CommitSha>,
    head: Option<String>,
    last_fetch: Option<OffsetDateTime>,
}

impl MemMirror {
    fn commit(&self, sha: CommitSha) -> Result<&MemCommit, GitError> {
        self.db.commits.get(&sha).ok_or(GitError::NotFound)
    }

    fn content(&self, oid: BlobOid) -> Result<&[u8], GitError> {
        self.db.blobs.get(&oid).map(|b| b.as_slice()).ok_or_else(|| GitError::Corrupt(format!("blob {oid} missing")))
    }

    fn heads_and_tags(&self) -> BTreeMap<String, CommitSha> {
        self.refs.iter().filter(|(n, _)| n.starts_with("refs/heads/") || n.starts_with("refs/tags/")).map(|(n, s)| (n.clone(), *s)).collect()
    }

    fn raw_entries(&self, commit: &MemCommit) -> Vec<RawTreeEntry> {
        commit
            .files
            .iter()
            .map(|(path, e)| RawTreeEntry {
                path: path.as_bytes().to_vec(),
                kind: match e.kind {
                    MemKind::File { executable } => RawEntryKind::File { oid: e.oid, executable },
                    MemKind::Symlink => RawEntryKind::Symlink { oid: e.oid },
                },
                size: self.db.blobs.get(&e.oid).map(|b| b.len() as u64),
            })
            .collect()
    }

    fn file_diffs(&self, from: &MemCommit, to: &MemCommit, opts: &TreeDiffOptions) -> Result<Vec<FileDiff>, GitError> {
        let mut diffs = Vec::new();
        for c in tree_changes(&from.files, &to.files, &opts.paths, opts.renames) {
            let old = c.old.map(|e| self.content(e.oid)).transpose()?.unwrap_or(&[]);
            let new = c.new.map(|e| self.content(e.oid)).transpose()?.unwrap_or(&[]);
            let binary = is_binary(old) || is_binary(new);
            let mut change = c.change;
            if !binary {
                let (adds, dels) = count_changes(old, new);
                change.adds = Some(adds);
                change.dels = Some(dels);
            }
            let (mut hunks, mut truncated) = (Vec::new(), false);
            if opts.hunks && !binary {
                let all = unified_hunks(old, new, usize::from(opts.context));
                let count = all.len();
                let mut total = 0usize;
                for hunk in all {
                    total += hunk.text.len();
                    if total > opts.max_bytes_per_file as usize {
                        break;
                    }
                    hunks.push(hunk);
                }
                truncated = hunks.len() < count;
            }
            diffs.push(FileDiff { change, hunks, binary, truncated });
        }
        Ok(diffs)
    }

    fn first_parent_files(&self, commit: &MemCommit) -> Result<Vec<FileChange>, GitError> {
        let empty = BTreeMap::new();
        let parent_files = match commit.info.parents.first() {
            Some(p) => self.commit(*p)?.files.clone(),
            None => Arc::new(empty),
        };
        let parent = MemCommit { info: commit.info.clone(), files: parent_files };
        let opts = TreeDiffOptions { paths: Vec::new(), hunks: false, context: 0, max_bytes_per_file: 0, renames: true };
        Ok(self.file_diffs(&parent, commit, &opts)?.into_iter().map(|d| d.change).collect())
    }

    fn file_lines(&self, commit: &MemCommit, path: &str) -> Result<Option<Vec<Vec<u8>>>, GitError> {
        match commit.files.get(path) {
            Some(e) => Ok(Some(split_lines(self.content(e.oid)?).into_iter().map(<[u8]>::to_vec).collect())),
            None => Ok(None),
        }
    }
}

#[derive(Default)]
struct MemState {
    remotes: HashMap<String, MemRemote>,
    mirrors: BTreeMap<MirrorRef, MemMirror>,
    fail: HashMap<GitMethod, VecDeque<GitError>>,
    calls: Vec<(GitMethod, MirrorRef)>,
}

/// In-memory `GitRepoReader` (module docs).
#[derive(Default)]
pub struct MemGitRepo {
    state: Mutex<MemState>,
}

impl MemGitRepo {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, MemState> {
        self.state.lock()
    }

    /// Registers a remote reachable at `url` (matched against `FetchSpec.url.as_str()`); returns
    /// the commit shas in fixture order.
    pub fn add_remote(&self, url: &str, fixture: &FixtureRepo) -> Vec<CommitSha> {
        let (db, shas) = build_remote(fixture);
        let refs = fixture
            .refs
            .iter()
            .map(|(name, index)| match shas.get(*index) {
                Some(sha) => (name.clone(), *sha),
                None => panic!("fixture ref {name} points at missing commit {index}"),
            })
            .collect();
        self.lock().remotes.insert(url.to_owned(), MemRemote { db, refs, head: format!("refs/heads/{}", fixture.default_branch) });
        shas
    }

    /// Force-push (`Some`) or delete (`None`) a remote ref; commits stay fetchable by id.
    pub fn set_remote_ref(&self, url: &str, refname: &str, target: Option<CommitSha>) {
        let mut state = self.lock();
        let Some(remote) = state.remotes.get_mut(url) else { panic!("no remote {url}") };
        match target {
            Some(sha) => {
                assert!(remote.db.commits.contains_key(&sha), "remote {url} has no commit {sha}");
                remote.refs.insert(refname.to_owned(), sha);
            }
            None => {
                remote.refs.remove(refname);
            }
        }
    }

    /// Next call of `method` fails with `err` (FIFO per method).
    pub fn fail_next(&self, method: GitMethod, err: GitError) {
        self.lock().fail.entry(method).or_default().push_back(err);
    }

    /// Every call in order.
    pub fn calls(&self) -> Vec<(GitMethod, MirrorRef)> {
        self.lock().calls.clone()
    }

    /// Pinned shas of `m`, sorted.
    pub fn pins(&self, m: &MirrorRef) -> Vec<CommitSha> {
        let state = self.lock();
        let Some(mirror) = state.mirrors.get(m) else { return Vec::new() };
        let mut pins: Vec<CommitSha> = mirror.refs.iter().filter(|(n, _)| n.starts_with("refs/mage/keep/")).map(|(_, s)| *s).collect();
        pins.sort();
        pins
    }

    /// Records the call and pops an injected failure.
    fn enter(&self, method: GitMethod, m: Option<&MirrorRef>) -> Result<MutexGuard<'_, MemState>, GitError> {
        let mut state = self.lock();
        if let Some(m) = m {
            state.calls.push((method, *m));
        }
        if let Some(err) = state.fail.get_mut(&method).and_then(VecDeque::pop_front) {
            return Err(err);
        }
        Ok(state)
    }

    fn with_mirror<T>(&self, method: GitMethod, m: &MirrorRef, f: impl FnOnce(&MemMirror) -> Result<T, GitError>) -> Result<T, GitError> {
        let state = self.enter(method, Some(m))?;
        let mirror = state.mirrors.get(m).ok_or(GitError::NotFound)?;
        f(mirror)
    }
}

fn io_err(e: std::io::Error) -> GitError {
    GitError::Unavailable(e.to_string())
}

#[async_trait]
impl GitRepoReader for MemGitRepo {
    async fn mirror_state(&self, m: &MirrorRef) -> Result<MirrorState, GitError> {
        let state = self.enter(GitMethod::MirrorState, Some(m))?;
        Ok(match state.mirrors.get(m) {
            None => MirrorState { exists: false, size_bytes: 0, last_fetch: None },
            Some(mirror) => {
                let blobs: u64 = mirror.db.blobs.values().map(|b| b.len() as u64).sum();
                MirrorState { exists: true, size_bytes: blobs + 256 * mirror.db.commits.len() as u64 + 64, last_fetch: mirror.last_fetch }
            }
        })
    }

    async fn fetch(&self, m: &MirrorRef, spec: &FetchSpec, wants: &FetchWants) -> Result<FetchOutcome, GitError> {
        let mut state = self.enter(GitMethod::Fetch, Some(m))?;
        let MemState { remotes, mirrors, .. } = &mut *state;
        let remote = remotes.get(spec.url.as_str()).ok_or(GitError::NotFound)?;
        let mirror = mirrors.entry(*m).or_default();
        let before = mirror.heads_and_tags();
        if wants.all_heads_and_tags {
            let remote_refs: BTreeMap<&String, &CommitSha> =
                remote.refs.iter().filter(|(n, _)| n.starts_with("refs/heads/") || n.starts_with("refs/tags/")).collect();
            mirror.refs.retain(|name, _| !(name.starts_with("refs/heads/") || name.starts_with("refs/tags/")) || remote_refs.contains_key(name));
            for (name, sha) in remote_refs {
                mirror.db.copy_reachable(&remote.db, *sha);
                mirror.refs.insert(name.clone(), *sha);
            }
            if remote.refs.contains_key(&remote.head) {
                mirror.head = Some(remote.head.clone());
            }
        }
        let mut missing = Vec::new();
        let mut seen = HashSet::new();
        for sha in wants.shas.iter().filter(|s| seen.insert(**s)) {
            if mirror.db.commits.contains_key(sha) {
                continue;
            }
            if remote.db.commits.contains_key(sha) {
                mirror.db.copy_reachable(&remote.db, *sha);
            } else {
                missing.push(*sha);
            }
        }
        let after = mirror.heads_and_tags();
        let updated_refs = after.into_iter().filter(|(name, sha)| before.get(name) != Some(sha)).collect();
        mirror.last_fetch = Some(OffsetDateTime::now_utc());
        Ok(FetchOutcome { updated_refs, missing })
    }

    async fn pin(&self, m: &MirrorRef, sha: CommitSha) -> Result<(), GitError> {
        let mut state = self.enter(GitMethod::Pin, Some(m))?;
        let mirror = state.mirrors.get_mut(m).ok_or(GitError::NotFound)?;
        mirror.commit(sha)?;
        mirror.refs.insert(format!("refs/mage/keep/{sha}"), sha);
        Ok(())
    }

    async fn unpin(&self, m: &MirrorRef, sha: CommitSha) -> Result<(), GitError> {
        let mut state = self.enter(GitMethod::Unpin, Some(m))?;
        let mirror = state.mirrors.get_mut(m).ok_or(GitError::NotFound)?;
        mirror.refs.remove(&format!("refs/mage/keep/{sha}"));
        Ok(())
    }

    async fn resolve(&self, m: &MirrorRef, rev: &str) -> Result<CommitSha, GitError> {
        if rev.is_empty() || rev.starts_with('-') || rev.contains(['\0', '\n', ' ']) {
            // Recorded like the adapter would, then rejected.
            let _state = self.enter(GitMethod::Resolve, Some(m))?;
            return Err(GitError::Invalid(format!("invalid rev {rev:?}")));
        }
        self.with_mirror(GitMethod::Resolve, m, |mirror| {
            if let Ok(sha) = rev.parse::<CommitSha>() {
                return mirror.commit(sha).map(|_| sha);
            }
            if rev == "HEAD" {
                return mirror.head.as_ref().and_then(|h| mirror.refs.get(h)).copied().ok_or(GitError::NotFound);
            }
            [rev.to_owned(), format!("refs/{rev}"), format!("refs/tags/{rev}"), format!("refs/heads/{rev}")]
                .iter()
                .find_map(|name| mirror.refs.get(name))
                .copied()
                .ok_or(GitError::NotFound)
        })
    }

    async fn has_commit(&self, m: &MirrorRef, sha: CommitSha) -> Result<bool, GitError> {
        let state = self.enter(GitMethod::HasCommit, Some(m))?;
        Ok(state.mirrors.get(m).is_some_and(|mirror| mirror.db.commits.contains_key(&sha)))
    }

    async fn commit(&self, m: &MirrorRef, sha: CommitSha) -> Result<CommitInfo, GitError> {
        self.with_mirror(GitMethod::Commit, m, |mirror| Ok(mirror.commit(sha)?.info.clone()))
    }

    async fn list_tree(&self, m: &MirrorRef, sha: CommitSha) -> Result<TreeListing, GitError> {
        self.with_mirror(GitMethod::ListTree, m, |mirror| {
            let commit = mirror.commit(sha)?;
            let mut entries = Vec::with_capacity(commit.files.len());
            let mut skipped_paths = 0;
            for (path, e) in commit.files.iter() {
                let Ok(path) = RepoPath::new(path.clone()) else {
                    skipped_paths += 1;
                    continue;
                };
                let kind = match e.kind {
                    MemKind::File { executable } => TreeEntryKind::File { oid: e.oid, executable },
                    MemKind::Symlink => TreeEntryKind::Symlink { oid: e.oid },
                };
                entries.push(TreeEntry { path, kind, size: Some(mirror.content(e.oid)?.len() as u64) });
            }
            Ok(TreeListing { tree: commit.info.tree, entries, skipped_paths })
        })
    }

    async fn read_blobs(&self, m: &MirrorRef, oids: &[BlobOid], max_bytes: u64) -> Result<Vec<BlobRead>, GitError> {
        self.with_mirror(GitMethod::ReadBlobs, m, |mirror| {
            oids.iter()
                .map(|oid| {
                    let blob = mirror.db.blobs.get(oid).ok_or(GitError::NotFound)?;
                    let size = blob.len() as u64;
                    Ok(BlobRead { oid: *oid, size, bytes: (size <= max_bytes).then(|| blob.to_vec()) })
                })
                .collect()
        })
    }

    async fn diff(&self, m: &MirrorRef, from: CommitSha, to: CommitSha, opts: &TreeDiffOptions) -> Result<Vec<FileDiff>, GitError> {
        self.with_mirror(GitMethod::Diff, m, |mirror| mirror.file_diffs(mirror.commit(from)?, mirror.commit(to)?, opts))
    }

    async fn log(&self, m: &MirrorRef, q: &LogQuery) -> Result<Vec<CommitInfo>, GitError> {
        self.with_mirror(GitMethod::Log, m, |mirror| {
            for sha in q.include.iter().chain(&q.exclude) {
                mirror.commit(*sha)?;
            }
            let excluded = mirror.db.reachable(&q.exclude);
            let mut commits: Vec<&MemCommit> =
                mirror.db.reachable(&q.include).into_iter().filter(|s| !excluded.contains(s)).filter_map(|s| mirror.db.commits.get(&s)).collect();
            if let Some(since) = q.since {
                commits.retain(|c| c.info.committer_time >= since);
            }
            if let Some(prefix) = &q.path_prefix {
                let prefixes = std::slice::from_ref(prefix);
                let under = |files: &BTreeMap<String, MemEntry>| -> Vec<(String, MemEntry)> {
                    files.iter().filter(|(p, _)| within(p, prefixes)).map(|(p, e)| (p.clone(), *e)).collect()
                };
                commits.retain(|c| {
                    let mine = under(&c.files);
                    if c.info.parents.is_empty() {
                        return !mine.is_empty();
                    }
                    c.info.parents.iter().all(|p| mirror.db.commits.get(p).is_none_or(|parent| under(&parent.files) != mine))
                });
            }
            commits.sort_by(|a, b| b.info.committer_time.cmp(&a.info.committer_time).then(a.info.sha.cmp(&b.info.sha)));
            commits
                .into_iter()
                .skip(q.skip as usize)
                .take(q.max as usize)
                .map(|c| {
                    let mut info = c.info.clone();
                    if q.with_files {
                        info.files = Some(mirror.first_parent_files(c)?);
                    }
                    Ok(info)
                })
                .collect()
        })
    }

    async fn blame(&self, m: &MirrorRef, sha: CommitSha, path: &RepoPath, lines: LineRange) -> Result<Vec<BlameHunk>, GitError> {
        self.with_mirror(GitMethod::Blame, m, |mirror| {
            let tip = mirror.commit(sha)?;
            let tip_lines = mirror.file_lines(tip, path.as_str())?.ok_or(GitError::NotFound)?;
            // Like git: a start past the end is an error, the end is clamped.
            let line_count = tip_lines.len();
            if lines.start() as usize > line_count {
                return Err(GitError::Invalid(format!("line range {lines} outside {path}")));
            }
            // Final line (0-based) → (commit, orig line 0-based) once assigned.
            let mut owner: Vec<Option<(CommitSha, usize)>> = vec![None; tip_lines.len()];
            // Unassigned final lines → their index in the current commit's version.
            let mut tracking: Vec<(usize, usize)> = (0..tip_lines.len()).map(|i| (i, i)).collect();
            let mut current = tip;
            let mut current_lines = tip_lines;
            while !tracking.is_empty() {
                let parent = current.info.parents.first().map(|p| mirror.commit(*p)).transpose()?;
                let parent_lines = match parent {
                    Some(p) => mirror.file_lines(p, path.as_str())?,
                    None => None,
                };
                let (Some(parent), Some(parent_lines)) = (parent, parent_lines) else {
                    for (fin, cur) in tracking.drain(..) {
                        owner[fin] = Some((current.info.sha, cur));
                    }
                    break;
                };
                let a: Vec<&[u8]> = parent_lines.iter().map(Vec::as_slice).collect();
                let b: Vec<&[u8]> = current_lines.iter().map(Vec::as_slice).collect();
                let map: HashMap<usize, usize> = line_ops(&a, &b).into_iter().filter_map(|op| if let Op::Equal(i, j) = op { Some((j, i)) } else { None }).collect();
                let mut next = Vec::new();
                for (fin, cur) in tracking.drain(..) {
                    match map.get(&cur) {
                        Some(prev) => next.push((fin, *prev)),
                        None => owner[fin] = Some((current.info.sha, cur)),
                    }
                }
                tracking = next;
                current = parent;
                current_lines = parent_lines;
            }
            let mut hunks: Vec<BlameHunk> = Vec::new();
            let first = (lines.start() - 1) as usize;
            let last = (lines.end() as usize).min(line_count);
            for (fin, slot) in owner[first..last].iter().enumerate().map(|(i, s)| (first + i, s)) {
                let Some((commit, orig)) = *slot else { return Err(GitError::Corrupt("blame left a line unassigned".into())) };
                let (fin_no, orig_no) = (fin as u32 + 1, orig as u32 + 1);
                if let Some(last) = hunks.last_mut() {
                    if last.commit == commit && last.start + last.lines == fin_no && last.orig_start + last.lines == orig_no {
                        last.lines += 1;
                        continue;
                    }
                }
                let info = &mirror.commit(commit)?.info;
                hunks.push(BlameHunk {
                    start: fin_no,
                    lines: 1,
                    commit,
                    author_name: info.author_name.clone(),
                    author_time: info.author_time,
                    subject: info.subject.clone(),
                    orig_path: path.clone(),
                    orig_start: orig_no,
                });
            }
            Ok(hunks)
        })
    }

    async fn archive(&self, m: &MirrorRef, sha: CommitSha, dest: &Path) -> Result<ArchiveInfo, GitError> {
        self.with_mirror(GitMethod::Archive, m, |mirror| {
            let commit = mirror.commit(sha)?;
            let plan = TreePlan::new(&mirror.raw_entries(commit))?;
            let file = std::fs::File::create(dest).map_err(io_err)?;
            let mut writer = BundleWriter::new(std::io::BufWriter::new(file), commit.info.committer_time.unix_timestamp()).map_err(io_err)?;
            for item in &plan.items {
                let content = item.blob().map(|oid| mirror.content(oid)).transpose()?.unwrap_or(&[]);
                writer.append(item, content).map_err(io_err)?;
            }
            let summary = writer.finish().map_err(io_err)?;
            summary.out.into_inner().map_err(|e| io_err(e.into_error()))?;
            Ok(ArchiveInfo { tree: commit.info.tree, bytes: summary.bytes, sha256: summary.sha256, entries: summary.entries })
        })
    }

    async fn materialize(&self, m: &MirrorRef, sha: CommitSha, dest: &Path) -> Result<(), GitError> {
        self.with_mirror(GitMethod::Materialize, m, |mirror| {
            let commit = mirror.commit(sha)?;
            let plan = TreePlan::new(&mirror.raw_entries(commit))?;
            let checkout = Checkout::create(dest)?;
            for item in &plan.items {
                let content = item.blob().map(|oid| mirror.content(oid)).transpose()?.unwrap_or(&[]);
                checkout.write(item, content).map_err(io_err)?;
            }
            Ok(())
        })
    }

    async fn maintain(&self, m: &MirrorRef) -> Result<(), GitError> {
        self.with_mirror(GitMethod::Maintain, m, |_| Ok(()))
    }

    async fn remove_mirror(&self, m: &MirrorRef) -> Result<(), GitError> {
        let mut state = self.enter(GitMethod::RemoveMirror, Some(m))?;
        state.mirrors.remove(m);
        Ok(())
    }

    async fn list_mirrors(&self) -> Result<Vec<MirrorRef>, GitError> {
        let state = self.enter(GitMethod::ListMirrors, None)?;
        Ok(state.mirrors.keys().copied().collect())
    }
}

#[cfg(test)]
mod tests {
    use mage_core::domain::{OrgId, RepoId};

    use super::contract::{all_wants, at};
    use super::*;

    fn mirror() -> MirrorRef {
        MirrorRef::new(OrgId::from_uuid(uuid::Uuid::from_u128(1)), RepoId::from_uuid(uuid::Uuid::from_u128(2)))
    }

    #[tokio::test]
    async fn mem_git_repo_passes_the_contract() {
        contract_git_repo_reader(|fixture| async move {
            let fake = Arc::new(MemGitRepo::new());
            let url = "https://github.com/acme/app.git";
            let shas = fake.add_remote(url, &fixture);
            GitContractSubject { reader: fake, mirror: mirror(), spec: test_fetch_spec(url), shas }
        })
        .await;
    }

    #[test]
    fn object_ids_match_git() {
        // `printf 'hello\n' | git hash-object --stdin`, `git hash-object -t tree /dev/null`.
        assert_eq!(git_blob_oid(b"hello\n").to_hex(), "ce013625030ba8dba906f756967f9e9ca394464a");
        assert_eq!(tree_id(&BTreeMap::new()).to_hex(), "4b825dc642cb6eb9a060e54bf8d69288fbee4904");
    }

    #[tokio::test]
    async fn force_push_fail_injection_and_calls() {
        let fake = MemGitRepo::new();
        let url = "https://github.com/acme/app.git";
        let fixture = FixtureRepo::linear("main", vec![FixtureCommit::new("a", at(0), &[("f", "1\n")]), FixtureCommit::new("b", at(1), &[("f", "2\n")])]);
        let shas = fake.add_remote(url, &fixture);
        let (m, spec) = (mirror(), test_fetch_spec(url));
        fake.fail_next(GitMethod::Fetch, GitError::Unauthorized);
        assert_eq!(fake.fetch(&m, &spec, &all_wants(&[])).await, Err(GitError::Unauthorized));
        fake.fetch(&m, &spec, &all_wants(&[])).await.unwrap();
        fake.pin(&m, shas[1]).await.unwrap();
        fake.set_remote_ref(url, "refs/heads/main", Some(shas[0]));
        let outcome = fake.fetch(&m, &spec, &all_wants(&[])).await.unwrap();
        assert_eq!(outcome.updated_refs, vec![("refs/heads/main".to_string(), shas[0])]);
        assert!(fake.has_commit(&m, shas[1]).await.unwrap());
        assert_eq!(fake.pins(&m), vec![shas[1]]);
        fake.set_remote_ref(url, "refs/heads/main", None);
        fake.fetch(&m, &spec, &all_wants(&[])).await.unwrap();
        assert_eq!(fake.resolve(&m, "main").await, Err(GitError::NotFound));
        let methods: Vec<GitMethod> = fake.calls().into_iter().map(|(g, _)| g).collect();
        assert_eq!(methods[..4], [GitMethod::Fetch, GitMethod::Fetch, GitMethod::Pin, GitMethod::Fetch]);
        let unknown = test_fetch_spec("https://example.invalid/none.git");
        assert_eq!(fake.fetch(&m, &unknown, &all_wants(&[])).await, Err(GitError::NotFound));
    }
}
