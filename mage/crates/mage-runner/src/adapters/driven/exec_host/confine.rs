//! Confined file operations on an area root (executor.md "Confinement and limits", "Security
//! notes"). Every path is walked component by component with `openat(O_NOFOLLOW | O_DIRECTORY)`
//! from a directory fd, so a symlink can never redirect an operation outside the area: symlinks
//! are resolved lexically against the area root, absolute targets are refused, and `..` past the
//! root is `Confinement`. Synchronous; callers run these on the blocking pool.

use std::collections::VecDeque;
use std::ffi::OsStr;
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use nix::errno::Errno;
use nix::fcntl::{OFlag, openat, readlinkat, renameat};
use nix::sys::stat::{FileStat, Mode, SFlag, fchmod, fstat, fstatat, mkdirat};
use nix::unistd::{Gid, Uid, fchown};
use sha2::{Digest, Sha256};

use crate::ports::executor::{
    Area, DirEntry, EntryKind, ExecError, ExportReport, FileAccess, FileRead, ListOpts, Listing, ReadCollector, ReadRange, RelPath, WriteMode,
    WriteReport,
};

/// Who owns agent-facing files. `agent = None` in dev mode (no chown; one user).
#[derive(Debug, Clone, Copy)]
pub(crate) struct Owners {
    /// `(uid, gid)` of `mage-agent` in enforced mode.
    pub agent: Option<(u32, u32)>,
}

const MAX_SYMLINK_HOPS: u32 = 40;

/// An area root (`<fork>/tree` or `<fork>/scratch`).
#[derive(Debug, Clone)]
pub(crate) struct AreaRoot {
    pub dir: PathBuf,
    pub area: Area,
    pub owners: Owners,
}

/// Where a path landed: the fd of its parent directory, the final name (`None` = the area root
/// itself), and the resolved components below the root.
struct Located {
    dir: OwnedFd,
    name: Option<String>,
    resolved: Vec<String>,
}

#[derive(Clone, Copy)]
enum DirCreate {
    /// Agent-owned 0755 dirs (`write`).
    Agent,
    /// Runner-owned dirs with the given access (`put_file`, `make_dir`).
    Access(FileAccess),
}

fn io(err: Errno, what: &str) -> ExecError {
    match err {
        Errno::ENOENT => ExecError::NotFound(what.to_owned()),
        Errno::EEXIST => ExecError::Conflict(format!("{what} exists")),
        Errno::ENOTDIR => ExecError::Invalid(format!("{what}: not a directory")),
        Errno::EISDIR => ExecError::Invalid(format!("{what} is a directory")),
        Errno::ENOSPC | Errno::EDQUOT => ExecError::QuotaExceeded(format!("{what}: no space left")),
        other => ExecError::Io(format!("{what}: {other}")),
    }
}

fn owned(fd: RawFd) -> OwnedFd {
    // SAFETY: `fd` was just returned by a successful open call and is owned by nobody else.
    unsafe { OwnedFd::from_raw_fd(fd) }
}

fn is_kind(st: &FileStat, kind: SFlag) -> bool {
    SFlag::from_bits_truncate(st.st_mode as _) & SFlag::S_IFMT == kind
}

fn dir_mode(access: FileAccess, enforced: bool) -> u32 {
    match (access, enforced) {
        (FileAccess::AgentReadOnly, true) => 0o550,
        // Dev: runner and agent are one user; the owner keeps write so the runner can manage it.
        (FileAccess::AgentReadOnly, false) => 0o750,
        (FileAccess::AgentWritable, _) => 0o700,
        (FileAccess::AgentCreateOnly, _) => 0o1770,
    }
}

fn file_mode(access: FileAccess) -> u32 {
    match access {
        FileAccess::AgentReadOnly => 0o440,
        FileAccess::AgentWritable | FileAccess::AgentCreateOnly => 0o600,
    }
}

