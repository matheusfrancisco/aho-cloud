//! `GitRepoReader` over bare mirrors with the `git` CLI ([`GitCli`]).
//!
//! Deviation from knowledge.md "Adapter internals": every operation (reads included) runs the
//! `git` CLI through `tokio::process` instead of `gix`. One hardened invocation path (see
//! [`cmd`]) keeps the security surface in one place; reads that would stream many objects use a
//! single `git cat-file --batch` process.
//!
//! - Mirrors live at `{data_dir}/mirrors/{org}/{repo}.git` (`OrgId`/`RepoId` `Display` forms,
//!   which parse back for [`GitRepoReader::list_mirrors`]); `{mirror}.lock` is the `flock` file,
//!   `{mirror}.corrupt` marks a mirror the next `fetch` re-creates.
//! - `fetch`, `pin`, `unpin`, `maintain`, `remove_mirror` hold a per-mirror `tokio` mutex plus the
//!   `flock`; readers take no lock.
//! - `archive`/`materialize` use the shared tree walker in [`bundle`] (own walk over `ls-tree` +
//!   `cat-file --batch`; never `git archive`/`checkout`).
//! - Tests: git is always present on dev machines, so the adapter tests run by default; when
//!   `git --version` fails they print `skipping: git not available` and return.

pub mod bundle;
mod cmd;
mod parse;
#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::{self, BufWriter};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use mage_core::domain::{BlobOid, CommitSha, LineRange, OrgId, RepoId, RepoPath, TreeOid};
use mage_core::ports::source_host::FetchSpec;
use mage_core::wire::indexer::{BlameHunk, CommitInfo, FileDiff, LogQuery, TreeDiffOptions};
use nix::fcntl::{Flock, FlockArg};
use serde::Deserialize;
use time::OffsetDateTime;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdout};
use tokio::sync::mpsc;

use self::bundle::{BundleWriter, Checkout, PlanItem, RawEntryKind, TreePlan};
use self::cmd::{Credentials, GitEnv, classify, remote_url, run, run_ok};
use crate::ports::git::{
    ArchiveInfo, BlobRead, FetchOutcome, FetchWants, GitError, GitRepoReader, MirrorRef, MirrorState, TreeEntry, TreeEntryKind, TreeListing,
};

/// Reads (everything except fetch/maintain/archive/materialize) never wait longer than this.
const READ_TIMEOUT_CAP: Duration = Duration::from_secs(120);
const LAST_FETCH_FILE: &str = "mage-last-fetch";

fn default_git_bin() -> PathBuf {
    PathBuf::from("git")
}

fn default_command_timeout_secs() -> u64 {
    600
}

#[derive(Deserialize, Clone, Debug)]
pub struct GitConfig {
    pub data_dir: PathBuf,
    #[serde(default = "default_git_bin")]
    pub git_bin: PathBuf,
    /// Timeout of `fetch`, `maintain`, `archive`, `materialize`; other reads use
    /// `min(this, 120 s)`. Must be > 0.
    #[serde(default = "default_command_timeout_secs")]
    pub command_timeout_secs: u64,
    /// Tests only: also allow `file://` remotes (`-c protocol.file.allow=always`).
    #[serde(default)]
    pub allow_file_protocol: bool,
}

/// `GitRepoReader` adapter (module docs).
pub struct GitCli {
    env: GitEnv,
    mirrors: PathBuf,
    timeout: Duration,
    read_timeout: Duration,
    locks: parking_lot::Mutex<HashMap<MirrorRef, Arc<tokio::sync::Mutex<()>>>>,
}

/// Held while mutating a mirror: in-process mutex plus the `flock` on `{mirror}.lock`.
struct MirrorLock {
    _flock: Flock<File>,
    _guard: tokio::sync::OwnedMutexGuard<()>,
}

fn io_err(what: &str) -> impl Fn(io::Error) -> GitError + '_ {
    move |e| GitError::Unavailable(format!("{what}: {e}"))
}

impl GitCli {
    /// Creates `{data_dir}/mirrors`, `{data_dir}/home` (empty `HOME` of every git process) and
    /// `{data_dir}/tmp`.
    pub fn new(config: GitConfig) -> Result<GitCli, GitError> {
        if config.command_timeout_secs == 0 {
            return Err(GitError::Invalid("git.command_timeout_secs must be > 0".into()));
        }
        let mirrors = config.data_dir.join("mirrors");
        let home = config.data_dir.join("home");
        for dir in [&mirrors, &home, &config.data_dir.join("tmp")] {
            std::fs::create_dir_all(dir).map_err(io_err(&format!("create {}", dir.display())))?;
        }
        let timeout = Duration::from_secs(config.command_timeout_secs);
        Ok(GitCli {
            env: GitEnv { bin: config.git_bin, home, path: std::env::var_os("PATH"), allow_file_protocol: config.allow_file_protocol },
            mirrors,
            timeout,
            read_timeout: timeout.min(READ_TIMEOUT_CAP),
            locks: parking_lot::Mutex::new(HashMap::new()),
        })
    }

