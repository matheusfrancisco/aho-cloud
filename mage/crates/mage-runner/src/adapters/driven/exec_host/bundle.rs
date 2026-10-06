//! SourceBundle download and extraction (executor.md "Source bundle download and extract").
//!
//! `fetch` streams the signed URL into the bundle cache, verifying size and sha256 before the
//! archive becomes visible under its final name. `extract` unpacks the zstd-compressed tar entry by
//! entry, refusing anything that could escape the tree or create special files. The signed URL
//! never reaches errors or logs.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fmt;
use std::fs::{self, File, FileTimes, OpenOptions, Permissions};
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use nix::libc;
use nix::sys::stat::{UtimensatFlags, utimensat};
use nix::sys::time::TimeSpec;
use sha2::{Digest, Sha256};
use tar::EntryType;
use tokio::io::AsyncWriteExt;

use crate::ports::executor::{BundleSource, ExecError};

const GIB: u64 = 1 << 30;
const MAX_ENTRIES: u64 = 500_000;
const MAX_FILE_BYTES: u64 = 2 * GIB;

const FETCH_ATTEMPTS: u32 = 3;
const FETCH_BACKOFF_BASE: Duration = Duration::from_secs(1);
const FETCH_DEADLINE: Duration = Duration::from_secs(600);
/// Bytes of a 400/403 body inspected for an expired-signature message.
const ERROR_BODY_MAX: usize = 64 * 1024;
const DOWNLOAD_WRITE_BUFFER: usize = 1 << 20;
const EXTRACT_COPY_BUFFER: usize = 256 * 1024;
/// Longest entry name echoed into a rejection message.
const SHOW_MAX: usize = 200;

/// Extraction caps (`prepare.max_bundle_unpacked_gib`, 500 000 entries, 2 GiB per file).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BundleLimits {
    pub max_unpacked_bytes: u64,
    pub max_entries: u64,
    pub max_file_bytes: u64,
}

impl BundleLimits {
    pub(crate) fn from_gib(max_unpacked_gib: u64) -> Self {
        Self { max_unpacked_bytes: max_unpacked_gib.saturating_mul(GIB), max_entries: MAX_ENTRIES, max_file_bytes: MAX_FILE_BYTES }
    }
}

/// What `extract` created: archive entries materialized (files, directories, symlinks, hard
/// links; skipped metadata headers and the root entry excluded) and file bytes written.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ExtractStats {
    pub entries: u64,
    pub bytes: u64,
}

// ---------------------------------------------------------------------------------------------
// Download
// ---------------------------------------------------------------------------------------------

/// Path of the verified cached archive `<cache_dir>/<hex sha256>.tar.zst`.
///
/// A cache hit (regular file of the expected size) returns without network. Otherwise the body is
/// streamed with `client` (built by the caller without a proxy) into a per-call
/// `<hex>.<nonce>.part` file, hashed while writing, verified, and renamed into place. 3 attempts
/// with 1 s / 2 s backoff on connect, 5xx, and body errors, bounded by 10 minutes overall.
/// Errors: size or sha256 mismatch → `BundleRejected`; HTTP 400/403 mentioning `expired` →
/// `SourceUrlExpired`; other 4xx, exhausted retries, or the deadline → `Unavailable`. The part
/// file is removed on every failure, including cancellation.
pub(crate) async fn fetch(client: &reqwest::Client, cache_dir: &Path, src: &BundleSource) -> Result<PathBuf, ExecError> {
    let policy = RetryPolicy { attempts: FETCH_ATTEMPTS, backoff_base: FETCH_BACKOFF_BASE, deadline: FETCH_DEADLINE };
    fetch_with(client, cache_dir, src, policy).await
}

#[derive(Debug, Clone, Copy)]
struct RetryPolicy {
    attempts: u32,
    /// Sleep before retry `n` (1-based) is `backoff_base * 2^(n-1)`.
    backoff_base: Duration,
    /// Bound on the whole download, backoff included.
    deadline: Duration,
}