impl AreaRoot {
    pub(crate) fn new(dir: PathBuf, area: Area, owners: Owners) -> Self {
        Self { dir, area, owners }
    }

    fn escape(&self, rel: &RelPath) -> ExecError {
        ExecError::Confinement { area: self.area, path: rel.as_str().to_owned() }
    }

    fn open_root(&self) -> Result<OwnedFd, ExecError> {
        let fd = openat(None, &self.dir, OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC, Mode::empty())
            .map_err(|e| io(e, &format!("area root {}", self.dir.display())))?;
        Ok(owned(fd))
    }

    /// Applies owner and mode for a runner-created entry.
    fn apply_access(&self, fd: &OwnedFd, access: FileAccess, is_dir: bool) -> Result<(), ExecError> {
        let enforced = self.owners.agent.is_some();
        let mode = if is_dir { dir_mode(access, enforced) } else { file_mode(access) };
        if let Some((uid, gid)) = self.owners.agent {
            let (owner, group) = match access {
                FileAccess::AgentWritable => (Some(Uid::from_raw(uid)), Some(Gid::from_raw(gid))),
                FileAccess::AgentReadOnly | FileAccess::AgentCreateOnly => (None, Some(Gid::from_raw(gid))),
            };
            fchown(fd.as_raw_fd(), owner, group).map_err(|e| io(e, "chown"))?;
        }
        fchmod(fd.as_raw_fd(), Mode::from_bits_truncate(mode as _)).map_err(|e| io(e, "chmod"))?;
        Ok(())
    }

    /// Agent-owned entries (`write`, extracted trees): chown to `mage-agent` in enforced mode.
    fn chown_agent(&self, fd: &OwnedFd) -> Result<(), ExecError> {
        if let Some((uid, gid)) = self.owners.agent {
            fchown(fd.as_raw_fd(), Some(Uid::from_raw(uid)), Some(Gid::from_raw(gid))).map_err(|e| io(e, "chown"))?;
        }
        Ok(())
    }

    fn mkdir_one(&self, parent: &OwnedFd, name: &str, create: DirCreate) -> Result<(), ExecError> {
        match mkdirat(Some(parent.as_raw_fd()), name, Mode::from_bits_truncate(0o700)) {
            Ok(()) | Err(Errno::EEXIST) => {}
            Err(e) => return Err(io(e, name)),
        }
        let fd = openat(Some(parent.as_raw_fd()), name, OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC, Mode::empty())
            .map_err(|e| io(e, name))?;
        let fd = owned(fd);
        match create {
            DirCreate::Agent => {
                fchmod(fd.as_raw_fd(), Mode::from_bits_truncate(0o755)).map_err(|e| io(e, "chmod"))?;
                self.chown_agent(&fd)
            }
            DirCreate::Access(access) => {
                let dir_access = if access == FileAccess::AgentCreateOnly { FileAccess::AgentReadOnly } else { access };
                self.apply_access(&fd, dir_access, true)
            }
        }
    }