    /// `{data_dir}/mirrors/{org}/{repo}.git`.
    pub fn mirror_path(&self, m: &MirrorRef) -> PathBuf {
        self.mirrors.join(m.org.to_string()).join(format!("{}.git", m.repo))
    }

    fn sibling(&self, m: &MirrorRef, suffix: &str) -> PathBuf {
        let mut path = self.mirror_path(m).into_os_string();
        path.push(suffix);
        PathBuf::from(path)
    }

    async fn lock(&self, m: &MirrorRef) -> Result<MirrorLock, GitError> {
        let mutex = {
            let mut locks = self.locks.lock();
            locks.entry(*m).or_default().clone()
        };
        let guard = mutex.lock_owned().await;
        let lock_path = self.sibling(m, ".lock");
        let flock = tokio::task::spawn_blocking(move || -> Result<Flock<File>, GitError> {
            if let Some(parent) = lock_path.parent() {
                std::fs::create_dir_all(parent).map_err(io_err("create mirror dir"))?;
            }
            let file = File::options().create(true).truncate(false).write(true).open(&lock_path).map_err(io_err("open mirror lock"))?;
            Flock::lock(file, FlockArg::LockExclusive).map_err(|(_, errno)| GitError::Unavailable(format!("flock mirror: {errno}")))
        })
        .await
        .map_err(|e| GitError::Unavailable(format!("flock task: {e}")))??;
        Ok(MirrorLock { _flock: flock, _guard: guard })
    }

