//! PID 1 duties: early mounts, the volume, `/etc/resolv.conf`, resolving the image's `User`,
//! and powering off. Syscalls are Linux-only; file-format and file-tree helpers are portable
//! (and tested).
//!
//! Volumes behave like Docker named volumes: a fresh one (nothing but `lost+found`) is first
//! seeded with what the image has at the mount path ([`seed_volume`]: modes, ownership and
//! symlinks kept, symlinks never followed, special files skipped); a volume with data is never
//! touched. So an image may ship content under its volume path (a sandbox's `/workspace`, with
//! the user's home in it) and every new disk starts with it.

use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::net::Ipv4Addr;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

use super::GuestError;

/// What `mkfs.ext4` leaves on a new filesystem.
const LOST_FOUND: &str = "lost+found";
/// Where [`seed_volume`] builds the copy before moving it into place; a leftover one (an
/// interrupted seed) still counts as fresh and is redone.
const SEEDING: &str = ".cloud-guest-seeding";

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

/// Where the volume is mounted while it is checked and seeded (on the `/run` tmpfs).
#[cfg(target_os = "linux")]
const VOLUME_STAGING: &str = "/run/cloud-guest-volume";

/// Mounts the ext4 volume disk where the process expects it, seeding a fresh one with the
/// image's content at that path first ([`seed_volume`]).
#[cfg(target_os = "linux")]
pub fn mount_volume(volume: &super::VolumeMount) -> Result<(), GuestError> {
    let flags = nix::mount::MsFlags::MS_NOSUID | nix::mount::MsFlags::MS_NODEV;
    mount_one(&volume.device, VOLUME_STAGING, "ext4", flags, None)?;
    let seeded = seed_volume(Path::new(&volume.mount), Path::new(VOLUME_STAGING));
    let unmounted = nix::mount::umount(VOLUME_STAGING).map_err(|e| GuestError::Io(format!("unmount {VOLUME_STAGING}"), e.into()));
    if seeded? {
        tracing::info!(mount = %volume.mount, "fresh volume seeded from the image");
    }
    unmounted?;
    mount_one(&volume.device, &volume.mount, "ext4", flags, None)
}

