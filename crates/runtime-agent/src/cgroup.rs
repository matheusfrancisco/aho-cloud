//! cgroup v2 ceilings: one child cgroup per replica under a configurable root, so a VM (the
//! Firecracker process and its vCPU threads) can never take more than its share of the host.

use std::io;
use std::path::{Path, PathBuf};

/// Controllers the agent enables for its children.
const CONTROLLERS: &str = "+cpu +memory +pids";
/// `cpu.max` period in microseconds (the kernel default).
pub const CPU_PERIOD_US: u64 = 100_000;

#[derive(Debug, thiserror::Error)]
pub enum CgroupError {
    #[error("invalid cgroup name `{0}`")]
    Name(String),
    #[error("cgroup {path}: {source}")]
    Io { path: PathBuf, source: io::Error },
}

fn io_err(path: &Path) -> impl FnOnce(io::Error) -> CgroupError + '_ {
    move |source| CgroupError::Io { path: path.to_owned(), source }
}

/// Resource ceilings for one replica.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// CPU time per [`CPU_PERIOD_US`], in microseconds.
    pub cpu_quota_us: u64,
    pub memory_max_bytes: u64,
    pub pids_max: u32,
}

impl Limits {
    /// Ceilings for a VM with `vcpus` full CPUs and `mem_mib` of guest memory, plus
    /// `overhead_mib` for the VMM itself.
    pub fn for_vm(vcpus: u8, mem_mib: u32, overhead_mib: u32, pids_max: u32) -> Self {
        Limits {
            cpu_quota_us: u64::from(vcpus) * CPU_PERIOD_US,
            memory_max_bytes: (u64::from(mem_mib) + u64::from(overhead_mib)) * 1024 * 1024,
            pids_max,
        }
    }
}

/// The agent's cgroup subtree.
#[derive(Clone, Debug)]
pub struct Cgroups {
    root: PathBuf,
}

/// One replica's cgroup.
#[derive(Clone, Debug)]
pub struct Cgroup {
    path: PathBuf,
}

impl Cgroups {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Cgroups { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Creates the root and delegates the cpu, memory and pids controllers to its children.
    pub fn init(&self) -> Result<(), CgroupError> {
        std::fs::create_dir_all(&self.root).map_err(io_err(&self.root))?;
        let control = self.root.join("cgroup.subtree_control");
        std::fs::write(&control, CONTROLLERS).map_err(io_err(&control))
    }

    fn child(&self, name: &str) -> Result<PathBuf, CgroupError> {
        let ok = !name.is_empty() && name.len() <= 64 && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
        if !ok {
            return Err(CgroupError::Name(name.to_owned()));
        }
        Ok(self.root.join(name))
    }

    /// Creates (or reuses) the child `name` and writes `cpu.max`, `memory.max` and `pids.max`.
    pub fn create(&self, name: &str, limits: &Limits) -> Result<Cgroup, CgroupError> {
        let path = self.child(name)?;
        match std::fs::create_dir(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(io_err(&path)(e)),
        }
        let cg = Cgroup { path };
        cg.write("cpu.max", &format!("{} {CPU_PERIOD_US}", limits.cpu_quota_us))?;
        cg.write("memory.max", &limits.memory_max_bytes.to_string())?;
        cg.write("pids.max", &limits.pids_max.to_string())?;
        Ok(cg)
    }

    /// Removes the child `name`; a missing cgroup is not an error. On cgroupfs the directory
    /// is removable once its processes exited; a plain-directory root is removed recursively.
    pub fn remove(&self, name: &str) -> Result<(), CgroupError> {
        let path = self.child(name)?;
        match std::fs::remove_dir(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::DirectoryNotEmpty => std::fs::remove_dir_all(&path).map_err(io_err(&path)),
            Err(e) => Err(io_err(&path)(e)),
        }
    }
}

impl Cgroup {
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn write(&self, file: &str, value: &str) -> Result<(), CgroupError> {
        let path = self.path.join(file);
        std::fs::write(&path, value).map_err(io_err(&path))
    }

    /// Moves `pid` (and, on cgroupfs, all its threads) into this cgroup.
    pub fn add_pid(&self, pid: u32) -> Result<(), CgroupError> {
        self.write("cgroup.procs", &pid.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_scale_with_the_vm_size() {
        let l = Limits::for_vm(2, 512, 64, 128);
        assert_eq!(l.cpu_quota_us, 200_000);
        assert_eq!(l.memory_max_bytes, 576 * 1024 * 1024);
        assert_eq!(l.pids_max, 128);
    }

    #[test]
    fn create_writes_ceilings_and_remove_cleans_up() {
        let dir = tempfile::tempdir().unwrap();
        let cgroups = Cgroups::new(dir.path().join("aho-cloud"));
        cgroups.init().unwrap();
        assert_eq!(std::fs::read_to_string(dir.path().join("aho-cloud/cgroup.subtree_control")).unwrap(), "+cpu +memory +pids");
        let cg = cgroups.create("vm-1a2b", &Limits::for_vm(1, 256, 32, 64)).unwrap();
        let read = |f: &str| std::fs::read_to_string(cg.path().join(f)).unwrap();
        assert_eq!(read("cpu.max"), "100000 100000");
        assert_eq!(read("memory.max"), (288u64 * 1024 * 1024).to_string());
        assert_eq!(read("pids.max"), "64");
        cg.add_pid(4242).unwrap();
        assert_eq!(read("cgroup.procs"), "4242");

        // Re-creating updates the ceilings in place.
        cgroups.create("vm-1a2b", &Limits::for_vm(3, 256, 32, 64)).unwrap();
        assert_eq!(read("cpu.max"), "300000 100000");

        cgroups.remove("vm-1a2b").unwrap();
        assert!(!cg.path().exists());
        cgroups.remove("vm-1a2b").unwrap();
    }

    #[test]
    fn names_cannot_escape_the_root() {
        let dir = tempfile::tempdir().unwrap();
        let cgroups = Cgroups::new(dir.path());
        let limits = Limits::for_vm(1, 128, 0, 16);
        for bad in ["", "../x", "a/b", "UPPER", ".."] {
            assert!(matches!(cgroups.create(bad, &limits), Err(CgroupError::Name(_))), "{bad}");
            assert!(matches!(cgroups.remove(bad), Err(CgroupError::Name(_))), "{bad}");
        }
    }
}