    fn git<I, S>(&self, dir: &Path, args: I) -> tokio::process::Command
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        self.env.command(Some(dir), args, None)
    }

    /// The mirror dir, `NotFound` when absent.
    async fn require(&self, m: &MirrorRef) -> Result<PathBuf, GitError> {
        let dir = self.mirror_path(m);
        if tokio::fs::try_exists(&dir).await.map_err(io_err("stat mirror"))? { Ok(dir) } else { Err(GitError::NotFound) }
    }

    /// Marks the mirror for re-creation when `result` says it is corrupt.
    async fn observe<T>(&self, m: &MirrorRef, result: Result<T, GitError>) -> Result<T, GitError> {
        if let Err(GitError::Corrupt(msg)) = &result {
            tracing::warn!(org = %m.org, repo = %m.repo, error = %msg, "git mirror corrupt; next fetch re-creates it");
            if let Err(e) = tokio::fs::write(self.sibling(m, ".corrupt"), msg.as_bytes()).await {
                tracing::warn!(error = %e, "could not write the corrupt marker");
            }
        }
        result
    }

    /// `cat-file -e`; a damaged mirror is `Corrupt`, an absent object `false`.
    async fn object_exists(&self, dir: &Path, spec: &str) -> Result<bool, GitError> {
        let out = run(self.git(dir, ["cat-file", "-e", spec]), self.read_timeout).await?;
        if out.success {
            return Ok(true);
        }
        match classify(&out.stderr) {
            err @ GitError::Corrupt(_) => Err(err),
            _ => Ok(false),
        }
    }

    async fn require_commit(&self, dir: &Path, sha: CommitSha) -> Result<(), GitError> {
        if self.object_exists(dir, &format!("{sha}^{{commit}}")).await? { Ok(()) } else { Err(GitError::NotFound) }
    }

    /// Heads and tags peeled to commits.
    async fn refs(&self, dir: &Path) -> Result<BTreeMap<String, CommitSha>, GitError> {
        let out = run_ok(
            self.git(dir, ["for-each-ref", "--format=%(refname)%00%(objecttype)%00%(objectname)%00%(*objecttype)%00%(*objectname)", "refs/heads", "refs/tags"]),
            self.read_timeout,
        )
        .await?;
        let mut refs = BTreeMap::new();
        for line in String::from_utf8_lossy(&out).lines() {
            let fields: Vec<&str> = line.split('\0').collect();
            let [name, kind, oid, peeled_kind, peeled] = fields[..] else { continue };
            let target = if peeled_kind == "commit" { peeled } else if kind == "commit" { oid } else { continue };
            if let Ok(sha) = target.parse() {
                refs.insert(name.to_owned(), sha);
            }
        }
        Ok(refs)
    }

    async fn create_mirror(&self, dir: &Path, url: &str) -> Result<(), GitError> {
        if let Some(parent) = dir.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(io_err("create mirror parent"))?;
        }
        let init = async {
            let init = [std::ffi::OsStr::new("init"), std::ffi::OsStr::new("--bare"), std::ffi::OsStr::new("-q"), std::ffi::OsStr::new("--initial-branch=main"), dir.as_os_str()];
            run_ok(self.env.command(None, init, None), self.read_timeout).await?;
            run_ok(self.git(dir, ["config", "remote.origin.url", url]), self.read_timeout).await?;
            run_ok(self.git(dir, ["config", "fetch.prune", "true"]), self.read_timeout).await?;
            Ok::<(), GitError>(())
        };
        if let Err(e) = init.await {
            let _ = tokio::fs::remove_dir_all(dir).await;
            return Err(e);
        }
        Ok(())
    }

    async fn fetch_locked(&self, m: &MirrorRef, spec: &FetchSpec, wants: &FetchWants) -> Result<FetchOutcome, GitError> {
        let dir = self.mirror_path(m);
        let url = remote_url(spec)?;
        let marker = self.sibling(m, ".corrupt");
        let exists = tokio::fs::try_exists(&dir).await.map_err(io_err("stat mirror"))?;
        let marked = tokio::fs::try_exists(&marker).await.map_err(io_err("stat corrupt marker"))?;
        let healthy = exists && tokio::fs::try_exists(dir.join("HEAD")).await.unwrap_or(false);
        if marked || (exists && !healthy) {
            tracing::info!(org = %m.org, repo = %m.repo, "re-creating git mirror");
            if exists {
                tokio::fs::remove_dir_all(&dir).await.map_err(io_err("remove corrupt mirror"))?;
            }
            if marked {
                tokio::fs::remove_file(&marker).await.map_err(io_err("remove corrupt marker"))?;
            }
        }
        if !tokio::fs::try_exists(&dir).await.map_err(io_err("stat mirror"))? {
            self.create_mirror(&dir, &url).await?;
        } else {
            run_ok(self.git(&dir, ["config", "remote.origin.url", url.as_str()]), self.read_timeout).await?;
        }
        let creds = Credentials::from_spec(spec);
        let before = self.refs(&dir).await?;
        if wants.all_heads_and_tags {
            let fetch = ["fetch", "-q", "--prune", "--no-tags", "--no-recurse-submodules", "origin", "+refs/heads/*:refs/heads/*", "+refs/tags/*:refs/tags/*"];
            run_ok(self.env.command(Some(&dir), fetch, Some(&creds)), self.timeout).await?;
            let head = run_ok(self.env.command(Some(&dir), ["ls-remote", "--symref", "origin", "HEAD"], Some(&creds)), self.timeout).await?;
            let head = String::from_utf8_lossy(&head);
            let target = head.lines().find_map(|l| l.strip_prefix("ref: ").and_then(|r| r.split('\t').next()).map(str::to_owned));
            if let Some(target) = target.filter(|t| t.starts_with("refs/heads/")) {
                if self.object_exists(&dir, &format!("{target}^{{commit}}")).await? {
                    run_ok(self.git(&dir, ["symbolic-ref", "HEAD", target.as_str()]), self.read_timeout).await?;
                }
            }
        }
        let mut missing = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for sha in wants.shas.iter().filter(|s| seen.insert(**s)) {
            let spec = format!("{sha}^{{commit}}");
            if self.object_exists(&dir, &spec).await? {
                continue;
            }
            let out = run(self.env.command(Some(&dir), ["fetch", "-q", "--no-tags", "--no-recurse-submodules", "origin", &sha.to_string()], Some(&creds)), self.timeout).await?;
            if !out.success {
                let err = classify(&out.stderr);
                let lower = out.stderr.to_ascii_lowercase();
                let unknown_sha = ["not our ref", "couldn't find remote ref", "no such remote ref", "unadvertised object", "not found"].iter().any(|p| lower.contains(p));
                if err == GitError::Unauthorized || !unknown_sha {
                    return Err(err);
                }
            }
            if !self.object_exists(&dir, &spec).await? {
                missing.push(*sha);
            }
        }
        let after = self.refs(&dir).await?;
        let updated_refs = after.into_iter().filter(|(name, sha)| before.get(name) != Some(sha)).collect();
        let now = OffsetDateTime::now_utc().unix_timestamp();
        tokio::fs::write(dir.join(LAST_FETCH_FILE), now.to_string()).await.map_err(io_err("write last fetch"))?;
        Ok(FetchOutcome { updated_refs, missing })
    }

    async fn state(&self, m: &MirrorRef) -> Result<MirrorState, GitError> {
        let dir = self.mirror_path(m);
        if !tokio::fs::try_exists(&dir).await.map_err(io_err("stat mirror"))? {
            return Ok(MirrorState { exists: false, size_bytes: 0, last_fetch: None });
        }
        let last_fetch = match tokio::fs::read_to_string(dir.join(LAST_FETCH_FILE)).await {
            Ok(text) => text.trim().parse::<i64>().ok().and_then(|s| OffsetDateTime::from_unix_timestamp(s).ok()),
            Err(_) => None,
        };
        let walk = dir.clone();
        let size_bytes = tokio::task::spawn_blocking(move || dir_size(&walk))
            .await
            .map_err(|e| GitError::Unavailable(format!("size task: {e}")))?
            .map_err(io_err("mirror size"))?;
        Ok(MirrorState { exists: true, size_bytes, last_fetch })
    }

    async fn commit_info(&self, dir: &Path, sha: CommitSha) -> Result<CommitInfo, GitError> {
        self.require_commit(dir, sha).await?;
        let raw = run_ok(self.git(dir, ["cat-file", "commit", &sha.to_string()]), self.read_timeout).await?;
        parse::commit_object(sha, &raw)
    }

    async fn raw_tree(&self, dir: &Path, sha: CommitSha) -> Result<(TreeOid, Vec<bundle::RawTreeEntry>), GitError> {
        self.require_commit(dir, sha).await?;
        let tree = run_ok(self.git(dir, ["rev-parse", "--verify", &format!("{sha}^{{tree}}")]), self.read_timeout).await?;
        let tree: TreeOid = String::from_utf8_lossy(&tree).trim().parse().map_err(|_| GitError::Unavailable("rev-parse tree output".into()))?;
        let out = run_ok(self.git(dir, ["ls-tree", "-r", "-z", "-l", "--full-tree", &sha.to_string()]), self.read_timeout).await?;
        Ok((tree, parse::ls_tree(&out)?))
    }

    async fn batch(&self, dir: &Path, ids: Vec<String>) -> Result<BatchReader, GitError> {
        let mut cmd = self.git(dir, ["cat-file", "--batch"]);
        cmd.stdin(std::process::Stdio::piped()).stderr(std::process::Stdio::null());
        let mut child = cmd.spawn().map_err(|e| GitError::Unavailable(format!("spawn git cat-file: {e}")))?;
        let mut stdin = child.stdin.take().ok_or_else(|| GitError::Unavailable("cat-file stdin".into()))?;
        let stdout = child.stdout.take().ok_or_else(|| GitError::Unavailable("cat-file stdout".into()))?;
        let feeder = tokio::spawn(async move {
            for id in ids {
                stdin.write_all(id.as_bytes()).await?;
                stdin.write_all(b"\n").await?;
            }
            stdin.shutdown().await
        });
        Ok(BatchReader { child, stdout: BufReader::new(stdout), feeder })
    }

    async fn read_blobs_inner(&self, m: &MirrorRef, oids: &[BlobOid], max_bytes: u64) -> Result<Vec<BlobRead>, GitError> {
        let dir = self.require(m).await?;
        if oids.is_empty() {
            return Ok(Vec::new());
        }
        let work = async {
            let mut reader = self.batch(&dir, oids.iter().map(BlobOid::to_hex).collect()).await?;
            let mut out = Vec::with_capacity(oids.len());
            for oid in oids {
                match reader.next(max_bytes).await? {
                    BatchObject::Found { kind, size, bytes } if kind == "blob" => out.push(BlobRead { oid: *oid, size, bytes }),
                    _ => return Err(GitError::NotFound),
                }
            }
            reader.close().await?;
            Ok(out)
        };
        tokio::time::timeout(self.read_timeout, work).await.map_err(|_| GitError::Timeout)?
    }

    async fn diff_inner(&self, m: &MirrorRef, from: CommitSha, to: CommitSha, opts: &TreeDiffOptions) -> Result<Vec<FileDiff>, GitError> {
        let dir = self.require(m).await?;
        self.require_commit(&dir, from).await?;
        self.require_commit(&dir, to).await?;
        let renames = if opts.renames { "-M" } else { "--no-renames" };
        let (from_s, to_s) = (from.to_string(), to.to_string());
        let mut args: Vec<&str> = vec!["diff-tree", "--no-commit-id", "-r", "-z", "--raw", "--numstat", renames, &from_s, &to_s, "--"];
        args.extend(opts.paths.iter().map(RepoPath::as_str));
        let out = run_ok(self.git(&dir, &args), self.read_timeout).await?;
        let changes = parse::raw_numstat(&out)?;
        let mut diffs = Vec::with_capacity(changes.len());
        for change in changes {
            let (mut hunks, mut truncated) = (Vec::new(), false);
            if opts.hunks && !change.binary {
                let unified = format!("--unified={}", opts.context);
                let mut args: Vec<&str> = vec!["diff-tree", "--no-commit-id", "-p", "--no-color", "--no-ext-diff", "--no-textconv", &unified, renames, &from_s, &to_s, "--", change.change.path.as_str()];
                if let Some(old) = &change.change.old_path {
                    args.push(old.as_str());
                }
                let patch = run_ok(self.git(&dir, &args), self.read_timeout).await?;
                (hunks, truncated) = parse::hunks(&patch, opts.max_bytes_per_file)?;
            }
            diffs.push(FileDiff { change: change.change, hunks, binary: change.binary, truncated });
        }
        Ok(diffs)
    }

    async fn log_inner(&self, m: &MirrorRef, q: &LogQuery) -> Result<Vec<CommitInfo>, GitError> {
        let dir = self.require(m).await?;
        for sha in q.include.iter().chain(&q.exclude) {
            self.require_commit(&dir, *sha).await?;
        }
        if q.include.is_empty() || q.max == 0 {
            return Ok(Vec::new());
        }
        let mut args = vec!["rev-list".to_owned(), format!("--max-count={}", q.max), format!("--skip={}", q.skip)];
        if let Some(since) = q.since {
            args.push(format!("--max-age={}", since.unix_timestamp()));
        }
        args.extend(q.include.iter().map(CommitSha::to_hex));
        if !q.exclude.is_empty() {
            args.push("--not".into());
            args.extend(q.exclude.iter().map(CommitSha::to_hex));
        }
        args.push("--".into());
        if let Some(prefix) = &q.path_prefix {
            args.push(prefix.as_str().to_owned());
        }
        let out = run_ok(self.git(&dir, &args), self.read_timeout).await?;
        let shas: Vec<CommitSha> = String::from_utf8_lossy(&out)
            .lines()
            .map(|l| l.trim().parse().map_err(|_| GitError::Unavailable("rev-list output".into())))
            .collect::<Result<_, _>>()?;
        if shas.is_empty() {
            return Ok(Vec::new());
        }
        let work = async {
            let mut reader = self.batch(&dir, shas.iter().map(CommitSha::to_hex).collect()).await?;
            let mut commits = Vec::with_capacity(shas.len());
            for sha in &shas {
                match reader.next(u64::MAX).await? {
                    BatchObject::Found { kind, bytes: Some(bytes), .. } if kind == "commit" => commits.push(parse::commit_object(*sha, &bytes)?),
                    _ => return Err(GitError::Corrupt(format!("commit {sha} listed but unreadable"))),
                }
            }
            reader.close().await?;
            Ok::<_, GitError>(commits)
        };
        let mut commits = tokio::time::timeout(self.read_timeout, work).await.map_err(|_| GitError::Timeout)??;
        if q.with_files {
            for commit in &mut commits {
                let sha = commit.sha.to_hex();
                let args: Vec<String> = match commit.parents.first() {
                    Some(parent) => ["diff-tree", "--no-commit-id", "-r", "-z", "--raw", "--numstat", "-M", &parent.to_hex(), &sha].map(str::to_owned).to_vec(),
                    None => ["diff-tree", "--no-commit-id", "--root", "-r", "-z", "--raw", "--numstat", "-M", &sha].map(str::to_owned).to_vec(),
                };
                let out = run_ok(self.git(&dir, &args), self.read_timeout).await?;
                commit.files = Some(parse::raw_numstat(&out)?.into_iter().map(|c| c.change).collect());
            }
        }
        Ok(commits)
    }

    async fn blame_inner(&self, m: &MirrorRef, sha: CommitSha, path: &RepoPath, lines: LineRange) -> Result<Vec<BlameHunk>, GitError> {
        let dir = self.require(m).await?;
        self.require_commit(&dir, sha).await?;
        if !self.object_exists(&dir, &format!("{sha}:{path}")).await? {
            return Err(GitError::NotFound);
        }
        let range = format!("{},{}", lines.start(), lines.end());
        let out = run(self.git(&dir, ["blame", "--porcelain", "-L", &range, &sha.to_string(), "--", path.as_str()]), self.read_timeout).await?;
        if !out.success {
            if out.stderr.contains("has only") {
                return Err(GitError::Invalid(format!("line range {lines} outside {path}")));
            }
            return Err(classify(&out.stderr));
        }
        parse::blame_porcelain(&out.stdout)
    }

    /// Walks the tree at `sha` in [`TreePlan`] order, sending every item with its content to
    /// `consume` (run on a blocking thread).
    async fn walk_tree<R, C>(&self, m: &MirrorRef, sha: CommitSha, consume: C) -> Result<(CommitInfo, TreeOid, R), GitError>
    where
        R: Send + 'static,
        C: FnOnce(mpsc::Receiver<(PlanItem, Vec<u8>)>) -> Result<R, GitError> + Send + 'static,
    {
        let dir = self.require(m).await?;
        let commit = self.commit_info(&dir, sha).await?;
        let (tree, entries) = self.raw_tree(&dir, sha).await?;
        let plan = TreePlan::new(&entries)?;
        let (tx, rx) = mpsc::channel::<(PlanItem, Vec<u8>)>(32);
        let consumer = tokio::task::spawn_blocking(move || consume(rx));
        let produce = async {
            let ids: Vec<String> = plan.items.iter().filter_map(PlanItem::blob).map(|o| o.to_hex()).collect();
            let mut reader = if ids.is_empty() { None } else { Some(self.batch(&dir, ids).await?) };
            for item in plan.items {
                let content = match (item.blob(), reader.as_mut()) {
                    (Some(_), Some(reader)) => match reader.next(u64::MAX).await? {
                        BatchObject::Found { kind, bytes: Some(bytes), .. } if kind == "blob" => bytes,
                        _ => return Err(GitError::Corrupt(format!("blob of {} unreadable", String::from_utf8_lossy(&item.path)))),
                    },
                    _ => Vec::new(),
                };
                if tx.send((item, content)).await.is_err() {
                    break; // consumer failed; its error is reported below
                }
            }
            drop(tx);
            if let Some(reader) = reader {
                reader.close().await?;
            }
            Ok::<(), GitError>(())
        };
        let produced = tokio::time::timeout(self.timeout, produce).await.map_err(|_| GitError::Timeout).and_then(|r| r);
        let consumed = consumer.await.map_err(|e| GitError::Unavailable(format!("tree writer task: {e}")))?;
        let result = consumed?;
        produced?;
        Ok((commit, tree, result))
    }

    async fn maintain_locked(&self, m: &MirrorRef) -> Result<(), GitError> {
        let dir = self.require(m).await?;
        let out = run(self.git(&dir, ["maintenance", "run", "--auto"]), self.timeout).await?;
        if out.success {
            return Ok(());
        }
        let lower = out.stderr.to_ascii_lowercase();
        if lower.contains("is not a git command") || lower.contains("unknown subcommand") {
            run_ok(self.git(&dir, ["gc", "--auto"]), self.timeout).await?;
            return Ok(());
        }
        Err(classify(&out.stderr))
    }
}

