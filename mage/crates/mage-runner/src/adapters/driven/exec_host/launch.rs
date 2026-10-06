//! The one internal `Launcher` every repo-controlled process goes through (executor.md "Spawning
//! processes"). `Dev`: own process group, cleared env, real cwd, same uid. `Enforced` (Linux):
//! leaf cgroup, new session, private mount namespace with the fork bound at `/mage/{ws,home,
//! scratch}`, private `/tmp` `/var/tmp` `/dev/shm`, tmpfs over the data dir and `/etc/mage`,
//! `PR_SET_NO_NEW_PRIVS`, rlimits, then `mage-agent` uid/gid. The child runs only raw syscalls in
//! `pre_exec`; every string and fd is prepared before `fork()`.

use std::path::PathBuf;
use std::process::Stdio;

use nix::sys::signal::Signal;

use super::sys;
use crate::ports::executor::ExecError;

/// Sources of the per-process mounts (enforced mode).
#[derive(Debug, Clone)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) struct MountSet {
    pub tree: PathBuf,
    pub home: PathBuf,
    pub scratch: PathBuf,
    pub tmp: PathBuf,
    pub shm: PathBuf,
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) struct LaunchSpec {
    pub argv: Vec<String>,
    pub env: Vec<(String, String)>,
    /// Dev: real path. Enforced: the path inside the mount namespace (`/mage/ws/...`).
    pub cwd: PathBuf,
    pub stdin_piped: bool,
    /// Enforced only.
    pub mounts: Option<MountSet>,
    /// Enforced only: leaf cgroup directory the process joins before exec.
    pub cgroup_leaf: Option<PathBuf>,
}

pub(crate) struct Launched {
    pub child: tokio::process::Child,
    pub pid: i32,
    /// Equal to `pid` (new session / process group per spawn).
    pub pgid: i32,
    pub start_time: u64,
}

#[derive(Debug, Clone)]
pub(crate) enum Launcher {
    Dev,
    #[cfg(target_os = "linux")]
    Enforced(EnforcedLauncher),
}

impl Launcher {
    pub(crate) fn launch(&self, spec: LaunchSpec) -> Result<Launched, ExecError> {
        let (program, args) = spec.argv.split_first().ok_or_else(|| ExecError::Invalid("empty argv".into()))?;
        if spec.argv.iter().any(|a| a.contains('\0')) {
            return Err(ExecError::Invalid("argv contains NUL".into()));
        }
        let mut cmd = tokio::process::Command::new(program);
        cmd.args(args)
            .env_clear()
            .envs(spec.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .stdin(if spec.stdin_piped { Stdio::piped() } else { Stdio::null() })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(false);
        match self {
            Launcher::Dev => {
                cmd.current_dir(&spec.cwd).process_group(0);
            }
            #[cfg(target_os = "linux")]
            Launcher::Enforced(enforced) => enforced.configure(&mut cmd, &spec)?,
        }
        let child = cmd.spawn().map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => ExecError::NotFound(format!("executable {program:?}")),
            std::io::ErrorKind::PermissionDenied => ExecError::Invalid(format!("executable {program:?}: permission denied")),
            _ => ExecError::Io(format!("spawn {program:?}: {e}")),
        })?;
        let pid = child.id().map(|p| p as i32).ok_or_else(|| ExecError::Io("spawned process has no pid".into()))?;
        let start_time = sys::process_start_time(pid).unwrap_or(0);
        Ok(Launched { child, pid, pgid: pid, start_time })
    }
}

/// Kills a process group: Linux enforced → `cgroup.kill` on the leaf (also reaches processes that
/// left the group with `setsid`), then `killpg(SIGKILL)` in every mode.
pub(crate) fn kill_group(pgid: i32, cgroup_leaf: Option<&std::path::Path>) {
    if let Some(leaf) = cgroup_leaf {
        let _ = std::fs::write(leaf.join("cgroup.kill"), b"1");
    }
    sys::signal_group(pgid, Signal::SIGKILL);
}