    /// Resolves `rel` (see module docs). Intermediate components must be directories (created
    /// when `create` is set); the last component is followed when it is a symlink and
    /// `follow_last`.
    fn locate(&self, rel: &RelPath, follow_last: bool, create: Option<DirCreate>) -> Result<Located, ExecError> {
        let mut pending: VecDeque<String> = rel.segments().map(str::to_owned).collect();
        let mut stack: Vec<String> = Vec::new();
        let mut dirs: Vec<OwnedFd> = vec![self.open_root()?];
        let mut hops = 0u32;
        let splice = |pending: &mut VecDeque<String>, target: &OsStr, hops: &mut u32| -> Result<(), ExecError> {
            *hops += 1;
            if *hops > MAX_SYMLINK_HOPS {
                return Err(ExecError::Invalid(format!("too many symlinks resolving {rel}")));
            }
            let bytes = target.as_bytes();
            if bytes.first() == Some(&b'/') {
                return Err(self.escape(rel));
            }
            let text = std::str::from_utf8(bytes).map_err(|_| ExecError::Invalid(format!("non-UTF-8 symlink target in {rel}")))?;
            for (i, part) in text.split('/').filter(|p| !p.is_empty() && *p != ".").enumerate() {
                pending.insert(i, part.to_owned());
            }
            Ok(())
        };
        while let Some(seg) = pending.pop_front() {
            if seg == ".." {
                if stack.pop().is_none() {
                    return Err(self.escape(rel));
                }
                dirs.pop();
                continue;
            }
            let parent = dirs.last().ok_or_else(|| self.escape(rel))?.as_raw_fd();
            if pending.is_empty() {
                match fstatat(Some(parent), seg.as_str(), nix::fcntl::AtFlags::AT_SYMLINK_NOFOLLOW) {
                    Ok(st) if follow_last && is_kind(&st, SFlag::S_IFLNK) => {
                        let target = readlinkat(Some(parent), seg.as_str()).map_err(|e| io(e, rel.as_str()))?;
                        splice(&mut pending, &target, &mut hops)?;
                        continue;
                    }
                    _ => {
                        let dir = dirs.pop().ok_or_else(|| self.escape(rel))?;
                        stack.push(seg.clone());
                        return Ok(Located { dir, name: Some(seg), resolved: stack });
                    }
                }
            }
            let flags = OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC;
            match openat(Some(parent), seg.as_str(), flags, Mode::empty()) {
                Ok(fd) => {
                    stack.push(seg);
                    dirs.push(owned(fd));
                }
                Err(Errno::ENOENT) => match create {
                    Some(mode) => {
                        let parent_fd = dirs.last().ok_or_else(|| self.escape(rel))?;
                        self.mkdir_one(parent_fd, &seg, mode)?;
                        pending.push_front(seg);
                    }
                    None => return Err(ExecError::NotFound(format!("{rel}"))),
                },
                Err(Errno::ELOOP | Errno::ENOTDIR | Errno::EMLINK) => {
                    let st = fstatat(Some(parent), seg.as_str(), nix::fcntl::AtFlags::AT_SYMLINK_NOFOLLOW).map_err(|e| io(e, rel.as_str()))?;
                    if is_kind(&st, SFlag::S_IFLNK) {
                        let target = readlinkat(Some(parent), seg.as_str()).map_err(|e| io(e, rel.as_str()))?;
                        splice(&mut pending, &target, &mut hops)?;
                    } else {
                        return Err(ExecError::Invalid(format!("{rel}: {seg} is not a directory")));
                    }
                }
                Err(e) => return Err(io(e, rel.as_str())),
            }
        }
        let dir = dirs.pop().ok_or_else(|| self.escape(rel))?;
        Ok(Located { dir, name: None, resolved: stack })
    }

    /// Real path of a resolved location (for listing and process cwd).
    fn real_path(&self, resolved: &[String]) -> PathBuf {
        let mut p = self.dir.clone();
        p.extend(resolved);
        p
    }

    fn open_file(&self, rel: &RelPath) -> Result<(File, FileStat), ExecError> {
        let loc = self.locate(rel, true, None)?;
        let name = loc.name.ok_or_else(|| ExecError::Invalid(format!("{rel} is a directory")))?;
        let fd = openat(Some(loc.dir.as_raw_fd()), name.as_str(), OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC, Mode::empty())
            .map_err(|e| if e == Errno::ELOOP { self.escape(rel) } else { io(e, rel.as_str()) })?;
        let fd = owned(fd);
        let st = fstat(fd.as_raw_fd()).map_err(|e| io(e, rel.as_str()))?;
        if !is_kind(&st, SFlag::S_IFREG) {
            return Err(ExecError::Invalid(format!("{rel} is not a regular file")));
        }
        Ok((File::from(fd), st))
    }