fn dir_size(path: &Path) -> io::Result<u64> {
    let mut total = 0;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let meta = entry.metadata()?;
            if meta.is_dir() {
                stack.push(entry.path());
            } else {
                total += meta.len();
            }
        }
    }
    Ok(total)
}

enum BatchObject {
    Found { kind: String, size: u64, bytes: Option<Vec<u8>> },
    Missing,
}

/// One `git cat-file --batch` process; requests are fed by a background task, answers read in
/// order.
struct BatchReader {
    child: Child,
    stdout: BufReader<ChildStdout>,
    feeder: tokio::task::JoinHandle<io::Result<()>>,
}

impl BatchReader {
    /// Next answer; content larger than `max` is skipped (`bytes: None`).
    async fn next(&mut self, max: u64) -> Result<BatchObject, GitError> {
        let read_err = io_err("read cat-file");
        let mut header = Vec::new();
        if self.stdout.read_until(b'\n', &mut header).await.map_err(&read_err)? == 0 {
            return Err(GitError::Unavailable("cat-file ended early".into()));
        }
        let header = String::from_utf8_lossy(&header).trim_end().to_owned();
        let fields: Vec<&str> = header.split(' ').collect();
        match fields[..] {
            [_, "missing"] | [_, "ambiguous"] => Ok(BatchObject::Missing),
            [_, kind, size] => {
                let size: u64 = size.parse().map_err(|_| GitError::Unavailable(format!("cat-file header {header:?}")))?;
                let bytes = if size <= max {
                    let len = usize::try_from(size).map_err(|_| GitError::Unavailable("object too large".into()))?;
                    let mut buf = vec![0u8; len];
                    self.stdout.read_exact(&mut buf).await.map_err(&read_err)?;
                    Some(buf)
                } else {
                    let skipped = tokio::io::copy(&mut (&mut self.stdout).take(size), &mut tokio::io::sink()).await.map_err(&read_err)?;
                    if skipped != size {
                        return Err(GitError::Unavailable("cat-file ended early".into()));
                    }
                    None
                };
                let mut newline = [0u8; 1];
                self.stdout.read_exact(&mut newline).await.map_err(&read_err)?;
                Ok(BatchObject::Found { kind: kind.to_owned(), size, bytes })
            }
            _ => Err(GitError::Unavailable(format!("cat-file header {header:?}"))),
        }
    }

