//! Port contract suite for `GitRepoReader` (knowledge.md "Testing"): every scenario builds its own
//! fixture through the factory. Run against `MemGitRepo` and `GitCli`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;

use mage_core::domain::{BlobOid, CommitSha, LineRange, OrgId, RepoId, RepoPath};
use mage_core::ports::source_host::FetchSpec;
use mage_core::wire::indexer::{ChangeKind, CommitInfo, FileChange, FileDiff, Hunk, LogQuery, TreeDiffOptions};
use sha2::{Digest as _, Sha256};
use time::OffsetDateTime;

use super::{FixtureCommit, FixtureRepo, git_blob_oid};
use crate::ports::git::{BlobRead, FetchOutcome, FetchWants, GitError, GitRepoReader, MirrorRef, TreeEntryKind};

/// What a factory returns for one scenario: a reader holding no mirror yet (and no other
/// mirrors), the mirror to use, a spec reaching the fixture remote, and the fixture's commit
/// shas in fixture order.
pub struct GitContractSubject {
    pub reader: Arc<dyn GitRepoReader>,
    pub mirror: MirrorRef,
    pub spec: FetchSpec,
    pub shas: Vec<CommitSha>,
}

pub(super) fn at(minutes: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_700_000_000 + minutes * 60).unwrap_or(OffsetDateTime::UNIX_EPOCH)
}

fn path(p: &str) -> RepoPath {
    match RepoPath::new(p) {
        Ok(path) => path,
        Err(e) => panic!("fixture path {p}: {e}"),
    }
}

pub(super) fn all_wants(shas: &[CommitSha]) -> FetchWants {
    FetchWants { all_heads_and_tags: true, shas: shas.to_vec() }
}

fn log_query(include: &[CommitSha]) -> LogQuery {
    LogQuery { include: include.to_vec(), exclude: Vec::new(), path_prefix: None, since: None, skip: 0, max: 100, with_files: false }
}

fn diff_opts() -> TreeDiffOptions {
    TreeDiffOptions { paths: Vec::new(), hunks: false, context: 3, max_bytes_per_file: 8192, renames: true }
}

fn shas_of(commits: &[CommitInfo]) -> Vec<CommitSha> {
    commits.iter().map(|c| c.sha).collect()
}

/// Runs every scenario; each builds its own fixture through `make` (fresh remote + empty mirror).
pub async fn contract_git_repo_reader<F, Fut>(make: F)
where
    F: Fn(FixtureRepo) -> Fut,
    Fut: Future<Output = GitContractSubject>,
{
    contract_before_fetch(&make).await;
    contract_fetch_resolve_commit(&make).await;
    contract_list_tree_and_blobs(&make).await;
    contract_fetch_by_id_and_pins(&make).await;
    contract_log(&make).await;
    contract_diff(&make).await;
    contract_blame(&make).await;
    contract_archive(&make).await;
    contract_materialize(&make).await;
    contract_mirror_lifecycle(&make).await;
}

fn small_fixture() -> FixtureRepo {
    FixtureRepo::linear("main", vec![FixtureCommit::new("init", at(0), &[("a.txt", "a\n")])])
}

async fn contract_before_fetch<F, Fut>(make: &F)
where
    F: Fn(FixtureRepo) -> Fut,
    Fut: Future<Output = GitContractSubject>,
{
    let s = make(small_fixture()).await;
    let (r, m) = (&s.reader, &s.mirror);
    let state = r.mirror_state(m).await.unwrap();
    assert!(!state.exists && state.last_fetch.is_none(), "{state:?}");
    assert!(!r.has_commit(m, s.shas[0]).await.unwrap());
    assert_eq!(r.resolve(m, "main").await, Err(GitError::NotFound));
    assert_eq!(r.commit(m, s.shas[0]).await, Err(GitError::NotFound));
    assert_eq!(r.list_tree(m, s.shas[0]).await, Err(GitError::NotFound));
    assert_eq!(r.read_blobs(m, &[git_blob_oid(b"a\n")], 10).await, Err(GitError::NotFound));
    assert_eq!(r.log(m, &log_query(&s.shas)).await, Err(GitError::NotFound));
    assert_eq!(r.list_mirrors().await.unwrap(), Vec::<MirrorRef>::new());
    r.remove_mirror(m).await.unwrap();
}

