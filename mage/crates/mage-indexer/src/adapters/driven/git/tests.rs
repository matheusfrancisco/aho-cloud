//! `GitCli` against fixture repos built with the `git` CLI: the port contract suite plus the
//! git-only cases of knowledge.md "Testing" (renames, symlinks, submodule, exec bit, non-UTF-8
//! path, force-push, pins across `gc --prune=now`, archive equals the tree, traversal rejected).

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use mage_core::domain::{CommitSha, OrgId, RepoId, RepoPath};
use mage_core::wire::indexer::{ChangeKind, LogQuery, TreeDiffOptions};
use time::OffsetDateTime;

use super::{GitCli, GitConfig};
use crate::ports::git::{FetchWants, GitError, GitRepoReader, MirrorRef, TreeEntryKind};
use crate::testing::git::{
    BundleFile, FixtureCommit, FixtureEntry, FixtureRepo, GitContractSubject, MemGitRepo, contract_git_repo_reader, fixture_email, fixture_message,
    read_bundle, test_fetch_spec,
};

/// `(path bytes, mode, oid)` of one `mktree` input entry.
type TreeInput<'a> = (Vec<u8>, &'a str, String);

fn git_available() -> bool {
    let ok = Command::new("git").arg("--version").stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success());
    if !ok {
        eprintln!("skipping: git not available");
    }
    ok
}

/// A bare fixture repo driven by the plumbing commands (no work tree, no user config).
struct Remote {
    dir: PathBuf,
    home: PathBuf,
}

impl Remote {
    fn init(dir: &Path, branch: &str) -> Remote {
        let home = dir.with_extension("home");
        std::fs::create_dir_all(&home).unwrap();
        let remote = Remote { dir: dir.to_path_buf(), home };
        remote.run(None, &["init", "-q", "--bare", &format!("--initial-branch={branch}"), dir.to_str().unwrap()], None, &[]);
        remote
    }

    fn run(&self, git_dir: Option<&Path>, args: &[&str], stdin: Option<&[u8]>, env: &[(&str, String)]) -> Vec<u8> {
        let mut cmd = Command::new("git");
        cmd.env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &self.home)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null");
        for (k, v) in env {
            cmd.env(k, v);
        }
        if let Some(dir) = git_dir {
            cmd.arg(format!("--git-dir={}", dir.display()));
        }
        cmd.args(args).stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() }).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = cmd.spawn().unwrap();
        if let Some(input) = stdin {
            child.stdin.take().unwrap().write_all(input).unwrap();
        }
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        out.stdout
    }

    fn git(&self, args: &[&str]) -> String {
        String::from_utf8(self.run(Some(&self.dir), args, None, &[])).unwrap().trim().to_owned()
    }

    fn blob(&self, content: &[u8]) -> String {
        String::from_utf8(self.run(Some(&self.dir), &["hash-object", "-w", "--stdin"], Some(content), &[])).unwrap().trim().to_owned()
    }

    /// Builds nested trees from `(path bytes, mode, oid)` with `git mktree -z` (no name checks).
    fn tree(&self, entries: &[TreeInput<'_>]) -> String {
        let mut leaves: Vec<(Vec<u8>, String, String)> = Vec::new();
        let mut dirs: BTreeMap<Vec<u8>, Vec<TreeInput<'_>>> = BTreeMap::new();
        for (path, mode, oid) in entries {
            match path.iter().position(|b| *b == b'/') {
                Some(i) => dirs.entry(path[..i].to_vec()).or_default().push((path[i + 1..].to_vec(), mode, oid.clone())),
                None => leaves.push((path.clone(), (*mode).to_owned(), oid.clone())),
            }
        }
        for (name, children) in dirs {
            let oid = self.tree(&children);
            leaves.push((name, "040000".to_owned(), oid));
        }
        let mut input = Vec::new();
        for (name, mode, oid) in leaves {
            let kind = match mode.as_str() {
                "040000" => "tree",
                "160000" => "commit",
                _ => "blob",
            };
            input.extend_from_slice(format!("{mode} {kind} {oid}\t").as_bytes());
            input.extend_from_slice(&name);
            input.push(0);
        }
        String::from_utf8(self.run(Some(&self.dir), &["mktree", "-z", "--missing"], Some(&input), &[])).unwrap().trim().to_owned()
    }

    fn commit(&self, tree: &str, parents: &[String], author: &str, time: OffsetDateTime, subject: &str, body: &str) -> String {
        let when = format!("{} +0000", time.unix_timestamp());
        let env = [
            ("GIT_AUTHOR_NAME", author.to_owned()),
            ("GIT_AUTHOR_EMAIL", fixture_email(author)),
            ("GIT_AUTHOR_DATE", when.clone()),
            ("GIT_COMMITTER_NAME", author.to_owned()),
            ("GIT_COMMITTER_EMAIL", fixture_email(author)),
            ("GIT_COMMITTER_DATE", when),
        ];
        let mut args = vec!["commit-tree".to_owned(), tree.to_owned()];
        for p in parents {
            args.extend(["-p".to_owned(), p.clone()]);
        }
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let out = self.run(Some(&self.dir), &args, Some(fixture_message(subject, body).as_bytes()), &env);
        String::from_utf8(out).unwrap().trim().to_owned()
    }

    fn url(&self) -> String {
        format!("file://{}", self.dir.display())
    }
}