    async fn close(mut self) -> Result<(), GitError> {
        self.feeder.await.map_err(|e| GitError::Unavailable(format!("cat-file feeder: {e}")))?.map_err(io_err("write cat-file"))?;
        drop(self.stdout);
        let status = self.child.wait().await.map_err(io_err("wait cat-file"))?;
        if status.success() { Ok(()) } else { Err(GitError::Unavailable(format!("cat-file exited with {status}"))) }
    }
}

#[async_trait]
impl GitRepoReader for GitCli {
    async fn mirror_state(&self, m: &MirrorRef) -> Result<MirrorState, GitError> {
        self.state(m).await
    }

    async fn fetch(&self, m: &MirrorRef, spec: &FetchSpec, wants: &FetchWants) -> Result<FetchOutcome, GitError> {
        let _lock = self.lock(m).await?;
        let result = self.fetch_locked(m, spec, wants).await;
        self.observe(m, result).await
    }

    async fn pin(&self, m: &MirrorRef, sha: CommitSha) -> Result<(), GitError> {
        let _lock = self.lock(m).await?;
        let result = async {
            let dir = self.require(m).await?;
            self.require_commit(&dir, sha).await?;
            run_ok(self.git(&dir, ["update-ref", &format!("refs/mage/keep/{sha}"), &sha.to_string()]), self.read_timeout).await.map(drop)
        }
        .await;
        self.observe(m, result).await
    }