async fn contract_fetch_resolve_commit<F, Fut>(make: &F)
where
    F: Fn(FixtureRepo) -> Fut,
    Fut: Future<Output = GitContractSubject>,
{
    let fixture = FixtureRepo::linear(
        "main",
        vec![
            FixtureCommit::new("first", at(0), &[("a.txt", "a\n")]),
            FixtureCommit::new("second change", at(5), &[("a.txt", "a\nb\n")]).with_body("Why:\nbecause.\n\nMore."),
        ],
    )
    .with_ref("refs/tags/v1", 0)
    .with_ref("refs/heads/dev", 0);
    let s = make(fixture).await;
    let (r, m, c0, c1) = (&s.reader, &s.mirror, s.shas[0], s.shas[1]);

    let outcome = r.fetch(m, &s.spec, &all_wants(&[c1])).await.unwrap();
    assert_eq!(
        outcome,
        FetchOutcome { updated_refs: vec![("refs/heads/dev".into(), c0), ("refs/heads/main".into(), c1), ("refs/tags/v1".into(), c0)], missing: vec![] }
    );
    assert!(r.has_commit(m, c0).await.unwrap() && r.has_commit(m, c1).await.unwrap());
    assert!(!r.has_commit(m, CommitSha([0xab; 20])).await.unwrap());
    assert_eq!(r.resolve(m, "main").await, Ok(c1));
    assert_eq!(r.resolve(m, "v1").await, Ok(c0));
    assert_eq!(r.resolve(m, "refs/heads/dev").await, Ok(c0));
    assert_eq!(r.resolve(m, "HEAD").await, Ok(c1));
    assert_eq!(r.resolve(m, &c0.to_hex()).await, Ok(c0));
    assert_eq!(r.resolve(m, "nope").await, Err(GitError::NotFound));
    assert!(matches!(r.resolve(m, "--output=/x").await, Err(GitError::Invalid(_))));

    let info = r.commit(m, c1).await.unwrap();
    assert_eq!(info.sha, c1);
    assert_eq!(info.parents, vec![c0]);
    assert_eq!(info.subject, "second change");
    assert_eq!(info.body, "Why:\nbecause.\n\nMore.");
    assert_eq!(info.author_name, "Ada");
    assert_eq!((info.author_time, info.committer_time), (at(5), at(5)));
    assert_eq!(info.files, None);
    assert_eq!(r.list_tree(m, c1).await.unwrap().tree, info.tree);
    assert_eq!(r.commit(m, c0).await.unwrap().parents, vec![]);
    assert_eq!(r.commit(m, c0).await.unwrap().body, "");

    let again = r.fetch(m, &s.spec, &all_wants(&[])).await.unwrap();
    assert_eq!(again, FetchOutcome::default());
    let state = r.mirror_state(m).await.unwrap();
    assert!(state.exists && state.size_bytes > 0 && state.last_fetch.is_some(), "{state:?}");
}

