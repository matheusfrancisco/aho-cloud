//! Shared tree walker for `archive` and `materialize` (knowledge.md "SourceBundle" and "Adapter
//! internals"): a [`TreePlan`] turns raw tree entries into the sorted item list (parent
//! directories included, traversal rejected), [`BundleWriter`] writes the deterministic tar.zst,
//! [`Checkout`] writes a read-only checkout. Pure (no git); `MemGitRepo` reuses it so the fake
//! produces byte-identical bundles.
//!
//! Tar layout: ustar headers; a path longer than 100 bytes (or a symlink target longer than 100
//! bytes) is carried by a pax extended header (`x`, records `path=` / `linkpath=`) named
//! `././@PaxHeader` placed right before its entry. No GNU long-name entries.

use std::collections::BTreeSet;
use std::fs;
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use mage_core::domain::{BlobOid, CommitSha};
use sha2::{Digest, Sha256};

use crate::ports::git::GitError;

/// zstd level of source bundles.
pub const BUNDLE_ZSTD_LEVEL: i32 = 3;

const BLOCK: usize = 512;
const NAME_LEN: usize = 100;

/// One entry of a git tree as stored (raw path bytes, before validation).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawTreeEntry {
    pub path: Vec<u8>,
    pub kind: RawEntryKind,
    /// Blob size; `None` for gitlinks.
    pub size: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RawEntryKind {
    File { oid: BlobOid, executable: bool },
    Symlink { oid: BlobOid },
    Submodule { commit: CommitSha },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanItemKind {
    /// Parent directory or submodule (empty directory).
    Dir,
    File { oid: BlobOid, executable: bool },
    /// Target is the blob content.
    Symlink { oid: BlobOid },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanItem {
    /// Repo-relative path bytes, no trailing slash.
    pub path: Vec<u8>,
    pub kind: PlanItemKind,
}

impl PlanItem {
    /// Blob holding the content (files and symlinks).
    pub fn blob(&self) -> Option<BlobOid> {
        match self.kind {
            PlanItemKind::Dir => None,
            PlanItemKind::File { oid, .. } | PlanItemKind::Symlink { oid } => Some(oid),
        }
    }
}

/// Items of one tree sorted by path bytes; every parent directory precedes its children.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreePlan {
    pub items: Vec<PlanItem>,
}

impl TreePlan {
    /// Validates every path (no empty, `.`, `..` or `.git` component, not absolute, no NUL) and
    /// adds the parent directories. Submodules become empty directories.
    pub fn new(entries: &[RawTreeEntry]) -> Result<TreePlan, GitError> {
        let mut dirs: BTreeSet<Vec<u8>> = BTreeSet::new();
        let mut items = Vec::with_capacity(entries.len());
        for entry in entries {
            check_path(&entry.path)?;
            for (i, b) in entry.path.iter().enumerate() {
                if *b == b'/' {
                    dirs.insert(entry.path[..i].to_vec());
                }
            }
            let kind = match entry.kind {
                RawEntryKind::File { oid, executable } => PlanItemKind::File { oid, executable },
                RawEntryKind::Symlink { oid } => PlanItemKind::Symlink { oid },
                RawEntryKind::Submodule { .. } => PlanItemKind::Dir,
            };
            items.push(PlanItem { path: entry.path.clone(), kind });
        }
        let leaves: BTreeSet<&[u8]> = items.iter().map(|i| i.path.as_slice()).collect();
        if leaves.len() != items.len() {
            return Err(GitError::Invalid("tree lists a path twice".into()));
        }
        for dir in &dirs {
            if leaves.contains(dir.as_slice()) {
                return Err(GitError::Invalid(format!("tree path {} is both a directory and an entry", String::from_utf8_lossy(dir))));
            }
        }
        items.extend(dirs.into_iter().map(|path| PlanItem { path, kind: PlanItemKind::Dir }));
        items.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(TreePlan { items })
    }
}

fn check_path(path: &[u8]) -> Result<(), GitError> {
    let bad = |why: &str| Err(GitError::Invalid(format!("tree path {:?} {why}", String::from_utf8_lossy(path))));
    if path.is_empty() {
        return bad("is empty");
    }
    if path[0] == b'/' {
        return bad("is absolute");
    }
    if path.contains(&0) {
        return bad("contains NUL");
    }
    for component in path.split(|b| *b == b'/') {
        if component.is_empty() || component == b"." || component == b".." {
            return bad("has an empty, `.` or `..` component");
        }
        if component.eq_ignore_ascii_case(b".git") {
            return bad("has a `.git` component");
        }
    }
    Ok(())
}

/// Counts and hashes the bytes written through it.
struct HashingWriter<W: Write> {
    inner: W,
    hasher: Sha256,
    bytes: u64,
}

impl<W: Write> Write for HashingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hasher.update(&buf[..n]);
        self.bytes += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Result of [`BundleWriter::finish`].
#[derive(Debug)]
pub struct BundleSummary<W> {
    pub out: W,
    /// Compressed size.
    pub bytes: u64,
    /// SHA-256 of the compressed bytes.
    pub sha256: [u8; 32],
    /// Tar entries (pax headers not counted).
    pub entries: u32,
}

/// Deterministic tar.zst writer. Feed [`TreePlan::items`] in order with their blob content
/// (empty for directories).
pub struct BundleWriter<W: Write> {
    zstd: zstd::stream::write::Encoder<'static, HashingWriter<W>>,
    mtime: u64,
    entries: u32,
}

impl<W: Write> BundleWriter<W> {
    /// `mtime`: commit `committer_time` (unix seconds, negative clamped to 0).
    pub fn new(out: W, mtime: i64) -> io::Result<Self> {
        let hashing = HashingWriter { inner: out, hasher: Sha256::new(), bytes: 0 };
        let zstd = zstd::stream::write::Encoder::new(hashing, BUNDLE_ZSTD_LEVEL)?;
        Ok(Self { zstd, mtime: u64::try_from(mtime).unwrap_or(0), entries: 0 })
    }

    pub fn append(&mut self, item: &PlanItem, content: &[u8]) -> io::Result<()> {
        let (entry_type, mode, link, data): (tar::EntryType, u32, &[u8], &[u8]) = match item.kind {
            PlanItemKind::Dir => (tar::EntryType::Directory, 0o755, b"", b""),
            PlanItemKind::File { executable, .. } => (tar::EntryType::Regular, if executable { 0o755 } else { 0o644 }, b"", content),
            PlanItemKind::Symlink { .. } => (tar::EntryType::Symlink, 0o777, content, b""),
        };
        let mut name = item.path.clone();
        if entry_type == tar::EntryType::Directory {
            name.push(b'/');
        }
        let mut pax = Vec::new();
        if name.len() > NAME_LEN {
            pax_record(&mut pax, "path", &name);
        }
        if link.len() > NAME_LEN {
            pax_record(&mut pax, "linkpath", link);
        }
        if !pax.is_empty() {
            let mut header = self.header(tar::EntryType::XHeader, 0o644, pax.len() as u64)?;
            set_field(&mut header, b"././@PaxHeader", false)?;
            header.set_cksum();
            self.write_entry(&header, &pax)?;
        }
        let mut header = self.header(entry_type, mode, data.len() as u64)?;
        set_field(&mut header, &name[..name.len().min(NAME_LEN)], false)?;
        set_field(&mut header, &link[..link.len().min(NAME_LEN)], true)?;
        header.set_cksum();
        self.write_entry(&header, data)?;
        self.entries += 1;
        Ok(())
    }

    fn header(&self, entry_type: tar::EntryType, mode: u32, size: u64) -> io::Result<tar::Header> {
        let mut header = tar::Header::new_ustar();
        header.set_entry_type(entry_type);
        header.set_mode(mode);
        header.set_uid(0);
        header.set_gid(0);
        header.set_size(size);
        header.set_mtime(self.mtime);
        Ok(header)
    }

    fn write_entry(&mut self, header: &tar::Header, data: &[u8]) -> io::Result<()> {
        self.zstd.write_all(header.as_bytes())?;
        self.zstd.write_all(data)?;
        let pad = (BLOCK - data.len() % BLOCK) % BLOCK;
        self.zstd.write_all(&[0u8; BLOCK][..pad])
    }

    /// Writes the end-of-archive blocks and the zstd frame end.
    pub fn finish(mut self) -> io::Result<BundleSummary<W>> {
        self.zstd.write_all(&[0u8; 2 * BLOCK])?;
        let mut hashing = self.zstd.finish()?;
        hashing.flush()?;
        Ok(BundleSummary { bytes: hashing.bytes, sha256: hashing.hasher.finalize().into(), entries: self.entries, out: hashing.inner })
    }
}

fn set_field(header: &mut tar::Header, value: &[u8], link: bool) -> io::Result<()> {
    let ustar = header.as_ustar_mut().ok_or_else(|| io::Error::other("not a ustar header"))?;
    let field: &mut [u8; NAME_LEN] = if link { &mut ustar.linkname } else { &mut ustar.name };
    field.fill(0);
    field[..value.len()].copy_from_slice(value);
    Ok(())
}

/// Appends one pax record `"<len> <key>=<value>\n"` (`len` counts the whole record).
fn pax_record(out: &mut Vec<u8>, key: &str, value: &[u8]) {
    let rest = key.len() + value.len() + 3; // ' ', '=', '\n'
    let mut len = rest + 1;
    while len != rest + len.to_string().len() {
        len = rest + len.to_string().len();
    }
    out.extend_from_slice(len.to_string().as_bytes());
    out.push(b' ');
    out.extend_from_slice(key.as_bytes());
    out.push(b'=');
    out.extend_from_slice(value);
    out.push(b'\n');
}

/// Writes a [`TreePlan`] as plain files: files `0444` (`0555` when executable), directories
/// `0755`, symlinks created as symlinks and never followed, submodules as empty directories.
/// Paths that are not UTF-8 are skipped: checkouts serve `TextSearch`, which only knows
/// `RepoPath`s, and some filesystems (APFS) reject such names.
pub struct Checkout {
    root: PathBuf,
}

impl Checkout {
    /// Creates `dest` (and its parents); an existing non-empty `dest` is a `Conflict`.
    pub fn create(dest: &Path) -> Result<Checkout, GitError> {
        let io_err = |e: io::Error| GitError::Unavailable(format!("checkout {}: {e}", dest.display()));
        match fs::read_dir(dest) {
            Ok(mut entries) => {
                if entries.next().is_some() {
                    return Err(GitError::Conflict);
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => fs::create_dir_all(dest).map_err(io_err)?,
            Err(e) => return Err(io_err(e)),
        }
        Ok(Checkout { root: dest.to_path_buf() })
    }

    /// Items must come in [`TreePlan`] order (parents first). Nothing existing is overwritten.
    pub fn write(&self, item: &PlanItem, content: &[u8]) -> io::Result<()> {
        let Ok(rel) = std::str::from_utf8(&item.path) else { return Ok(()) };
        let path = self.root.join(rel);
        match item.kind {
            PlanItemKind::Dir => {
                fs::create_dir(&path)?;
                fs::set_permissions(&path, fs::Permissions::from_mode(0o755))
            }
            PlanItemKind::File { executable, .. } => {
                let mode = if executable { 0o555 } else { 0o444 };
                let mut file = fs::OpenOptions::new().write(true).create_new(true).mode(mode).open(&path)?;
                file.write_all(content)?;
                // The creation mode is filtered by the umask; set it exactly.
                file.set_permissions(fs::Permissions::from_mode(mode))
            }
            PlanItemKind::Symlink { .. } => std::os::unix::fs::symlink(std::ffi::OsStr::from_bytes(content), &path),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use super::*;

    fn file(path: &str, byte: u8) -> RawTreeEntry {
        RawTreeEntry { path: path.as_bytes().to_vec(), kind: RawEntryKind::File { oid: BlobOid([byte; 20]), executable: false }, size: Some(1) }
    }

    #[test]
    fn plan_sorts_by_bytes_and_adds_parent_dirs_first() {
        let plan = TreePlan::new(&[file("a/b/c", 1), file("a/b-c", 2), file("z", 3)]).unwrap();
        let paths: Vec<_> = plan.items.iter().map(|i| String::from_utf8(i.path.clone()).unwrap()).collect();
        assert_eq!(paths, ["a", "a/b", "a/b-c", "a/b/c", "z"]);
        assert_eq!(plan.items[1].kind, PlanItemKind::Dir);
    }

    #[test]
    fn plan_rejects_traversal_and_dot_git() {
        for bad in ["../x", "a/../b", "/etc/passwd", "a/.git/config", ".GIT/hooks", "a//b", "./a"] {
            assert!(matches!(TreePlan::new(&[file(bad, 1)]), Err(GitError::Invalid(_))), "{bad}");
        }
        assert!(TreePlan::new(&[file(".gitignore", 1), file("a/.github/x", 2)]).is_ok());
    }

    #[test]
    fn pax_record_length_counts_itself() {
        let mut out = Vec::new();
        pax_record(&mut out, "path", &[b'x'; 95]);
        let text = String::from_utf8(out.clone()).unwrap();
        let (len, _) = text.split_once(' ').unwrap();
        assert_eq!(len.parse::<usize>().unwrap(), out.len());
    }

    #[test]
    fn long_paths_and_links_round_trip_through_pax() {
        let long = format!("{}/file.txt", "d".repeat(150));
        let target = "t".repeat(130);
        let plan = TreePlan::new(&[
            RawTreeEntry { path: long.as_bytes().to_vec(), kind: RawEntryKind::File { oid: BlobOid([1; 20]), executable: true }, size: Some(2) },
            RawTreeEntry { path: b"link".to_vec(), kind: RawEntryKind::Symlink { oid: BlobOid([2; 20]) }, size: Some(130) },
        ])
        .unwrap();
        let mut writer = BundleWriter::new(Vec::new(), 1_700_000_000).unwrap();
        for item in &plan.items {
            let content: &[u8] = match item.kind {
                PlanItemKind::Dir => b"",
                PlanItemKind::File { .. } => b"hi",
                PlanItemKind::Symlink { .. } => target.as_bytes(),
            };
            writer.append(item, content).unwrap();
        }
        let summary = writer.finish().unwrap();
        assert_eq!(summary.entries, 3);
        assert_eq!(summary.bytes, summary.out.len() as u64);
        assert_eq!(summary.sha256, <[u8; 32]>::from(Sha256::digest(&summary.out)));

        let tar_bytes = zstd::decode_all(summary.out.as_slice()).unwrap();
        let mut archive = tar::Archive::new(tar_bytes.as_slice());
        let mut seen = Vec::new();
        for entry in archive.entries().unwrap() {
            let mut entry = entry.unwrap();
            let path = entry.path().unwrap().to_string_lossy().into_owned();
            let header = entry.header().clone();
            assert_eq!(header.mtime().unwrap(), 1_700_000_000);
            assert_eq!(header.uid().unwrap(), 0);
            assert_eq!(header.username_bytes(), Some(&b""[..]));
            let link = entry.link_name().unwrap().map(|l| l.to_string_lossy().into_owned());
            let mut body = Vec::new();
            entry.read_to_end(&mut body).unwrap();
            seen.push((path, header.mode().unwrap(), link, body));
        }
        let dir = "d".repeat(150);
        assert_eq!(seen[0], (format!("{dir}/"), 0o755, None, vec![]));
        assert_eq!(seen[1], (long.clone(), 0o755, None, b"hi".to_vec()));
        assert_eq!(seen[2], ("link".into(), 0o777, Some(target.clone()), vec![]));
    }

    #[test]
    fn checkout_writes_modes_symlinks_and_refuses_non_empty_dest() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("co");
        let plan = TreePlan::new(&[
            RawTreeEntry { path: b"bin/run".to_vec(), kind: RawEntryKind::File { oid: BlobOid([1; 20]), executable: true }, size: Some(2) },
            RawTreeEntry { path: b"bin/link".to_vec(), kind: RawEntryKind::Symlink { oid: BlobOid([2; 20]) }, size: Some(3) },
            RawTreeEntry { path: b"sub".to_vec(), kind: RawEntryKind::Submodule { commit: CommitSha([3; 20]) }, size: None },
        ])
        .unwrap();
        let checkout = Checkout::create(&dest).unwrap();
        for item in &plan.items {
            let content: &[u8] = if matches!(item.kind, PlanItemKind::Symlink { .. }) { b"run" } else { b"#!" };
            checkout.write(item, content).unwrap();
        }
        let meta = fs::metadata(dest.join("bin/run")).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o555);
        assert_eq!(fs::read_link(dest.join("bin/link")).unwrap(), Path::new("run"));
        assert!(fs::metadata(dest.join("sub")).unwrap().is_dir());
        assert!(matches!(Checkout::create(&dest), Err(GitError::Conflict)));
    }
}