    /// Existing directory → its real path (process cwd).
    pub(crate) fn dir_path(&self, rel: &RelPath) -> Result<(PathBuf, Vec<String>), ExecError> {
        let loc = self.locate(rel, true, None)?;
        if let Some(name) = &loc.name {
            let st = fstatat(Some(loc.dir.as_raw_fd()), name.as_str(), nix::fcntl::AtFlags::AT_SYMLINK_NOFOLLOW)
                .map_err(|e| io(e, rel.as_str()))?;
            if !is_kind(&st, SFlag::S_IFDIR) {
                return Err(ExecError::Invalid(format!("{rel}: not a directory")));
            }
        }
        Ok((self.real_path(&loc.resolved), loc.resolved))
    }

    /// Real path of an existing regular file (symlinks inside the area resolved), for tools the
    /// daemon hands a path to (compose files).
    pub(crate) fn file_path(&self, rel: &RelPath) -> Result<PathBuf, ExecError> {
        let loc = self.locate(rel, true, None)?;
        let name = loc.name.as_deref().ok_or_else(|| ExecError::Invalid(format!("{rel} is a directory")))?;
        let st = fstatat(Some(loc.dir.as_raw_fd()), name, nix::fcntl::AtFlags::AT_SYMLINK_NOFOLLOW).map_err(|e| io(e, rel.as_str()))?;
        if !is_kind(&st, SFlag::S_IFREG) {
            return Err(ExecError::Invalid(format!("{rel} is not a regular file")));
        }
        Ok(self.real_path(&loc.resolved))
    }

