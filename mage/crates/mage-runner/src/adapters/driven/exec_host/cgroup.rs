//! cgroup v2 handling for enforced mode (executor.md "v1 isolation rules", "Copy-on-write fork"
//! step 5, "Destroy"). The daemon's unit has `Delegate=yes`; the daemon moves itself into a
//! `daemon` leaf so its cgroup can delegate controllers to one child per env (`env-<slug>`) or
//! prepare (`prep-<base id>`), each with leaves per process (`harness-<n>`, `svc-<name>`, ...).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::config::CgroupConfig;
use crate::ports::executor::ExecError;

#[cfg(target_os = "linux")]
const CGROUP_FS: &str = "/sys/fs/cgroup";

fn unavailable(what: &str, path: &Path, e: std::io::Error) -> ExecError {
    ExecError::Unavailable(format!("cgroup {what} {}: {e}", path.display()))
}

#[cfg(target_os = "linux")]
/// The configured root, or the daemon's own cgroup from `/proc/self/cgroup` (`0::<path>`).
pub(crate) fn runner_root(configured: Option<&Path>) -> Result<PathBuf, ExecError> {
    if let Some(root) = configured {
        return Ok(root.to_path_buf());
    }
    let text = std::fs::read_to_string("/proc/self/cgroup").map_err(|e| unavailable("read", Path::new("/proc/self/cgroup"), e))?;
    let rel = text
        .lines()
        .find_map(|l| l.strip_prefix("0::"))
        .ok_or_else(|| ExecError::Unavailable("cgroup v2 unified hierarchy not found in /proc/self/cgroup".into()))?;
    let rel = rel.trim().trim_start_matches('/');
    // Already inside our own `daemon` leaf (restart within the same unit).
    let rel = rel.strip_suffix("/daemon").unwrap_or(rel);
    Ok(Path::new(CGROUP_FS).join(rel))
}

#[cfg(target_os = "linux")]
/// Moves the daemon into `<root>/daemon` and enables the memory, pids, and cpu controllers for
/// children of `root`.
pub(crate) fn setup_root(root: &Path) -> Result<(), ExecError> {
    let daemon = root.join("daemon");
    std::fs::create_dir_all(&daemon).map_err(|e| unavailable("create", &daemon, e))?;
    let procs = daemon.join("cgroup.procs");
    std::fs::write(&procs, std::process::id().to_string()).map_err(|e| unavailable("join", &procs, e))?;
    let control = root.join("cgroup.subtree_control");
    std::fs::write(&control, "+memory +pids +cpu").map_err(|e| unavailable("enable controllers", &control, e))?;
    Ok(())
}

/// Creates `<root>/<name>` with the configured limits.
pub(crate) fn create(root: &Path, name: &str, limits: &CgroupConfig) -> Result<PathBuf, ExecError> {
    let dir = root.join(name);
    std::fs::create_dir_all(&dir).map_err(|e| unavailable("create", &dir, e))?;
    for (file, value) in [
        ("memory.max", limits.memory_max.clone()),
        ("pids.max", limits.pids_max.to_string()),
        ("cpu.weight", limits.cpu_weight.to_string()),
    ] {
        let path = dir.join(file);
        std::fs::write(&path, value).map_err(|e| unavailable("limit", &path, e))?;
    }
    Ok(dir)
}

/// `cgroup.kill`, then removes leaves and the cgroup itself (waiting briefly for the kill).
pub(crate) fn destroy(dir: &Path) -> Result<(), ExecError> {
    if !dir.exists() {
        return Ok(());
    }
    let _ = std::fs::write(dir.join("cgroup.kill"), b"1");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let mut leaves: Vec<PathBuf> = std::fs::read_dir(dir)
            .map(|rd| rd.flatten().filter(|e| e.file_type().is_ok_and(|t| t.is_dir())).map(|e| e.path()).collect())
            .unwrap_or_default();
        leaves.sort();
        for leaf in &leaves {
            let _ = std::fs::write(leaf.join("cgroup.kill"), b"1");
            let _ = std::fs::remove_dir(leaf);
        }
        match std::fs::remove_dir(dir) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) if Instant::now() >= deadline => return Err(unavailable("remove", dir, e)),
            Err(_) => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}

/// Env and prepare cgroups under the root (orphan scan).
pub(crate) fn mage_children(root: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(root)
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                .filter(|e| e.file_name().to_str().is_some_and(|n| n.starts_with("env-") || n.starts_with("prep-")))
                .map(|e| e.path())
                .collect()
        })
        .unwrap_or_default()
}
