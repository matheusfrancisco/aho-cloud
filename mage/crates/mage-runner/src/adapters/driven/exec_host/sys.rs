//! OS helpers: process identity (start time, boot id), signals to process groups, disk usage,
//! users.

use std::path::{Path, PathBuf};

use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;

use crate::ports::executor::ExecError;

/// Linux `/proc/sys/kernel/random/boot_id`; macOS `kern.boottime` (`<sec>.<usec>`).
pub(crate) fn boot_id() -> String {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string("/proc/sys/kernel/random/boot_id").map(|s| s.trim().to_owned()).unwrap_or_default()
    }
    #[cfg(target_os = "macos")]
    {
        let mut tv = nix::libc::timeval { tv_sec: 0, tv_usec: 0 };
        let mut size = std::mem::size_of::<nix::libc::timeval>();
        // SAFETY: `kern.boottime` writes one `timeval` into the buffer we pass with its size.
        let rc = unsafe {
            nix::libc::sysctlbyname(c"kern.boottime".as_ptr(), (&raw mut tv).cast(), &raw mut size, std::ptr::null_mut(), 0)
        };
        if rc == 0 { format!("{}.{}", tv.tv_sec, tv.tv_usec) } else { String::new() }
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        String::new()
    }
}

/// Linux `/proc/<pid>/stat` field 22 (clock ticks since boot); macOS process start in µs.
/// `None` when the process does not exist.
pub(crate) fn process_start_time(pid: i32) -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        // comm (field 2) may contain spaces and parens: fields after the last ')' start at 3.
        let rest = &stat[stat.rfind(')')? + 1..];
        rest.split_whitespace().nth(19)?.parse().ok()
    }
    #[cfg(target_os = "macos")]
    {
        // SAFETY: zeroed POD struct filled by proc_pidinfo with the size we pass.
        let mut info: nix::libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<nix::libc::proc_bsdinfo>() as nix::libc::c_int;
        // SAFETY: buffer and size describe `info`.
        let n = unsafe { nix::libc::proc_pidinfo(pid, nix::libc::PROC_PIDTBSDINFO, 0, (&raw mut info).cast(), size) };
        (n == size).then(|| info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = pid;
        None
    }
}

/// `killpg`; a missing group is not an error.
pub(crate) fn signal_group(pgid: i32, signal: Signal) -> bool {
    if pgid <= 1 {
        return false;
    }
    killpg(Pid::from_raw(pgid), signal).is_ok()
}

/// Whether any process of the group is alive.
pub(crate) fn group_alive(pgid: i32) -> bool {
    pgid > 1 && killpg(Pid::from_raw(pgid), None).is_ok()
}

/// `(available bytes, total bytes)` of the filesystem holding `path`.
pub(crate) fn disk_space(path: &Path) -> Result<(u64, u64), ExecError> {
    let st = nix::sys::statvfs::statvfs(path).map_err(|e| ExecError::Io(format!("statvfs {}: {e}", path.display())))?;
    let frsize = st.fragment_size() as u64;
    Ok((st.blocks_available() as u64 * frsize, st.blocks() as u64 * frsize))
}

/// Allocated bytes (`st_blocks × 512`) under `path`, never following symlinks.
pub(crate) fn allocated_bytes(path: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    let mut total = 0u64;
    let mut stack = vec![path.to_path_buf()];
    while let Some(p) = stack.pop() {
        let Ok(meta) = std::fs::symlink_metadata(&p) else { continue };
        total += meta.blocks() * 512;
        if meta.is_dir() {
            if let Ok(rd) = std::fs::read_dir(&p) {
                stack.extend(rd.flatten().map(|e| e.path()));
            }
        }
    }
    total
}

/// Name of the user running the daemon.
pub(crate) fn current_user() -> String {
    nix::unistd::User::from_uid(nix::unistd::getuid()).ok().flatten().map(|u| u.name).unwrap_or_else(|| "mage".to_owned())
}

#[cfg(target_os = "linux")]
/// `(uid, gid)` of a user.
pub(crate) fn lookup_user(name: &str) -> Result<(u32, u32), ExecError> {
    let user = nix::unistd::User::from_name(name)
        .map_err(|e| ExecError::Unavailable(format!("user {name}: {e}")))?
        .ok_or_else(|| ExecError::Unavailable(format!("user {name} does not exist")))?;
    Ok((user.uid.as_raw(), user.gid.as_raw()))
}

/// Renames `path` into `trash` under a fresh name (same filesystem) and returns the new path.
pub(crate) fn move_to_trash(path: &Path, trash: &Path) -> Result<Option<PathBuf>, ExecError> {
    if std::fs::symlink_metadata(path).is_err() {
        return Ok(None);
    }
    std::fs::create_dir_all(trash)?;
    let dest = trash.join(uuid::Uuid::new_v4().simple().to_string());
    std::fs::rename(path, &dest).map_err(|e| ExecError::Io(format!("trash {}: {e}", path.display())))?;
    Ok(Some(dest))
}

/// Deletes a trashed dir on a blocking thread (`remove_dir_all` never follows symlinks).
pub(crate) fn remove_in_background(path: PathBuf) {
    tokio::task::spawn_blocking(move || {
        if let Err(err) = remove_tree(&path) {
            tracing::warn!(path = %path.display(), error = %err, "trash cleanup failed");
        }
    });
}

/// `remove_dir_all` that first restores owner write permission on directories (dev-mode
/// `AgentReadOnly` dirs are 0550/0750-like and fixtures may drop write bits).
pub(crate) fn remove_tree(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(first) => {
            let mut stack = vec![path.to_path_buf()];
            while let Some(p) = stack.pop() {
                let Ok(meta) = std::fs::symlink_metadata(&p) else { continue };
                if meta.is_dir() {
                    let mode = meta.permissions().mode();
                    let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode((mode | 0o700) & 0o7777));
                    if let Ok(rd) = std::fs::read_dir(&p) {
                        stack.extend(rd.flatten().map(|e| e.path()));
                    }
                }
            }
            std::fs::remove_dir_all(path).map_err(|_| first)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn own_process_identity_is_stable() {
        let pid = std::process::id() as i32;
        let a = process_start_time(pid).expect("own start time");
        assert_eq!(process_start_time(pid), Some(a));
        assert!(!boot_id().is_empty());
        assert_eq!(process_start_time(i32::MAX - 7), None);
    }

    #[test]
    fn disk_space_reports_something() {
        let (free, total) = disk_space(Path::new("/")).unwrap();
        assert!(total > 0 && free <= total);
    }
}
