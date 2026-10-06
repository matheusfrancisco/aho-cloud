//! Baseline git repositories for bases and forks (executor.md "Baselines (git)").
//!
//! A base's tree has no `.git`: its history lives in a separate git dir that agent processes
//! cannot write. `refs/mage/source` is the extracted bundle; `refs/mage/prepared` is the tree after
//! build + migrate + seed (an alias of `source` for SourceOnly bases). A fork gets a fresh git dir
//! whose object store borrows the base's through `objects/info/alternates`.
//!
//! Every git call is argv-only with a cleared environment: `PATH` from the daemon, `HOME` = an
//! empty dir under `scratch_dir`, no system/global config, fixed identity and dates, and hooks,
//! fsmonitor, CRLF conversion, and automatic gc disabled. Diffs, cleanliness checks, and patch
//! checks stage into throwaway index files under `scratch_dir`, so they never share or mutate the
//! repository index and never write refs.

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};

use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use uuid::Uuid;

use crate::ports::executor::{
    ApplyMode, ApplyOutcome, Baseline, ExecError, FileChange, PATCH_MAX_BYTES, PatchFile, PatchOutput, PathGlob,
    RejectReason, RelPath,
};

/// Global `-c` options on every invocation. `gc.auto=0` / `maintenance.auto=false` keep `commit`
/// from detaching a background `git gc --auto` that would outlive the call.
const GIT_OPTIONS: &[&str] = &[
    "-c",
    "core.hooksPath=/dev/null",
    "-c",
    "core.fsmonitor=false",
    "-c",
    "core.autocrlf=false",
    "-c",
    "core.symlinks=true",
    "-c",
    "core.quotepath=false",
    "-c",
    "user.name=mage",
    "-c",
    "user.email=mage@localhost",
    "-c",
    "gc.auto=0",
    "-c",
    "maintenance.auto=false",
];

/// Fixed environment on every invocation (besides `PATH`, `HOME`, `GIT_DIR`, `GIT_WORK_TREE`,
/// `GIT_INDEX_FILE`). Fixed identity and dates make baseline commit ids reproducible.
const GIT_ENV: &[(&str, &str)] = &[
    ("GIT_CONFIG_NOSYSTEM", "1"),
    ("GIT_CONFIG_GLOBAL", "/dev/null"),
    ("GIT_ATTR_NOSYSTEM", "1"),
    ("GIT_TERMINAL_PROMPT", "0"),
    ("LC_ALL", "C"),
    ("GIT_AUTHOR_NAME", "mage"),
    ("GIT_AUTHOR_EMAIL", "mage@localhost"),
    ("GIT_AUTHOR_DATE", "@0 +0000"),
    ("GIT_COMMITTER_NAME", "mage"),
    ("GIT_COMMITTER_EMAIL", "mage@localhost"),
    ("GIT_COMMITTER_DATE", "@0 +0000"),
];

/// Branch `HEAD` points at in a baseline repository.
const BRANCH: &str = "mage";
const SOURCE: &str = "source";
const SOURCE_REF: &str = "refs/mage/source";
const PREPARED_REF: &str = "refs/mage/prepared";
/// Longest accepted baseline name (`refs/mage/<name>`).
const NAME_MAX: usize = 64;

/// Empty `HOME` for git, under `scratch_dir`.
const HOME_DIR: &str = "git-home";
/// Cap on `Rejected.detail`.
const DETAIL_MAX_BYTES: usize = 4096;
/// Cap on the stderr text quoted in `ExecError`s.
const ERROR_TAIL_BYTES: usize = 1024;
/// Stderr kept per invocation (the tail); failure paths are parsed from it.
const STDERR_KEEP_BYTES: usize = 1 << 20;
/// Cap on listing outputs (`--raw`, `--numstat`, `--name-only`).
const LISTING_MAX_BYTES: usize = 256 << 20;

/// Lines on `git apply` stderr naming a path that did not apply (`error: <path><suffix>`).
const FAILURE_SUFFIXES: &[&str] = &[
    ": does not exist in index",
    ": No such file or directory",
    ": already exists in working directory",
    ": already exists in index",
    ": patch does not apply",
    ": does not match index",
    ": wrong type",
];

/// `git apply` parse errors that may be reported as `fatal:` rather than `error:`.
const PARSE_FAILURES: &[&str] = &[
    "corrupt patch",
    "unrecognized input",
    "No valid patches in input",
    "patch fragment without header",
    "lacks filename information",
    "only garbage",
    "bad git-diff",
];

/// A baseline git dir plus the tree it describes.
#[derive(Debug, Clone)]
pub(crate) struct GitBaseline {
    git_dir: PathBuf,
    work_tree: PathBuf,
    scratch_dir: PathBuf,
}

impl GitBaseline {
    /// Handle on an existing (or about to be initialised) baseline. Relative paths resolve
    /// against the daemon's cwd at call time; `scratch_dir` holds temp indexes, patch files, and
    /// git's empty `HOME`, and is created on demand.
    pub(crate) fn new(git_dir: PathBuf, work_tree: PathBuf, scratch_dir: PathBuf) -> Self {
        Self { git_dir, work_tree, scratch_dir }
    }

    #[cfg(test)]
    pub(crate) fn git_dir(&self) -> &Path {
        &self.git_dir
    }

    /// Initialise the git dir (bare-style with `core.bare=false`, no templates), write
    /// `info/exclude` (one pattern per line, e.g. `node_modules/`), and commit the whole tree as
    /// `refs/mage/source`. Returns the commit id (hex).
    pub(crate) async fn init_source(&self, excludes: &[String]) -> Result<String, ExecError> {
        let mut exclude_file = String::new();
        for pattern in excludes {
            if pattern.is_empty() || pattern.contains(['\n', '\r', '\0']) {
                return Err(ExecError::Invalid(format!("exclude pattern {pattern:?} must be one non-empty line")));
            }
            exclude_file.push_str(pattern);
            exclude_file.push('\n');
        }
        // Without GIT_WORK_TREE so init does not record `core.worktree`; `--bare` keeps the
        // GIT_DIR layout flat. The work tree is always passed through the environment instead.
        let init_branch = format!("--initial-branch={BRANCH}");
        let init = ["init", "--bare", "--quiet", "--template=", init_branch.as_str()];
        self.run_ok(init, Opts { work_tree: false, ..Opts::plain() }).await?;
        self.run_ok(["config", "core.bare", "false"], Opts::plain()).await?;
        let info = self.git_dir.join("info");
        tokio::fs::create_dir_all(&info).await?;
        tokio::fs::write(info.join("exclude"), exclude_file).await?;
        self.commit_baseline(SOURCE).await
    }

    /// Stage the whole tree, commit it on top of `HEAD` with message `name`, and point
    /// `refs/mage/<name>` at the commit. Returns the commit id (hex).
    pub(crate) async fn commit_baseline(&self, name: &str) -> Result<String, ExecError> {
        let refname = mage_ref(name)?;
        self.run_ok(["add", "-A"], Opts::plain()).await?;
        self.run_ok(["commit", "--allow-empty", "--quiet", "-m", name], Opts::plain()).await?;
        let oid = self.commit_of("HEAD").await?;
        self.run_ok(["update-ref", refname.as_str(), oid.as_str()], Opts::plain()).await?;
        Ok(oid)
    }