/// Writes `fixture` as a bare repo at `dir`; returns the commit shas in fixture order.
fn build_fixture(dir: &Path, fixture: &FixtureRepo) -> (Remote, Vec<CommitSha>) {
    let remote = Remote::init(dir, &fixture.default_branch);
    let mut shas: Vec<String> = Vec::new();
    for commit in &fixture.commits {
        let entries: Vec<(Vec<u8>, &str, String)> = commit
            .tree
            .iter()
            .map(|f| match &f.entry {
                FixtureEntry::File { content, executable } => (f.path.as_bytes().to_vec(), if *executable { "100755" } else { "100644" }, remote.blob(content)),
                FixtureEntry::Symlink { target } => (f.path.as_bytes().to_vec(), "120000", remote.blob(target.as_bytes())),
            })
            .collect();
        let tree = remote.tree(&entries);
        let parents: Vec<String> = commit.parents.iter().map(|p| shas[*p].clone()).collect();
        shas.push(remote.commit(&tree, &parents, &commit.author_name, commit.time, &commit.subject, &commit.body));
    }
    for (name, index) in &fixture.refs {
        remote.git(&["update-ref", name, &shas[*index]]);
    }
    let shas = shas.iter().map(|s| s.parse().unwrap()).collect();
    (remote, shas)
}

fn cli(data_dir: PathBuf) -> GitCli {
    GitCli::new(GitConfig { data_dir, git_bin: "git".into(), command_timeout_secs: 60, allow_file_protocol: true }).unwrap()
}

fn mirror(n: u128) -> MirrorRef {
    MirrorRef::new(OrgId::from_uuid(uuid::Uuid::from_u128(0xa0 + n)), RepoId::from_uuid(uuid::Uuid::from_u128(0xb0 + n)))
}

fn at(minutes: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_700_000_000 + minutes * 60).unwrap()
}

fn all() -> FetchWants {
    FetchWants { all_heads_and_tags: true, shas: Vec::new() }
}

/// Runs `git` against a mirror outside the adapter (test inspection only).
fn raw_git(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .arg(format!("--git-dir={}", dir.display()))
        .args(args)
        .output()
        .unwrap()
}