/// Whether `dir` is a fresh filesystem: nothing in it but `lost+found` (and the leftovers of
/// an interrupted [`seed_volume`]).
pub fn is_fresh_volume(dir: &Path) -> Result<bool, GuestError> {
    for entry in fs::read_dir(dir).map_err(io(dir.display().to_string()))? {
        let entry = entry.map_err(io(dir.display().to_string()))?;
        if entry.file_name() != LOST_FOUND && entry.file_name() != SEEDING {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Docker named-volume semantics: when `volume` is fresh ([`is_fresh_volume`]), copies the tree
/// at `image_dir` into it (directories, regular files and symlinks with their modes and
/// ownership; symlinks are recreated, never followed; devices, FIFOs and sockets are skipped
/// with a log line) and gives the volume root `image_dir`'s mode and owner. A volume with data
/// is left alone. The copy is built aside and moved into place, so an interrupted seed is
/// redone on the next boot. Whether anything was copied.
pub fn seed_volume(image_dir: &Path, volume: &Path) -> Result<bool, GuestError> {
    if !is_fresh_volume(volume)? {
        return Ok(false);
    }
    let source = match fs::symlink_metadata(image_dir) {
        Ok(m) if m.is_dir() => m,
        // Nothing to copy (or a symlink, which is never followed).
        Ok(_) => return Ok(false),
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(io(image_dir.display().to_string())(e)),
    };
    let staging = volume.join(SEEDING);
    match fs::remove_dir_all(&staging) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(io(staging.display().to_string())(e)),
    }
    fs::create_dir(&staging).map_err(io(staging.display().to_string()))?;
    let mut copied = Vec::new();
    for entry in fs::read_dir(image_dir).map_err(io(image_dir.display().to_string()))? {
        let entry = entry.map_err(io(image_dir.display().to_string()))?;
        let name = entry.file_name();
        if name == LOST_FOUND || name == SEEDING {
            continue;
        }
        if let Some(meta) = copy_entry(&entry.path(), &staging.join(&name), false)? {
            copied.push((name, meta));
        }
    }
    for (name, meta) in &copied {
        let to = volume.join(name);
        fs::rename(staging.join(name), &to).map_err(io(to.display().to_string()))?;
        // Moving a directory may need write access to it, so its own mode comes last.
        if meta.is_dir() {
            set_owner_and_mode(&to, meta)?;
        }
    }
    fs::remove_dir(&staging).map_err(io(staging.display().to_string()))?;
    set_owner_and_mode(volume, &source)?;
    Ok(true)
}

/// Copies one entry of the image tree (recursively for directories); its metadata, `None` when
/// skipped. A directory's own owner and mode are left to the caller unless `settle`.
fn copy_entry(from: &Path, to: &Path, settle: bool) -> Result<Option<fs::Metadata>, GuestError> {
    let at = |p: &Path| io(p.display().to_string());
    let meta = fs::symlink_metadata(from).map_err(at(from))?;
    let kind = meta.file_type();
    if kind.is_symlink() {
        let target = fs::read_link(from).map_err(at(from))?;
        std::os::unix::fs::symlink(&target, to).map_err(at(to))?;
        copy_owner(to, &meta)?;
    } else if kind.is_dir() {
        // Written with a permissive mode, which is narrowed once its children are in.
        fs::create_dir(to).map_err(at(to))?;
        for entry in fs::read_dir(from).map_err(at(from))? {
            let entry = entry.map_err(at(from))?;
            copy_entry(&entry.path(), &to.join(entry.file_name()), true)?;
        }
        if settle {
            set_owner_and_mode(to, &meta)?;
        }
    } else if kind.is_file() {
        let nofollow = nix::fcntl::OFlag::O_NOFOLLOW.bits();
        let mut src = OpenOptions::new().read(true).custom_flags(nofollow).open(from).map_err(at(from))?;
        let mut dst = OpenOptions::new().write(true).create_new(true).mode(0o600).custom_flags(nofollow).open(to).map_err(at(to))?;
        io::copy(&mut src, &mut dst).map_err(at(to))?;
        drop::<File>(dst);
        set_owner_and_mode(to, &meta)?;
    } else {
        tracing::warn!(path = %from.display(), "volume seed: skipping a device, FIFO or socket");
        return Ok(None);
    }
    Ok(Some(meta))
}

/// Gives `path` the owner of `like` (when it differs), then its mode (after the owner, since a
/// change of owner clears setuid bits).
fn set_owner_and_mode(path: &Path, like: &fs::Metadata) -> Result<(), GuestError> {
    copy_owner(path, like)?;
    fs::set_permissions(path, fs::Permissions::from_mode(like.mode() & 0o7777)).map_err(io(path.display().to_string()))
}

/// `lchown` to `like`'s owner, skipped when it already matches (non-root callers may not
/// chown at all).
fn copy_owner(path: &Path, like: &fs::Metadata) -> Result<(), GuestError> {
    let now = fs::symlink_metadata(path).map_err(io(path.display().to_string()))?;
    if (now.uid(), now.gid()) != (like.uid(), like.gid()) {
        std::os::unix::fs::lchown(path, Some(like.uid()), Some(like.gid())).map_err(io(path.display().to_string()))?;
    }
    Ok(())
}

/// Hands a volume whose root is still owned by root (as `mkfs.ext4` leaves it, when the image
/// had nothing to seed it with) to the process's user, so a non-root process can write where
/// it expects its data. Only the root directory changes, and only while root owns it.
pub fn claim_volume(mount: &Path, (uid, gid): (u32, u32)) -> Result<(), GuestError> {
    let what = || mount.display().to_string();
    let owner = fs::metadata(mount).map_err(io(what()))?.uid();
    if uid != 0 && owner == 0 {
        std::os::unix::fs::chown(mount, Some(uid), Some(gid)).map_err(io(what()))?;
    }
    Ok(())
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

    fn mode(path: &Path) -> u32 {
        fs::symlink_metadata(path).unwrap().mode() & 0o7777
    }

    #[test]
    fn freshness_ignores_lost_and_found_only() {
        let dir = tempfile::tempdir().unwrap();
        assert!(is_fresh_volume(dir.path()).unwrap());
        fs::create_dir(dir.path().join(LOST_FOUND)).unwrap();
        assert!(is_fresh_volume(dir.path()).unwrap());
        fs::write(dir.path().join(".keep"), "").unwrap();
        assert!(!is_fresh_volume(dir.path()).unwrap());
    }

    #[test]
    fn a_fresh_volume_gets_the_image_tree_with_modes_and_symlinks_never_followed() {
        let image = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("host-secret"), "secret").unwrap();
        let src = image.path().join("workspace");
        fs::create_dir_all(src.join(".aho/home/.config")).unwrap();
        fs::write(src.join(".aho/home/.bashrc"), "PS1='$ '\n").unwrap();
        fs::set_permissions(src.join(".aho/home/.bashrc"), fs::Permissions::from_mode(0o640)).unwrap();
        fs::write(src.join("run.sh"), "#!/bin/sh\n").unwrap();
        fs::set_permissions(src.join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::set_permissions(src.join(".aho/home"), fs::Permissions::from_mode(0o750)).unwrap();
        std::os::unix::fs::symlink(".aho/home/.bashrc", src.join("rc")).unwrap();
        std::os::unix::fs::symlink(outside.path().join("host-secret"), src.join("leak")).unwrap();
        std::os::unix::fs::symlink(outside.path(), src.join("leak-dir")).unwrap();
        fs::create_dir(src.join("locked")).unwrap();
        fs::write(src.join("locked/inside"), "x").unwrap();
        fs::set_permissions(src.join("locked"), fs::Permissions::from_mode(0o555)).unwrap();
        nix::unistd::mkfifo(&src.join("pipe"), nix::sys::stat::Mode::from_bits_truncate(0o600)).unwrap();
        fs::set_permissions(&src, fs::Permissions::from_mode(0o770)).unwrap();

        let volume = tempfile::tempdir().unwrap();
        let vol = volume.path();
        fs::create_dir(vol.join(LOST_FOUND)).unwrap();
        assert!(seed_volume(&src, vol).unwrap());

        assert_eq!(fs::read_to_string(vol.join(".aho/home/.bashrc")).unwrap(), "PS1='$ '\n");
        assert!(vol.join(".aho/home/.config").is_dir());
        assert_eq!(mode(&vol.join(".aho/home/.bashrc")), 0o640);
        assert_eq!(mode(&vol.join("run.sh")), 0o755);
        assert_eq!(mode(&vol.join(".aho/home")), 0o750);
        assert_eq!(mode(&vol.join("locked")), 0o555, "narrowed after its children were written");
        assert_eq!(fs::read_to_string(vol.join("locked/inside")).unwrap(), "x");
        assert_eq!(mode(vol), 0o770, "the root takes the image directory's mode");
        assert_eq!(fs::metadata(vol).unwrap().uid(), fs::metadata(&src).unwrap().uid());
        for (link, target) in
            [("rc", Path::new(".aho/home/.bashrc")), ("leak", &outside.path().join("host-secret")), ("leak-dir", outside.path())]
        {
            assert!(fs::symlink_metadata(vol.join(link)).unwrap().file_type().is_symlink(), "{link} became a copy");
            assert_eq!(fs::read_link(vol.join(link)).unwrap(), target);
        }
        assert!(!vol.join("pipe").exists() && fs::symlink_metadata(vol.join("pipe")).is_err(), "special files are skipped");
        assert!(!vol.join(SEEDING).exists());
        assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 1, "nothing written through a symlink");

        // A second boot finds data and copies nothing.
        fs::write(src.join("new-in-image"), "").unwrap();
        assert!(!seed_volume(&src, vol).unwrap());
        assert!(!vol.join("new-in-image").exists());
        for locked in [src.join("locked"), vol.join("locked")] {
            fs::set_permissions(locked, fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    #[test]
    fn a_volume_with_data_is_never_touched_and_an_interrupted_seed_is_redone() {
        let image = tempfile::tempdir().unwrap();
        fs::write(image.path().join("seed"), "from the image").unwrap();

        let used = tempfile::tempdir().unwrap();
        fs::write(used.path().join("notes"), "mine").unwrap();
        let before = mode(used.path());
        assert!(!seed_volume(image.path(), used.path()).unwrap());
        assert!(!used.path().join("seed").exists());
        assert_eq!(mode(used.path()), before);
        assert_eq!(fs::read_to_string(used.path().join("notes")).unwrap(), "mine");

        let interrupted = tempfile::tempdir().unwrap();
        fs::create_dir(interrupted.path().join(SEEDING)).unwrap();
        fs::write(interrupted.path().join(SEEDING).join("seed"), "half").unwrap();
        assert!(seed_volume(image.path(), interrupted.path()).unwrap());
        assert_eq!(fs::read_to_string(interrupted.path().join("seed")).unwrap(), "from the image");
        assert!(!interrupted.path().join(SEEDING).exists());

        // No image content at the path (or only a symlink there): nothing to copy.
        let empty = tempfile::tempdir().unwrap();
        assert!(!seed_volume(&image.path().join("absent"), empty.path()).unwrap());
        std::os::unix::fs::symlink(image.path(), image.path().join("link")).unwrap();
        assert!(!seed_volume(&image.path().join("link"), empty.path()).unwrap());
        assert!(is_fresh_volume(empty.path()).unwrap());
    }
}
