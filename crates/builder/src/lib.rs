//! Source → OCI image for aho cloud. The cloud rebuilds: local builds are never trusted, the
//! manifest names the source and this crate produces the digest.
//!
//! A [`BuildJob`] names a source and a recipe. The builder fetches the source into a scratch
//! directory, checks it is exactly what the manifest names, builds, pushes
//! `<registry>/<project>:<release>`, and answers with the pushed manifest digest:
//!
//! | source | fetch and check |
//! |---|---|
//! | `git` | `git init`, `git fetch --depth 1 <repo> <commit>`, `git checkout FETCH_HEAD`; `HEAD` must equal `commit`. `https://` repositories only. |
//! | `archive` | `GET archive_url` (with the service token); blake3 must equal `hash`; extracted by [`archive::extract_tar_gz`]. |
//! | `oci` | digest format only; nothing is fetched or run (`build.kind = prebuilt`). |
//!
//! | build | tool | digest from |
//! |---|---|---|
//! | `dockerfile` | `buildctl build --frontend dockerfile.v0 … --output type=image,name=…,push=true` | `containerimage.digest` in `--metadata-file` |
//! | `command` | a generated Dockerfile (`FROM base`, `COPY . /workspace`, `WORKDIR /workspace`, `RUN [exec form]`, kept outside the source tree), then as `dockerfile` | same |
//! | `buildpack` | `pack build <image> --builder <b> --path <src> --publish` | the exporter's `*** Digest:` line ([`digest::from_pack_output`]) |
//!
//! Every failure comes back as [`BuildResult::Failed`] with the reason and the tail of the tool
//! output; [`server::router`] serves it all at [`cloud_api::internal::routes::BUILD`].

#![forbid(unsafe_code)]

pub mod archive;
pub mod digest;
mod exec;
mod paths;
pub mod server;

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use cloud_api::internal::{BuildJob, BuildResult};
use cloud_contract::manifest::{Build, Source};
use tokio::io::AsyncWriteExt;
use tokio::sync::Semaphore;

pub use crate::archive::ArchiveError;
pub use crate::digest::DigestError;
use crate::exec::{Deadline, Log, Output};
use crate::paths::PathError;
pub use crate::server::router;

/// Environment variable holding the internal service token; removed from every tool's environment.
pub const SERVICE_TOKEN_ENV: &str = "AHO_CLOUD_SERVICE_TOKEN";

/// Where the tools are, where images go, and the limits every build runs under.
#[derive(Clone, Debug)]
pub struct Config {
    pub git: PathBuf,
    pub buildctl: PathBuf,
    pub pack: PathBuf,
    /// Passed as `buildctl --addr`; `None` leaves buildctl's default (`BUILDKIT_HOST` or the local socket).
    pub buildkit_addr: Option<String>,
    /// Repository prefix images are pushed under, e.g. `registry.internal:5000/aho-cloud`.
    pub registry: String,
    /// Holds one scratch directory per running build; each is removed when its build ends.
    pub work_dir: PathBuf,
    /// Builds running at once; later jobs wait for a slot.
    pub concurrency: usize,
    /// Wall-clock limit for one build (fetch, build, push), counted from when it gets a slot.
    pub timeout: Duration,
    /// Bytes of log returned per build: the tail, after a one-line note of what was dropped.
    pub log_limit: usize,
    /// Required as `Authorization: Bearer` on incoming requests, and sent when fetching `archive_url`.
    pub service_token: String,
}