    /// `refs/mage/<to>` := `refs/mage/<from>` (SourceOnly bases alias `prepared` → `source`).
    pub(crate) async fn alias_ref(&self, from: &str, to: &str) -> Result<(), ExecError> {
        let from = mage_ref(from)?;
        let to = mage_ref(to)?;
        let oid = self.commit_of(&from).await?;
        self.run_ok(["update-ref", to.as_str(), oid.as_str()], Opts::plain()).await?;
        Ok(())
    }

    /// Create `git_dir` fresh, borrowing the base's objects through `objects/info/alternates`
    /// (absolute path) and copying `HEAD`, `config`, `info/exclude`, `refs/`, and `packed-refs`.
    /// No index is created; every operation stages into its own temp index.
    pub(crate) fn fork_from(
        base_git_dir: &Path,
        git_dir: PathBuf,
        work_tree: PathBuf,
        scratch_dir: PathBuf,
    ) -> Result<Self, ExecError> {
        let objects = base_git_dir.join("objects");
        let objects = fs::canonicalize(&objects).map_err(|err| match err.kind() {
            io::ErrorKind::NotFound => ExecError::NotFound(format!("base objects {}", objects.display())),
            _ => ExecError::Io(format!("resolving {}: {err}", objects.display())),
        })?;
        let mut alternates = objects.into_os_string().into_vec();
        if alternates.contains(&b'\n') {
            return Err(ExecError::Invalid("base git dir path contains a newline".into()));
        }
        alternates.push(b'\n');
        if let Some(parent) = git_dir.parent() {
            fs::create_dir_all(parent)?;
        }
        match fs::create_dir(&git_dir) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                return Err(ExecError::Conflict(format!("fork git dir {} already exists", git_dir.display())));
            }
            Err(err) => return Err(err.into()),
        }
        if let Err(err) = populate_fork(base_git_dir, &git_dir, &alternates) {
            // Only what this call just created.
            let _ = fs::remove_dir_all(&git_dir);
            return Err(err);
        }
        Ok(Self::new(git_dir, work_tree, scratch_dir))
    }

    /// The work tree (untracked non-ignored files included) as a patch against
    /// `refs/mage/<against>`, minus `exclude` globs and binary files (executor.md
    /// `diff_against_base`).
    pub(crate) async fn diff(&self, against: Baseline, exclude: &[PathGlob]) -> Result<PatchOutput, ExecError> {
        let base = self.resolve(against).await?;
        let mut pathspec: Vec<String> = exclude.iter().map(|g| format!(":(exclude,glob){}", g.as_str())).collect();
        let index = self.stage(&base, &pathspec).await?;
        let opts = Opts::index(index.path());

        let numstat = ["diff", "--cached", "--numstat", "-z", "--no-renames", base.as_str(), "--", "."];
        let numstat = self.run_ok(numstat.into_iter().chain(pathspec.iter().map(String::as_str)), opts).await?;
        let binary = binary_paths(&numstat)?;

        let raw = ["diff", "--cached", "--raw", "-z", "--no-abbrev", "--no-renames", base.as_str(), "--", "."];
        let raw = self.run_ok(raw.into_iter().chain(pathspec.iter().map(String::as_str)), opts).await?;
        let files = changed_files(&raw, &binary)?;

        pathspec.extend(binary.iter().map(|p| format!(":(exclude,literal){}", p.as_str())));
        let patch_args = [
            "diff",
            "--cached",
            "--no-renames",
            "--no-color",
            "--no-ext-diff",
            "--full-index",
            "-U3",
            "--src-prefix=a/",
            "--dst-prefix=b/",
            base.as_str(),
            "--",
            ".",
        ];
        let patch_args = patch_args.into_iter().chain(pathspec.iter().map(String::as_str));
        let out = self.exec(patch_args, Opts { stdout_max: PATCH_MAX_BYTES, ..opts }).await?;
        if out.overflow {
            return Err(ExecError::Invalid("patch too large".into()));
        }
        if !out.status.success() {
            return Err(out.failure());
        }

        let excluded = if exclude.is_empty() { Vec::new() } else { self.excluded(&base, &index, exclude).await? };
        Ok(PatchOutput { patch: out.stdout, files, excluded, binary })
    }

    /// True when the work tree (untracked non-ignored files included) equals
    /// `refs/mage/<against>`.
    #[cfg(test)]
    pub(crate) async fn is_clean(&self, against: Baseline) -> Result<bool, ExecError> {
        let base = self.resolve(against).await?;
        let index = self.stage(&base, &[]).await?;
        let args = ["diff", "--cached", "--quiet", "--no-renames", base.as_str(), "--"];
        let out = self.exec(args, Opts::index(index.path())).await?;
        match out.status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err(out.failure()),
        }
    }

    /// Paths that differ from `refs/mage/<against>` (for rebuild scopes); sorted.
    pub(crate) async fn changed_paths(&self, against: Baseline) -> Result<Vec<RelPath>, ExecError> {
        let base = self.resolve(against).await?;
        let index = self.stage(&base, &[]).await?;
        let args = ["diff", "--cached", "--name-only", "-z", "--no-renames", base.as_str(), "--"];
        let names = self.run_ok(args, Opts::index(index.path())).await?;
        nul_paths(&names)
    }

    /// Apply `patch` to the work tree with `git apply` (zero fuzz; atomic). `Exact` first
    /// requires the tree to equal `refs/mage/prepared` (`BaseMismatch` lists the differing
    /// paths); `AllowOffset` lets hunks move.
    pub(crate) async fn apply(&self, patch: &[u8], mode: ApplyMode) -> Result<ApplyOutcome, ExecError> {
        if patch.is_empty() {
            return Ok(rejected(RejectReason::Malformed, "empty patch".into(), Vec::new()));
        }
        match mode {
            ApplyMode::Exact => {
                let changed = self.changed_paths(Baseline::Prepared).await?;
                if !changed.is_empty() {
                    let detail = format!("work tree differs from {PREPARED_REF} in {} path(s)", changed.len());
                    return Ok(rejected(RejectReason::BaseMismatch, detail, changed));
                }
            }
            ApplyMode::AllowOffset => {}
        }
        let file = self.write_patch(patch).await?;
        let files = match self.patch_paths(file.path()).await? {
            Ok(files) => files,
            Err(malformed) => return Ok(malformed),
        };
        let args = [OsStr::new("apply"), OsStr::new("--whitespace=nowarn"), file.path().as_os_str()];
        let out = self.exec(args, Opts::plain()).await?;
        if out.status.success() { Ok(applied(files, patch)) } else { apply_failure(&out) }
    }

    /// Whether `patch` applies to `refs/mage/<baseline>` (`git apply --cached --check` on a temp
    /// index). Never touches the work tree. `Applied` means "would apply".
    pub(crate) async fn check(&self, patch: &[u8], baseline: Baseline) -> Result<ApplyOutcome, ExecError> {
        if patch.is_empty() {
            return Ok(rejected(RejectReason::Malformed, "empty patch".into(), Vec::new()));
        }
        let base = self.resolve(baseline).await?;
        let file = self.write_patch(patch).await?;
        let files = match self.patch_paths(file.path()).await? {
            Ok(files) => files,
            Err(malformed) => return Ok(malformed),
        };
        let index = self.temp_file("index");
        let opts = Opts::index(index.path());
        self.run_ok(["read-tree", base.as_str()], opts).await?;
        let args = [
            OsStr::new("apply"),
            OsStr::new("--cached"),
            OsStr::new("--check"),
            OsStr::new("--whitespace=nowarn"),
            file.path().as_os_str(),
        ];
        let out = self.exec(args, opts).await?;
        if out.status.success() { Ok(applied(files, patch)) } else { apply_failure(&out) }
    }

    // -----------------------------------------------------------------------------------------
    // Internals
    // -----------------------------------------------------------------------------------------

    async fn resolve(&self, baseline: Baseline) -> Result<String, ExecError> {
        let refname = match baseline {
            Baseline::Source => SOURCE_REF,
            Baseline::Prepared => PREPARED_REF,
        };
        self.commit_of(refname).await
    }

    /// Commit id `rev` points at; `NotFound` when it does not resolve to a commit.
    async fn commit_of(&self, rev: &str) -> Result<String, ExecError> {
        let spec = format!("{rev}^{{commit}}");
        let out = self.exec(["rev-parse", "--verify", "--quiet", spec.as_str()], Opts::plain()).await?;
        match out.status.code() {
            Some(0) => {}
            Some(1) => return Err(ExecError::NotFound(rev.to_owned())),
            _ => return Err(out.failure()),
        }
        let oid = std::str::from_utf8(&out.stdout).map(str::trim_end).unwrap_or_default();
        if !matches!(oid.len(), 40 | 64) || !oid.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(ExecError::Io(format!("git rev-parse {rev}: unexpected output")));
        }
        Ok(oid.to_owned())
    }

    /// A fresh temp index holding `base`, with the work tree staged over it
    /// (`git add -A -- . <pathspec>`).
    async fn stage(&self, base: &str, pathspec: &[String]) -> Result<TempFile, ExecError> {
        let index = self.temp_file("index");
        let opts = Opts::index(index.path());
        self.run_ok(["read-tree", base], opts).await?;
        let add = ["add", "-A", "--", "."].into_iter().chain(pathspec.iter().map(String::as_str));
        self.run_ok(add, opts).await?;
        Ok(index)
    }

    /// Paths matching `exclude` that differ from `base`, from a second index staged without the
    /// excludes. It starts as a copy of `staged` (whose entries keep their stat data), so only
    /// the excluded paths are hashed again.
    async fn excluded(&self, base: &str, staged: &TempFile, exclude: &[PathGlob]) -> Result<Vec<RelPath>, ExecError> {
        let index = self.temp_file("index");
        tokio::fs::copy(staged.path(), index.path()).await?;
        let opts = Opts::index(index.path());
        self.run_ok(["add", "-A", "--", "."], opts).await?;
        let globs: Vec<String> = exclude.iter().map(|g| format!(":(glob){}", g.as_str())).collect();
        let args = ["diff", "--cached", "--name-only", "-z", "--no-renames", base, "--"];
        let names = self.run_ok(args.into_iter().chain(globs.iter().map(String::as_str)), opts).await?;
        nul_paths(&names)
    }

    /// Paths `patch` touches (`git apply --numstat`), or the `Malformed` rejection.
    async fn patch_paths(&self, file: &Path) -> Result<Result<Vec<RelPath>, ApplyOutcome>, ExecError> {
        let args = [OsStr::new("apply"), OsStr::new("--numstat"), OsStr::new("-z"), file.as_os_str()];
        let out = self.exec(args, Opts::plain()).await?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            if is_parse_failure(&stderr) {
                return Ok(Err(rejected(RejectReason::Malformed, tail(&stderr, DETAIL_MAX_BYTES), Vec::new())));
            }
            return Err(out.failure());
        }
        Ok(apply_numstat_paths(&out.stdout).map_err(|detail| rejected(RejectReason::Malformed, detail, Vec::new())))
    }

    async fn write_patch(&self, patch: &[u8]) -> Result<TempFile, ExecError> {
        use tokio::io::AsyncWriteExt;
        tokio::fs::create_dir_all(&self.scratch_dir).await?;
        let file = self.temp_file("patch");
        let mut handle = tokio::fs::OpenOptions::new().write(true).create_new(true).open(file.path()).await?;
        handle.write_all(patch).await?;
        handle.flush().await?;
        Ok(file)
    }

    /// Unique path under `scratch_dir`, removed (with a stale `.lock`) on drop.
    fn temp_file(&self, kind: &str) -> TempFile {
        TempFile(self.scratch_dir.join(format!("git-{kind}-{}", Uuid::new_v4().simple())))
    }

    async fn run_ok<I, S>(&self, args: I, opts: Opts<'_>) -> Result<Vec<u8>, ExecError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let out = self.exec(args, opts).await?;
        if out.overflow {
            return Err(ExecError::Io(format!("git {} output exceeds {} bytes", out.label, opts.stdout_max)));
        }
        if !out.status.success() {
            return Err(out.failure());
        }
        Ok(out.stdout)
    }

    /// Run git with the hardened environment; never fails on a non-zero exit.
    async fn exec<I, S>(&self, args: I, opts: Opts<'_>) -> Result<Output, ExecError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let git_dir = std::path::absolute(&self.git_dir)?;
        let work_tree = std::path::absolute(&self.work_tree)?;
        let home = std::path::absolute(self.scratch_dir.join(HOME_DIR))?;
        tokio::fs::create_dir_all(&home).await?;

        let mut cmd = Command::new("git");
        cmd.env_clear();
        if let Some(path) = std::env::var_os("PATH") {
            cmd.env("PATH", path);
        }
        cmd.env("HOME", &home).env("GIT_DIR", &git_dir).envs(GIT_ENV.iter().copied());
        if opts.work_tree {
            cmd.env("GIT_WORK_TREE", &work_tree);
        }
        if let Some(index) = opts.index {
            cmd.env("GIT_INDEX_FILE", std::path::absolute(index)?);
        }
        cmd.args(GIT_OPTIONS);
        let mut label = String::new();
        for (i, arg) in args.into_iter().enumerate() {
            if i == 0 {
                label = arg.as_ref().to_string_lossy().into_owned();
            }
            cmd.arg(arg);
        }
        cmd.current_dir(&work_tree)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(err) if err.kind() == io::ErrorKind::NotFound && !work_tree.is_dir() => {
                return Err(ExecError::Io(format!("work tree {} does not exist", work_tree.display())));
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Err(ExecError::Unavailable("git".into())),
            Err(err) => return Err(ExecError::Io(format!("spawning git {label}: {err}"))),
        };
        let stdout = child.stdout.take().ok_or_else(|| ExecError::Io("git stdout is not piped".into()))?;
        let stderr = child.stderr.take().ok_or_else(|| ExecError::Io("git stderr is not piped".into()))?;
        // `read_capped` drops the pipe once over the cap, so git dies of EPIPE instead of
        // blocking while stderr is still being drained.
        let (stdout, stderr) =
            tokio::join!(read_capped(stdout, opts.stdout_max), read_tail(stderr, STDERR_KEEP_BYTES));
        let (stdout, overflow) = stdout?;
        let stderr = stderr?;
        if overflow {
            let _ = child.start_kill();
        }
        let status = child.wait().await?;
        Ok(Output { label, status, stdout, stderr, overflow })
    }
}