async fn fetch_with(client: &reqwest::Client, cache_dir: &Path, src: &BundleSource, policy: RetryPolicy) -> Result<PathBuf, ExecError> {
    let hex = hex::encode(src.sha256);
    let final_path = cache_dir.join(format!("{hex}.tar.zst"));
    match tokio::fs::symlink_metadata(&final_path).await {
        Ok(meta) if meta.file_type().is_file() && meta.len() == src.bytes => return Ok(final_path),
        Ok(meta) => {
            tracing::warn!(bundle = %hex, len = meta.len(), expected = src.bytes, "discarding damaged cached bundle");
            tokio::fs::remove_file(&final_path).await.map_err(|e| io_err(&final_path, e))?;
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(io_err(&final_path, e)),
    }
    tokio::fs::create_dir_all(cache_dir).await.map_err(|e| io_err(cache_dir, e))?;

    let mut part = PartFile { path: cache_dir.join(format!("{hex}.{}.part", uuid::Uuid::new_v4().simple())), armed: true };
    let download = async {
        let mut last = String::new();
        for attempt in 1..=policy.attempts {
            match download_once(client, src, &part.path).await {
                Ok(()) => return Ok(()),
                Err(Attempt::Fatal(err)) => return Err(err),
                Err(Attempt::Retry(msg)) => {
                    tracing::warn!(bundle = %hex, attempt, error = %msg, "bundle download attempt failed");
                    last = msg;
                    if attempt < policy.attempts {
                        let factor = 2u32.saturating_pow(attempt - 1);
                        tokio::time::sleep(policy.backoff_base.saturating_mul(factor)).await;
                    }
                }
            }
        }
        Err(ExecError::Unavailable(format!("bundle {hex}: download failed after {} attempts: {last}", policy.attempts)))
    };
    match tokio::time::timeout(policy.deadline, download).await {
        Ok(result) => result?,
        Err(_) => {
            return Err(ExecError::Unavailable(format!("bundle {hex}: download timed out after {:?}", policy.deadline)));
        }
    }
    tokio::fs::rename(&part.path, &final_path).await.map_err(|e| io_err(&part.path, e))?;
    part.armed = false;
    Ok(final_path)
}

enum Attempt {
    /// Connect error, 5xx, or interrupted body: worth another try.
    Retry(String),
    Fatal(ExecError),
}

/// One GET into `part` (truncated first). Leaves `part` fully written and verified on success.
async fn download_once(client: &reqwest::Client, src: &BundleSource, part: &Path) -> Result<(), Attempt> {
    let mut resp = client.get(&src.url).send().await.map_err(|e| {
        if e.is_builder() {
            Attempt::Fatal(ExecError::Invalid(format!("bundle URL: {}", describe(e))))
        } else if e.is_redirect() {
            Attempt::Fatal(ExecError::Unavailable(format!("bundle download: {}", describe(e))))
        } else {
            Attempt::Retry(describe(e))
        }
    })?;
    let status = resp.status();
    if status.is_server_error() {
        return Err(Attempt::Retry(format!("HTTP {}", status.as_u16())));
    }
    if status == reqwest::StatusCode::BAD_REQUEST || status == reqwest::StatusCode::FORBIDDEN {
        let body = read_error_body(&mut resp).await;
        let msg = format!("bundle download: HTTP {}", status.as_u16());
        return Err(Attempt::Fatal(if mentions_expired(&body) { ExecError::SourceUrlExpired(msg) } else { ExecError::Unavailable(msg) }));
    }
    if !status.is_success() {
        return Err(Attempt::Fatal(ExecError::Unavailable(format!("bundle download: HTTP {}", status.as_u16()))));
    }
    if let Some(len) = resp.content_length() {
        if len != src.bytes {
            return Err(Attempt::Fatal(ExecError::BundleRejected(format!(
                "size mismatch: server announced {len} bytes, expected {}",
                src.bytes
            ))));
        }
    }

    let fatal_io = |e: io::Error| Attempt::Fatal(io_err(part, e));
    let file = tokio::fs::OpenOptions::new().write(true).create(true).truncate(true).open(part).await.map_err(fatal_io)?;
    let mut out = tokio::io::BufWriter::with_capacity(DOWNLOAD_WRITE_BUFFER, file);
    let mut hasher = Sha256::new();
    let mut received: u64 = 0;
    while let Some(chunk) = resp.chunk().await.map_err(|e| Attempt::Retry(describe(e)))? {
        received = received.saturating_add(chunk.len() as u64);
        if received > src.bytes {
            return Err(Attempt::Fatal(ExecError::BundleRejected(format!("size mismatch: body exceeds the expected {} bytes", src.bytes))));
        }
        hasher.update(&chunk);
        out.write_all(&chunk).await.map_err(fatal_io)?;
    }
    if received != src.bytes {
        return Err(Attempt::Fatal(ExecError::BundleRejected(format!("size mismatch: received {received} bytes, expected {}", src.bytes))));
    }
    let digest: [u8; 32] = hasher.finalize().into();
    if digest != src.sha256 {
        return Err(Attempt::Fatal(ExecError::BundleRejected(format!(
            "sha256 mismatch: got {}, expected {}",
            hex::encode(digest),
            hex::encode(src.sha256)
        ))));
    }
    out.flush().await.map_err(fatal_io)?;
    out.into_inner().sync_all().await.map_err(fatal_io)?;
    Ok(())
}

/// First `ERROR_BODY_MAX` bytes of an error response; read failures yield what arrived so far.
async fn read_error_body(resp: &mut reqwest::Response) -> Vec<u8> {
    let mut body = Vec::new();
    while body.len() < ERROR_BODY_MAX {
        match resp.chunk().await {
            Ok(Some(chunk)) => {
                let take = chunk.len().min(ERROR_BODY_MAX - body.len());
                body.extend_from_slice(&chunk[..take]);
            }
            Ok(None) | Err(_) => break,
        }
    }
    body
}

fn mentions_expired(body: &[u8]) -> bool {
    body.windows(b"expired".len()).any(|w| w.eq_ignore_ascii_case(b"expired"))
}

/// Error text with the cause chain and without the (signed) URL.
fn describe(err: reqwest::Error) -> String {
    let err = err.without_url();
    let mut msg = err.to_string();
    let mut cause = std::error::Error::source(&err);
    while let Some(c) = cause {
        msg.push_str(": ");
        msg.push_str(&c.to_string());
        cause = c.source();
    }
    msg
}

/// Deletes the in-progress download unless it was renamed into place.
struct PartFile {
    path: PathBuf,
    armed: bool,
}

impl Drop for PartFile {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        match fs::remove_file(&self.path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!(path = %self.path.display(), error = %e, "could not remove partial bundle"),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Extraction
// ---------------------------------------------------------------------------------------------

/// Extract `archive` (zstd-compressed tar) into the existing empty directory `dest` on a blocking
/// task, entry by entry.
///
/// Any unsafe or over-cap entry → `ExecError::BundleRejected("<entry>: <reason>")`; the caller
/// deletes `dest`. Files get mode 0o755 (any exec bit set) or 0o644, directories 0o755; header
/// mtimes are preserved. `chown`: `Some((uid, gid))` in enforced mode → every created entry and
/// `dest` itself are chowned without following symlinks.
pub(crate) async fn extract(
    archive: PathBuf,
    dest: PathBuf,
    limits: BundleLimits,
    chown: Option<(u32, u32)>,
) -> Result<ExtractStats, ExecError> {
    tokio::task::spawn_blocking(move || Extractor::new(&dest, limits, chown).run(&archive))
        .await
        .map_err(|e| ExecError::Io(format!("bundle extraction task: {e}")))?
}

/// What the extractor created at a tree-relative path. The map is authoritative: directories are
/// never replaced, so a path recorded as `Dir` stays a real directory for the whole extraction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Node {
    Dir,
    File,
    Symlink,
    HardLink,
}

#[derive(Debug, Clone, Copy)]
struct Mtime {
    secs: libc::time_t,
    time: SystemTime,
}

struct Extractor<'a> {
    root: &'a Path,
    limits: BundleLimits,
    chown: Option<(u32, u32)>,
    /// Keyed by normalized `a/b/c` path bytes.
    nodes: HashMap<Vec<u8>, Node>,
    /// Header mtimes of explicit directory entries, applied once all children exist.
    dir_mtimes: HashMap<Vec<u8>, Mtime>,
    /// `(link path, target)` re-checked against the final tree.
    symlinks: Vec<(Vec<u8>, Vec<u8>)>,
    /// Every header the tar reader yielded, metadata included (entry cap).
    members: u64,
    stats: ExtractStats,
    buf: Vec<u8>,
}

impl<'a> Extractor<'a> {
    fn new(root: &'a Path, limits: BundleLimits, chown: Option<(u32, u32)>) -> Self {
        Self {
            root,
            limits,
            chown,
            nodes: HashMap::new(),
            dir_mtimes: HashMap::new(),
            symlinks: Vec::new(),
            members: 0,
            stats: ExtractStats::default(),
            buf: vec![0; EXTRACT_COPY_BUFFER],
        }
    }

    fn run(mut self, archive: &Path) -> Result<ExtractStats, ExecError> {
        let file = File::open(archive).map_err(|e| io_err(archive, e))?;
        let decoder = zstd::stream::read::Decoder::new(file).map_err(|e| io_err(archive, e))?;
        let mut tar = tar::Archive::new(decoder);
        for entry in tar.entries().map_err(malformed)? {
            let mut entry = entry.map_err(malformed)?;
            self.member(&mut entry)?;
        }
        // Lexical checks ran per entry; `..` after a component is only safe when that component
        // is a real directory, which is known once every entry exists.
        for (key, target) in &self.symlinks {
            let parents = parent_components(key);
            self.check_symlink_target(key, &parents, target, true)?;
        }
        self.finish_dirs()?;
        if let Some((uid, gid)) = self.chown {
            std::os::unix::fs::lchown(self.root, Some(uid), Some(gid)).map_err(|e| io_err(self.root, e))?;
        }
        Ok(self.stats)
    }

    fn member<R: Read>(&mut self, entry: &mut tar::Entry<'_, R>) -> Result<(), ExecError> {
        self.members += 1;
        if self.members > self.limits.max_entries {
            return Err(ExecError::BundleRejected(format!("archive has more than {} entries", self.limits.max_entries)));
        }
        let kind = entry.header().entry_type();
        if matches!(kind, EntryType::XGlobalHeader | EntryType::XHeader | EntryType::GNULongName | EntryType::GNULongLink) {
            return Ok(());
        }
        let raw = entry.path_bytes().into_owned();
        let comps = split_entry_path(&raw).map_err(|reason| reject(&raw, reason))?;
        let Some((_, parents)) = comps.split_last() else {
            return if kind == EntryType::Directory { Ok(()) } else { Err(reject(&raw, "entry names the archive root")) };
        };
        let key = comps.join(&b'/');
        match kind {
            EntryType::Regular | EntryType::Continuous => self.file(entry, &raw, parents, key)?,
            EntryType::Directory => self.dir(entry.header(), &raw, parents, key)?,
            EntryType::Symlink => self.symlink(entry, &raw, parents, key)?,
            EntryType::Link => self.hard_link(entry, &raw, parents, key)?,
            other => return Err(reject(&raw, format!("unsupported entry type: {}", type_name(other)))),
        }
        self.stats.entries += 1;
        Ok(())
    }

    fn file<R: Read>(&mut self, entry: &mut tar::Entry<'_, R>, raw: &[u8], parents: &[&[u8]], key: Vec<u8>) -> Result<(), ExecError> {
        let mode = entry.header().mode().map_err(|e| reject(raw, format!("bad mode: {e}")))?;
        let mtime = header_mtime(entry.header(), raw)?;
        let size = entry.size();
        self.check_size(raw, size)?;
        self.ensure_parents(raw, parents)?;
        let path = self.abs(&key);
        match self.nodes.get(&key) {
            None => {}
            Some(Node::File) => fs::remove_file(&path).map_err(|e| io_err(&path, e))?,
            Some(_) => return Err(reject(raw, "conflicts with an earlier entry of another type")),
        }
        let mut out = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)
            .map_err(|e| create_err(raw, &path, e))?;
        let written = self.copy_data(entry, &mut out, raw, &path)?;
        if written != size {
            return Err(reject(raw, format!("truncated: {written} of {size} bytes")));
        }
        let io = |e| io_err(&path, e);
        out.set_permissions(Permissions::from_mode(file_mode(mode))).map_err(io)?;
        out.set_times(FileTimes::new().set_modified(mtime.time)).map_err(io)?;
        if let Some((uid, gid)) = self.chown {
            std::os::unix::fs::fchown(&out, Some(uid), Some(gid)).map_err(io)?;
        }
        self.nodes.insert(key, Node::File);
        Ok(())
    }

    /// Copies the entry body, enforcing the caps on bytes actually read (not just the header).
    fn copy_data(&mut self, entry: &mut impl Read, out: &mut File, raw: &[u8], path: &Path) -> Result<u64, ExecError> {
        let mut written: u64 = 0;
        loop {
            let n = match entry.read(&mut self.buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(reject(raw, format!("unreadable data: {e}"))),
            };
            written = written.saturating_add(n as u64);
            self.check_size(raw, written)?;
            out.write_all(&self.buf[..n]).map_err(|e| io_err(path, e))?;
        }
        self.stats.bytes += written;
        Ok(written)
    }

    /// `size` more bytes for one file on top of what is already written.
    fn check_size(&self, raw: &[u8], size: u64) -> Result<(), ExecError> {
        if size > self.limits.max_file_bytes {
            return Err(reject(raw, format!("file of {size} bytes exceeds the {} byte per-file cap", self.limits.max_file_bytes)));
        }
        if self.stats.bytes.saturating_add(size) > self.limits.max_unpacked_bytes {
            return Err(reject(raw, format!("bundle exceeds the {} byte unpacked cap", self.limits.max_unpacked_bytes)));
        }
        Ok(())
    }

    fn dir(&mut self, header: &tar::Header, raw: &[u8], parents: &[&[u8]], key: Vec<u8>) -> Result<(), ExecError> {
        let mtime = header_mtime(header, raw)?;
        self.ensure_parents(raw, parents)?;
        match self.nodes.get(&key) {
            Some(Node::Dir) => {}
            Some(_) => return Err(reject(raw, "conflicts with an earlier entry of another type")),
            None => {
                self.make_dir(raw, &key)?;
                self.nodes.insert(key.clone(), Node::Dir);
            }
        }
        self.dir_mtimes.insert(key, mtime);
        Ok(())
    }

    fn symlink<R: Read>(&mut self, entry: &tar::Entry<'_, R>, raw: &[u8], parents: &[&[u8]], key: Vec<u8>) -> Result<(), ExecError> {
        let mtime = header_mtime(entry.header(), raw)?;
        let target = entry.link_name_bytes().ok_or_else(|| reject(raw, "symlink without a target"))?.into_owned();
        self.check_symlink_target(raw, parents, &target, false)?;
        self.ensure_parents(raw, parents)?;
        if self.nodes.contains_key(&key) {
            return Err(reject(raw, "conflicts with an earlier entry"));
        }
        let path = self.abs(&key);
        std::os::unix::fs::symlink(OsStr::from_bytes(&target), &path).map_err(|e| create_err(raw, &path, e))?;
        utimensat(None, &path, &TimeSpec::UTIME_OMIT, &TimeSpec::new(mtime.secs, 0), UtimensatFlags::NoFollowSymlink)
            .map_err(|e| io_err(&path, e.into()))?;
        if let Some((uid, gid)) = self.chown {
            std::os::unix::fs::lchown(&path, Some(uid), Some(gid)).map_err(|e| io_err(&path, e))?;
        }
        self.nodes.insert(key.clone(), Node::Symlink);
        self.symlinks.push((key, target));
        Ok(())
    }

    fn hard_link<R: Read>(&mut self, entry: &tar::Entry<'_, R>, raw: &[u8], parents: &[&[u8]], key: Vec<u8>) -> Result<(), ExecError> {
        let target = entry.link_name_bytes().ok_or_else(|| reject(raw, "hard link without a target"))?.into_owned();
        let target_comps =
            split_entry_path(&target).map_err(|reason| reject(raw, format!("hard link target {}: {reason}", show(&target))))?;
        let target_key = target_comps.join(&b'/');
        if self.nodes.get(&target_key) != Some(&Node::File) {
            return Err(reject(raw, format!("hard link target {} is not an earlier regular file", show(&target))));
        }
        self.ensure_parents(raw, parents)?;
        if self.nodes.contains_key(&key) {
            return Err(reject(raw, "conflicts with an earlier entry"));
        }
        let path = self.abs(&key);
        fs::hard_link(self.abs(&target_key), &path).map_err(|e| create_err(raw, &path, e))?;
        if let Some((uid, gid)) = self.chown {
            std::os::unix::fs::lchown(&path, Some(uid), Some(gid)).map_err(|e| io_err(&path, e))?;
        }
        self.nodes.insert(key, Node::HardLink);
        Ok(())
    }

    /// Creates missing parent directories. Every component must be a real directory: one this
    /// extraction recorded, or (case-folding filesystems) one found on disk that is not a symlink.
    fn ensure_parents(&mut self, raw: &[u8], parents: &[&[u8]]) -> Result<(), ExecError> {
        let mut prefix: Vec<u8> = Vec::new();
        for comp in parents {
            if !prefix.is_empty() {
                prefix.push(b'/');
            }
            prefix.extend_from_slice(comp);
            match self.nodes.get(&prefix) {
                Some(Node::Dir) => continue,
                Some(_) => return Err(reject(raw, format!("parent {} is not a directory", show(&prefix)))),
                None => {}
            }
            self.make_dir(raw, &prefix)?;
            self.nodes.insert(prefix.clone(), Node::Dir);
        }
        Ok(())
    }

    /// `mkdir` (never follows a final symlink); an existing real directory is accepted.
    fn make_dir(&self, raw: &[u8], key: &[u8]) -> Result<(), ExecError> {
        let path = self.abs(key);
        match fs::DirBuilder::new().mode(0o700).create(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => match fs::symlink_metadata(&path) {
                Ok(meta) if meta.file_type().is_dir() => Ok(()),
                Ok(_) => Err(reject(raw, format!("{} exists and is not a directory", show(key)))),
                Err(e) => Err(io_err(&path, e)),
            },
            Err(e) => Err(create_err(raw, &path, e)),
        }
    }

    /// Resolves `target` lexically from the link's directory; it must never climb above the
    /// root. With `require_dirs`, each `..` must leave a directory recorded in the tree, so a
    /// `..` following a symlink component (which the kernel resolves physically) is refused.
    fn check_symlink_target(&self, raw: &[u8], parents: &[&[u8]], target: &[u8], require_dirs: bool) -> Result<(), ExecError> {
        if target.is_empty() {
            return Err(reject(raw, "symlink with an empty target"));
        }
        if target.contains(&0) {
            return Err(reject(raw, "NUL byte in symlink target"));
        }
        if target.first() == Some(&b'/') {
            return Err(reject(raw, format!("symlink target {} is absolute", show(target))));
        }
        let mut stack: Vec<&[u8]> = parents.to_vec();
        for seg in target.split(|b| *b == b'/') {
            match seg {
                b"" | b"." => {}
                b".." => {
                    if stack.is_empty() {
                        return Err(reject(raw, format!("symlink target {} escapes the tree", show(target))));
                    }
                    if require_dirs {
                        let current = stack.join(&b'/');
                        if self.nodes.get(&current) != Some(&Node::Dir) {
                            return Err(reject(
                                raw,
                                format!(
                                    "symlink target {} applies `..` to {}, which is not a directory in the bundle",
                                    show(target),
                                    show(&current)
                                ),
                            ));
                        }
                    }
                    stack.pop();
                }
                name => stack.push(name),
            }
        }
        Ok(())
    }

    /// Final directory modes and mtimes (after all children exist), then ownership.
    fn finish_dirs(&self) -> Result<(), ExecError> {
        for (key, node) in &self.nodes {
            if *node != Node::Dir {
                continue;
            }
            let path = self.abs(key);
            let io = |e| io_err(&path, e);
            let dir = OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW).open(&path).map_err(io)?;
            dir.set_permissions(Permissions::from_mode(0o755)).map_err(io)?;
            if let Some(mtime) = self.dir_mtimes.get(key) {
                dir.set_times(FileTimes::new().set_modified(mtime.time)).map_err(io)?;
            }
            if let Some((uid, gid)) = self.chown {
                std::os::unix::fs::fchown(&dir, Some(uid), Some(gid)).map_err(io)?;
            }
        }
        Ok(())
    }

    fn abs(&self, key: &[u8]) -> PathBuf {
        self.root.join(OsStr::from_bytes(key))
    }
}