impl Config {
    /// Tools found on `PATH`, two concurrent builds, 30 minutes each, 256 KiB of log.
    pub fn new(registry: impl Into<String>, work_dir: impl Into<PathBuf>, service_token: impl Into<String>) -> Self {
        Self {
            git: "git".into(),
            buildctl: "buildctl".into(),
            pack: "pack".into(),
            buildkit_addr: None,
            registry: registry.into(),
            work_dir: work_dir.into(),
            concurrency: 2,
            timeout: Duration::from_secs(30 * 60),
            log_limit: 256 * 1024,
            service_token: service_token.into(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("the service token is empty")]
    EmptyServiceToken,
    #[error("registry `{0}` is not a repository prefix like `host:port/path` (lowercase, no tag)")]
    Registry(String),
    #[error("concurrency must be between 1 and {}", Semaphore::MAX_PERMITS)]
    Concurrency,
    #[error("the build timeout must be positive")]
    Timeout,
    #[error("work dir `{path}`: {source}")]
    WorkDir { path: PathBuf, source: io::Error },
    #[error("http client: {0}")]
    Http(#[source] reqwest::Error),
}

/// Why a build failed; its `Display` is [`BuildResult::Failed::reason`].
#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error("invalid job: {0}")]
    InvalidJob(String),
    #[error("`build.kind = prebuilt` goes with `source.kind = oci`, and only with it")]
    SourceBuildMismatch,
    #[error("fetching the archive: {0}")]
    Download(#[source] reqwest::Error),
    #[error("fetching the archive: the server answered {0}")]
    DownloadStatus(u16),
    #[error("archive hash mismatch: the manifest names {expected}, the fetched bytes are {actual}")]
    HashMismatch { expected: String, actual: String },
    #[error("extracting the archive: {0}")]
    Archive(#[from] ArchiveError),
    #[error("fetched HEAD `{actual}` is not the requested commit {expected}")]
    CommitMismatch { expected: String, actual: String },
    #[error("`{field}` {problem}")]
    SourcePath { field: &'static str, problem: String },
    #[error("could not start `{program}`: {source}")]
    Spawn { program: String, source: io::Error },
    #[error("`{program}` failed ({status})")]
    StepFailed { program: String, status: String },
    #[error("build timed out after {}s", .0.as_secs())]
    TimedOut(Duration),
    #[error("reading buildctl metadata: {0}")]
    Metadata(#[source] io::Error),
    #[error("{0}")]
    Digest(#[from] DigestError),
    #[error("build scratch: {0}")]
    Io(#[from] io::Error),
}

/// Turns [`BuildJob`]s into pushed image digests, at most `concurrency` at a time.
pub struct Builder {
    config: Config,
    http: reqwest::Client,
    slots: Semaphore,
    token: blake3::Hash,
}

/// What to run once the source is in place; validated before anything is fetched.
enum Recipe<'a> {
    Dockerfile { path: PathBuf, context: PathBuf },
    Buildpack { builder: &'a str },
    Command { dockerfile: String },
}

/// A build's scratch directory: the source tree in `src`, tool files beside it.
struct Scratch {
    dir: tempfile::TempDir,
    src: PathBuf,
}

impl Builder {
    /// Checks `config`, creates `work_dir`, and makes it absolute (tools run in other directories).
    pub fn new(mut config: Config) -> Result<Self, ConfigError> {
        if config.service_token.is_empty() {
            return Err(ConfigError::EmptyServiceToken);
        }
        if !is_registry(&config.registry) {
            return Err(ConfigError::Registry(config.registry));
        }
        if config.concurrency == 0 || config.concurrency > Semaphore::MAX_PERMITS {
            return Err(ConfigError::Concurrency);
        }
        if config.timeout.is_zero() {
            return Err(ConfigError::Timeout);
        }
        let work_dir_error = |source| ConfigError::WorkDir { path: config.work_dir.clone(), source };
        std::fs::create_dir_all(&config.work_dir).map_err(work_dir_error)?;
        let work_dir = std::fs::canonicalize(&config.work_dir).map_err(work_dir_error)?;
        config.work_dir = work_dir;
        let http = reqwest::Client::builder().build().map_err(ConfigError::Http)?;
        Ok(Self { token: blake3::hash(config.service_token.as_bytes()), slots: Semaphore::new(config.concurrency), http, config })
    }

    /// Constant-time check of a presented service token (compares blake3 hashes, so lengths leak nothing either).
    pub(crate) fn accepts_token(&self, presented: &str) -> bool {
        blake3::hash(presented.as_bytes()) == self.token
    }

    /// Runs `job` to completion: waits for a slot, then fetches, checks, builds and pushes within
    /// the configured timeout. Never panics on bad input; every failure is [`BuildResult::Failed`].
    pub async fn build(&self, job: &BuildJob) -> BuildResult {
        let Ok(_slot) = self.slots.acquire().await else {
            return BuildResult::Failed { reason: "the builder is shutting down".into(), log: String::new() };
        };
        let log = Log::new(self.config.log_limit);
        let result = self.run(job, Deadline::after(self.config.timeout), &log).await;
        match result {
            Ok(image_digest) => {
                tracing::info!(project = %job.project, release = %job.release, %image_digest, "built");
                log.note(format!("image {image_digest}"));
                BuildResult::Built { image_digest, log: log.finish() }
            }
            Err(e) => {
                let reason = e.to_string();
                tracing::warn!(project = %job.project, release = %job.release, %reason, "build failed");
                log.note(format!("failed: {reason}"));
                BuildResult::Failed { reason, log: log.finish() }
            }
        }
    }

    async fn run(&self, job: &BuildJob, deadline: Deadline, log: &Log) -> Result<String, BuildError> {
        let image = self.image_name(job)?;
        let (fetch, recipe) = match (&job.source, &job.build) {
            (Source::Oci { digest }, Build::Prebuilt) => {
                if !digest::is_sha256_digest(digest) {
                    return Err(BuildError::InvalidJob(format!("`{digest}` is not a sha256 image digest")));
                }
                log.note(format!("prebuilt image {digest}: nothing to build"));
                return Ok(digest.clone());
            }
            (Source::Oci { .. }, _) | (_, Build::Prebuilt) => return Err(BuildError::SourceBuildMismatch),
            (Source::Git { repo, commit }, build) => (Fetch::git(repo, commit)?, plan(build)?),
            (Source::Archive { hash }, build) => (Fetch::archive(job.archive_url.as_deref(), hash)?, plan(build)?),
        };
        let scratch = self.scratch(job)?;
        let result = async {
            match fetch {
                Fetch::Git { repo, commit } => self.fetch_git(repo, commit, &scratch, deadline, log).await?,
                Fetch::Archive { url, hash } => self.fetch_archive(url, hash, &scratch, deadline, log).await?,
            }
            self.build_image(&recipe, &image, &scratch, deadline, log).await
        }
        .await;
        // Removing a source tree is blocking work; keep it off the async threads.
        let _ = tokio::task::spawn_blocking(move || drop(scratch)).await;
        result
    }

    /// `<registry>/<project>:<release>`, refusing names that are not a slug and a tag (they end up
    /// inside tool arguments such as buildctl's `--output type=image,name=…`).
    fn image_name(&self, job: &BuildJob) -> Result<String, BuildError> {
        if !is_slug(&job.project) {
            return Err(BuildError::InvalidJob(format!("project `{}` is not a slug", job.project)));
        }
        if !is_tag(&job.release) {
            return Err(BuildError::InvalidJob(format!("release `{}` is not a valid image tag", job.release)));
        }
        Ok(format!("{}/{}:{}", self.config.registry, job.project, job.release))
    }

    fn scratch(&self, job: &BuildJob) -> Result<Scratch, BuildError> {
        let dir = tempfile::Builder::new().prefix(&format!("{}-{}-", job.project, job.release)).tempdir_in(&self.config.work_dir)?;
        let src = dir.path().join("src");
        std::fs::create_dir(&src)?;
        Ok(Scratch { dir, src })
    }

    async fn fetch_git(&self, repo: &str, commit: &str, scratch: &Scratch, deadline: Deadline, log: &Log) -> Result<(), BuildError> {
        let git = async |args: &[&str]| {
            let args: Vec<OsString> = args.iter().map(OsString::from).collect();
            exec::run(&self.config.git, &args, &scratch.src, deadline, log).await
        };
        git(&["init"]).await?;
        git(&["fetch", "--depth", "1", repo, commit]).await?;
        git(&["checkout", "FETCH_HEAD"]).await?;
        let Output { stdout, .. } = git(&["rev-parse", "HEAD"]).await?;
        let head = stdout.trim();
        if head != commit {
            return Err(BuildError::CommitMismatch { expected: commit.to_owned(), actual: head.to_owned() });
        }
        log.note(format!("checked out {commit}"));
        Ok(())
    }

    async fn fetch_archive(&self, url: &str, hash: &str, scratch: &Scratch, deadline: Deadline, log: &Log) -> Result<(), BuildError> {
        let file = scratch.dir.path().join("source.tar.gz");
        log.note(format!("fetching {url}"));
        let (bytes, actual) = deadline.within(self.download(url, &file)).await??;
        if actual != hash {
            return Err(BuildError::HashMismatch { expected: hash.to_owned(), actual });
        }
        log.note(format!("fetched {bytes} bytes, {hash}"));
        let src = scratch.src.clone();
        let extract = tokio::task::spawn_blocking(move || archive::extract_tar_gz(io::BufReader::new(File::open(file)?), &src));
        deadline.within(extract).await?.map_err(|e| BuildError::Io(io::Error::other(e)))??;
        Ok(())
    }

    /// Streams `url` into `dest`, hashing as it goes. Returns the size and `blake3:<hex>`.
    async fn download(&self, url: &str, dest: &Path) -> Result<(u64, String), BuildError> {
        let mut response = self.http.get(url).bearer_auth(&self.config.service_token).send().await.map_err(BuildError::Download)?;
        if !response.status().is_success() {
            return Err(BuildError::DownloadStatus(response.status().as_u16()));
        }
        let mut out = tokio::fs::File::create(dest).await?;
        let mut hasher = blake3::Hasher::new();
        let mut bytes = 0u64;
        while let Some(chunk) = response.chunk().await.map_err(BuildError::Download)? {
            hasher.update(&chunk);
            out.write_all(&chunk).await?;
            bytes += chunk.len() as u64;
        }
        out.flush().await?;
        Ok((bytes, format!("blake3:{}", hasher.finalize().to_hex())))
    }

    async fn build_image(
        &self,
        recipe: &Recipe<'_>,
        image: &str,
        scratch: &Scratch,
        deadline: Deadline,
        log: &Log,
    ) -> Result<String, BuildError> {
        match recipe {
            Recipe::Dockerfile { path, context } => {
                let context = resolve(&scratch.src, context, "build.context")?;
                if !context.is_dir() {
                    return Err(BuildError::SourcePath { field: "build.context", problem: "is not a directory in the source".into() });
                }
                let dockerfile = resolve(&scratch.src, path, "build.path")?;
                if !dockerfile.is_file() {
                    return Err(BuildError::SourcePath { field: "build.path", problem: "is not a file in the source".into() });
                }
                self.buildctl(image, &context, &dockerfile, scratch, deadline, log).await
            }
            Recipe::Command { dockerfile } => {
                let dir = scratch.dir.path().join("command");
                tokio::fs::create_dir(&dir).await?;
                let file = dir.join("Dockerfile");
                tokio::fs::write(&file, dockerfile).await?;
                log.note(format!("generated Dockerfile:\n{dockerfile}"));
                self.buildctl(image, &scratch.src, &file, scratch, deadline, log).await
            }
            Recipe::Buildpack { builder } => {
                let args: Vec<OsString> = vec![
                    "build".into(),
                    image.into(),
                    "--builder".into(),
                    (*builder).into(),
                    "--path".into(),
                    scratch.src.clone().into(),
                    "--publish".into(),
                ];
                let out = exec::run(&self.config.pack, &args, scratch.dir.path(), deadline, log).await?;
                Ok(digest::from_pack_output(&format!("{}\n{}", out.stdout, out.stderr))?)
            }
        }
    }

    /// `buildctl build` of `dockerfile` (a file path) against `context`, pushing `image`.
    async fn buildctl(
        &self,
        image: &str,
        context: &Path,
        dockerfile: &Path,
        scratch: &Scratch,
        deadline: Deadline,
        log: &Log,
    ) -> Result<String, BuildError> {
        let (Some(dockerfile_dir), Some(filename)) = (dockerfile.parent(), dockerfile.file_name()) else {
            return Err(BuildError::SourcePath { field: "build.path", problem: "is not a file in the source".into() });
        };
        let metadata = scratch.dir.path().join("metadata.json");
        for p in [context, dockerfile, metadata.as_path()] {
            // buildctl splits `--local`/`--opt` values on commas.
            if p.as_os_str().to_string_lossy().contains(',') {
                return Err(BuildError::InvalidJob(format!("path `{}` contains a comma, which buildctl cannot take", p.display())));
            }
        }
        let mut args: Vec<OsString> = Vec::new();
        if let Some(addr) = &self.config.buildkit_addr {
            args.extend(["--addr".into(), addr.into()]);
        }
        args.extend([
            "build".into(),
            "--progress".into(),
            "plain".into(),
            "--frontend".into(),
            "dockerfile.v0".into(),
            "--local".into(),
            prefixed("context=", context.as_os_str()),
            "--local".into(),
            prefixed("dockerfile=", dockerfile_dir.as_os_str()),
            "--opt".into(),
            prefixed("filename=", filename),
            "--output".into(),
            format!("type=image,name={image},push=true").into(),
            "--metadata-file".into(),
            metadata.clone().into(),
        ]);
        exec::run(&self.config.buildctl, &args, scratch.dir.path(), deadline, log).await?;
        let bytes = tokio::fs::read(&metadata).await.map_err(BuildError::Metadata)?;
        Ok(digest::from_buildctl_metadata(&bytes)?)
    }
}

/// Where the source comes from, validated before anything is fetched.
enum Fetch<'a> {
    Git { repo: &'a str, commit: &'a str },
    Archive { url: &'a str, hash: &'a str },
}

impl<'a> Fetch<'a> {
    fn git(repo: &'a str, commit: &'a str) -> Result<Self, BuildError> {
        // https only: no local paths or `file://` (the builder's own disk), no `ext::` helpers,
        // nothing git could read as an option.
        if !is_https_url(repo) {
            return Err(BuildError::InvalidJob(format!("git repository `{repo}` must be an https:// URL")));
        }
        if !is_hex(commit, 40) {
            return Err(BuildError::InvalidJob(format!("commit `{commit}` is not a full 40-hex id")));
        }
        Ok(Self::Git { repo, commit })
    }

    fn archive(url: Option<&'a str>, hash: &'a str) -> Result<Self, BuildError> {
        let url = url.ok_or_else(|| BuildError::InvalidJob("archive sources need `archive_url`".into()))?;
        if !hash.strip_prefix("blake3:").is_some_and(|h| is_hex(h, 64)) {
            return Err(BuildError::InvalidJob(format!("archive hash `{hash}` is not `blake3:` plus 64 hex")));
        }
        Ok(Self::Archive { url, hash })
    }
}

fn prefixed(prefix: &str, value: &OsStr) -> OsString {
    let mut s = OsString::from(prefix);
    s.push(value);
    s
}

/// Checks the recipe's own fields, so a bad job fails before any network or disk work.
fn plan(build: &Build) -> Result<Recipe<'_>, BuildError> {
    let relative = |p: &str, field: &'static str| {
        paths::plain_relative(Path::new(p)).map_err(|e| BuildError::SourcePath { field, problem: e.to_string() })
    };
    match build {
        Build::Prebuilt => Err(BuildError::SourceBuildMismatch),
        Build::Dockerfile { path, context } => {
            let path = relative(path, "build.path")?;
            if path.as_os_str().is_empty() {
                return Err(BuildError::SourcePath { field: "build.path", problem: "is empty".into() });
            }
            Ok(Recipe::Dockerfile { path, context: relative(context, "build.context")? })
        }
        Build::Buildpack { builder } if is_image_ref(builder) => Ok(Recipe::Buildpack { builder }),
        Build::Buildpack { builder } => Err(BuildError::InvalidJob(format!("builder `{builder}` is not an image reference"))),
        Build::Command { command, base_image } => {
            if command.is_empty() {
                return Err(BuildError::InvalidJob("command builds need a command".into()));
            }
            if !is_image_ref(base_image) {
                return Err(BuildError::InvalidJob(format!("base image `{base_image}` is not an image reference")));
            }
            Ok(Recipe::Command { dockerfile: command_dockerfile(base_image, command)? })
        }
    }
}

/// The Dockerfile for a `command` build. The command goes in exec form as a JSON array, so no
/// shell and no Dockerfile syntax can be smuggled through it.
fn command_dockerfile(base_image: &str, command: &[String]) -> Result<String, BuildError> {
    let exec = serde_json::to_string(command).map_err(|e| BuildError::InvalidJob(e.to_string()))?;
    Ok(format!("FROM {base_image}\nCOPY . /workspace\nWORKDIR /workspace\nRUN {exec}\n"))
}

/// `rel` inside `src`, following the tree's own symlinks but never out of it.
fn resolve(src: &Path, rel: &Path, field: &'static str) -> Result<PathBuf, BuildError> {
    // `join("")` would add a trailing slash; the root itself is just `src`.
    let resolved = |r: PathBuf| if r.as_os_str().is_empty() { src.to_path_buf() } else { src.join(r) };
    paths::resolve_in_root(src, rel).map(resolved).map_err(|e| match e {
        PathError::Io(e) => BuildError::Io(e),
        e => BuildError::SourcePath { field, problem: e.to_string() },
    })
}

fn is_hex(s: &str, len: usize) -> bool {
    s.len() == len && s.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}

/// Project slug: 1–40 of `[a-z0-9-]`, starting with a letter.
fn is_slug(s: &str) -> bool {
    let b = s.as_bytes();
    (1..=40).contains(&b.len()) && b[0].is_ascii_lowercase() && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

/// OCI tag: 1–128 of `[A-Za-z0-9_.-]`, not starting with `.` or `-`.
fn is_tag(s: &str) -> bool {
    let b = s.as_bytes();
    (1..=128).contains(&b.len())
        && (b[0].is_ascii_alphanumeric() || b[0] == b'_')
        && b.iter().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'.' | b'-'))
}

/// An image reference as a tool argument or `FROM` line: starts alphanumeric (never an option),
/// only reference characters (never whitespace, a newline or a comma).
fn is_image_ref(s: &str) -> bool {
    let b = s.as_bytes();
    (1..=512).contains(&b.len())
        && b[0].is_ascii_alphanumeric()
        && b.iter().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-' | b'/' | b':' | b'@'))
}

/// Registry repository prefix: lowercase `host[:port][/path]`, no tag or digest.
fn is_registry(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with('/')
        && !s.ends_with('/')
        && !s.contains("//")
        && s.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'.' | b'_' | b'-' | b'/' | b':'))
        && s.split('/').skip(1).all(|seg| !seg.contains(':'))
}

fn is_https_url(s: &str) -> bool {
    s.strip_prefix("https://").is_some_and(|rest| !rest.is_empty() && !rest.starts_with('/'))
        && s.len() <= 2048
        && !s.chars().any(|c| c.is_whitespace() || c.is_control())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_dockerfile_uses_exec_form_so_nothing_escapes_the_run_line() {
        let df = command_dockerfile("alpine:3.20", &["sh".into(), "-c".into(), "echo \"hi\"\nRUN evil \\".into()]).unwrap();
        assert_eq!(
            df,
            "FROM alpine:3.20\nCOPY . /workspace\nWORKDIR /workspace\nRUN [\"sh\",\"-c\",\"echo \\\"hi\\\"\\nRUN evil \\\\\"]\n"
        );
        assert_eq!(df.lines().count(), 4);
    }

    #[test]
    fn names_that_reach_tool_arguments_are_strict() {
        assert!(is_tag("r-2026.09.28_1") && is_tag("R1"));
        for bad in ["", ".r", "-r", "r1,push=false", "r 1", "r/1", &"r".repeat(129)] {
            assert!(!is_tag(bad), "{bad}");
        }
        assert!(is_image_ref("paketobuildpacks/builder-jammy-base:0.4") && is_image_ref("alpine@sha256:abc"));
        for bad in ["", "--help", "alpine\nRUN x", "a b", "a,b"] {
            assert!(!is_image_ref(bad), "{bad}");
        }
        assert!(is_registry("registry.internal:5000/aho-cloud") && is_registry("localhost:5000"));
        for bad in ["", "/x", "x/", "a//b", "Reg/x", "r/p:tag", "r/p@sha256:x", "r,x"] {
            assert!(!is_registry(bad), "{bad}");
        }
        assert!(is_https_url("https://github.com/acme/shop.git"));
        for bad in ["http://x/r", "file:///etc", "--upload-pack=x", "https://", "https:///x", "https://x/ y", "ext::sh -c x"] {
            assert!(!is_https_url(bad), "{bad}");
        }
    }
}