    async fn unpin(&self, m: &MirrorRef, sha: CommitSha) -> Result<(), GitError> {
        let _lock = self.lock(m).await?;
        let result = async {
            let dir = self.require(m).await?;
            run_ok(self.git(&dir, ["update-ref", "-d", &format!("refs/mage/keep/{sha}")]), self.read_timeout).await.map(drop)
        }
        .await;
        self.observe(m, result).await
    }

    async fn resolve(&self, m: &MirrorRef, rev: &str) -> Result<CommitSha, GitError> {
        if rev.is_empty() || rev.starts_with('-') || rev.contains(['\0', '\n', ' ']) {
            return Err(GitError::Invalid(format!("invalid rev {rev:?}")));
        }
        let result = async {
            let dir = self.require(m).await?;
            let out = run(self.git(&dir, ["rev-parse", "--verify", "--quiet", "--end-of-options", &format!("{rev}^{{commit}}")]), self.read_timeout).await?;
            if !out.success {
                return Err(if out.stderr.trim().is_empty() { GitError::NotFound } else { classify(&out.stderr) });
            }
            String::from_utf8_lossy(&out.stdout).trim().parse().map_err(|_| GitError::Unavailable("rev-parse output".into()))
        }
        .await;
        self.observe(m, result).await
    }