/// Splits a raw tar path into components. Absolute paths, `..`, and NUL are refused; empty and
/// `.` components are dropped. An empty result names the root.
fn split_entry_path(raw: &[u8]) -> Result<Vec<&[u8]>, &'static str> {
    if raw.is_empty() {
        return Err("empty path");
    }
    if raw.contains(&0) {
        return Err("NUL byte in path");
    }
    if raw.first() == Some(&b'/') {
        return Err("absolute path");
    }
    let mut comps = Vec::new();
    for seg in raw.split(|b| *b == b'/') {
        match seg {
            b"" | b"." => {}
            b".." => return Err("path has a `..` component"),
            name => comps.push(name),
        }
    }
    Ok(comps)
}

/// Parent components of a normalized key (`a/b/c` → `[a, b]`).
fn parent_components(key: &[u8]) -> Vec<&[u8]> {
    let mut comps: Vec<&[u8]> = key.split(|b| *b == b'/').collect();
    comps.pop();
    comps
}

fn header_mtime(header: &tar::Header, raw: &[u8]) -> Result<Mtime, ExecError> {
    let secs = header.mtime().map_err(|e| reject(raw, format!("bad mtime: {e}")))?;
    let time = SystemTime::UNIX_EPOCH.checked_add(Duration::from_secs(secs));
    match (libc::time_t::try_from(secs), time) {
        (Ok(secs), Some(time)) => Ok(Mtime { secs, time }),
        _ => Err(reject(raw, "mtime out of range")),
    }
}