/// Knobs of one git invocation.
#[derive(Clone, Copy)]
struct Opts<'a> {
    /// `GIT_INDEX_FILE`; the repository index when `None`.
    index: Option<&'a Path>,
    /// Export `GIT_WORK_TREE`.
    work_tree: bool,
    /// Stdout cap; beyond it the run is cut short and flagged `overflow`.
    stdout_max: usize,
}

impl<'a> Opts<'a> {
    const fn plain() -> Self {
        Self { index: None, work_tree: true, stdout_max: LISTING_MAX_BYTES }
    }

    const fn index(index: &'a Path) -> Self {
        Self { index: Some(index), work_tree: true, stdout_max: LISTING_MAX_BYTES }
    }
}

struct Output {
    /// Subcommand, for error messages.
    label: String,
    status: ExitStatus,
    stdout: Vec<u8>,
    /// The last `STDERR_KEEP_BYTES`.
    stderr: Vec<u8>,
    overflow: bool,
}

impl Output {
    fn failure(&self) -> ExecError {
        let stderr = String::from_utf8_lossy(&self.stderr);
        ExecError::Io(format!("git {} failed ({}): {}", self.label, self.status, tail(&stderr, ERROR_TAIL_BYTES)))
    }
}

/// A scratch file removed on drop, together with a `.lock` git may have left beside it.
struct TempFile(PathBuf);