    pub(crate) fn read(&self, rel: &RelPath, range: ReadRange) -> Result<FileRead, ExecError> {
        let (mut file, _) = self.open_file(rel)?;
        let mut collector = ReadCollector::new(range);
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = file.read(&mut buf).map_err(|e| ExecError::Io(format!("read {rel}: {e}")))?;
            if n == 0 {
                break;
            }
            collector.push(&buf[..n]);
        }
        Ok(collector.finish())
    }

    /// Agent-visible write: parents created agent-owned, file 0644 owned by `mage-agent`.
    pub(crate) fn write(&self, rel: &RelPath, bytes: &[u8], mode: WriteMode) -> Result<WriteReport, ExecError> {
        let loc = self.locate(rel, true, Some(DirCreate::Agent))?;
        let name = loc.name.ok_or_else(|| ExecError::Invalid("cannot write the area root".into()))?;
        let dir = loc.dir.as_raw_fd();
        let base = OFlag::O_WRONLY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC;
        let perm = Mode::from_bits_truncate(0o644);
        let (fd, created) = match openat(Some(dir), name.as_str(), base | OFlag::O_CREAT | OFlag::O_EXCL, perm) {
            Ok(fd) => (owned(fd), true),
            Err(Errno::EEXIST) if mode == WriteMode::Create => return Err(ExecError::Conflict(format!("{rel} exists"))),
            Err(Errno::EEXIST) => {
                let extra = if mode == WriteMode::Append { OFlag::O_APPEND } else { OFlag::empty() };
                let fd = openat(Some(dir), name.as_str(), base | extra, perm)
                    .map_err(|e| if e == Errno::ELOOP { self.escape(rel) } else { io(e, rel.as_str()) })?;
                (owned(fd), false)
            }
            Err(e) => return Err(io(e, rel.as_str())),
        };
        let st = fstat(fd.as_raw_fd()).map_err(|e| io(e, rel.as_str()))?;
        if !is_kind(&st, SFlag::S_IFREG) {
            return Err(ExecError::Invalid(format!("{rel} is not a regular file")));
        }
        if created {
            self.chown_agent(&fd)?;
            fchmod(fd.as_raw_fd(), perm).map_err(|e| io(e, "chmod"))?;
        }
        let mut file = File::from(fd);
        if mode == WriteMode::Overwrite && !created {
            file.set_len(0).map_err(|e| ExecError::Io(format!("truncate {rel}: {e}")))?;
        }
        file.write_all(bytes).map_err(|e| io(Errno::from_raw(e.raw_os_error().unwrap_or(0)), rel.as_str()))?;
        Ok(WriteReport { bytes: bytes.len() as u64, created })
    }

    pub(crate) fn list(&self, rel: &RelPath, opts: &ListOpts) -> Result<Listing, ExecError> {
        if !(1..=4).contains(&opts.depth) {
            return Err(ExecError::Invalid("depth must be 1..=4".into()));
        }
        let (start, resolved) = self.dir_path(rel)?;
        let prefix = resolved.join("/");
        let mut walker = ignore::WalkBuilder::new(&start);
        walker
            .max_depth(Some(usize::from(opts.depth)))
            .hidden(!opts.hidden)
            .git_ignore(opts.respect_gitignore)
            .git_exclude(false)
            .git_global(false)
            .ignore(false)
            .parents(false)
            .require_git(false)
            .follow_links(false)
            .sort_by_file_path(|a, b| a.cmp(b));
        let mut entries = Vec::new();
        let mut truncated = false;
        for item in walker.build() {
            let Ok(entry) = item else { continue };
            if entry.depth() == 0 {
                continue;
            }
            let Ok(sub) = entry.path().strip_prefix(&start) else { continue };
            let Some(sub) = sub.to_str() else { continue };
            let full = if prefix.is_empty() { sub.to_owned() } else { format!("{prefix}/{sub}") };
            if opts.glob.as_ref().is_some_and(|g| !g.matches(&full)) {
                continue;
            }
            if entries.len() >= opts.max_entries as usize {
                truncated = true;
                break;
            }
            let Ok(meta) = std::fs::symlink_metadata(entry.path()) else { continue };
            let kind = if meta.file_type().is_symlink() {
                EntryKind::Symlink
            } else if meta.is_dir() {
                EntryKind::Dir
            } else {
                EntryKind::File
            };
            let Ok(path) = RelPath::new(&full) else { continue };
            entries.push(DirEntry { path, kind, bytes: if kind == EntryKind::Dir { 0 } else { meta.len() } });
        }
        Ok(Listing { entries, truncated })
    }

    /// Copies a file to a runner-local `dest`, hashing while copying.
    pub(crate) fn export(&self, rel: &RelPath, dest: &Path) -> Result<ExportReport, ExecError> {
        let (mut file, _) = self.open_file(rel)?;
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut out = File::create(dest).map_err(|e| ExecError::Io(format!("export to {}: {e}", dest.display())))?;
        let mut hasher = Sha256::new();
        let mut total = 0u64;
        let mut buf = vec![0u8; 256 * 1024];
        loop {
            let n = file.read(&mut buf).map_err(|e| ExecError::Io(format!("read {rel}: {e}")))?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            out.write_all(&buf[..n]).map_err(|e| ExecError::Io(format!("write {}: {e}", dest.display())))?;
            total += n as u64;
        }
        out.sync_all().map_err(|e| ExecError::Io(format!("sync {}: {e}", dest.display())))?;
        Ok(ExportReport { bytes: total, sha256: hasher.finalize().into() })
    }

    /// Runner-owned file with explicit agent access, replaced atomically (temp + rename).
    pub(crate) fn put_file(&self, rel: &RelPath, bytes: &[u8], access: FileAccess) -> Result<WriteReport, ExecError> {
        if access == FileAccess::AgentCreateOnly {
            return Err(ExecError::Invalid("AgentCreateOnly applies to directories only".into()));
        }
        let loc = self.locate(rel, false, Some(DirCreate::Access(FileAccess::AgentReadOnly)))?;
        let name = loc.name.ok_or_else(|| ExecError::Conflict(format!("{rel} is a directory")))?;
        let dir = loc.dir.as_raw_fd();
        let created = match fstatat(Some(dir), name.as_str(), nix::fcntl::AtFlags::AT_SYMLINK_NOFOLLOW) {
            Ok(st) if is_kind(&st, SFlag::S_IFDIR) => return Err(ExecError::Conflict(format!("{rel} is a directory"))),
            Ok(_) => false,
            Err(Errno::ENOENT) => true,
            Err(e) => return Err(io(e, rel.as_str())),
        };
        let tmp = format!(".mage-put-{}", uuid::Uuid::new_v4().simple());
        let fd = openat(
            Some(dir),
            tmp.as_str(),
            OFlag::O_WRONLY | OFlag::O_CREAT | OFlag::O_EXCL | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
            Mode::from_bits_truncate(0o600),
        )
        .map_err(|e| io(e, rel.as_str()))?;
        let fd = owned(fd);
        let result = (|| {
            let mut file = File::from(fd.try_clone().map_err(|e| ExecError::Io(e.to_string()))?);
            file.write_all(bytes).map_err(|e| ExecError::Io(format!("write {rel}: {e}")))?;
            file.sync_all().map_err(|e| ExecError::Io(format!("sync {rel}: {e}")))?;
            self.apply_access(&fd, access, false)?;
            renameat(Some(dir), tmp.as_str(), Some(dir), name.as_str()).map_err(|e| io(e, rel.as_str()))
        })();
        if result.is_err() {
            let _ = nix::unistd::unlinkat(Some(dir), tmp.as_str(), nix::unistd::UnlinkatFlags::NoRemoveDir);
        }
        result?;
        Ok(WriteReport { bytes: bytes.len() as u64, created })
    }

    /// Runner-owned directory with explicit agent access; idempotent (re-applies the access).
    pub(crate) fn make_dir(&self, rel: &RelPath, access: FileAccess) -> Result<(), ExecError> {
        let parent_access = if access == FileAccess::AgentCreateOnly { FileAccess::AgentReadOnly } else { access };
        let loc = self.locate(rel, false, Some(DirCreate::Access(parent_access)))?;
        let fd = match &loc.name {
            None => loc.dir,
            Some(name) => {
                let dir = loc.dir.as_raw_fd();
                match mkdirat(Some(dir), name.as_str(), Mode::from_bits_truncate(0o700)) {
                    Ok(()) | Err(Errno::EEXIST) => {}
                    Err(e) => return Err(io(e, rel.as_str())),
                }
                let fd = openat(Some(dir), name.as_str(), OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC, Mode::empty())
                    .map_err(|e| match e {
                        Errno::ENOTDIR | Errno::ELOOP => ExecError::Conflict(format!("{rel} exists and is not a directory")),
                        e => io(e, rel.as_str()),
                    })?;
                owned(fd)
            }
        };
        self.apply_access(&fd, access, true)
    }
}