#[tokio::test]
async fn git_cli_passes_the_contract() {
    if !git_available() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let base = root.path().to_path_buf();
    let counter = Arc::new(AtomicUsize::new(0));
    contract_git_repo_reader(|fixture| {
        let n = counter.fetch_add(1, Ordering::SeqCst);
        let dir = base.join(format!("s{n}"));
        async move {
            let (remote, shas) = build_fixture(&dir.join("remote.git"), &fixture);
            GitContractSubject { reader: Arc::new(cli(dir.join("data"))), mirror: mirror(0), spec: test_fetch_spec(&remote.url()), shas }
        }
    })
    .await;
}

/// The fake computes the same object ids and bundle bytes, and answers log/diff/blame alike.
#[tokio::test]
async fn fake_agrees_with_git() {
    if !git_available() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let fixture = FixtureRepo::linear(
        "trunk",
        vec![
            FixtureCommit::new("one", at(0), &[("src/a.rs", "fn a() {}\n"), ("README", "r\n"), ("no-newline", "x")]).with_body("Body text\n\nsecond paragraph"),
            FixtureCommit::new("two", at(1), &[("src/a.rs", "fn a() { 1 }\n"), ("src/b.rs", "b\n"), ("no-newline", "y")])
                .with_executable("run.sh", "#!/bin/sh\n")
                .with_symlink("lnk", "src/a.rs"),
        ],
    )
    .with_ref("refs/tags/v0", 0);
    let (remote, git_shas) = build_fixture(&tmp.path().join("remote.git"), &fixture);
    let fake = MemGitRepo::new();
    let fake_shas = fake.add_remote(&remote.url(), &fixture);
    assert_eq!(fake_shas, git_shas, "canonical commit objects match git");

    let git = cli(tmp.path().join("data"));
    let spec = test_fetch_spec(&remote.url());
    let m = mirror(1);
    assert_eq!(git.fetch(&m, &spec, &all()).await.unwrap(), fake.fetch(&m, &spec, &all()).await.unwrap());
    let (c0, c1) = (git_shas[0], git_shas[1]);
    assert_eq!(git.commit(&m, c0).await.unwrap(), fake.commit(&m, c0).await.unwrap());
    assert_eq!(git.list_tree(&m, c1).await.unwrap(), fake.list_tree(&m, c1).await.unwrap());
    let q = LogQuery { include: vec![c1], exclude: vec![], path_prefix: None, since: None, skip: 0, max: 10, with_files: true };
    assert_eq!(git.log(&m, &q).await.unwrap(), fake.log(&m, &q).await.unwrap());
    let opts = TreeDiffOptions { paths: vec![], hunks: true, context: 3, max_bytes_per_file: 8192, renames: true };
    assert_eq!(git.diff(&m, c0, c1, &opts).await.unwrap(), fake.diff(&m, c0, c1, &opts).await.unwrap());
    let path = RepoPath::new("src/a.rs").unwrap();
    let lines = mage_core::domain::LineRange::new(1, 1).unwrap();
    assert_eq!(git.blame(&m, c1, &path, lines).await.unwrap(), fake.blame(&m, c1, &path, lines).await.unwrap());

    let (pg, pf) = (tmp.path().join("git.tar.zst"), tmp.path().join("fake.tar.zst"));
    assert_eq!(git.archive(&m, c1, &pg).await.unwrap(), fake.archive(&m, c1, &pf).await.unwrap());
    assert_eq!(std::fs::read(&pg).unwrap(), std::fs::read(&pf).unwrap(), "fake and adapter bundles are byte-identical");
}

