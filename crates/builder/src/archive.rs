//! Extracting an uploaded source archive (`tar.gz`) so that nothing lands, or later points,
//! outside the source root.
//!
//! Rules: entry paths must be relative and free of `..`; no entry is written through a symlink
//! (so every entry lands where its name says); hard links must name a regular file already
//! extracted; after extraction every symlink must resolve inside the root, following chains of
//! links the way the kernel would. Device nodes and FIFOs are refused; the pax global header
//! `git archive` writes is skipped.

use std::fs::{self, OpenOptions, Permissions};
use std::io::{self, Read};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use flate2::read::GzDecoder;
use tar::EntryType;

use crate::paths::{self, PathError};

#[derive(Debug, thiserror::Error)]
pub enum ArchiveError {
    #[error("entry `{0}` has an absolute path")]
    Absolute(String),
    #[error("entry `{0}` climbs out of the source root with `..`")]
    Traversal(String),
    #[error("link `{0}` points outside the source root")]
    LinkEscape(String),
    #[error("link `{0}` goes through too many links")]
    LinkLoop(String),
    #[error("entry `{0}` would be written through a symlink")]
    ThroughLink(String),
    #[error("hard link `{path}` must name a regular file earlier in the archive, not `{target}`")]
    HardLink { path: String, target: String },
    #[error("entry `{0}` would replace a directory")]
    ReplacesDirectory(String),
    #[error("entry `{path}` has unsupported type {kind}")]
    Unsupported { path: String, kind: String },
    #[error("{0}")]
    Io(#[from] io::Error),
}

/// Extracts the gzip-compressed tar stream `reader` into `root` (created if missing).
pub fn extract_tar_gz(reader: impl Read, root: &Path) -> Result<(), ArchiveError> {
    fs::create_dir_all(root)?;
    let mut archive = tar::Archive::new(GzDecoder::new(reader));
    let mut symlinks = Vec::new();
    for entry in archive.entries()? {
        let mut entry = entry?;
        let kind = entry.header().entry_type();
        if kind.is_pax_global_extensions() {
            continue;
        }
        let raw = entry.path()?.into_owned();
        let shown = raw.display().to_string();
        let rel = paths::plain_relative(&raw).map_err(|e| match e {
            PathError::Absolute => ArchiveError::Absolute(shown.clone()),
            _ => ArchiveError::Traversal(shown.clone()),
        })?;
        if rel.as_os_str().is_empty() {
            continue;
        }
        let dest = root.join(&rel);
        match kind {
            EntryType::Directory => {
                no_links_along(root, &rel, &shown)?;
                fs::create_dir_all(&dest)?;
            }
            EntryType::Regular | EntryType::Continuous | EntryType::GNUSparse => {
                make_slot(root, &rel, &shown)?;
                let mode = entry.header().mode()? & 0o777;
                let mut file = OpenOptions::new().write(true).create_new(true).open(&dest)?;
                io::copy(&mut entry, &mut file)?;
                file.set_permissions(Permissions::from_mode(mode))?;
            }
            EntryType::Symlink => {
                let target = link_name(&entry, &shown)?;
                make_slot(root, &rel, &shown)?;
                symlink(&target, &dest)?;
                symlinks.push((rel, shown));
            }
            EntryType::Link => {
                let target = link_name(&entry, &shown)?;
                let refused = || ArchiveError::HardLink { path: shown.clone(), target: target.display().to_string() };
                let source = paths::plain_relative(&target).map_err(|_| refused())?;
                // The source itself is checked below: a symlink is not a regular file.
                no_links_along(root, source.parent().unwrap_or(Path::new("")), &shown)?;
                let source = root.join(source);
                if !fs::symlink_metadata(&source).is_ok_and(|m| m.is_file()) {
                    return Err(refused());
                }
                make_slot(root, &rel, &shown)?;
                fs::hard_link(&source, &dest)?;
            }
            other => return Err(ArchiveError::Unsupported { path: shown, kind: format!("{other:?}") }),
        }
    }
    for (rel, shown) in symlinks {
        match paths::resolve_in_root(root, &rel) {
            Ok(_) => {}
            Err(PathError::Io(e)) => return Err(e.into()),
            Err(PathError::TooManyLinks) => return Err(ArchiveError::LinkLoop(shown)),
            Err(_) => return Err(ArchiveError::LinkEscape(shown)),
        }
    }
    Ok(())
}

fn link_name<R: Read>(entry: &tar::Entry<'_, R>, shown: &str) -> Result<PathBuf, ArchiveError> {
    entry
        .link_name()?
        .map(|p| p.into_owned())
        .ok_or_else(|| ArchiveError::Unsupported { path: shown.to_owned(), kind: "link without a target".into() })
}

/// Refuses when any existing component of `rel` under `root` is a symlink.
fn no_links_along(root: &Path, rel: &Path, shown: &str) -> Result<(), ArchiveError> {
    let mut here = root.to_path_buf();
    for c in rel.components() {
        here.push(c);
        match fs::symlink_metadata(&here) {
            Ok(m) if m.file_type().is_symlink() => return Err(ArchiveError::ThroughLink(shown.to_owned())),
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

/// Readies `root/rel` for a non-directory entry: real parent directories, and nothing in the way
/// except an earlier non-directory entry of the same name (later entries win, as with `tar`).
fn make_slot(root: &Path, rel: &Path, shown: &str) -> Result<(), ArchiveError> {
    let parent = rel.parent().unwrap_or(Path::new(""));
    no_links_along(root, parent, shown)?;
    fs::create_dir_all(root.join(parent))?;
    let dest = root.join(rel);
    match fs::symlink_metadata(&dest) {
        Ok(m) if m.is_dir() => Err(ArchiveError::ReplacesDirectory(shown.to_owned())),
        Ok(_) => Ok(fs::remove_file(&dest)?),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::MetadataExt;

    use flate2::Compression;
    use flate2::write::GzEncoder;

    use super::*;

    /// One raw tar entry; names are written byte-for-byte so hostile paths survive.
    struct E<'a> {
        kind: EntryType,
        name: &'a str,
        link: &'a str,
        data: &'a [u8],
        mode: u32,
    }

    fn file<'a>(name: &'a str, data: &'a [u8]) -> E<'a> {
        E { kind: EntryType::Regular, name, link: "", data, mode: 0o644 }
    }
    fn dir(name: &str) -> E<'_> {
        E { kind: EntryType::Directory, name, link: "", data: b"", mode: 0o755 }
    }
    fn symlink_entry<'a>(name: &'a str, link: &'a str) -> E<'a> {
        E { kind: EntryType::Symlink, name, link, data: b"", mode: 0o777 }
    }
    fn hardlink<'a>(name: &'a str, link: &'a str) -> E<'a> {
        E { kind: EntryType::Link, name, link, data: b"", mode: 0o644 }
    }

    fn tgz(entries: &[E<'_>]) -> Vec<u8> {
        let mut b = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::fast()));
        for e in entries {
            let mut h = tar::Header::new_ustar();
            h.as_old_mut().name[..e.name.len()].copy_from_slice(e.name.as_bytes());
            h.as_old_mut().linkname[..e.link.len()].copy_from_slice(e.link.as_bytes());
            h.set_entry_type(e.kind);
            h.set_size(e.data.len() as u64);
            h.set_mode(e.mode);
            h.set_cksum();
            b.append(&h, e.data).unwrap();
        }
        b.into_inner().unwrap().finish().unwrap()
    }

    /// `root` is `<tmp>/root`, so a successful escape would show up in `<tmp>`.
    fn extract(entries: &[E<'_>]) -> (tempfile::TempDir, Result<(), ArchiveError>) {
        let tmp = tempfile::tempdir().unwrap();
        let r = extract_tar_gz(&tgz(entries)[..], &tmp.path().join("root"));
        (tmp, r)
    }

    fn outside(tmp: &tempfile::TempDir) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(tmp.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n != "root")
            .collect();
        names.sort();
        names
    }

    #[test]
    fn extracts_files_dirs_modes_and_links_that_stay_inside() {
        let global = b"52 comment=0123456789abcdef0123456789abcdef01234567\n";
        let (tmp, r) = extract(&[
            E { kind: EntryType::XGlobalHeader, name: "pax_global_header", link: "", data: global, mode: 0o666 },
            dir("./"),
            dir("app/"),
            E { kind: EntryType::Regular, name: "app/run.sh", link: "", data: b"#!/bin/sh\n", mode: 0o755 },
            file("app/data/config.json", b"{}"),
            symlink_entry("app/current", "data"),
            symlink_entry("app/data/up", "../.."),
            hardlink("app/run2.sh", "app/run.sh"),
            file("app/data/config.json", b"{\"v\":2}"),
        ]);
        r.unwrap();
        let root = tmp.path().join("root");
        assert!(!root.join("pax_global_header").exists());
        assert_eq!(fs::metadata(root.join("app/run.sh")).unwrap().mode() & 0o777, 0o755);
        assert_eq!(fs::read(root.join("app/current/config.json")).unwrap(), b"{\"v\":2}");
        assert_eq!(fs::read_link(root.join("app/current")).unwrap(), PathBuf::from("data"));
        assert_eq!(fs::metadata(root.join("app/run2.sh")).unwrap().ino(), fs::metadata(root.join("app/run.sh")).unwrap().ino());
        assert!(outside(&tmp).is_empty());
    }

    #[test]
    fn refuses_absolute_and_parent_paths() {
        let (tmp, r) = extract(&[file("/tmp-escape", b"x")]);
        assert!(matches!(r, Err(ArchiveError::Absolute(p)) if p == "/tmp-escape"));
        assert!(outside(&tmp).is_empty());

        for name in ["../evil", "a/../../evil", "ok/.."] {
            let (tmp, r) = extract(&[file(name, b"x")]);
            assert!(matches!(&r, Err(ArchiveError::Traversal(p)) if p == name), "{name}: {r:?}");
            assert!(outside(&tmp).is_empty(), "{name}");
        }
    }

    #[test]
    fn refuses_symlinks_that_escape_directly_or_through_other_links() {
        for entries in [
            vec![symlink_entry("etc", "/etc")],
            vec![symlink_entry("up", "..")],
            vec![dir("a/"), symlink_entry("a/up", "../../x")],
            // Each link alone looks inside; followed together the second leaves the root.
            vec![dir("d/e/"), symlink_entry("d/e/top", "../.."), symlink_entry("t", "d/e/top/..")],
            // Dangling, but it would name `<tmp>/x` once something created it.
            vec![symlink_entry("t", "missing/../../x")],
        ] {
            let (tmp, r) = extract(&entries);
            assert!(matches!(r, Err(ArchiveError::LinkEscape(_))), "{r:?}");
            assert!(outside(&tmp).is_empty());
        }
    }

    #[test]
    fn refuses_writing_through_a_symlink_even_one_pointing_inside() {
        let (tmp, r) = extract(&[dir("real/"), symlink_entry("alias", "real"), file("alias/f", b"x")]);
        assert!(matches!(r, Err(ArchiveError::ThroughLink(p)) if p == "alias/f"));
        assert!(!tmp.path().join("root/real/f").exists());

        // The classic: plant `l -> ..`, then write `l/pwned`. Refused at the write, before the
        // post-pass would have caught the link.
        let (tmp, r) = extract(&[symlink_entry("l", ".."), file("l/pwned", b"x")]);
        assert!(matches!(r, Err(ArchiveError::ThroughLink(_))), "{r:?}");
        assert!(outside(&tmp).is_empty());
    }

    #[test]
    fn refuses_hard_links_out_of_the_root_or_to_links() {
        for entries in [
            vec![hardlink("h", "../secret")],
            vec![hardlink("h", "/etc/passwd")],
            vec![hardlink("h", "missing")],
            vec![symlink_entry("s", "f"), file("f", b"x"), hardlink("h", "s")],
        ] {
            let (tmp, r) = extract(&entries);
            assert!(matches!(r, Err(ArchiveError::HardLink { .. })), "{r:?}");
            assert!(!tmp.path().join("root/h").exists());
        }
    }

    #[test]
    fn refuses_devices_and_fifos_and_files_over_directories() {
        for kind in [EntryType::Char, EntryType::Block, EntryType::Fifo] {
            let (_tmp, r) = extract(&[E { kind, name: "dev", link: "", data: b"", mode: 0o600 }]);
            assert!(matches!(r, Err(ArchiveError::Unsupported { .. })), "{kind:?}: {r:?}");
        }
        let (_tmp, r) = extract(&[dir("d/"), file("d", b"x")]);
        assert!(matches!(r, Err(ArchiveError::ReplacesDirectory(_))), "{r:?}");
    }

    #[test]
    fn corrupt_input_is_an_error_not_a_panic() {
        let tmp = tempfile::tempdir().unwrap();
        let r = extract_tar_gz(&b"not gzip at all"[..], tmp.path());
        assert!(matches!(r, Err(ArchiveError::Io(_))), "{r:?}");
    }
}