/// `lchown -R` to `(uid, gid)`, never following symlinks (enforced-mode trees).
#[cfg(target_os = "macos")]
pub(crate) fn lchown_tree(root: &Path, uid: u32, gid: u32) -> Result<(), ExecError> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(p) = stack.pop() {
        std::os::unix::fs::lchown(&p, Some(uid), Some(gid)).map_err(|e| ExecError::Io(format!("lchown {}: {e}", p.display())))?;
        let meta = std::fs::symlink_metadata(&p)?;
        if meta.is_dir() {
            for entry in std::fs::read_dir(&p)? {
                stack.push(entry?.path());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::{PermissionsExt, symlink};

    use super::*;

    fn area() -> (tempfile::TempDir, AreaRoot) {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        std::fs::create_dir(&ws).unwrap();
        std::fs::write(tmp.path().join("secret"), b"outside").unwrap();
        (tmp, AreaRoot::new(ws, Area::Workspace, Owners { agent: None }))
    }

    fn rel(s: &str) -> RelPath {
        RelPath::new(s).unwrap()
    }

    #[test]
    fn write_read_append_and_create_conflict() {
        let (_t, a) = area();
        let r = a.write(&rel("a/b/c.txt"), b"one\n", WriteMode::Overwrite).unwrap();
        assert!(r.created);
        assert_eq!(a.write(&rel("a/b/c.txt"), b"two\n", WriteMode::Append).unwrap(), WriteReport { bytes: 4, created: false });
        assert_eq!(a.read(&rel("a/b/c.txt"), ReadRange::Whole { max_bytes: 100 }).unwrap().bytes, b"one\ntwo\n");
        assert!(matches!(a.write(&rel("a/b/c.txt"), b"x", WriteMode::Create), Err(ExecError::Conflict(_))));
        a.write(&rel("a/b/c.txt"), b"z", WriteMode::Overwrite).unwrap();
        assert_eq!(a.read(&rel("a/b/c.txt"), ReadRange::Whole { max_bytes: 100 }).unwrap().bytes, b"z");
        let mode = std::fs::metadata(a.dir.join("a/b/c.txt")).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o644);
        assert!(matches!(a.read(&rel("missing"), ReadRange::Whole { max_bytes: 1 }), Err(ExecError::NotFound(_))));
        assert!(matches!(a.read(&rel("a"), ReadRange::Whole { max_bytes: 1 }), Err(ExecError::Invalid(_))));
    }

    #[test]
    fn symlinks_inside_are_followed_and_escapes_refused() {
        let (_t, a) = area();
        a.write(&rel("real/file.txt"), b"in", WriteMode::Overwrite).unwrap();
        symlink("real", a.dir.join("alias")).unwrap();
        symlink("../real/file.txt", a.dir.join("real/self")).unwrap();
        assert_eq!(a.read(&rel("alias/file.txt"), ReadRange::Whole { max_bytes: 9 }).unwrap().bytes, b"in");
        assert_eq!(a.read(&rel("real/self"), ReadRange::Whole { max_bytes: 9 }).unwrap().bytes, b"in");

        symlink("/", a.dir.join("root")).unwrap();
        symlink("../secret", a.dir.join("up")).unwrap();
        symlink("hop2", a.dir.join("hop1")).unwrap();
        symlink("real/../../secret", a.dir.join("hop2")).unwrap();
        symlink("/etc/passwd", a.dir.join("passwd")).unwrap();
        for path in ["root/etc/passwd", "up", "hop1", "passwd"] {
            let err = a.read(&rel(path), ReadRange::Whole { max_bytes: 9 }).unwrap_err();
            assert!(matches!(err, ExecError::Confinement { area: Area::Workspace, .. }), "{path}: {err:?}");
        }
        // Writes through escaping links are refused too, and nothing is created outside.
        assert!(matches!(a.write(&rel("root/tmp/x"), b"x", WriteMode::Overwrite), Err(ExecError::Confinement { .. })));
        assert!(matches!(a.write(&rel("up"), b"x", WriteMode::Overwrite), Err(ExecError::Confinement { .. })));
        assert_eq!(std::fs::read(a.dir.parent().unwrap().join("secret")).unwrap(), b"outside");
        // A symlink loop is reported, not followed forever.
        symlink("loop", a.dir.join("loop")).unwrap();
        assert!(a.read(&rel("loop"), ReadRange::Whole { max_bytes: 1 }).is_err());
    }

    #[test]
    fn list_respects_depth_hidden_gitignore_glob_and_cap() {
        let (_t, a) = area();
        for p in ["src/a.ts", "src/deep/b.ts", ".hidden/x", "dist/out.js", "README.md"] {
            a.write(&rel(p), b"x", WriteMode::Overwrite).unwrap();
        }
        a.write(&rel(".gitignore"), b"dist/\n", WriteMode::Overwrite).unwrap();
        let names = |l: Listing| l.entries.into_iter().map(|e| e.path.as_str().to_owned()).collect::<Vec<_>>();
        let top = a.list(&RelPath::root(), &ListOpts::default()).unwrap();
        assert_eq!(names(top), vec!["README.md", "src"]);
        let deep = a.list(&rel("src"), &ListOpts { depth: 2, ..ListOpts::default() }).unwrap();
        assert_eq!(names(deep), vec!["src/a.ts", "src/deep", "src/deep/b.ts"]);
        let all = a.list(&RelPath::root(), &ListOpts { hidden: true, respect_gitignore: false, ..ListOpts::default() }).unwrap();
        assert_eq!(names(all), vec![".gitignore", ".hidden", "README.md", "dist", "src"]);
        let globbed = a
            .list(&RelPath::root(), &ListOpts { depth: 4, glob: Some(crate::ports::executor::PathGlob::new("**/*.ts").unwrap()), ..ListOpts::default() })
            .unwrap();
        assert_eq!(names(globbed), vec!["src/a.ts", "src/deep/b.ts"]);
        let capped = a.list(&RelPath::root(), &ListOpts { depth: 4, max_entries: 2, ..ListOpts::default() }).unwrap();
        assert!(capped.truncated && capped.entries.len() == 2);
        assert!(matches!(a.list(&rel("README.md"), &ListOpts::default()), Err(ExecError::Invalid(_))));
        assert!(matches!(a.list(&rel("nope"), &ListOpts::default()), Err(ExecError::NotFound(_))));
    }

    #[test]
    fn put_file_and_make_dir_apply_access_and_replace_atomically() {
        let (_t, a) = area();
        a.make_dir(&rel("harness"), FileAccess::AgentReadOnly).unwrap();
        a.make_dir(&rel("harness"), FileAccess::AgentReadOnly).unwrap();
        a.make_dir(&rel("harness/agent"), FileAccess::AgentCreateOnly).unwrap();
        let mode = |p: &str| std::fs::symlink_metadata(a.dir.join(p)).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode("harness"), 0o750);
        assert_eq!(mode("harness/agent"), 0o1770);
        let r = a.put_file(&rel("harness/config.json"), b"{}", FileAccess::AgentReadOnly).unwrap();
        assert!(r.created);
        assert_eq!(mode("harness/config.json"), 0o440);
        let r = a.put_file(&rel("harness/config.json"), b"{\"a\":1}", FileAccess::AgentReadOnly).unwrap();
        assert!(!r.created);
        assert_eq!(std::fs::read(a.dir.join("harness/config.json")).unwrap(), b"{\"a\":1}");
        // A symlink planted at the target is replaced, never written through.
        symlink("../../secret", a.dir.join("harness/agent/token")).unwrap();
        a.put_file(&rel("harness/agent/token"), b"t", FileAccess::AgentWritable).unwrap();
        assert!(!std::fs::symlink_metadata(a.dir.join("harness/agent/token")).unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read(a.dir.parent().unwrap().join("secret")).unwrap(), b"outside");
        assert!(matches!(a.make_dir(&rel("harness/config.json"), FileAccess::AgentWritable), Err(ExecError::Conflict(_))));
        assert!(matches!(a.put_file(&rel("harness"), b"x", FileAccess::AgentReadOnly), Err(ExecError::Conflict(_))));
        let ls = a.list(&rel("harness"), &ListOpts::default()).unwrap();
        assert!(ls.entries.iter().all(|e| !e.path.as_str().contains(".mage-put-")), "no temp files left");
    }

    #[test]
    fn export_hashes_while_copying() {
        let (t, a) = area();
        a.write(&rel("out/video.webm"), b"abc", WriteMode::Overwrite).unwrap();
        let dest = t.path().join("outbox/x/video.webm");
        let rep = a.export(&rel("out/video.webm"), &dest).unwrap();
        assert_eq!(rep.bytes, 3);
        assert_eq!(rep.sha256, <[u8; 32]>::from(Sha256::digest(b"abc")));
        assert_eq!(std::fs::read(dest).unwrap(), b"abc");
    }
}