impl TempFile {
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
        let mut lock = self.0.clone().into_os_string();
        lock.push(".lock");
        let _ = fs::remove_file(lock);
    }
}

fn populate_fork(base: &Path, git_dir: &Path, alternates: &[u8]) -> Result<(), ExecError> {
    fs::create_dir_all(git_dir.join("objects").join("info"))?;
    fs::create_dir(git_dir.join("objects").join("pack"))?;
    fs::write(git_dir.join("objects").join("info").join("alternates"), alternates)?;
    fs::copy(base.join("HEAD"), git_dir.join("HEAD"))?;
    fs::copy(base.join("config"), git_dir.join("config"))?;
    let exclude = base.join("info").join("exclude");
    if exclude.is_file() {
        fs::create_dir(git_dir.join("info"))?;
        fs::copy(&exclude, git_dir.join("info").join("exclude"))?;
    }
    let packed = base.join("packed-refs");
    if packed.is_file() {
        fs::copy(&packed, git_dir.join("packed-refs"))?;
    }
    copy_tree(&base.join("refs"), &git_dir.join("refs"))
}

/// Copy a directory of regular files (a `refs/` tree).
fn copy_tree(from: &Path, to: &Path) -> Result<(), ExecError> {
    fs::create_dir(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let target = to.join(entry.file_name());
        if kind.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else if kind.is_file() {
            fs::copy(entry.path(), target)?;
        } else {
            return Err(ExecError::Io(format!("unexpected non-regular entry {}", entry.path().display())));
        }
    }
    Ok(())
}

/// `refs/mage/<name>` for a validated baseline name (`[A-Za-z0-9_-]`, not starting with `-`).
fn mage_ref(name: &str) -> Result<String, ExecError> {
    let valid = !name.is_empty()
        && name.len() <= NAME_MAX
        && !name.starts_with('-')
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    if !valid {
        return Err(ExecError::Invalid(format!("baseline name {name:?} must match [A-Za-z0-9_-]{{1,64}}")));
    }
    Ok(format!("refs/mage/{name}"))
}

fn rel_path(raw: &[u8]) -> Result<RelPath, ExecError> {
    let path = std::str::from_utf8(raw)
        .map_err(|_| ExecError::Invalid(format!("path {:?} is not UTF-8", String::from_utf8_lossy(raw))))?;
    RelPath::new(path)
}

/// NUL-separated paths (`--name-only -z`), sorted and deduplicated.
fn nul_paths(out: &[u8]) -> Result<Vec<RelPath>, ExecError> {
    let mut paths = out.split(|&b| b == 0).filter(|p| !p.is_empty()).map(rel_path).collect::<Result<Vec<_>, _>>()?;
    paths.sort();
    paths.dedup();
    Ok(paths)
}

/// Binary paths from `git diff --numstat -z --no-renames` (`-\t-\t<path>` records).
fn binary_paths(numstat: &[u8]) -> Result<Vec<RelPath>, ExecError> {
    let mut paths = Vec::new();
    for record in numstat.split(|&b| b == 0).filter(|r| !r.is_empty()) {
        let mut fields = record.splitn(3, |&b| b == b'\t');
        match (fields.next(), fields.next(), fields.next()) {
            (Some([b'-']), Some([b'-']), Some(path)) => paths.push(rel_path(path)?),
            (Some(_), Some(_), Some(path)) if !path.is_empty() => {}
            _ => return Err(unexpected_output("diff --numstat", record)),
        }
    }
    paths.sort();
    paths.dedup();
    Ok(paths)
}