#[tokio::test]
async fn renames_symlinks_submodule_exec_and_non_utf8_paths() {
    if !git_available() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let remote = Remote::init(&tmp.path().join("remote.git"), "main");
    let long_text = "line one\nline two\nline three\nline four\nline five\n";
    let gitlink = "1234567890abcdef1234567890abcdef12345678";
    let long_path = format!("{}/deep.txt", "nested-directory-name".repeat(6));
    let t0 = remote.tree(&[
        (b"docs/guide.md".to_vec(), "100644", remote.blob(long_text.as_bytes())),
        (b"bin/tool".to_vec(), "100755", remote.blob(b"#!/bin/sh\n")),
        (b"current".to_vec(), "120000", remote.blob(b"docs/guide.md")),
        (b"vendor/lib".to_vec(), "160000", gitlink.to_owned()),
        (b"bad\xff.txt".to_vec(), "100644", remote.blob(b"latin1\n")),
        (long_path.as_bytes().to_vec(), "100644", remote.blob(b"deep\n")),
    ]);
    let c0 = remote.commit(&t0, &[], "Ada", at(0), "initial", "");
    let renamed = long_text.replace("five", "5");
    let t1 = remote.tree(&[
        (b"manual/guide.md".to_vec(), "100644", remote.blob(renamed.as_bytes())),
        (b"bin/tool".to_vec(), "100755", remote.blob(b"#!/bin/sh\n")),
        (b"current".to_vec(), "120000", remote.blob(b"manual/guide.md")),
        (b"vendor/lib".to_vec(), "160000", gitlink.to_owned()),
    ]);
    let c1 = remote.commit(&t1, std::slice::from_ref(&c0), "Ada", at(1), "move docs", "");
    remote.git(&["update-ref", "refs/heads/main", &c1]);
    let (c0, c1): (CommitSha, CommitSha) = (c0.parse().unwrap(), c1.parse().unwrap());

    let git = cli(tmp.path().join("data"));
    let m = mirror(2);
    git.fetch(&m, &test_fetch_spec(&remote.url()), &all()).await.unwrap();

    let listing = git.list_tree(&m, c0).await.unwrap();
    assert_eq!(listing.skipped_paths, 1, "non-UTF-8 path skipped and counted");
    let kinds: BTreeMap<&str, (TreeEntryKind, Option<u64>)> = listing.entries.iter().map(|e| (e.path.as_str(), (e.kind, e.size))).collect();
    assert!(matches!(kinds["bin/tool"].0, TreeEntryKind::File { executable: true, .. }));
    assert!(matches!(kinds["current"].0, TreeEntryKind::Symlink { .. }));
    assert_eq!(kinds["vendor/lib"], (TreeEntryKind::Submodule { commit: gitlink.parse().unwrap() }, None));
    assert_eq!(kinds["docs/guide.md"].1, Some(long_text.len() as u64));
    let paths: Vec<&str> = listing.entries.iter().map(|e| e.path.as_str()).collect();
    let mut sorted = paths.clone();
    sorted.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
    assert_eq!(paths, sorted);

    let opts = TreeDiffOptions { paths: vec![], hunks: true, context: 1, max_bytes_per_file: 8192, renames: true };
    let diffs = git.diff(&m, c0, c1, &opts).await.unwrap();
    let guide = diffs.iter().find(|d| d.change.path.as_str() == "manual/guide.md").unwrap();
    assert_eq!(guide.change.change, ChangeKind::Renamed);
    assert_eq!(guide.change.old_path.as_ref().map(RepoPath::as_str), Some("docs/guide.md"));
    assert_eq!((guide.change.adds, guide.change.dels), (Some(1), Some(1)));
    assert_eq!(guide.hunks.len(), 1);
    assert_eq!(guide.hunks[0].text, " line four\n-line five\n+line 5\n");
    let q = LogQuery { include: vec![c1], exclude: vec![], path_prefix: None, since: None, skip: 0, max: 1, with_files: true };
    let files = git.log(&m, &q).await.unwrap().remove(0).files.unwrap();
    assert!(files.iter().any(|f| f.change == ChangeKind::Renamed && f.path.as_str() == "manual/guide.md"), "{files:?}");

    // The bundle carries the full tree: non-UTF-8 name, pax long path, submodule dir, modes.
    let bundle = tmp.path().join("b.tar.zst");
    let info = git.archive(&m, c0, &bundle).await.unwrap();
    let entries = read_bundle(&std::fs::read(&bundle).unwrap());
    assert_eq!(info.entries as usize, entries.len());
    let by_path: BTreeMap<&str, &BundleFile> = entries.iter().map(|e| (e.path.as_str(), e)).collect();
    assert_eq!(by_path["bin/tool"].mode, 0o755);
    assert_eq!(by_path["current"].link.as_deref(), Some("docs/guide.md"));
    assert_eq!(by_path["vendor/lib/"].kind, b'5');
    assert_eq!(by_path[long_path.as_str()].content, b"deep\n");
    assert!(by_path.contains_key("bad\u{fffd}.txt"));
    assert!(entries.iter().all(|e| e.mtime == at(0).unix_timestamp() as u64));
    let raw_paths: Vec<&str> = entries.iter().map(|e| e.path.trim_end_matches('/')).collect();
    assert!(raw_paths.windows(2).all(|w| w[0] != w[1]));

    let dest = tmp.path().join("checkout");
    git.materialize(&m, c0, &dest).await.unwrap();
    assert!(std::fs::metadata(dest.join("vendor/lib")).unwrap().is_dir());
    assert_eq!(std::fs::read_link(dest.join("current")).unwrap(), Path::new("docs/guide.md"));
    assert_eq!(std::fs::read(dest.join(&long_path)).unwrap(), b"deep\n");
}

