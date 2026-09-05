//! Paths named by customers (manifest fields, archive entries, symlink targets) resolved inside a
//! source tree without ever leaving it.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

/// Symlinks followed while resolving one path before giving up (the Linux `MAXSYMLINKS`).
const MAX_LINKS: usize = 40;

#[derive(Debug, thiserror::Error)]
pub enum PathError {
    #[error("is absolute")]
    Absolute,
    #[error("climbs out with `..`")]
    ParentDir,
    #[error("resolves outside the source tree")]
    Escapes,
    #[error("follows more than {MAX_LINKS} links")]
    TooManyLinks,
    #[error("{0}")]
    Io(#[from] io::Error),
}

/// `path` as a plain relative path: no root, no `..`; `.` components are dropped, so `.` is empty.
pub fn plain_relative(path: &Path) -> Result<PathBuf, PathError> {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::Normal(name) => out.push(name),
            Component::CurDir => {}
            Component::ParentDir => return Err(PathError::ParentDir),
            Component::RootDir | Component::Prefix(_) => return Err(PathError::Absolute),
        }
    }
    Ok(out)
}

enum Part {
    Up,
    Name(OsString),
}

fn parts(path: &Path) -> Result<Vec<Part>, PathError> {
    let mut out = Vec::new();
    for c in path.components() {
        match c {
            Component::Normal(name) => out.push(Part::Name(name.to_owned())),
            Component::ParentDir => out.push(Part::Up),
            Component::CurDir => {}
            Component::RootDir | Component::Prefix(_) => return Err(PathError::Escapes),
        }
    }
    Ok(out)
}

/// Follows `rel` inside `root` the way the kernel would, symlinks included, and refuses the moment
/// any step (a `..`, an absolute link target) would leave `root`. Components that do not exist are
/// taken literally. Returns the resolved path relative to `root`, free of links.
pub fn resolve_in_root(root: &Path, rel: &Path) -> Result<PathBuf, PathError> {
    let mut queue: VecDeque<Part> = parts(rel)?.into();
    let mut at: Vec<OsString> = Vec::new();
    let mut links = 0;
    while let Some(part) = queue.pop_front() {
        let name = match part {
            Part::Up => {
                at.pop().ok_or(PathError::Escapes)?;
                continue;
            }
            Part::Name(name) => name,
        };
        at.push(name);
        let here: PathBuf = std::iter::once(root.as_os_str()).chain(at.iter().map(OsString::as_os_str)).collect();
        let meta = match fs::symlink_metadata(&here) {
            Ok(meta) => meta,
            Err(e) if matches!(e.kind(), io::ErrorKind::NotFound | io::ErrorKind::NotADirectory) => continue,
            Err(e) => return Err(e.into()),
        };
        if meta.file_type().is_symlink() {
            links += 1;
            if links > MAX_LINKS {
                return Err(PathError::TooManyLinks);
            }
            let target = fs::read_link(&here)?;
            at.pop();
            for p in parts(&target)?.into_iter().rev() {
                queue.push_front(p);
            }
        }
    }
    Ok(at.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use super::*;

    #[test]
    fn plain_relative_drops_dots_and_refuses_roots_and_parents() {
        assert_eq!(plain_relative(Path::new("./a/./b")).unwrap(), PathBuf::from("a/b"));
        assert_eq!(plain_relative(Path::new(".")).unwrap(), PathBuf::new());
        assert!(matches!(plain_relative(Path::new("/etc")), Err(PathError::Absolute)));
        assert!(matches!(plain_relative(Path::new("a/../../b")), Err(PathError::ParentDir)));
        assert!(matches!(plain_relative(Path::new("a/..")), Err(PathError::ParentDir)));
    }

    #[test]
    fn follows_links_inside_and_refuses_links_out() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("deep/deeper")).unwrap();
        symlink("deep/deeper", root.join("in")).unwrap();
        symlink("../..", root.join("deep/deeper/up")).unwrap();
        symlink("/etc", root.join("abs")).unwrap();
        symlink("..", root.join("out")).unwrap();

        assert_eq!(resolve_in_root(root, Path::new("in/file")).unwrap(), PathBuf::from("deep/deeper/file"));
        // `up` lands exactly on the root; one more `..` through it leaves.
        assert_eq!(resolve_in_root(root, Path::new("deep/deeper/up")).unwrap(), PathBuf::new());
        assert!(matches!(resolve_in_root(root, Path::new("deep/deeper/up/..")), Err(PathError::Escapes)));
        assert!(matches!(resolve_in_root(root, Path::new("abs/passwd")), Err(PathError::Escapes)));
        assert!(matches!(resolve_in_root(root, Path::new("out")), Err(PathError::Escapes)));
        // A missing component is literal, so `..` after it cancels it rather than following anything.
        assert_eq!(resolve_in_root(root, Path::new("missing/../deep")).unwrap(), PathBuf::from("deep"));
    }

    #[test]
    fn link_cycles_stop() {
        let dir = tempfile::tempdir().unwrap();
        symlink("b", dir.path().join("a")).unwrap();
        symlink("a", dir.path().join("b")).unwrap();
        assert!(matches!(resolve_in_root(dir.path(), Path::new("a")), Err(PathError::TooManyLinks)));
    }
}