#[cfg(target_os = "linux")]
pub(crate) use enforced::EnforcedLauncher;

#[cfg(target_os = "linux")]
mod enforced {
    use std::ffi::CString;
    use std::os::fd::{AsRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    use std::path::{Path, PathBuf};

    use nix::libc;

    use super::LaunchSpec;
    use crate::ports::executor::ExecError;

    #[derive(Debug, Clone)]
    pub(crate) struct EnforcedLauncher {
        pub uid: u32,
        pub gid: u32,
        /// Hidden behind a tmpfs in every env process.
        pub data_dir: PathBuf,
        pub etc_mage: PathBuf,
    }

    fn cstr(p: &Path) -> Result<CString, ExecError> {
        CString::new(p.as_os_str().as_bytes()).map_err(|_| ExecError::Invalid(format!("path contains NUL: {}", p.display())))
    }

    struct Prepared {
        cgroup_procs: Option<OwnedFd>,
        binds: Vec<(CString, CString)>,
        hide: Vec<CString>,
        tmpfs: CString,
        tmpfs_opts: CString,
        root: CString,
        cwd: CString,
        uid: libc::uid_t,
        gid: libc::gid_t,
    }

    fn check(rc: libc::c_int) -> std::io::Result<()> {
        if rc == -1 { Err(std::io::Error::last_os_error()) } else { Ok(()) }
    }

    impl EnforcedLauncher {
        pub(super) fn configure(&self, cmd: &mut tokio::process::Command, spec: &LaunchSpec) -> Result<(), ExecError> {
            let mounts = spec.mounts.as_ref().ok_or_else(|| ExecError::Invalid("enforced launch without mounts".into()))?;
            let cgroup_procs = match &spec.cgroup_leaf {
                Some(leaf) => {
                    std::fs::create_dir_all(leaf).map_err(|e| ExecError::Unavailable(format!("cgroup leaf {}: {e}", leaf.display())))?;
                    let file = std::fs::OpenOptions::new()
                        .write(true)
                        .open(leaf.join("cgroup.procs"))
                        .map_err(|e| ExecError::Unavailable(format!("cgroup.procs {}: {e}", leaf.display())))?;
                    Some(OwnedFd::from(file))
                }
                None => None,
            };
            let ws = Path::new("/mage/ws");
            let home = Path::new("/mage/home");
            let scratch = Path::new("/mage/scratch");
            let binds = vec![
                (cstr(&mounts.tree)?, cstr(ws)?),
                (cstr(&mounts.home)?, cstr(home)?),
                (cstr(&mounts.scratch)?, cstr(scratch)?),
                (cstr(&mounts.tmp)?, cstr(Path::new("/tmp"))?),
                (cstr(&mounts.tmp)?, cstr(Path::new("/var/tmp"))?),
                (cstr(&mounts.shm)?, cstr(Path::new("/dev/shm"))?),
            ];
            let hide = vec![cstr(&self.data_dir)?, cstr(&self.etc_mage)?];
            let mut prepared = Prepared {
                cgroup_procs,
                binds,
                hide,
                tmpfs: CString::from(c"tmpfs"),
                tmpfs_opts: CString::from(c"size=64k,mode=000"),
                root: CString::from(c"/"),
                cwd: cstr(&spec.cwd)?,
                uid: self.uid,
                gid: self.gid,
            };
            // SAFETY: the closure only performs async-signal-safe raw syscalls on data prepared
            // above (no allocation, no locks), as `pre_exec` requires.
            unsafe {
                cmd.pre_exec(move || prepared.run());
            }
            Ok(())
        }
    }

    impl Prepared {
        /// executor.md `pre_exec` sequence, steps 1–8.
        fn run(&mut self) -> std::io::Result<()> {
            // SAFETY: raw syscalls with valid NUL-terminated strings and owned fds.
            unsafe {
                // 1. Join the leaf cgroup.
                if let Some(fd) = &self.cgroup_procs {
                    let zero = b"0";
                    if libc::write(fd.as_raw_fd(), zero.as_ptr().cast(), zero.len()) != 1 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                // 2. New session and process group.
                check(libc::setsid())?;
                // 3. Private mount namespace.
                check(libc::unshare(libc::CLONE_NEWNS))?;
                check(libc::mount(std::ptr::null(), self.root.as_ptr(), std::ptr::null(), libc::MS_REC | libc::MS_PRIVATE, std::ptr::null()))?;
                // 4. Bind mounts, then remount nosuid,nodev.
                for (src, dst) in &self.binds {
                    check(libc::mount(src.as_ptr(), dst.as_ptr(), std::ptr::null(), libc::MS_BIND | libc::MS_REC, std::ptr::null()))?;
                    check(libc::mount(
                        std::ptr::null(),
                        dst.as_ptr(),
                        std::ptr::null(),
                        libc::MS_BIND | libc::MS_REMOUNT | libc::MS_NOSUID | libc::MS_NODEV,
                        std::ptr::null(),
                    ))?;
                }
                // 5. Hide the data dir and /etc/mage (after the binds resolved their sources).
                for target in &self.hide {
                    let rc = libc::mount(
                        self.tmpfs.as_ptr(),
                        target.as_ptr(),
                        self.tmpfs.as_ptr(),
                        libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC,
                        self.tmpfs_opts.as_ptr().cast(),
                    );
                    // A missing /etc/mage is fine; anything else is fatal.
                    if rc == -1 && *libc::__errno_location() != libc::ENOENT {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                // 6. No new privileges, no ambient capabilities, rlimits.
                check(libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0))?;
                check(libc::prctl(libc::PR_CAP_AMBIENT, libc::PR_CAP_AMBIENT_CLEAR_ALL as libc::c_ulong, 0, 0, 0))?;
                let core = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
                check(libc::setrlimit(libc::RLIMIT_CORE, &core))?;
                let nofile = libc::rlimit { rlim_cur: 65536, rlim_max: 65536 };
                check(libc::setrlimit(libc::RLIMIT_NOFILE, &nofile))?;
                // 7. Drop to mage-agent.
                check(libc::setgroups(0, std::ptr::null()))?;
                check(libc::setresgid(self.gid, self.gid, self.gid))?;
                check(libc::setresuid(self.uid, self.uid, self.uid))?;
                // 8. cwd inside the namespace.
                check(libc::chdir(self.cwd.as_ptr()))?;
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn dev_launch_clears_env_and_uses_new_group() {
        let dir = tempfile::tempdir().unwrap();
        let launched = Launcher::Dev
            .launch(LaunchSpec {
                argv: vec!["/bin/sh".into(), "-c".into(), "echo \"$FOO|$HOME|$(pwd)\"; ps -o pgid= -p $$".into()],
                env: vec![("FOO".into(), "bar".into()), ("PATH".into(), "/usr/bin:/bin".into())],
                cwd: dir.path().to_path_buf(),
                stdin_piped: false,
                mounts: None,
                cgroup_leaf: None,
            })
            .unwrap();
        let out = launched.child.wait_with_output().await.unwrap();
        let text = String::from_utf8(out.stdout).unwrap();
        let mut lines = text.lines();
        let first = lines.next().unwrap();
        let real = dir.path().canonicalize().unwrap();
        assert_eq!(first, format!("bar||{}", real.display()));
        assert_eq!(lines.next().unwrap().trim().parse::<i32>().unwrap(), launched.pgid);
        assert_eq!(launched.pid, launched.pgid);
    }

    #[test]
    fn missing_executable_is_not_found() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let err = Launcher::Dev
                .launch(LaunchSpec {
                    argv: vec!["/definitely/not/here".into()],
                    env: vec![],
                    cwd: PathBuf::from("/"),
                    stdin_piped: false,
                    mounts: None,
                    cgroup_leaf: None,
                })
                .err()
                .unwrap();
            assert!(matches!(err, ExecError::NotFound(_)));
        });
    }
}