#[tokio::test]
async fn pin_survives_gc_after_the_branch_is_deleted() {
    if !git_available() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let fixture = FixtureRepo::linear("main", vec![FixtureCommit::new("base", at(0), &[("f", "0\n")])]);
    let mut fixture = fixture;
    fixture.commits.push(FixtureCommit::new("kept", at(1), &[("f", "kept\n")]).with_parents(&[0]));
    fixture.commits.push(FixtureCommit::new("dropped", at(2), &[("f", "dropped\n")]).with_parents(&[0]));
    let fixture = fixture.with_ref("refs/heads/feature", 1).with_ref("refs/heads/scratch", 2);
    let (remote, shas) = build_fixture(&tmp.path().join("remote.git"), &fixture);
    let git = cli(tmp.path().join("data"));
    let m = mirror(3);
    let spec = test_fetch_spec(&remote.url());
    git.fetch(&m, &spec, &all()).await.unwrap();
    git.pin(&m, shas[1]).await.unwrap();

    remote.git(&["update-ref", "-d", "refs/heads/feature"]);
    remote.git(&["update-ref", "-d", "refs/heads/scratch"]);
    git.fetch(&m, &spec, &all()).await.unwrap();
    assert_eq!(git.resolve(&m, "feature").await, Err(GitError::NotFound), "pruned");
    let dir = git.mirror_path(&m);
    let gc = raw_git(&dir, &["-c", "gc.reflogExpire=now", "-c", "gc.reflogExpireUnreachable=now", "gc", "--prune=now", "-q"]);
    assert!(gc.status.success(), "{}", String::from_utf8_lossy(&gc.stderr));

    assert!(git.has_commit(&m, shas[1]).await.unwrap(), "pinned commit survives gc");
    assert_eq!(git.list_tree(&m, shas[1]).await.unwrap().entries.len(), 1);
    assert!(!git.has_commit(&m, shas[2]).await.unwrap(), "unpinned unreachable commit is pruned (control)");
}

