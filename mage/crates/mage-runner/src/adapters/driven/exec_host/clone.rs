//! Copy-on-write fork of base directories (executor.md "Copy-on-write fork"): macOS
//! `clonefile(CLONE_NOFOLLOW)` on the whole directory; Linux a parallel walk that never follows
//! symlinks, recreates dirs and symlinks, reflinks regular files (FICLONE), preserves mode and
//! mtime, and turns hard links into independent files. Without reflink support the clone
//! restarts as a full copy (`ForkMode::Copy`) when allowed.

use std::fs::{self, FileTimes};
use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::ports::executor::{ExecError, ForkMode};

const WORKERS: usize = 16;

/// Probes reflink support of the data dir's filesystem (daemon start).
pub(crate) fn probe(data_dir: &Path) -> ForkMode {
    let a = data_dir.join("probe-a");
    let b = data_dir.join("probe-b");
    let _ = fs::remove_file(&b);
    if fs::write(&a, b"mage reflink probe").is_err() {
        return ForkMode::Copy;
    }
    let mode = match reflink_copy::reflink(&a, &b) {
        Ok(()) if cfg!(target_os = "macos") => ForkMode::Clonefile,
        Ok(()) => ForkMode::Reflink,
        Err(_) => ForkMode::Copy,
    };
    let _ = fs::remove_file(&a);
    let _ = fs::remove_file(&b);
    mode
}

fn reflink_unsupported(err: &io::Error) -> bool {
    matches!(err.raw_os_error(), Some(code) if code == nix::libc::EOPNOTSUPP || code == nix::libc::EXDEV || code == nix::libc::EINVAL || code == nix::libc::ENOTSUP)
}

/// Clones `src` into the not-yet-existing `dst`. `chown` = lchown every entry (enforced
/// `tree/`, `home/`). Returns the mode actually used.
pub(crate) fn clone_dir(src: &Path, dst: &Path, mode: ForkMode, allow_copy: bool, chown: Option<(u32, u32)>) -> Result<ForkMode, ExecError> {
    if !src.exists() {
        fs::create_dir_all(dst)?;
        return Ok(mode);
    }
    #[cfg(target_os = "macos")]
    if mode == ForkMode::Clonefile {
        match clonefile(src, dst) {
            Ok(()) => {
                if let Some((uid, gid)) = chown {
                    super::confine::lchown_tree(dst, uid, gid)?;
                }
                return Ok(ForkMode::Clonefile);
            }
            Err(err) if reflink_unsupported(&err) => {
                let _ = super::sys::remove_tree(dst);
            }
            Err(err) => return Err(ExecError::Io(format!("clonefile {}: {err}", src.display()))),
        }
    }
    if mode != ForkMode::Copy {
        match walk_clone(src, dst, false, chown) {
            Ok(()) => return Ok(if mode == ForkMode::Clonefile { ForkMode::Clonefile } else { ForkMode::Reflink }),
            Err(WalkError::Unsupported) => {
                let _ = super::sys::remove_tree(dst);
            }
            Err(WalkError::Io(e)) => return Err(e),
        }
    }
    if !allow_copy {
        return Err(ExecError::Unavailable("filesystem lacks reflink and runner.fork.allow_copy_fallback = false".into()));
    }
    match walk_clone(src, dst, true, chown) {
        Ok(()) => Ok(ForkMode::Copy),
        Err(WalkError::Unsupported) => Err(ExecError::Io("copy fallback failed".into())),
        Err(WalkError::Io(e)) => Err(e),
    }
}

#[cfg(target_os = "macos")]
fn clonefile(src: &Path, dst: &Path) -> io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    const CLONE_NOFOLLOW: u32 = 0x0001;
    let s = CString::new(src.as_os_str().as_bytes()).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    let d = CString::new(dst.as_os_str().as_bytes()).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    // SAFETY: valid NUL-terminated paths.
    let rc = unsafe { nix::libc::clonefile(s.as_ptr(), d.as_ptr(), CLONE_NOFOLLOW) };
    if rc == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

enum WalkError {
    Unsupported,
    Io(ExecError),
}

impl From<io::Error> for WalkError {
    fn from(e: io::Error) -> Self {
        WalkError::Io(ExecError::Io(e.to_string()))
    }
}

fn set_mtime(path: &Path, meta: &fs::Metadata) {
    if let Ok(modified) = meta.modified() {
        if let Ok(file) = fs::File::open(path) {
            let _ = file.set_times(FileTimes::new().set_modified(modified));
        }
    }
}