    async fn has_commit(&self, m: &MirrorRef, sha: CommitSha) -> Result<bool, GitError> {
        let dir = self.mirror_path(m);
        if !tokio::fs::try_exists(&dir).await.map_err(io_err("stat mirror"))? {
            return Ok(false);
        }
        let result = self.object_exists(&dir, &format!("{sha}^{{commit}}")).await;
        self.observe(m, result).await
    }

    async fn commit(&self, m: &MirrorRef, sha: CommitSha) -> Result<CommitInfo, GitError> {
        let result = async {
            let dir = self.require(m).await?;
            self.commit_info(&dir, sha).await
        }
        .await;
        self.observe(m, result).await
    }

    async fn list_tree(&self, m: &MirrorRef, sha: CommitSha) -> Result<TreeListing, GitError> {
        let result = async {
            let dir = self.require(m).await?;
            let (tree, raw) = self.raw_tree(&dir, sha).await?;
            let mut entries = Vec::with_capacity(raw.len());
            let mut skipped_paths = 0u32;
            for entry in raw {
                let Some(path) = String::from_utf8(entry.path).ok().and_then(|p| RepoPath::new(p).ok()) else {
                    skipped_paths += 1;
                    continue;
                };
                let kind = match entry.kind {
                    RawEntryKind::File { oid, executable } => TreeEntryKind::File { oid, executable },
                    RawEntryKind::Symlink { oid } => TreeEntryKind::Symlink { oid },
                    RawEntryKind::Submodule { commit } => TreeEntryKind::Submodule { commit },
                };
                entries.push(TreeEntry { path, kind, size: entry.size });
            }
            Ok(TreeListing { tree, entries, skipped_paths })
        }
        .await;
        self.observe(m, result).await
    }