async fn contract_list_tree_and_blobs<F, Fut>(make: &F)
where
    F: Fn(FixtureRepo) -> Fut,
    Fut: Future<Output = GitContractSubject>,
{
    let commit = FixtureCommit::new("tree", at(0), &[("b.txt", "bee\n"), ("a/z.txt", "z\n"), ("a/b/c.txt", "c\n"), ("a.txt", "a\n"), ("a-b", "dash\n")])
        .with_executable("run.sh", "#!/bin/sh\necho hi\n")
        .with_symlink("link", "a.txt");
    let s = make(FixtureRepo::linear("main", vec![commit])).await;
    let (r, m, c0) = (&s.reader, &s.mirror, s.shas[0]);
    r.fetch(m, &s.spec, &all_wants(&[])).await.unwrap();

    let listing = r.list_tree(m, c0).await.unwrap();
    assert_eq!(listing.skipped_paths, 0);
    let got: Vec<(&str, TreeEntryKind, Option<u64>)> = listing.entries.iter().map(|e| (e.path.as_str(), e.kind, e.size)).collect();
    let file = |c: &str| TreeEntryKind::File { oid: git_blob_oid(c.as_bytes()), executable: false };
    assert_eq!(
        got,
        vec![
            ("a-b", file("dash\n"), Some(5)),
            ("a.txt", file("a\n"), Some(2)),
            ("a/b/c.txt", file("c\n"), Some(2)),
            ("a/z.txt", file("z\n"), Some(2)),
            ("b.txt", file("bee\n"), Some(4)),
            ("link", TreeEntryKind::Symlink { oid: git_blob_oid(b"a.txt") }, Some(5)),
            ("run.sh", TreeEntryKind::File { oid: git_blob_oid(b"#!/bin/sh\necho hi\n"), executable: true }, Some(18)),
        ]
    );

    let (bee, a, link) = (git_blob_oid(b"bee\n"), git_blob_oid(b"a\n"), git_blob_oid(b"a.txt"));
    let blobs = r.read_blobs(m, &[bee, a, link, bee], 4).await.unwrap();
    assert_eq!(
        blobs,
        vec![
            BlobRead { oid: bee, size: 4, bytes: Some(b"bee\n".to_vec()) },
            BlobRead { oid: a, size: 2, bytes: Some(b"a\n".to_vec()) },
            BlobRead { oid: link, size: 5, bytes: None },
            BlobRead { oid: bee, size: 4, bytes: Some(b"bee\n".to_vec()) },
        ]
    );
    assert_eq!(r.read_blobs(m, &[], 4).await, Ok(vec![]));
    assert_eq!(r.read_blobs(m, &[a, BlobOid([0x5a; 20])], 100).await, Err(GitError::NotFound));
    assert_eq!(r.list_tree(m, CommitSha([0x5a; 20])).await, Err(GitError::NotFound));
}

async fn contract_fetch_by_id_and_pins<F, Fut>(make: &F)
where
    F: Fn(FixtureRepo) -> Fut,
    Fut: Future<Output = GitContractSubject>,
{
    // main was force-pushed from c1 to c2: c1 still exists on the remote, unreachable.
    let fixture = FixtureRepo {
        default_branch: "main".into(),
        commits: vec![
            FixtureCommit::new("base", at(0), &[("f", "0\n")]),
            FixtureCommit::new("old tip", at(1), &[("f", "1\n")]).with_parents(&[0]),
            FixtureCommit::new("new tip", at(2), &[("f", "2\n")]).with_parents(&[0]),
        ],
        refs: vec![("refs/heads/main".into(), 2)],
    };
    let s = make(fixture).await;
    let (r, m) = (&s.reader, &s.mirror);
    let (c0, c1, c2) = (s.shas[0], s.shas[1], s.shas[2]);
    let never = CommitSha([0xcd; 20]);

    let outcome = r.fetch(m, &s.spec, &all_wants(&[c1, never, c2])).await.unwrap();
    assert_eq!(outcome.missing, vec![never]);
    assert_eq!(outcome.updated_refs, vec![("refs/heads/main".to_string(), c2)]);
    assert!(r.has_commit(m, c1).await.unwrap());
    assert_eq!(r.commit(m, c1).await.unwrap().parents, vec![c0]);

    let keep = format!("refs/mage/keep/{c1}");
    r.pin(m, c1).await.unwrap();
    r.pin(m, c1).await.unwrap();
    assert_eq!(r.resolve(m, &keep).await, Ok(c1));
    let refetch = r.fetch(m, &s.spec, &all_wants(&[])).await.unwrap();
    assert_eq!(refetch.updated_refs, vec![]);
    assert_eq!(r.resolve(m, &keep).await, Ok(c1), "pins survive a pruning fetch");
    r.unpin(m, c1).await.unwrap();
    r.unpin(m, c1).await.unwrap();
    assert_eq!(r.resolve(m, &keep).await, Err(GitError::NotFound));
    assert_eq!(r.pin(m, never).await, Err(GitError::NotFound));
}