/// 0o755 when any exec bit is set, else 0o644; setuid/setgid/sticky never survive.
fn file_mode(header_mode: u32) -> u32 {
    if header_mode & 0o111 != 0 { 0o755 } else { 0o644 }
}

fn type_name(kind: EntryType) -> String {
    match kind {
        EntryType::Char => "character device".to_owned(),
        EntryType::Block => "block device".to_owned(),
        EntryType::Fifo => "FIFO".to_owned(),
        EntryType::GNUSparse => "GNU sparse file".to_owned(),
        other => format!("type byte {:?}", char::from(other.as_byte())),
    }
}

/// Entry name for messages: lossy, quoted with control characters escaped, length-capped.
fn show(bytes: &[u8]) -> String {
    let head = bytes.get(..SHOW_MAX).unwrap_or(bytes);
    let more = if bytes.len() > SHOW_MAX { "…" } else { "" };
    format!("{:?}{more}", String::from_utf8_lossy(head))
}

fn reject(raw: &[u8], reason: impl fmt::Display) -> ExecError {
    ExecError::BundleRejected(format!("{}: {reason}", show(raw)))
}

fn malformed(err: io::Error) -> ExecError {
    ExecError::BundleRejected(format!("malformed archive: {err}"))
}

fn io_err(path: &Path, err: io::Error) -> ExecError {
    ExecError::Io(format!("{}: {err}", path.display()))
}