fn walk_clone(src: &Path, dst: &Path, copy: bool, chown: Option<(u32, u32)>) -> Result<(), WalkError> {
    let mut files: Vec<(PathBuf, PathBuf, fs::Metadata)> = Vec::new();
    let mut dirs: Vec<(PathBuf, fs::Metadata)> = Vec::new();
    let mut stack = vec![(src.to_path_buf(), dst.to_path_buf())];
    while let Some((from, to)) = stack.pop() {
        let meta = fs::symlink_metadata(&from)?;
        let ft = meta.file_type();
        if ft.is_symlink() {
            let target = fs::read_link(&from)?;
            std::os::unix::fs::symlink(&target, &to)?;
            if let Some((uid, gid)) = chown {
                std::os::unix::fs::lchown(&to, Some(uid), Some(gid))?;
            }
        } else if ft.is_dir() {
            fs::create_dir(&to)?;
            fs::set_permissions(&to, fs::Permissions::from_mode(meta.mode() & 0o7777))?;
            for entry in fs::read_dir(&from)? {
                let entry = entry?;
                stack.push((entry.path(), to.join(entry.file_name())));
            }
            dirs.push((to, meta));
        } else if ft.is_file() {
            files.push((from, to, meta));
        }
        // Sockets, FIFOs, and devices are not cloned.
    }
    let queue = Mutex::new(files.into_iter());
    let failure: Mutex<Option<WalkError>> = Mutex::new(None);
    std::thread::scope(|scope| {
        for _ in 0..WORKERS {
            scope.spawn(|| {
                loop {
                    let next = queue.lock().ok().and_then(|mut q| q.next());
                    let Some((from, to, meta)) = next else { break };
                    if failure.lock().map(|f| f.is_some()).unwrap_or(true) {
                        break;
                    }
                    let result = if copy {
                        reflink_copy::reflink_or_copy(&from, &to).map(|_| ())
                    } else {
                        reflink_copy::reflink(&from, &to)
                    };
                    let outcome = match result {
                        Ok(()) => {
                            let _ = fs::set_permissions(&to, fs::Permissions::from_mode(meta.mode() & 0o7777));
                            set_mtime(&to, &meta);
                            match chown {
                                Some((uid, gid)) => std::os::unix::fs::lchown(&to, Some(uid), Some(gid)).map_err(WalkError::from),
                                None => Ok(()),
                            }
                        }
                        Err(e) if !copy && reflink_unsupported(&e) => Err(WalkError::Unsupported),
                        Err(e) => Err(WalkError::Io(ExecError::Io(format!("clone {}: {e}", from.display())))),
                    };
                    if let Err(err) = outcome {
                        if let Ok(mut f) = failure.lock() {
                            f.get_or_insert(err);
                        }
                        break;
                    }
                }
            });
        }
    });
    if let Some(err) = failure.into_inner().ok().flatten() {
        return Err(err);
    }
    // Directory mtimes and ownership last (creating children touched them).
    for (dir, meta) in dirs.iter().rev() {
        set_mtime(dir, meta);
        if let Some((uid, gid)) = chown {
            std::os::unix::fs::lchown(dir, Some(uid), Some(gid))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clone_preserves_content_modes_links_and_mtime_and_is_independent() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        fs::create_dir_all(src.join("a/b")).unwrap();
        fs::write(src.join("a/b/file.txt"), b"hello").unwrap();
        fs::write(src.join("run.sh"), b"#!/bin/sh\n").unwrap();
        fs::set_permissions(src.join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink("a/b/file.txt", src.join("link")).unwrap();
        fs::hard_link(src.join("a/b/file.txt"), src.join("hard")).unwrap();
        let old = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        fs::File::open(src.join("a/b/file.txt")).unwrap().set_times(FileTimes::new().set_modified(old)).unwrap();

        let mode = probe(tmp.path());
        let dst = tmp.path().join("dst");
        let used = clone_dir(&src, &dst, mode, true, None).unwrap();
        assert!(matches!(used, ForkMode::Clonefile | ForkMode::Reflink | ForkMode::Copy));
        assert_eq!(fs::read(dst.join("a/b/file.txt")).unwrap(), b"hello");
        assert_eq!(fs::metadata(dst.join("run.sh")).unwrap().mode() & 0o777, 0o755);
        assert_eq!(fs::read_link(dst.join("link")).unwrap(), PathBuf::from("a/b/file.txt"));
        assert_eq!(fs::metadata(dst.join("a/b/file.txt")).unwrap().modified().unwrap(), old);
        fs::write(dst.join("hard"), b"changed").unwrap();
        assert_eq!(fs::read(dst.join("a/b/file.txt")).unwrap(), b"hello", "hard links become independent files");
        fs::write(dst.join("a/b/file.txt"), b"fork").unwrap();
        assert_eq!(fs::read(src.join("a/b/file.txt")).unwrap(), b"hello", "base unchanged");
    }

    #[test]
    fn copy_mode_without_fallback_permission_is_unavailable_only_when_needed() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("f"), b"x").unwrap();
        let err = clone_dir(&src, &tmp.path().join("dst"), ForkMode::Copy, false, None).unwrap_err();
        assert!(matches!(err, ExecError::Unavailable(_)));
    }
}