async fn contract_log<F, Fut>(make: &F)
where
    F: Fn(FixtureRepo) -> Fut,
    Fut: Future<Output = GitContractSubject>,
{
    let fixture = FixtureRepo::linear(
        "main",
        vec![
            FixtureCommit::new("c0", at(0), &[("src/a.rs", "a\n"), ("docs/x.md", "x\n")]),
            FixtureCommit::new("c1", at(1), &[("src/a.rs", "a\na2\n"), ("docs/x.md", "x\n")]),
            FixtureCommit::new("c2", at(2), &[("src/a.rs", "a\na2\n"), ("docs/x.md", "x\ny\n")]),
            FixtureCommit::new("c3", at(3), &[("src/a.rs", "a\na2\n"), ("src/b.rs", "b\n"), ("docs/x.md", "x\ny\n")]),
            FixtureCommit::new("c4", at(4), &[("src/a.rs", "a\na2\n"), ("src/b.rs", "b\n")]),
        ],
    );
    let s = make(fixture).await;
    let (r, m, c) = (&s.reader, &s.mirror, &s.shas);
    r.fetch(m, &s.spec, &all_wants(&[])).await.unwrap();

    let all = r.log(m, &log_query(&[c[4]])).await.unwrap();
    assert_eq!(shas_of(&all), vec![c[4], c[3], c[2], c[1], c[0]]);
    assert!(all.iter().all(|i| i.files.is_none()));
    assert_eq!(all[1].subject, "c3");
    let q = LogQuery { exclude: vec![c[1]], ..log_query(&[c[4]]) };
    assert_eq!(shas_of(&r.log(m, &q).await.unwrap()), vec![c[4], c[3], c[2]]);
    let q = LogQuery { skip: 1, max: 2, ..log_query(&[c[4]]) };
    assert_eq!(shas_of(&r.log(m, &q).await.unwrap()), vec![c[3], c[2]]);
    let q = LogQuery { path_prefix: Some(path("src")), ..log_query(&[c[4]]) };
    assert_eq!(shas_of(&r.log(m, &q).await.unwrap()), vec![c[3], c[1], c[0]]);
    let q = LogQuery { since: Some(at(2)), ..log_query(&[c[4]]) };
    assert_eq!(shas_of(&r.log(m, &q).await.unwrap()), vec![c[4], c[3], c[2]]);
    let q = LogQuery { include: vec![c[2], c[4]], exclude: vec![c[2]], ..log_query(&[]) };
    assert_eq!(shas_of(&r.log(m, &q).await.unwrap()), vec![c[4], c[3]]);
    assert_eq!(r.log(m, &log_query(&[CommitSha([0x11; 20])])).await, Err(GitError::NotFound));

    let q = LogQuery { with_files: true, ..log_query(&[c[4]]) };
    let with = r.log(m, &q).await.unwrap();
    let change = |p: &str, change: ChangeKind, adds: u32, dels: u32| FileChange { path: path(p), old_path: None, change, adds: Some(adds), dels: Some(dels) };
    assert_eq!(with[0].files, Some(vec![change("docs/x.md", ChangeKind::Deleted, 0, 2)]));
    assert_eq!(with[1].files, Some(vec![change("src/b.rs", ChangeKind::Added, 1, 0)]));
    assert_eq!(with[3].files, Some(vec![change("src/a.rs", ChangeKind::Modified, 1, 0)]));
    assert_eq!(with[4].files, Some(vec![change("docs/x.md", ChangeKind::Added, 1, 0), change("src/a.rs", ChangeKind::Added, 1, 0)]));
}