    async fn read_blobs(&self, m: &MirrorRef, oids: &[BlobOid], max_bytes: u64) -> Result<Vec<BlobRead>, GitError> {
        let result = self.read_blobs_inner(m, oids, max_bytes).await;
        self.observe(m, result).await
    }

    async fn diff(&self, m: &MirrorRef, from: CommitSha, to: CommitSha, opts: &TreeDiffOptions) -> Result<Vec<FileDiff>, GitError> {
        let result = self.diff_inner(m, from, to, opts).await;
        self.observe(m, result).await
    }

    async fn log(&self, m: &MirrorRef, q: &LogQuery) -> Result<Vec<CommitInfo>, GitError> {
        let result = self.log_inner(m, q).await;
        self.observe(m, result).await
    }

    async fn blame(&self, m: &MirrorRef, sha: CommitSha, path: &RepoPath, lines: LineRange) -> Result<Vec<BlameHunk>, GitError> {
        let result = self.blame_inner(m, sha, path, lines).await;
        self.observe(m, result).await
    }

    async fn archive(&self, m: &MirrorRef, sha: CommitSha, dest: &Path) -> Result<ArchiveInfo, GitError> {
        let dest_path = dest.to_path_buf();
        // The writer needs the commit time before the first entry: read it first.
        let mtime = match self.require(m).await {
            Ok(dir) => self.commit_info(&dir, sha).await.map(|c| c.committer_time.unix_timestamp()),
            Err(e) => Err(e),
        };
        let mtime = self.observe(m, mtime).await?;
        let result = self
            .walk_tree(m, sha, move |mut rx| {
                let write_err = io_err("write bundle");
                let file = File::create(&dest_path).map_err(&write_err)?;
                let mut writer = BundleWriter::new(BufWriter::new(file), mtime).map_err(&write_err)?;
                while let Some((item, content)) = rx.blocking_recv() {
                    writer.append(&item, &content).map_err(&write_err)?;
                }
                let summary = writer.finish().map_err(&write_err)?;
                let file = summary.out.into_inner().map_err(|e| write_err(e.into_error()))?;
                file.sync_all().map_err(&write_err)?;
                Ok((summary.bytes, summary.sha256, summary.entries))
            })
            .await;
        let (_, tree, (bytes, sha256, entries)) = self.observe(m, result).await?;
        Ok(ArchiveInfo { tree, bytes, sha256, entries })
    }

    async fn materialize(&self, m: &MirrorRef, sha: CommitSha, dest: &Path) -> Result<(), GitError> {
        let dest_path = dest.to_path_buf();
        let result = self
            .walk_tree(m, sha, move |mut rx| {
                let checkout = Checkout::create(&dest_path)?;
                while let Some((item, content)) = rx.blocking_recv() {
                    checkout.write(&item, &content).map_err(io_err("write checkout"))?;
                }
                Ok(())
            })
            .await;
        self.observe(m, result).await.map(drop)
    }

    async fn maintain(&self, m: &MirrorRef) -> Result<(), GitError> {
        let _lock = self.lock(m).await?;
        let result = self.maintain_locked(m).await;
        self.observe(m, result).await
    }

    async fn remove_mirror(&self, m: &MirrorRef) -> Result<(), GitError> {
        let _lock = self.lock(m).await?;
        let dir = self.mirror_path(m);
        match tokio::fs::remove_dir_all(&dir).await {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(io_err("remove mirror")(e)),
        }
        match tokio::fs::remove_file(self.sibling(m, ".corrupt")).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(io_err("remove corrupt marker")(e)),
        }
    }

    async fn list_mirrors(&self) -> Result<Vec<MirrorRef>, GitError> {
        let root = self.mirrors.clone();
        tokio::task::spawn_blocking(move || -> io::Result<Vec<MirrorRef>> {
            let mut out = Vec::new();
            for org_entry in std::fs::read_dir(&root)? {
                let org_entry = org_entry?;
                let Some(org) = org_entry.file_name().to_str().and_then(|s| s.parse::<OrgId>().ok()) else { continue };
                if !org_entry.file_type()?.is_dir() {
                    continue;
                }
                for repo_entry in std::fs::read_dir(org_entry.path())? {
                    let repo_entry = repo_entry?;
                    if !repo_entry.file_type()?.is_dir() {
                        continue;
                    }
                    let name = repo_entry.file_name();
                    let Some(repo) = name.to_str().and_then(|s| s.strip_suffix(".git")).and_then(|s| s.parse::<RepoId>().ok()) else { continue };
                    out.push(MirrorRef::new(org, repo));
                }
            }
            out.sort();
            Ok(out)
        })
        .await
        .map_err(|e| GitError::Unavailable(format!("list mirrors task: {e}")))?
        .map_err(io_err("list mirrors"))
    }
}