#[tokio::test]
async fn force_push_then_fetch_by_id_and_token_stays_off_disk() {
    if !git_available() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let fixture = FixtureRepo::linear("main", vec![FixtureCommit::new("a", at(0), &[("f", "a\n")]), FixtureCommit::new("b", at(1), &[("f", "b\n")])]);
    let (remote, shas) = build_fixture(&tmp.path().join("remote.git"), &fixture);
    let spec = test_fetch_spec(&remote.url());
    let git = cli(tmp.path().join("data"));
    let (m1, m2) = (mirror(4), mirror(5));
    git.fetch(&m1, &spec, &all()).await.unwrap();

    // Force-push main back to `a`: `b` is unreachable on the remote but still stored there.
    remote.git(&["update-ref", "refs/heads/main", &shas[0].to_hex()]);
    let outcome = git.fetch(&m1, &spec, &all()).await.unwrap();
    assert_eq!(outcome.updated_refs, vec![("refs/heads/main".to_string(), shas[0])]);
    let fresh = git.fetch(&m2, &spec, &FetchWants { all_heads_and_tags: true, shas: vec![shas[1]] }).await.unwrap();
    assert_eq!(fresh.missing, vec![]);
    assert!(git.has_commit(&m2, shas[1]).await.unwrap());

    let token = spec.token.token.expose().to_owned();
    for path in [git.mirror_path(&m1).join("config"), git.mirror_path(&m1).join("FETCH_HEAD")] {
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        assert!(!text.contains(&token) && !text.to_ascii_lowercase().contains("extraheader"), "{}: {text}", path.display());
    }
    let url = raw_git(&git.mirror_path(&m1), &["config", "remote.origin.url"]);
    assert_eq!(String::from_utf8_lossy(&url.stdout).trim(), remote.url());
}

#[tokio::test]
async fn archive_and_materialize_reject_traversal_entries() {
    if !git_available() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let remote = Remote::init(&tmp.path().join("remote.git"), "main");
    let ok = remote.blob(b"ok\n");
    let evil = remote.blob(b"pwned\n");
    let dotdot = remote.tree(&[(b"a.txt".to_vec(), "100644", ok.clone()), (b"../escape".to_vec(), "100644", evil.clone())]);
    let dotgit = remote.tree(&[(b"a.txt".to_vec(), "100644", ok), (b".git/hooks/post-checkout".to_vec(), "100755", evil)]);
    let c0 = remote.commit(&dotdot, &[], "Ada", at(0), "dotdot", "");
    let c1 = remote.commit(&dotgit, &[], "Ada", at(1), "dotgit", "");
    remote.git(&["update-ref", "refs/heads/main", &c0]);
    remote.git(&["update-ref", "refs/heads/other", &c1]);
    let (c0, c1): (CommitSha, CommitSha) = (c0.parse().unwrap(), c1.parse().unwrap());

    let git = cli(tmp.path().join("data"));
    let m = mirror(6);
    git.fetch(&m, &test_fetch_spec(&remote.url()), &all()).await.unwrap();

    let listing = git.list_tree(&m, c0).await.unwrap();
    assert_eq!(listing.skipped_paths, 1);
    for sha in [c0, c1] {
        let dest = tmp.path().join(format!("co-{sha}"));
        assert!(matches!(git.materialize(&m, sha, &dest).await, Err(GitError::Invalid(_))), "{sha}");
        assert!(matches!(git.archive(&m, sha, &tmp.path().join(format!("{sha}.tar.zst"))).await, Err(GitError::Invalid(_))));
        assert!(!dest.join(".git").exists());
    }
    assert!(!tmp.path().join("escape").exists());
}

#[tokio::test]
async fn corrupt_mirror_is_recreated_by_the_next_fetch() {
    if !git_available() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let fixture = FixtureRepo::linear("main", vec![FixtureCommit::new("a", at(0), &[("f", "a\n")])]);
    let (remote, shas) = build_fixture(&tmp.path().join("remote.git"), &fixture);
    let spec = test_fetch_spec(&remote.url());
    let git = cli(tmp.path().join("data"));
    let m = mirror(7);
    git.fetch(&m, &spec, &all()).await.unwrap();
    std::fs::write(git.mirror_path(&m).join("HEAD"), "garbage\n").unwrap();

    assert!(matches!(git.has_commit(&m, shas[0]).await, Err(GitError::Corrupt(_))));
    git.fetch(&m, &spec, &all()).await.unwrap();
    assert!(git.has_commit(&m, shas[0]).await.unwrap());
    assert_eq!(git.resolve(&m, "HEAD").await, Ok(shas[0]));
    assert_eq!(git.list_mirrors().await.unwrap(), vec![m]);
}