async fn contract_diff<F, Fut>(make: &F)
where
    F: Fn(FixtureRepo) -> Fut,
    Fut: Future<Output = GitContractSubject>,
{
    let renamed = "same content\nfor the rename\n";
    let fixture = FixtureRepo::linear(
        "main",
        vec![
            FixtureCommit::new("c0", at(0), &[("keep.txt", "k\n"), ("mod.txt", "a\nb\nc\n"), ("gone.txt", "g\n"), ("old.txt", renamed), ("dir/x", "x\n")]),
            FixtureCommit::new("c1", at(1), &[("keep.txt", "k\n"), ("mod.txt", "a\nB\nc\n"), ("new.txt", renamed), ("added.txt", "n\n"), ("dir/x", "x2\n")]),
        ],
    );
    let s = make(fixture).await;
    let (r, m, c0, c1) = (&s.reader, &s.mirror, s.shas[0], s.shas[1]);
    r.fetch(m, &s.spec, &all_wants(&[])).await.unwrap();

    let summary = |diffs: &[FileDiff]| -> Vec<(String, ChangeKind, Option<String>)> {
        diffs.iter().map(|d| (d.change.path.to_string(), d.change.change, d.change.old_path.as_ref().map(ToString::to_string))).collect()
    };
    let diffs = r.diff(m, c0, c1, &diff_opts()).await.unwrap();
    assert_eq!(
        summary(&diffs),
        vec![
            ("added.txt".into(), ChangeKind::Added, None),
            ("dir/x".into(), ChangeKind::Modified, None),
            ("gone.txt".into(), ChangeKind::Deleted, None),
            ("mod.txt".into(), ChangeKind::Modified, None),
            ("new.txt".into(), ChangeKind::Renamed, Some("old.txt".into())),
        ]
    );
    assert!(diffs.iter().all(|d| d.hunks.is_empty() && !d.binary && !d.truncated));
    assert_eq!((diffs[3].change.adds, diffs[3].change.dels), (Some(1), Some(1)));

    let no_renames = r.diff(m, c0, c1, &TreeDiffOptions { renames: false, ..diff_opts() }).await.unwrap();
    let kinds: Vec<(String, ChangeKind)> = no_renames.iter().map(|d| (d.change.path.to_string(), d.change.change)).collect();
    assert!(kinds.contains(&("new.txt".into(), ChangeKind::Added)) && kinds.contains(&("old.txt".into(), ChangeKind::Deleted)), "{kinds:?}");

    let opts = TreeDiffOptions { paths: vec![path("mod.txt"), path("dir")], hunks: true, context: 1, ..diff_opts() };
    let diffs = r.diff(m, c0, c1, &opts).await.unwrap();
    assert_eq!(summary(&diffs).len(), 2);
    assert_eq!(diffs[1].hunks, vec![Hunk { old_start: 1, old_lines: 3, new_start: 1, new_lines: 3, text: " a\n-b\n+B\n c\n".into() }]);
    assert_eq!(diffs[0].hunks, vec![Hunk { old_start: 1, old_lines: 1, new_start: 1, new_lines: 1, text: "-x\n+x2\n".into() }]);
    let cut = r.diff(m, c0, c1, &TreeDiffOptions { max_bytes_per_file: 4, ..opts }).await.unwrap();
    assert!(cut.iter().all(|d| d.truncated && d.hunks.is_empty()));
    assert_eq!(r.diff(m, c0, CommitSha([0x22; 20]), &diff_opts()).await, Err(GitError::NotFound));
}

async fn contract_blame<F, Fut>(make: &F)
where
    F: Fn(FixtureRepo) -> Fut,
    Fut: Future<Output = GitContractSubject>,
{
    let fixture = FixtureRepo::linear(
        "main",
        vec![
            FixtureCommit::new("one", at(0), &[("f.txt", "l1\nl2\nl3\nl4\n")]),
            FixtureCommit { author_name: "Bob".into(), ..FixtureCommit::new("two", at(1), &[("f.txt", "l1\nX\nl3\nl4\n")]) },
        ],
    );
    let s = make(fixture).await;
    let (r, m, c0, c1) = (&s.reader, &s.mirror, s.shas[0], s.shas[1]);
    r.fetch(m, &s.spec, &all_wants(&[])).await.unwrap();

    let f = path("f.txt");
    let range = |a, b| match LineRange::new(a, b) {
        Ok(r) => r,
        Err(e) => panic!("{e}"),
    };
    let hunks = r.blame(m, c1, &f, range(1, 4)).await.unwrap();
    let got: Vec<(u32, u32, CommitSha, &str, &str, u32)> = hunks.iter().map(|h| (h.start, h.lines, h.commit, h.author_name.as_str(), h.subject.as_str(), h.orig_start)).collect();
    assert_eq!(got, vec![(1, 1, c0, "Ada", "one", 1), (2, 1, c1, "Bob", "two", 2), (3, 2, c0, "Ada", "one", 3)]);
    assert!(hunks.iter().all(|h| h.orig_path == f));
    assert_eq!(hunks[1].author_time, at(1));
    let part = r.blame(m, c1, &f, range(3, 3)).await.unwrap();
    assert_eq!(part.iter().map(|h| (h.start, h.lines, h.commit)).collect::<Vec<_>>(), vec![(3, 1, c0)]);
    let clamped = r.blame(m, c1, &f, range(4, 9)).await.unwrap();
    assert_eq!(clamped.iter().map(|h| (h.start, h.lines, h.commit, h.orig_start)).collect::<Vec<_>>(), vec![(4, 1, c0, 4)], "end clamped");
    assert!(matches!(r.blame(m, c1, &f, range(5, 9)).await, Err(GitError::Invalid(_))));
    assert_eq!(r.blame(m, c1, &path("nope.txt"), range(1, 1)).await, Err(GitError::NotFound));
}

