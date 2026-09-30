//! The directory as a deterministic `.tar.gz`: files in path order, fixed mtimes and owners, so
//! the same tree always hashes the same and `source.hash` names the code, not the moment.
//!
//! Included: every non-directory entry of the shared walk ([`crate::walk`]'s rules: `.gitignore`
//! honored, `.git` and `.aho` pruned), except the root `deploy.json`: it names the archive's hash,
//! so it cannot be inside it; the manifest travels in the proposal. Symlinks are stored as links,
//! never followed.

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use cloud_api::FileKind;
use cloud_contract::draft::Draft;
use flate2::Compression;
use flate2::write::GzEncoder;
use serde_json::json;

use crate::error::{WorkdirError, io};
use crate::{MANIFEST, declare_subtree, walk};

pub struct Archive {
    pub bytes: Vec<u8>,
    /// `blake3:<hex>` of `bytes`, what the cloud reports back on upload.
    pub hash: String,
    /// Archived paths, `/`-separated, in archive order.
    pub files: Vec<String>,
}

impl Archive {
    /// `.env`-style files that made it in (not ignored). They often hold real values.
    pub fn env_files(&self) -> Vec<&str> {
        self.files
            .iter()
            .map(String::as_str)
            .filter(|f| {
                let name = f.rsplit('/').next().unwrap_or(f);
                (name == ".env" || name.starts_with(".env.")) && !matches!(name, ".env.example" | ".env.sample" | ".env.template")
            })
            .collect()
    }

    /// Declares `/source` as this archive, replacing whatever source the draft had.
    pub fn declare_source(&self, draft: &mut Draft) {
        declare_subtree(draft, "/source", json!({"kind": "archive", "hash": self.hash}));
    }
}

pub fn hash(bytes: &[u8]) -> String {
    format!("blake3:{}", blake3::hash(bytes).to_hex())
}

/// Packs `root` with no size cap.
pub fn pack(root: &Path) -> Result<Archive, WorkdirError> {
    pack_within(root, u64::MAX)
}

/// Packs `root`, giving up with [`WorkdirError::TooLarge`] as soon as the compressed archive
/// would exceed `max_bytes`, so an oversized tree is never held in memory whole.
pub fn pack_within(root: &Path, max_bytes: u64) -> Result<Archive, WorkdirError> {
    let root = root.canonicalize().map_err(io(root))?;
    let files = collect(&root)?;
    let failed = |path: &Path| {
        let path = path.to_owned();
        move |e: io::Error| match e.kind() {
            io::ErrorKind::FileTooLarge => WorkdirError::TooLarge { limit: max_bytes },
            _ => WorkdirError::Io { path, source: e },
        }
    };
    let mut tar = tar::Builder::new(GzEncoder::new(Capped { bytes: Vec::new(), limit: max_bytes }, Compression::default()));
    tar.mode(tar::HeaderMode::Deterministic);
    tar.follow_symlinks(false);
    let mut names = Vec::with_capacity(files.len());
    for (abs, rel) in files {
        tar.append_path_with_name(&abs, &rel).map_err(failed(&abs))?;
        names.push(rel);
    }
    let gz = tar.into_inner().map_err(failed(&root))?;
    let bytes = gz.finish().map_err(failed(&root))?.bytes;
    Ok(Archive { hash: hash(&bytes), bytes, files: names })
}

fn collect(root: &Path) -> Result<Vec<(PathBuf, String)>, WorkdirError> {
    let mut out = Vec::new();
    for entry in walk::walk(root) {
        let entry = entry?;
        if entry.kind == FileKind::Dir || entry.rel == MANIFEST {
            continue;
        }
        out.push((entry.dent.into_path(), entry.rel));
    }
    Ok(out)
}

/// The archive's buffer; a write past `limit` fails with `FileTooLarge`.
struct Capped {
    bytes: Vec<u8>,
    limit: u64,
}

impl Write for Capped {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if (self.bytes.len() as u64).saturating_add(buf.len() as u64) > self.limit {
            return Err(io::Error::new(io::ErrorKind::FileTooLarge, "the archive is over its size cap"));
        }
        self.bytes.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Read;

    use super::*;
    use crate::tests::tree;

    #[test]
    fn honors_gitignore_and_skips_git_aho_and_the_manifest() {
        let dir = tree();
        let a = pack(dir.path()).unwrap();
        assert_eq!(a.files, [".env", ".env.example", ".gitignore", "src/main.rs", "sub/.gitignore", "sub/secret.txt"]);
        assert_eq!(a.env_files(), [".env"]);
        let mut names = Vec::new();
        let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(a.bytes.as_slice()));
        for e in tar.entries().unwrap() {
            let mut e = e.unwrap();
            let mut s = String::new();
            e.read_to_string(&mut s).unwrap();
            names.push(e.path().unwrap().to_string_lossy().into_owned());
        }
        assert_eq!(names, a.files);
    }

    #[test]
    fn same_tree_same_hash() {
        let dir = tree();
        let first = pack(dir.path()).unwrap();
        // mtimes move; the archive does not.
        fs::write(dir.path().join("src/main.rs"), "fn main() {}").unwrap();
        assert_eq!(pack(dir.path()).unwrap().hash, first.hash);
        fs::write(dir.path().join("src/main.rs"), "fn main() { }").unwrap();
        assert_ne!(pack(dir.path()).unwrap().hash, first.hash);
        assert_eq!(first.hash, hash(&first.bytes));
    }

    #[test]
    fn a_cap_below_the_archive_refuses_and_one_at_it_packs() {
        let dir = tree();
        let full = pack(dir.path()).unwrap();
        let len = full.bytes.len() as u64;
        assert!(matches!(pack_within(dir.path(), len - 1), Err(WorkdirError::TooLarge { limit }) if limit == len - 1));
        assert_eq!(pack_within(dir.path(), len).unwrap().hash, full.hash);
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_stored_as_links_not_followed() {
        let dir = tree();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("host-secret"), "never archived").unwrap();
        std::os::unix::fs::symlink(outside.path().join("host-secret"), dir.path().join("link")).unwrap();
        let a = pack(dir.path()).unwrap();
        let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(a.bytes.as_slice()));
        let link = tar.entries().unwrap().map(Result::unwrap).find(|e| e.path().unwrap().to_str() == Some("link")).unwrap();
        assert_eq!(link.header().entry_type(), tar::EntryType::Symlink);
        assert_eq!(link.header().size().unwrap(), 0, "the target's bytes are not in the archive");
    }
}