/// `files` from `git diff --raw -z --no-abbrev --no-renames`
/// (`:<old mode> <new mode> <old oid> <new oid> <status>\0<path>\0`), sorted by path.
fn changed_files(raw: &[u8], binary: &[RelPath]) -> Result<Vec<PatchFile>, ExecError> {
    let mut tokens = raw.split(|&b| b == 0);
    let mut files = Vec::new();
    while let Some(header) = tokens.next() {
        if header.is_empty() {
            continue;
        }
        let fields = header
            .strip_prefix(b":")
            .map(|h| h.split(|&b| b == b' ').collect::<Vec<_>>())
            .filter(|f| f.len() == 5)
            .ok_or_else(|| unexpected_output("diff --raw", header))?;
        let (old_mode, new_mode, old_oid, new_oid, status) = (fields[0], fields[1], fields[2], fields[3], fields[4]);
        let path = tokens.next().filter(|p| !p.is_empty()).ok_or_else(|| unexpected_output("diff --raw", header))?;
        let path = rel_path(path)?;
        let change = match status.first() {
            Some(b'A') => FileChange::Added,
            Some(b'D') => FileChange::Deleted,
            Some(b'M') if old_oid == new_oid && old_mode != new_mode => FileChange::ModeChanged,
            Some(b'M' | b'T') => FileChange::Modified,
            _ => return Err(unexpected_output("diff --raw", header)),
        };
        let binary = binary.binary_search(&path).is_ok();
        files.push(PatchFile { path, change, binary });
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(files)
}

/// Paths from `git apply --numstat -z`: `<added>\t<deleted>\t<path>\0`, or for a rename/copy
/// `<added>\t<deleted>\t\0<old>\0<new>\0`. Sorted, deduplicated; an invalid path is an error
/// message for `Malformed`.
fn apply_numstat_paths(out: &[u8]) -> Result<Vec<RelPath>, String> {
    let mut tokens = out.split(|&b| b == 0);
    let mut raw = Vec::new();
    while let Some(record) = tokens.next() {
        if record.is_empty() {
            continue;
        }
        let mut fields = record.splitn(3, |&b| b == b'\t');
        let (Some(_), Some(_), Some(path)) = (fields.next(), fields.next(), fields.next()) else {
            return Err(format!("unexpected git apply --numstat record {:?}", String::from_utf8_lossy(record)));
        };
        if path.is_empty() {
            for _ in 0..2 {
                let path = tokens.next().filter(|p| !p.is_empty());
                raw.push(path.ok_or_else(|| "truncated git apply --numstat rename record".to_owned())?);
            }
        } else {
            raw.push(path);
        }
    }
    let mut paths = Vec::with_capacity(raw.len());
    for path in raw {
        paths.push(rel_path(path).map_err(|err| format!("patch touches an invalid path: {err}"))?);
    }
    paths.sort();
    paths.dedup();
    Ok(paths)
}

/// Whether a failed `git apply --numstat` complained about the patch itself (rather than, say,
/// reading the patch file).
fn is_parse_failure(stderr: &str) -> bool {
    stderr.lines().any(|line| {
        (line.starts_with("error: ") && !line.starts_with("error: can't open patch"))
            || PARSE_FAILURES.iter().any(|marker| line.contains(marker))
    })
}

/// Map a failed `git apply` (after the patch parsed): a patch outcome when git reported one
/// (exit 1 or `error:` lines), an `ExecError` otherwise.
fn apply_failure(out: &Output) -> Result<ApplyOutcome, ExecError> {
    let stderr = String::from_utf8_lossy(&out.stderr);
    if out.status.code() != Some(1) && !stderr.lines().any(|line| line.starts_with("error: ")) {
        return Err(out.failure());
    }
    Ok(rejected(RejectReason::DoesNotApply, tail(&stderr, DETAIL_MAX_BYTES), failure_paths(&stderr)))
}

/// Paths named by `error: patch failed: <path>:<line>` and `error: <path>: <reason>` lines.
fn failure_paths(stderr: &str) -> Vec<RelPath> {
    let mut paths: Vec<RelPath> =
        stderr.lines().filter_map(failure_path).filter_map(|path| RelPath::new(path).ok()).collect();
    paths.sort();
    paths.dedup();
    paths
}

fn failure_path(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("error: ")?;
    if let Some(rest) = rest.strip_prefix("patch failed: ") {
        let (path, line_no) = rest.rsplit_once(':')?;
        return (!line_no.is_empty() && line_no.bytes().all(|b| b.is_ascii_digit())).then_some(path);
    }
    FAILURE_SUFFIXES.iter().find_map(|suffix| rest.strip_suffix(suffix))
}

fn applied(files: Vec<RelPath>, patch: &[u8]) -> ApplyOutcome {
    ApplyOutcome::Applied { files, patch_sha256: Sha256::digest(patch).into() }
}

fn rejected(reason: RejectReason, detail: String, paths: Vec<RelPath>) -> ApplyOutcome {
    ApplyOutcome::Rejected { reason, detail, paths }
}

fn unexpected_output(what: &str, record: &[u8]) -> ExecError {
    ExecError::Io(format!("unexpected git {what} output {:?}", String::from_utf8_lossy(record)))
}

/// The last `max` bytes of `text` (trailing whitespace dropped), cut on a char boundary.
fn tail(text: &str, max: usize) -> String {
    let text = text.trim_end();
    let mut start = text.len().saturating_sub(max);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    text[start..].to_owned()
}

/// Read up to `max` bytes; `true` when the stream had more. The reader is dropped on return.
async fn read_capped<R: AsyncRead + Unpin>(mut reader: R, max: usize) -> io::Result<(Vec<u8>, bool)> {
    let mut buf = Vec::new();
    let limit = u64::try_from(max).unwrap_or(u64::MAX).saturating_add(1);
    (&mut reader).take(limit).read_to_end(&mut buf).await?;
    let overflow = buf.len() > max;
    if overflow {
        buf.clear();
    }
    Ok((buf, overflow))
}

/// Drain `reader`, keeping the last `keep` bytes.
async fn read_tail<R: AsyncRead + Unpin>(mut reader: R, keep: usize) -> io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    let mut chunk = vec![0u8; 8192];
    loop {
        let n = reader.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() > keep.saturating_mul(2) {
            buf.drain(..buf.len() - keep);
        }
    }
    if buf.len() > keep {
        buf.drain(..buf.len() - keep);
    }
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::os::unix::fs::PermissionsExt;

    use mage_core::domain::fix::Patch;
    use tempfile::TempDir;

    use super::*;

    fn git_available() -> bool {
        std::process::Command::new("git").arg("--version").output().is_ok_and(|out| out.status.success())
    }

    macro_rules! require_git {
        () => {
            if !git_available() {
                eprintln!("skipping: git is not on PATH");
                return;
            }
        };
    }

    fn rel(path: &str) -> RelPath {
        RelPath::new(path).unwrap()
    }

    fn glob(pattern: &str) -> PathGlob {
        PathGlob::new(pattern).unwrap()
    }

    fn put(root: &Path, path: &str, bytes: &[u8]) {
        let path = root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }

    fn set_mode(root: &Path, path: &str, mode: u32) {
        fs::set_permissions(root.join(path), fs::Permissions::from_mode(mode)).unwrap();
    }

    fn copy_dir(from: &Path, to: &Path) {
        fs::create_dir_all(to).unwrap();
        for entry in fs::read_dir(from).unwrap() {
            let entry = entry.unwrap();
            let target = to.join(entry.file_name());
            if entry.file_type().unwrap().is_dir() {
                copy_dir(&entry.path(), &target);
            } else {
                fs::copy(entry.path(), target).unwrap();
            }
        }
    }

    /// Every file under `root` with its bytes and executable bit.
    fn snapshot(root: &Path) -> BTreeMap<String, (Vec<u8>, bool)> {
        fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, (Vec<u8>, bool)>) {
            for entry in fs::read_dir(dir).unwrap() {
                let entry = entry.unwrap();
                if entry.file_type().unwrap().is_dir() {
                    walk(root, &entry.path(), out);
                } else {
                    let rel = entry.path().strip_prefix(root).unwrap().to_string_lossy().into_owned();
                    let exec = entry.metadata().unwrap().permissions().mode() & 0o111 != 0;
                    out.insert(rel, (fs::read(entry.path()).unwrap(), exec));
                }
            }
        }
        let mut out = BTreeMap::new();
        walk(root, root, &mut out);
        out
    }

    fn numbered(n: usize) -> String {
        (1..=n).map(|i| format!("line {i}\n")).collect()
    }

    fn file(path: &str, change: FileChange, binary: bool) -> PatchFile {
        PatchFile { path: rel(path), change, binary }
    }

    fn rejection(outcome: &ApplyOutcome) -> (RejectReason, Vec<RelPath>) {
        match outcome {
            ApplyOutcome::Rejected { reason, paths, .. } => (*reason, paths.clone()),
            other => panic!("expected a rejection, got {other:?}"),
        }
    }

    struct Fixture {
        dir: TempDir,
        base: GitBaseline,
    }

    impl Fixture {
        /// A base tree with `files`, committed as `source` (`node_modules/` excluded).
        async fn new(files: &[(&str, &[u8])]) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("base");
            for (path, bytes) in files {
                put(&root.join("tree"), path, bytes);
            }
            let base = GitBaseline::new(root.join("baseline.git"), root.join("tree"), root.join("run"));
            base.init_source(&["node_modules/".to_owned()]).await.unwrap();
            Self { dir, base }
        }

        /// `new` plus `prepared` aliased to `source` (a SourceOnly base).
        async fn prepared(files: &[(&str, &[u8])]) -> Self {
            let fixture = Self::new(files).await;
            fixture.base.alias_ref("source", "prepared").await.unwrap();
            fixture
        }

        fn tree(&self) -> &Path {
            &self.base.work_tree
        }

        /// A fork whose tree is a copy of the base tree as it is now.
        fn fork(&self, name: &str) -> GitBaseline {
            let root = self.dir.path().join(name);
            copy_dir(self.tree(), &root.join("tree"));
            GitBaseline::fork_from(self.base.git_dir(), root.join("baseline.git"), root.join("tree"), root.join("run"))
                .unwrap()
        }
    }

    #[tokio::test]
    async fn unchanged_tree_has_an_empty_diff_and_is_clean() {
        require_git!();
        let fx = Fixture::prepared(&[("a.txt", b"a\n"), ("src/b.rs", b"fn b() {}\n")]).await;
        let out = fx.base.diff(Baseline::Prepared, &[glob(".mage/**")]).await.unwrap();
        assert_eq!(out, PatchOutput { patch: Vec::new(), files: Vec::new(), excluded: Vec::new(), binary: Vec::new() });
        assert!(fx.base.is_clean(Baseline::Source).await.unwrap());
        assert!(fx.base.is_clean(Baseline::Prepared).await.unwrap());
        assert_eq!(fx.base.changed_paths(Baseline::Prepared).await.unwrap(), Vec::<RelPath>::new());
        // No temp index or patch file is left behind.
        let leftovers: Vec<_> = fs::read_dir(fx.dir.path().join("base/run"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(leftovers, vec![HOME_DIR.to_owned()]);
    }

    #[tokio::test]
    async fn diff_reports_each_change_kind_in_a_parseable_deterministic_patch() {
        require_git!();
        let fx = Fixture::prepared(&[
            ("keep.txt", b"keep\n"),
            ("mod.txt", b"one\ntwo\nthree\n"),
            ("del.txt", b"gone\n"),
            ("run.sh", b"echo hi\n"),
        ])
        .await;
        let tree = fx.tree();
        put(tree, "mod.txt", b"one\nTWO\nthree\n");
        fs::remove_file(tree.join("del.txt")).unwrap();
        put(tree, "dir/new.txt", b"fresh\n");
        set_mode(tree, "run.sh", 0o755);

        let out = fx.base.diff(Baseline::Prepared, &[]).await.unwrap();
        assert_eq!(
            out.files,
            vec![
                file("del.txt", FileChange::Deleted, false),
                file("dir/new.txt", FileChange::Added, false),
                file("mod.txt", FileChange::Modified, false),
                file("run.sh", FileChange::ModeChanged, false),
            ]
        );
        assert!(out.excluded.is_empty() && out.binary.is_empty());
        let parsed = Patch::parse(&out.patch).unwrap();
        let touched: Vec<_> = parsed.files().iter().map(|f| f.path().as_str().to_owned()).collect();
        assert_eq!(touched, ["del.txt", "dir/new.txt", "mod.txt", "run.sh"]);
        let text = String::from_utf8(out.patch.clone()).unwrap();
        assert!(text.contains("diff --git a/mod.txt b/mod.txt\n"), "{text}");
        assert!(text.contains("-two\n+TWO\n"), "{text}");
        assert!(text.contains("old mode 100644\nnew mode 100755\n"), "{text}");

        assert_eq!(fx.base.diff(Baseline::Prepared, &[]).await.unwrap(), out);
        assert_eq!(
            fx.base.changed_paths(Baseline::Prepared).await.unwrap(),
            vec![rel("del.txt"), rel("dir/new.txt"), rel("mod.txt"), rel("run.sh")]
        );
        assert!(!fx.base.is_clean(Baseline::Prepared).await.unwrap());
    }

    #[tokio::test]
    async fn untracked_files_count_but_gitignored_and_info_excluded_ones_do_not() {
        require_git!();
        let fx = Fixture::prepared(&[(".gitignore", b"*.log\n"), ("app.js", b"app\n")]).await;
        let tree = fx.tree();
        put(tree, "new.txt", b"untracked\n");
        put(tree, "debug.log", b"ignored by .gitignore\n");
        put(tree, "node_modules/pkg/index.js", b"ignored by info/exclude\n");

        let out = fx.base.diff(Baseline::Prepared, &[]).await.unwrap();
        assert_eq!(out.files, vec![file("new.txt", FileChange::Added, false)]);
        assert_eq!(fx.base.changed_paths(Baseline::Source).await.unwrap(), vec![rel("new.txt")]);

        fs::remove_file(tree.join("new.txt")).unwrap();
        assert!(fx.base.is_clean(Baseline::Prepared).await.unwrap());
    }

    #[tokio::test]
    async fn excludes_drop_paths_and_list_only_the_ones_that_changed() {
        require_git!();
        let fx = Fixture::prepared(&[
            (".mage/env.toml", b"version = 1\n"),
            ("package-lock.json", b"{}\n"),
            ("src/a.js", b"a\n"),
        ])
        .await;
        let tree = fx.tree();
        put(tree, ".mage/env.toml", b"version = 2\n");
        put(tree, ".mage/notes.md", b"new\n");
        put(tree, "src/a.js", b"A\n");
        let exclude = [glob(".mage/**"), glob("package-lock.json")];

        let out = fx.base.diff(Baseline::Prepared, &exclude).await.unwrap();
        assert_eq!(out.files, vec![file("src/a.js", FileChange::Modified, false)]);
        assert_eq!(out.excluded, vec![rel(".mage/env.toml"), rel(".mage/notes.md")]);
        let text = String::from_utf8(out.patch).unwrap();
        assert!(!text.contains(".mage"), "{text}");

        let all = fx.base.diff(Baseline::Prepared, &[]).await.unwrap();
        assert!(all.excluded.is_empty());
        let paths: Vec<_> = all.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, [".mage/env.toml", ".mage/notes.md", "src/a.js"]);
    }

    #[tokio::test]
    async fn binary_files_are_listed_but_never_encoded() {
        require_git!();
        let fx = Fixture::prepared(&[("text.txt", b"text\n"), ("img.png", b"\x89PNG\0\x01\x02")]).await;
        let tree = fx.tree();
        put(tree, "img.png", b"\x89PNG\0\x03\x04");
        put(tree, "new.bin", b"\0\0\0\x01");
        put(tree, "text.txt", b"TEXT\n");
        // Binaries are dropped by literal pathspec: as a glob, `[x].bin` would also drop `x.bin`.
        put(tree, "[x].bin", b"\0glob chars\0");
        put(tree, "x.bin", b"plain text\n");

        let out = fx.base.diff(Baseline::Prepared, &[]).await.unwrap();
        assert_eq!(out.binary, vec![rel("[x].bin"), rel("img.png"), rel("new.bin")]);
        assert_eq!(
            out.files,
            vec![
                file("[x].bin", FileChange::Added, true),
                file("img.png", FileChange::Modified, true),
                file("new.bin", FileChange::Added, true),
                file("text.txt", FileChange::Modified, false),
                file("x.bin", FileChange::Added, false),
            ]
        );
        let text = String::from_utf8(out.patch.clone()).unwrap();
        assert!(!text.contains("img.png") && !text.contains("new.bin") && !text.contains("Binary"), "{text}");
        assert!(!text.contains("[x].bin") && text.contains("diff --git a/x.bin b/x.bin\n"), "{text}");
        let parsed = Patch::parse(&out.patch).unwrap();
        let touched: Vec<_> = parsed.files().iter().map(|f| f.path().as_str().to_owned()).collect();
        assert_eq!(touched, ["text.txt", "x.bin"]);
    }

    #[tokio::test]
    async fn oversized_patch_is_invalid() {
        require_git!();
        let fx = Fixture::prepared(&[("a.txt", b"a\n")]).await;
        let big = "x".repeat(99) + "\n";
        put(fx.tree(), "big.txt", big.repeat(PATCH_MAX_BYTES / 100 + 1).as_bytes());
        let err = fx.base.diff(Baseline::Prepared, &[]).await.unwrap_err();
        assert_eq!(err, ExecError::Invalid("patch too large".into()));
    }

    #[tokio::test]
    async fn exact_apply_on_a_fresh_fork_reproduces_the_tree() {
        require_git!();
        let fx = Fixture::prepared(&[
            ("keep.txt", b"keep\n"),
            ("mod.txt", b"one\ntwo\nthree\n"),
            ("del.txt", b"gone\n"),
            ("run.sh", b"echo hi\n"),
            ("docs/crlf.txt", b"a\r\nb\r\n"),
        ])
        .await;
        let edited = fx.fork("edited");
        let tree = &edited.work_tree;
        put(tree, "mod.txt", b"one\nTWO\nthree\n");
        fs::remove_file(tree.join("del.txt")).unwrap();
        put(tree, "dir/new.txt", b"fresh\nno newline");
        put(tree, "docs/crlf.txt", b"a\r\nB\r\n");
        set_mode(tree, "run.sh", 0o755);
        let out = edited.diff(Baseline::Prepared, &[]).await.unwrap();

        let fresh = fx.fork("fresh");
        let outcome = fresh.apply(&out.patch, ApplyMode::Exact).await.unwrap();
        let expected_sha: [u8; 32] = Sha256::digest(&out.patch).into();
        assert_eq!(
            outcome,
            ApplyOutcome::Applied {
                files: vec![rel("del.txt"), rel("dir/new.txt"), rel("docs/crlf.txt"), rel("mod.txt"), rel("run.sh")],
                patch_sha256: expected_sha,
            }
        );
        assert_eq!(snapshot(&fresh.work_tree), snapshot(&edited.work_tree));
        assert_eq!(fresh.diff(Baseline::Prepared, &[]).await.unwrap().patch, out.patch);
        // The base and its tree are untouched.
        assert!(fx.base.is_clean(Baseline::Prepared).await.unwrap());
        // A fork never reuses an existing git dir.
        let err = GitBaseline::fork_from(
            fx.base.git_dir(),
            fresh.git_dir.clone(),
            fresh.work_tree.clone(),
            fresh.scratch_dir.clone(),
        )
        .unwrap_err();
        assert!(matches!(err, ExecError::Conflict(_)), "{err:?}");
    }

    #[tokio::test]
    async fn exact_apply_rejects_a_dirty_tree_without_touching_it() {
        require_git!();
        let fx = Fixture::prepared(&[("mod.txt", b"one\ntwo\n")]).await;
        let edited = fx.fork("edited");
        put(&edited.work_tree, "mod.txt", b"one\nTWO\n");
        let patch = edited.diff(Baseline::Prepared, &[]).await.unwrap().patch;

        let dirty = fx.fork("dirty");
        put(&dirty.work_tree, "stray.txt", b"agent leftovers\n");
        let before = snapshot(&dirty.work_tree);
        let outcome = dirty.apply(&patch, ApplyMode::Exact).await.unwrap();
        assert_eq!(rejection(&outcome), (RejectReason::BaseMismatch, vec![rel("stray.txt")]));
        assert_eq!(snapshot(&dirty.work_tree), before);
    }

    #[tokio::test]
    async fn allow_offset_applies_below_inserted_lines_where_exact_rejects() {
        require_git!();
        let original = numbered(20);
        let fx = Fixture::prepared(&[("lines.txt", original.as_bytes())]).await;
        let edited = fx.fork("edited");
        let changed = original.replace("line 15\n", "line fifteen\n");
        put(&edited.work_tree, "lines.txt", changed.as_bytes());
        let patch = edited.diff(Baseline::Prepared, &[]).await.unwrap().patch;

        let shifted = fx.fork("shifted");
        let header = "inserted 1\ninserted 2\ninserted 3\n";
        put(&shifted.work_tree, "lines.txt", format!("{header}{original}").as_bytes());
        let exact = shifted.apply(&patch, ApplyMode::Exact).await.unwrap();
        assert_eq!(rejection(&exact), (RejectReason::BaseMismatch, vec![rel("lines.txt")]));

        let offset = shifted.apply(&patch, ApplyMode::AllowOffset).await.unwrap();
        assert!(matches!(offset, ApplyOutcome::Applied { ref files, .. } if files == &[rel("lines.txt")]), "{offset:?}");
        assert_eq!(fs::read_to_string(shifted.work_tree.join("lines.txt")).unwrap(), format!("{header}{changed}"));
    }

    #[tokio::test]
    async fn conflicting_or_missing_targets_do_not_apply_and_name_the_path() {
        require_git!();
        let original = numbered(20);
        let fx = Fixture::prepared(&[("lines.txt", original.as_bytes()), ("other.txt", b"other\n")]).await;
        let edited = fx.fork("edited");
        put(&edited.work_tree, "lines.txt", original.replace("line 15\n", "line fifteen\n").as_bytes());
        put(&edited.work_tree, "other.txt", b"OTHER\n");
        let patch = edited.diff(Baseline::Prepared, &[]).await.unwrap().patch;

        let conflicting = fx.fork("conflicting");
        let theirs = original.replace("line 15\n", "line 15 (theirs)\n");
        put(&conflicting.work_tree, "lines.txt", theirs.as_bytes());
        let outcome = conflicting.apply(&patch, ApplyMode::AllowOffset).await.unwrap();
        assert_eq!(rejection(&outcome), (RejectReason::DoesNotApply, vec![rel("lines.txt")]));
        let ApplyOutcome::Rejected { detail, .. } = &outcome else { unreachable!() };
        assert!(detail.contains("lines.txt: patch does not apply"), "{detail}");
        // Atomic: the hunk that would have applied did not.
        assert_eq!(fs::read(conflicting.work_tree.join("other.txt")).unwrap(), b"other\n");
        assert_eq!(fs::read_to_string(conflicting.work_tree.join("lines.txt")).unwrap(), theirs);

        let missing = fx.fork("missing");
        fs::remove_file(missing.work_tree.join("lines.txt")).unwrap();
        let outcome = missing.apply(&patch, ApplyMode::AllowOffset).await.unwrap();
        assert_eq!(rejection(&outcome), (RejectReason::DoesNotApply, vec![rel("lines.txt")]));
    }

    #[tokio::test]
    async fn empty_garbage_truncated_and_escaping_patches_are_malformed() {
        require_git!();
        let fx = Fixture::prepared(&[("a.txt", b"one\ntwo\n")]).await;
        let escaping = "diff --git a/../x b/../x\nnew file mode 100644\n--- /dev/null\n+++ b/../x\n@@ -0,0 +1 @@\n+x\n";
        let truncated = "diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -1,2 +1,2 @@\n one\n-two\n";
        for patch in [&b""[..], b"garbage\n", truncated.as_bytes(), escaping.as_bytes()] {
            for mode in [ApplyMode::Exact, ApplyMode::AllowOffset] {
                let outcome = fx.base.apply(patch, mode).await.unwrap();
                assert_eq!(rejection(&outcome).0, RejectReason::Malformed, "{mode:?} {outcome:?}");
            }
            let outcome = fx.base.check(patch, Baseline::Source).await.unwrap();
            assert_eq!(rejection(&outcome).0, RejectReason::Malformed, "{outcome:?}");
        }
        assert!(!fx.dir.path().join("base").join("x").exists());
        assert_eq!(fs::read(fx.tree().join("a.txt")).unwrap(), b"one\ntwo\n");
    }

    #[tokio::test]
    async fn check_targets_the_requested_baseline_and_leaves_the_tree_alone() {
        require_git!();
        let source_gen = "generated v1\nshared 2\nshared 3\n";
        let fx = Fixture::new(&[("gen.txt", source_gen.as_bytes()), ("keep.txt", b"keep\n")]).await;
        // The build rewrites gen.txt; `prepared` absorbs it.
        let prepared_gen = "generated v2\nshared 2\nshared 3\n";
        put(fx.tree(), "gen.txt", prepared_gen.as_bytes());
        fx.base.commit_baseline("prepared").await.unwrap();

        let on_prepared = fx.fork("on-prepared");
        put(&on_prepared.work_tree, "gen.txt", prepared_gen.replace("shared 3", "fixed 3").as_bytes());
        let patch_prepared = on_prepared.diff(Baseline::Prepared, &[]).await.unwrap().patch;

        let on_source = fx.fork("on-source");
        put(&on_source.work_tree, "gen.txt", source_gen.replace("shared 3", "fixed 3").as_bytes());
        let patch_source = on_source.diff(Baseline::Source, &[]).await.unwrap().patch;

        let before = snapshot(fx.tree());
        let check = |patch: Vec<u8>, baseline| {
            let base = fx.base.clone();
            async move { base.check(&patch, baseline).await.unwrap() }
        };
        let ok = check(patch_prepared.clone(), Baseline::Prepared).await;
        assert!(matches!(ok, ApplyOutcome::Applied { ref files, .. } if files == &[rel("gen.txt")]), "{ok:?}");
        let ApplyOutcome::Applied { patch_sha256, .. } = ok else { unreachable!() };
        assert_eq!(patch_sha256, <[u8; 32]>::from(Sha256::digest(&patch_prepared)));
        let rejected = check(patch_prepared, Baseline::Source).await;
        assert_eq!(rejection(&rejected), (RejectReason::DoesNotApply, vec![rel("gen.txt")]));

        let ok = check(patch_source.clone(), Baseline::Source).await;
        assert!(matches!(ok, ApplyOutcome::Applied { .. }), "{ok:?}");
        let rejected = check(patch_source, Baseline::Prepared).await;
        assert_eq!(rejection(&rejected), (RejectReason::DoesNotApply, vec![rel("gen.txt")]));

        assert_eq!(snapshot(fx.tree()), before);
        assert!(fx.base.is_clean(Baseline::Prepared).await.unwrap());
    }

    #[tokio::test]
    async fn baselines_commit_alias_and_validate_names() {
        require_git!();
        let dir = tempfile::tempdir().unwrap();
        put(&dir.path().join("tree"), "a.txt", b"a\n");
        let base = GitBaseline::new(dir.path().join("baseline.git"), dir.path().join("tree"), dir.path().join("run"));
        let bad_exclude = base.init_source(&["dist/\nsecret".to_owned()]).await.unwrap_err();
        assert!(matches!(bad_exclude, ExecError::Invalid(_)), "{bad_exclude:?}");

        let source = base.init_source(&[]).await.unwrap();
        assert_eq!(base.commit_of("refs/mage/source").await.unwrap(), source);
        assert!(matches!(base.resolve(Baseline::Prepared).await, Err(ExecError::NotFound(_))));
        assert!(matches!(base.diff(Baseline::Prepared, &[]).await, Err(ExecError::NotFound(_))));

        base.alias_ref("source", "prepared").await.unwrap();
        assert_eq!(base.resolve(Baseline::Prepared).await.unwrap(), source);

        put(&base.work_tree, "a.txt", b"rebuilt\n");
        let prepared = base.commit_baseline("prepared").await.unwrap();
        assert_ne!(prepared, source);
        assert_eq!(base.resolve(Baseline::Prepared).await.unwrap(), prepared);
        assert_eq!(base.resolve(Baseline::Source).await.unwrap(), source);
        assert!(base.is_clean(Baseline::Prepared).await.unwrap());
        assert_eq!(base.changed_paths(Baseline::Source).await.unwrap(), vec![rel("a.txt")]);

        // Fixed identity and dates: the same tree yields the same source commit.
        let twin_dir = tempfile::tempdir().unwrap();
        put(&twin_dir.path().join("tree"), "a.txt", b"a\n");
        let twin =
            GitBaseline::new(twin_dir.path().join("baseline.git"), twin_dir.path().join("tree"), twin_dir.path().join("run"));
        assert_eq!(twin.init_source(&[]).await.unwrap(), source);

        assert!(matches!(base.alias_ref("missing", "other").await, Err(ExecError::NotFound(_))));
        for name in ["", "../evil", "-x", "a/b", "a b"] {
            assert!(matches!(base.commit_baseline(name).await, Err(ExecError::Invalid(_))), "{name:?}");
            assert!(matches!(base.alias_ref("source", name).await, Err(ExecError::Invalid(_))), "{name:?}");
        }
        // Forks see the same refs and objects.
        let fork_tree = dir.path().join("fork-tree");
        copy_dir(&base.work_tree, &fork_tree);
        let fork =
            GitBaseline::fork_from(base.git_dir(), dir.path().join("fork.git"), fork_tree, dir.path().join("fork-run"))
                .unwrap();
        assert_eq!(fork.resolve(Baseline::Source).await.unwrap(), source);
        assert_eq!(fork.resolve(Baseline::Prepared).await.unwrap(), prepared);
        assert!(fork.is_clean(Baseline::Prepared).await.unwrap());
        assert!(!dir.path().join("fork.git").join("index").exists());
    }

    #[test]
    fn failure_lines_name_paths() {
        let stderr = "error: patch failed: src/a b.rs:12\nerror: src/a b.rs: patch does not apply\n\
                      error: gone.txt: No such file or directory\nerror: new.txt: already exists in working directory\n\
                      error: x: does not exist in index\nwarning: something else\nerror: ../up: patch does not apply\n";
        assert_eq!(failure_paths(stderr), vec![rel("gone.txt"), rel("new.txt"), rel("src/a b.rs"), rel("x")]);
    }

    #[test]
    fn tail_keeps_the_end_on_a_char_boundary() {
        assert_eq!(tail("abc\n", 10), "abc");
        assert_eq!(tail("ééé", 3), "é");
    }
}