/// One tar entry of a source bundle, as read back by [`read_bundle`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleFile {
    /// Lossy UTF-8; directories end with `/`.
    pub path: String,
    pub mode: u32,
    /// Tar type flag (`b'0'` file, `b'2'` symlink, `b'5'` directory).
    pub kind: u8,
    pub link: Option<String>,
    pub content: Vec<u8>,
    pub mtime: u64,
}

/// Decompresses and lists a tar.zst bundle (pax headers applied).
pub fn read_bundle(bytes: &[u8]) -> Vec<BundleFile> {
    let tar_bytes = zstd::decode_all(bytes).expect("bundle is zstd");
    let mut archive = tar::Archive::new(tar_bytes.as_slice());
    let mut out = Vec::new();
    for entry in archive.entries().expect("bundle is tar") {
        let mut entry = entry.expect("tar entry");
        let header = entry.header().clone();
        let path = String::from_utf8_lossy(&entry.path_bytes()).into_owned();
        let link = entry.link_name_bytes().map(|l| String::from_utf8_lossy(&l).into_owned());
        let mut content = Vec::new();
        entry.read_to_end(&mut content).expect("tar content");
        out.push(BundleFile { path, mode: header.mode().unwrap(), kind: header.entry_type().as_byte(), link, content, mtime: header.mtime().unwrap() });
    }
    out
}

fn bundle_fixture() -> FixtureRepo {
    let commit = FixtureCommit::new("bundle", at(7), &[("src/main.rs", "fn main() {}\n"), ("src/lib/mod.rs", "// lib\n"), ("README", "hi\n"), ("src-x", "x\n")])
        .with_executable("bin/run", "#!/bin/sh\n")
        .with_symlink("docs", "src");
    FixtureRepo::linear("main", vec![commit])
}

