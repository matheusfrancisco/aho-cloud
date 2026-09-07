//! PID 1 duties: early mounts, the volume, `/etc/resolv.conf`, resolving the image's `User`,
//! and powering off. Syscalls are Linux-only; file-format helpers are portable (and tested).

use std::fmt::Write as _;
use std::net::Ipv4Addr;
use std::path::Path;

use super::GuestError;

fn io(what: impl Into<String>) -> impl FnOnce(std::io::Error) -> GuestError {
    let what = what.into();
    move |e| GuestError::Io(what, e)
}

/// Writes a resolv.conf naming `dns`.
pub fn write_resolv_conf(path: &Path, dns: &[Ipv4Addr]) -> Result<(), GuestError> {
    let mut text = String::new();
    for ip in dns {
        let _ = writeln!(text, "nameserver {ip}");
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(io(dir.display().to_string()))?;
    }
    std::fs::write(path, text).map_err(io(path.display().to_string()))
}

/// Resolves an OCI `User` (`name`, `uid`, `name:group`, `uid:gid`, …) against the image's
/// `/etc/passwd` and `/etc/group` under `root`. A bare uid not in passwd gets gid 0.
pub fn resolve_user(spec: &str, root: &Path) -> Result<(u32, u32), GuestError> {
    let read = |file: &str| std::fs::read_to_string(root.join(file)).unwrap_or_default();
    let (user, group) = match spec.split_once(':') {
        Some((u, g)) => (u, Some(g)),
        None => (spec, None),
    };
    let passwd = read("etc/passwd");
    // name:password:uid:gid:gecos:home:shell
    let entry = passwd
        .lines()
        .map(|l| l.split(':').collect::<Vec<_>>())
        .find(|f| f.len() >= 4 && (f[0] == user || user.parse::<u32>().is_ok_and(|uid| f[2].parse() == Ok(uid))));
    let uid = match user.parse::<u32>() {
        Ok(uid) => uid,
        Err(_) => entry.as_ref().and_then(|f| f[2].parse().ok()).ok_or_else(|| GuestError::User(spec.to_owned()))?,
    };
    let gid = match group {
        Some(g) => match g.parse::<u32>() {
            Ok(gid) => gid,
            Err(_) => read("etc/group")
                .lines()
                .map(|l| l.split(':').collect::<Vec<_>>())
                .find(|f| f.len() >= 3 && f[0] == g)
                .and_then(|f| f[2].parse().ok())
                .ok_or_else(|| GuestError::User(spec.to_owned()))?,
        },
        None => entry.and_then(|f| f[3].parse().ok()).unwrap_or(0),
    };
    Ok((uid, gid))
}

/// Mounts `/proc`, `/sys`, `/dev`, `/dev/pts`, `/dev/shm` and `/run`; already-mounted is fine.
#[cfg(target_os = "linux")]
pub fn mount_early() -> Result<(), GuestError> {
    use nix::mount::MsFlags as F;
    let table: [(&str, &str, &str, F, Option<&str>); 6] = [
        ("proc", "/proc", "proc", F::MS_NOSUID | F::MS_NODEV | F::MS_NOEXEC, None),
        ("sysfs", "/sys", "sysfs", F::MS_NOSUID | F::MS_NODEV | F::MS_NOEXEC, None),
        ("devtmpfs", "/dev", "devtmpfs", F::MS_NOSUID, Some("mode=0755")),
        ("devpts", "/dev/pts", "devpts", F::MS_NOSUID | F::MS_NOEXEC, Some("mode=0620,ptmxmode=0666")),
        ("tmpfs", "/dev/shm", "tmpfs", F::MS_NOSUID | F::MS_NODEV, Some("mode=1777")),
        ("tmpfs", "/run", "tmpfs", F::MS_NOSUID | F::MS_NODEV, Some("mode=0755")),
    ];
    for (source, target, fstype, flags, data) in table {
        mount_one(source, target, fstype, flags, data)?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn mount_one(source: &str, target: &str, fstype: &str, flags: nix::mount::MsFlags, data: Option<&str>) -> Result<(), GuestError> {
    std::fs::create_dir_all(target).map_err(io(target))?;
    match nix::mount::mount(Some(source), target, Some(fstype), flags, data) {
        Ok(()) | Err(nix::errno::Errno::EBUSY) => Ok(()),
        Err(e) => Err(GuestError::Io(format!("mount {fstype} on {target}"), e.into())),
    }
}

/// Mounts the ext4 volume disk where the process expects it.
#[cfg(target_os = "linux")]
pub fn mount_volume(volume: &super::VolumeMount) -> Result<(), GuestError> {
    mount_one(&volume.device, &volume.mount, "ext4", nix::mount::MsFlags::MS_NOSUID | nix::mount::MsFlags::MS_NODEV, None)
}

/// Ends the VM: with `reboot=k` Firecracker exits on the guest's reboot request.
#[cfg(target_os = "linux")]
pub fn power_off() -> ! {
    let _ = nix::sys::reboot::reboot(nix::sys::reboot::RebootMode::RB_AUTOBOOT);
    loop {
        std::thread::park();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("etc")).unwrap();
        std::fs::write(dir.path().join("etc/passwd"), "root:x:0:0:root:/root:/bin/sh\nnode:x:1000:1001::/home/node:/bin/sh\nbroken:x\n")
            .unwrap();
        std::fs::write(dir.path().join("etc/group"), "root:x:0:\nstaff:x:50:node\n").unwrap();
        dir
    }

    #[test]
    fn users_resolve_by_name_number_and_group() {
        let r = root();
        let p = r.path();
        assert_eq!(resolve_user("node", p).unwrap(), (1000, 1001));
        assert_eq!(resolve_user("1000", p).unwrap(), (1000, 1001));
        assert_eq!(resolve_user("4242", p).unwrap(), (4242, 0));
        assert_eq!(resolve_user("node:staff", p).unwrap(), (1000, 50));
        assert_eq!(resolve_user("65532:65532", p).unwrap(), (65532, 65532));
        assert!(matches!(resolve_user("ghost", p), Err(GuestError::User(_))));
        assert!(matches!(resolve_user("node:ghosts", p), Err(GuestError::User(_))));
    }

    #[test]
    fn resolv_conf_lists_every_resolver() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("etc/resolv.conf");
        write_resolv_conf(&path, &[Ipv4Addr::new(10, 77, 0, 1), Ipv4Addr::new(10, 77, 0, 9)]).unwrap();
        assert_eq!(std::fs::read_to_string(path).unwrap(), "nameserver 10.77.0.1\nnameserver 10.77.0.9\n");
    }
}