/// Creating an entry failed: collisions and symlink refusals are bundle problems, the rest I/O.
fn create_err(raw: &[u8], path: &Path, err: io::Error) -> ExecError {
    if err.kind() == io::ErrorKind::AlreadyExists {
        return reject(raw, "already exists");
    }
    match err.raw_os_error() {
        Some(libc::ELOOP) => reject(raw, "would write through a symlink"),
        Some(libc::ENAMETOOLONG) => reject(raw, "name too long"),
        _ => io_err(path, err),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const MTIME: u64 = 1_600_000_000;

    // ----- archive construction -------------------------------------------------------------

    struct Tar(tar::Builder<Vec<u8>>);

    impl Tar {
        fn new() -> Self {
            Self(tar::Builder::new(Vec::new()))
        }

        /// Appends a GNU header with `name`/`link` written as raw bytes, bypassing the
        /// builder's path validation so hostile names can be crafted.
        fn raw(mut self, kind: EntryType, name: &[u8], link: &[u8], mode: u32, data: &[u8]) -> Self {
            let mut h = tar::Header::new_gnu();
            {
                let old = h.as_old_mut();
                old.name[..name.len()].copy_from_slice(name);
                old.linkname[..link.len()].copy_from_slice(link);
            }
            h.set_entry_type(kind);
            h.set_size(data.len() as u64);
            h.set_mode(mode);
            h.set_mtime(MTIME);
            h.set_cksum();
            self.0.append(&h, data).unwrap();
            self
        }

        fn file(self, name: &str, mode: u32, data: &[u8]) -> Self {
            self.raw(EntryType::Regular, name.as_bytes(), b"", mode, data)
        }

        fn dir(self, name: &str) -> Self {
            self.raw(EntryType::Directory, name.as_bytes(), b"", 0o755, b"")
        }

        fn symlink(self, name: &str, target: &str) -> Self {
            self.raw(EntryType::Symlink, name.as_bytes(), target.as_bytes(), 0o777, b"")
        }

        fn hard_link(self, name: &str, target: &str) -> Self {
            self.raw(EntryType::Link, name.as_bytes(), target.as_bytes(), 0o644, b"")
        }

        /// Regular file through the builder, which emits a GNU long-name header when needed.
        fn long_file(mut self, name: &str, data: &[u8]) -> Self {
            let mut h = tar::Header::new_gnu();
            h.set_entry_type(EntryType::Regular);
            h.set_size(data.len() as u64);
            h.set_mode(0o644);
            h.set_mtime(MTIME);
            self.0.append_data(&mut h, name, data).unwrap();
            self
        }

        fn finish(self) -> Vec<u8> {
            let tar = self.0.into_inner().unwrap();
            zstd::encode_all(&tar[..], 0).unwrap()
        }
    }

    struct Extracted {
        dir: tempfile::TempDir,
        dest: PathBuf,
        result: Result<ExtractStats, ExecError>,
    }

    async fn extract_with(archive: Vec<u8>, limits: BundleLimits, chown: Option<(u32, u32)>) -> Extracted {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bundle.tar.zst");
        fs::write(&path, archive).unwrap();
        let dest = dir.path().join("tree");
        fs::create_dir(&dest).unwrap();
        let result = extract(path, dest.clone(), limits, chown).await;
        Extracted { dir, dest, result }
    }

    fn limits() -> BundleLimits {
        BundleLimits::from_gib(1)
    }

    #[track_caller]
    fn assert_rejected(result: &Result<ExtractStats, ExecError>, needle: &str) {
        match result {
            Err(ExecError::BundleRejected(msg)) => assert!(msg.contains(needle), "{msg:?} lacks {needle:?}"),
            other => panic!("expected BundleRejected containing {needle:?}, got {other:?}"),
        }
    }

    fn mode_of(path: &Path) -> u32 {
        fs::symlink_metadata(path).unwrap().mode() & 0o7777
    }

    fn mtime_of(path: &Path) -> i64 {
        fs::symlink_metadata(path).unwrap().mtime()
    }

    // ----- extraction -----------------------------------------------------------------------

    #[tokio::test]
    async fn extracts_tree_with_masked_modes_links_and_mtimes() {
        let archive = Tar::new()
            .dir("src/")
            .dir("src/nested/")
            .file("src/nested/a.txt", 0o640, b"alpha")
            .file("bin/run.sh", 0o700, b"#!/bin/sh\n")
            .file("bin/suid", 0o4755, b"x")
            .raw(EntryType::Directory, b"shared/", b"", 0o2777, b"")
            .symlink("src/link", "nested/a.txt")
            .symlink("bin/up", "../src/nested")
            .hard_link("src/hard", "src/nested/a.txt")
            .file("implicit/deep/b.txt", 0o644, b"beta")
            .finish();
        let me = (nix::unistd::getuid().as_raw(), nix::unistd::getgid().as_raw());
        let out = extract_with(archive, limits(), Some(me)).await;
        let stats = out.result.unwrap();
        let tree = &out.dest;

        assert_eq!(stats, ExtractStats { entries: 10, bytes: 5 + 10 + 1 + 4 });
        assert_eq!(fs::read(tree.join("src/nested/a.txt")).unwrap(), b"alpha");
        assert_eq!(mode_of(&tree.join("src/nested/a.txt")), 0o644);
        assert_eq!(mode_of(&tree.join("bin/run.sh")), 0o755, "exec bit kept");
        assert_eq!(mode_of(&tree.join("bin/suid")), 0o755, "setuid cleared");
        assert_eq!(mode_of(&tree.join("shared")), 0o755, "setgid/world-write cleared on dirs");
        assert_eq!(mode_of(&tree.join("implicit/deep")), 0o755);

        assert_eq!(fs::read_link(tree.join("src/link")).unwrap(), Path::new("nested/a.txt"));
        assert_eq!(fs::read(tree.join("src/link")).unwrap(), b"alpha");
        assert_eq!(fs::read(tree.join("bin/up/a.txt")).unwrap(), b"alpha");
        let hard = fs::metadata(tree.join("src/hard")).unwrap();
        assert_eq!(hard.ino(), fs::metadata(tree.join("src/nested/a.txt")).unwrap().ino());

        let mtime = MTIME as i64;
        assert_eq!(mtime_of(&tree.join("src/nested/a.txt")), mtime);
        assert_eq!(mtime_of(&tree.join("src/link")), mtime, "symlink mtime set without following");
        // Directory mtimes survive the children created after them.
        assert_eq!(mtime_of(&tree.join("src")), mtime);
        assert_eq!(mtime_of(&tree.join("src/nested")), mtime);
    }

    #[tokio::test]
    async fn accepts_git_archive_metadata_and_long_names() {
        // `git archive` starts with a pax global header carrying the commit id.
        let pax = b"52 comment=0123456789abcdef0123456789abcdef01234567\n";
        let long = format!("{}/leaf.txt", ["segment"; 20].join("/"));
        let archive = Tar::new().raw(EntryType::XGlobalHeader, b"pax_global_header", b"", 0o666, pax).long_file(&long, b"deep").finish();
        let out = extract_with(archive, limits(), None).await;
        assert_eq!(out.result.unwrap(), ExtractStats { entries: 1, bytes: 4 });
        assert_eq!(fs::read(out.dest.join(&long)).unwrap(), b"deep");
        assert!(!out.dest.join("pax_global_header").exists());
    }

    #[tokio::test]
    async fn later_regular_file_replaces_earlier_one() {
        let archive = Tar::new().file("f", 0o644, b"one").file("f", 0o755, b"two!").finish();
        let out = extract_with(archive, limits(), None).await;
        out.result.unwrap();
        assert_eq!(fs::read(out.dest.join("f")).unwrap(), b"two!");
        assert_eq!(mode_of(&out.dest.join("f")), 0o755);
    }

    #[tokio::test]
    async fn rejects_conflicting_duplicate() {
        let archive = Tar::new().file("f", 0o644, b"one").dir("f/").finish();
        assert_rejected(&extract_with(archive, limits(), None).await.result, "another type");
    }

    #[tokio::test]
    async fn rejects_parent_traversal() {
        let archive = Tar::new().raw(EntryType::Regular, b"../x", b"", 0o644, b"evil").finish();
        let out = extract_with(archive, limits(), None).await;
        assert_rejected(&out.result, "\"../x\"");
        assert!(!out.dir.path().join("x").exists());

        let archive = Tar::new().raw(EntryType::Regular, b"a/../../x", b"", 0o644, b"evil").finish();
        assert_rejected(&extract_with(archive, limits(), None).await.result, "`..`");
    }

    #[tokio::test]
    async fn rejects_absolute_path() {
        let archive = Tar::new().raw(EntryType::Regular, b"/abs", b"", 0o644, b"evil").finish();
        assert_rejected(&extract_with(archive, limits(), None).await.result, "absolute path");
    }

    #[tokio::test]
    async fn rejects_absolute_symlink() {
        let archive = Tar::new().symlink("etc", "/etc").finish();
        let out = extract_with(archive, limits(), None).await;
        assert_rejected(&out.result, "is absolute");
        assert!(fs::symlink_metadata(out.dest.join("etc")).is_err());
    }

    #[tokio::test]
    async fn rejects_symlink_escaping_lexically() {
        let archive = Tar::new().symlink("l", "a/../../x").finish();
        assert_rejected(&extract_with(archive, limits(), None).await.result, "escapes the tree");

        let archive = Tar::new().symlink("d/l", "../../x").finish();
        assert_rejected(&extract_with(archive, limits(), None).await.result, "escapes the tree");
    }

    #[tokio::test]
    async fn rejects_symlink_escaping_through_another_symlink() {
        // `a/b` → root (fine on its own); `s` → `a/b/../..` stays inside lexically but the kernel
        // resolves `a/b` first, so the second `..` would leave the tree.
        let archive = Tar::new().dir("a/").symlink("a/b", "..").symlink("s", "a/b/../..").finish();
        assert_rejected(&extract_with(archive, limits(), None).await.result, "not a directory in the bundle");
    }

    #[tokio::test]
    async fn never_writes_through_a_symlink() {
        let archive = Tar::new().dir("sub/").symlink("d", "sub").file("d/x", 0o644, b"x").finish();
        let out = extract_with(archive, limits(), None).await;
        assert_rejected(&out.result, "not a directory");
        assert!(!out.dest.join("sub/x").exists());
    }

    #[tokio::test]
    async fn rejects_hard_link_to_missing_or_later_entry() {
        let archive = Tar::new().hard_link("l", "missing").finish();
        assert_rejected(&extract_with(archive, limits(), None).await.result, "not an earlier regular file");

        let archive = Tar::new().hard_link("l", "f").file("f", 0o644, b"x").finish();
        assert_rejected(&extract_with(archive, limits(), None).await.result, "not an earlier regular file");

        let archive = Tar::new().hard_link("l", "../outside").finish();
        assert_rejected(&extract_with(archive, limits(), None).await.result, "`..`");
    }

    #[tokio::test]
    async fn rejects_fifo() {
        let archive = Tar::new().raw(EntryType::Fifo, b"pipe", b"", 0o644, b"").finish();
        let out = extract_with(archive, limits(), None).await;
        assert_rejected(&out.result, "FIFO");
        assert!(fs::symlink_metadata(out.dest.join("pipe")).is_err());
    }

    #[tokio::test]
    async fn enforces_entry_cap() {
        let capped = BundleLimits { max_entries: 3, ..limits() };
        let three = Tar::new().file("a", 0o644, b"").file("b", 0o644, b"").file("c", 0o644, b"").finish();
        assert_eq!(extract_with(three, capped, None).await.result.unwrap().entries, 3);

        let four = Tar::new().file("a", 0o644, b"").file("b", 0o644, b"").file("c", 0o644, b"").file("d", 0o644, b"");
        assert_rejected(&extract_with(four.finish(), capped, None).await.result, "more than 3 entries");
    }

    #[tokio::test]
    async fn enforces_total_size_cap() {
        let capped = BundleLimits { max_unpacked_bytes: 10, ..limits() };
        let exact = Tar::new().file("a", 0o644, &[1; 6]).file("b", 0o644, &[2; 4]).finish();
        assert_eq!(extract_with(exact, capped, None).await.result.unwrap().bytes, 10);

        let over = Tar::new().file("a", 0o644, &[1; 6]).file("b", 0o644, &[2; 5]).finish();
        let out = extract_with(over, capped, None).await;
        assert_rejected(&out.result, "unpacked cap");
        assert!(!out.dest.join("b").exists(), "rejected before writing");
    }

    #[tokio::test]
    async fn enforces_single_file_cap() {
        let capped = BundleLimits { max_file_bytes: 4, ..limits() };
        let archive = Tar::new().file("ok", 0o644, &[0; 4]).file("big", 0o644, &[0; 5]).finish();
        assert_rejected(&extract_with(archive, capped, None).await.result, "per-file cap");
    }

    #[tokio::test]
    async fn rejects_corrupt_archive() {
        let mut archive = Tar::new().file("a", 0o644, b"x").finish();
        archive.truncate(archive.len() / 2);
        assert!(matches!(extract_with(archive, limits(), None).await.result, Err(ExecError::BundleRejected(_))));
    }

    // ----- download -------------------------------------------------------------------------

    const SECRET: &str = "X-Goog-Signature=topsecret";

    fn client() -> reqwest::Client {
        reqwest::Client::builder().no_proxy().build().unwrap()
    }

    fn source(base: &str, body: &[u8]) -> BundleSource {
        BundleSource { url: format!("{base}/bundle?{SECRET}"), sha256: Sha256::digest(body).into(), bytes: body.len() as u64 }
    }

    fn fast() -> RetryPolicy {
        RetryPolicy { attempts: 3, backoff_base: Duration::from_millis(5), deadline: Duration::from_secs(20) }
    }

    fn cache_entries(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        names
    }

    #[track_caller]
    fn assert_no_secret(err: &ExecError) {
        let text = format!("{err} {err:?}");
        assert!(!text.contains("topsecret"), "signed URL leaked: {text}");
    }

    #[tokio::test]
    async fn fetch_verifies_and_serves_cache_hits_without_network() {
        let server = MockServer::start().await;
        let body = b"bundle bytes".to_vec();
        Mock::given(method("GET"))
            .and(path("/bundle"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(body.clone()))
            .expect(1)
            .mount(&server)
            .await;
        let cache = tempfile::tempdir().unwrap();
        let src = source(&server.uri(), &body);

        let first = fetch_with(&client(), cache.path(), &src, fast()).await.unwrap();
        assert_eq!(first, cache.path().join(format!("{}.tar.zst", hex::encode(src.sha256))));
        assert_eq!(fs::read(&first).unwrap(), body);
        let second = fetch_with(&client(), cache.path(), &src, fast()).await.unwrap();
        assert_eq!(second, first);
        assert_eq!(cache_entries(cache.path()).len(), 1, "no part file left behind");
        server.verify().await;
    }

    #[tokio::test]
    async fn fetch_rejects_sha_mismatch_and_leaves_no_file() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"tampered".to_vec()))
            .expect(1)
            .mount(&server)
            .await;
        let cache = tempfile::tempdir().unwrap();
        let src = BundleSource { sha256: [7; 32], ..source(&server.uri(), b"tampered") };

        let err = fetch_with(&client(), cache.path(), &src, fast()).await.unwrap_err();
        assert!(matches!(&err, ExecError::BundleRejected(m) if m.contains("sha256 mismatch")), "{err:?}");
        assert!(cache_entries(cache.path()).is_empty());
    }

    #[tokio::test]
    async fn fetch_rejects_size_mismatch() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"short".to_vec()))
            .expect(1)
            .mount(&server)
            .await;
        let cache = tempfile::tempdir().unwrap();
        let src = BundleSource { bytes: 6, ..source(&server.uri(), b"short") };

        let err = fetch_with(&client(), cache.path(), &src, fast()).await.unwrap_err();
        assert!(matches!(&err, ExecError::BundleRejected(m) if m.contains("size mismatch")), "{err:?}");
        assert!(cache_entries(cache.path()).is_empty());
    }

    #[tokio::test]
    async fn fetch_maps_expired_signature_without_retry() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(403)
                    .set_body_string("<Error><Code>AccessDenied</Code><Message>Request has expired</Message></Error>"),
            )
            .expect(1)
            .mount(&server)
            .await;
        let cache = tempfile::tempdir().unwrap();

        let err = fetch_with(&client(), cache.path(), &source(&server.uri(), b"x"), fast()).await.unwrap_err();
        assert!(matches!(err, ExecError::SourceUrlExpired(_)), "{err:?}");
        assert_no_secret(&err);
        server.verify().await;
    }

    #[tokio::test]
    async fn fetch_treats_other_client_errors_as_unavailable_without_retry() {
        for (status, body) in [(403, "AccessDenied"), (404, "NoSuchKey")] {
            let server = MockServer::start().await;
            Mock::given(method("GET")).respond_with(ResponseTemplate::new(status).set_body_string(body)).expect(1).mount(&server).await;
            let cache = tempfile::tempdir().unwrap();

            let err = fetch_with(&client(), cache.path(), &source(&server.uri(), b"x"), fast()).await.unwrap_err();
            assert!(matches!(err, ExecError::Unavailable(_)), "HTTP {status}: {err:?}");
            server.verify().await;
        }
    }

    #[tokio::test]
    async fn fetch_retries_server_errors() {
        let server = MockServer::start().await;
        let body = b"eventually".to_vec();
        Mock::given(method("GET")).respond_with(ResponseTemplate::new(500)).up_to_n_times(1).expect(1).mount(&server).await;
        Mock::given(method("GET")).respond_with(ResponseTemplate::new(200).set_body_bytes(body.clone())).expect(1).mount(&server).await;
        let cache = tempfile::tempdir().unwrap();

        let got = fetch_with(&client(), cache.path(), &source(&server.uri(), &body), fast()).await.unwrap();
        assert_eq!(fs::read(got).unwrap(), body);
        server.verify().await;
    }

    #[tokio::test]
    async fn fetch_gives_up_after_three_attempts() {
        let server = MockServer::start().await;
        Mock::given(method("GET")).respond_with(ResponseTemplate::new(503)).expect(3).mount(&server).await;
        let cache = tempfile::tempdir().unwrap();

        let err = fetch_with(&client(), cache.path(), &source(&server.uri(), b"x"), fast()).await.unwrap_err();
        assert!(matches!(&err, ExecError::Unavailable(m) if m.contains("3 attempts")), "{err:?}");
        assert!(cache_entries(cache.path()).is_empty());
        server.verify().await;
    }

    #[tokio::test]
    async fn fetch_connect_errors_never_leak_the_url() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let cache = tempfile::tempdir().unwrap();

        let err = fetch_with(&client(), cache.path(), &source(&format!("http://{addr}"), b"x"), fast()).await.unwrap_err();
        assert!(matches!(err, ExecError::Unavailable(_)), "{err:?}");
        assert_no_secret(&err);
    }

    #[tokio::test]
    async fn fetch_deadline_bounds_the_download_and_cleans_up() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"slow".to_vec()).set_delay(Duration::from_secs(5)))
            .mount(&server)
            .await;
        let cache = tempfile::tempdir().unwrap();
        let policy = RetryPolicy { deadline: Duration::from_millis(200), ..fast() };

        let err = fetch_with(&client(), cache.path(), &source(&server.uri(), b"slow"), policy).await.unwrap_err();
        assert!(matches!(&err, ExecError::Unavailable(m) if m.contains("timed out")), "{err:?}");
        assert!(cache_entries(cache.path()).is_empty());
    }
}