async fn contract_archive<F, Fut>(make: &F)
where
    F: Fn(FixtureRepo) -> Fut,
    Fut: Future<Output = GitContractSubject>,
{
    let s = make(bundle_fixture()).await;
    let (r, m, c0) = (&s.reader, &s.mirror, s.shas[0]);
    r.fetch(m, &s.spec, &all_wants(&[])).await.unwrap();
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
    let (p1, p2) = (tmp.path().join("one.tar.zst"), tmp.path().join("two.tar.zst"));
    let info1 = r.archive(m, c0, &p1).await.unwrap();
    let info2 = r.archive(m, c0, &p2).await.unwrap();
    let (b1, b2) = (std::fs::read(&p1).unwrap_or_default(), std::fs::read(&p2).unwrap_or_default());
    assert_eq!(b1, b2, "archive is byte-identical across runs");
    assert_eq!(info1, info2);
    assert_eq!(info1.bytes, b1.len() as u64);
    assert_eq!(info1.sha256, <[u8; 32]>::from(Sha256::digest(&b1)));
    assert_eq!(info1.tree, r.commit(m, c0).await.unwrap().tree);

    let entries = read_bundle(&b1);
    assert_eq!(info1.entries as usize, entries.len());
    let mtime = at(7).unix_timestamp() as u64;
    assert!(entries.iter().all(|e| e.mtime == mtime));
    type Shape<'a> = (&'a str, u32, u8, Option<&'a str>, &'a [u8]);
    let shape: Vec<Shape<'_>> = entries.iter().map(|e| (e.path.as_str(), e.mode, e.kind, e.link.as_deref(), e.content.as_slice())).collect();
    assert_eq!(
        shape,
        vec![
            ("README", 0o644, b'0', None, &b"hi\n"[..]),
            ("bin/", 0o755, b'5', None, &b""[..]),
            ("bin/run", 0o755, b'0', None, &b"#!/bin/sh\n"[..]),
            ("docs", 0o777, b'2', Some("src"), &b""[..]),
            ("src/", 0o755, b'5', None, &b""[..]),
            ("src-x", 0o644, b'0', None, &b"x\n"[..]),
            ("src/lib/", 0o755, b'5', None, &b""[..]),
            ("src/lib/mod.rs", 0o644, b'0', None, &b"// lib\n"[..]),
            ("src/main.rs", 0o644, b'0', None, &b"fn main() {}\n"[..]),
        ]
    );
    assert_eq!(r.archive(m, CommitSha([0x33; 20]), &tmp.path().join("x")).await, Err(GitError::NotFound));
}

async fn contract_materialize<F, Fut>(make: &F)
where
    F: Fn(FixtureRepo) -> Fut,
    Fut: Future<Output = GitContractSubject>,
{
    let s = make(bundle_fixture()).await;
    let (r, m, c0) = (&s.reader, &s.mirror, s.shas[0]);
    r.fetch(m, &s.spec, &all_wants(&[])).await.unwrap();
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
    let dest = tmp.path().join("co").join(c0.to_hex());
    r.materialize(m, c0, &dest).await.unwrap();
    let read = |p: &str| std::fs::read(dest.join(p)).unwrap_or_else(|e| panic!("{p}: {e}"));
    let mode = |p: &str| std::fs::symlink_metadata(dest.join(p)).map(|m| m.permissions().mode() & 0o777).unwrap_or(0);
    assert_eq!(read("src/main.rs"), b"fn main() {}\n");
    assert_eq!(read("src/lib/mod.rs"), b"// lib\n");
    assert_eq!(mode("README"), 0o444);
    assert_eq!(mode("bin/run"), 0o555);
    assert_eq!(std::fs::read_link(dest.join("docs")).ok(), Some(std::path::PathBuf::from("src")));
    assert_eq!(r.materialize(m, c0, &dest).await, Err(GitError::Conflict), "dest already holds files");
}

async fn contract_mirror_lifecycle<F, Fut>(make: &F)
where
    F: Fn(FixtureRepo) -> Fut,
    Fut: Future<Output = GitContractSubject>,
{
    let s = make(small_fixture()).await;
    let (r, m) = (&s.reader, &s.mirror);
    let other = MirrorRef::new(OrgId::from_uuid(uuid::Uuid::from_u128(0x0f)), RepoId::from_uuid(uuid::Uuid::from_u128(0x0e)));
    r.fetch(m, &s.spec, &all_wants(&[])).await.unwrap();
    r.fetch(&other, &s.spec, &all_wants(&[])).await.unwrap();
    let mut expected = vec![*m, other];
    expected.sort();
    assert_eq!(r.list_mirrors().await.unwrap(), expected);
    r.maintain(m).await.unwrap();
    assert!(r.has_commit(m, s.shas[0]).await.unwrap(), "maintain keeps reachable commits");

    r.remove_mirror(&other).await.unwrap();
    r.remove_mirror(&other).await.unwrap();
    assert_eq!(r.list_mirrors().await.unwrap(), vec![*m]);
    assert!(!r.mirror_state(&other).await.unwrap().exists);
    assert!(!r.has_commit(&other, s.shas[0]).await.unwrap());
    assert_eq!(r.maintain(&other).await, Err(GitError::NotFound));
    assert_eq!(r.pin(&other, s.shas[0]).await, Err(GitError::NotFound));
}

