//! The workspace as a deterministic `.tar.gz`: files in path order, fixed mtimes and owners, so
//! the same tree always hashes the same and `source.hash` names the code, not the moment.
//!
//! Included: everything `.gitignore` (and `.git/info/exclude`) does not ignore, whether or not
//! the directory is a git repository. Excluded: `.git`, `.aho` (session logs travel separately,
//! verified), and the root `deploy.json` (it names the archive's hash, so it cannot be inside
//! it; the manifest travels in the proposal). Symlinks are stored as links, never followed.

use std::path::{Path, PathBuf};

use flate2::Compression;
use flate2::write::GzEncoder;
use ignore::WalkBuilder;

use crate::error::{Error, Result};

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
}

pub fn hash(bytes: &[u8]) -> String {
    format!("blake3:{}", blake3::hash(bytes).to_hex())
}

pub fn pack(root: &Path) -> Result<Archive> {
    let root = root.canonicalize().map_err(Error::io(root))?;
    let files = collect(&root)?;
    let mut tar = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::default()));
    tar.mode(tar::HeaderMode::Deterministic);
    tar.follow_symlinks(false);
    let mut names = Vec::with_capacity(files.len());
    for (abs, rel) in files {
        tar.append_path_with_name(&abs, &rel).map_err(Error::io(&abs))?;
        names.push(rel);
    }
    let gz = tar.into_inner().map_err(Error::io(&root))?;
    let bytes = gz.finish().map_err(Error::io(&root))?;
    Ok(Archive { hash: hash(&bytes), bytes, files: names })
}

fn collect(root: &Path) -> Result<Vec<(PathBuf, String)>> {
    let walk = WalkBuilder::new(root)
        .hidden(false)
        .ignore(false)
        .parents(false)
        .git_global(false)
        .git_ignore(true)
        .git_exclude(true)
        .require_git(false)
        .follow_links(false)
        .sort_by_file_name(|a, b| a.cmp(b))
        .filter_entry(|e| !matches!(e.file_name().to_str(), Some(".git" | ".aho")))
        .build();
    let mut out = Vec::new();
    for entry in walk {
        let entry = entry.map_err(|e| Error::Invalid(format!("reading the workspace: {e}")))?;
        let Some(kind) = entry.file_type() else { continue };
        if kind.is_dir() {
            continue;
        }
        let rel =
            entry.path().strip_prefix(root).map_err(|_| Error::Invalid(format!("{} is outside the workspace", entry.path().display())))?;
        let name = rel
            .to_str()
            .ok_or_else(|| Error::Invalid(format!("{} is not UTF-8; rename it or ignore it", rel.display())))?
            .replace(std::path::MAIN_SEPARATOR, "/");
        if name == "deploy.json" {
            continue;
        }
        out.push((entry.path().to_owned(), name));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Read;

    use super::*;

    fn tree() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        for (f, body) in [
            ("src/main.rs", "fn main() {}"),
            (".gitignore", "target/\n*.log\n/secret.txt\n"),
            ("secret.txt", "x"),
            ("sub/secret.txt", "kept: the pattern is anchored"),
            ("target/debug/app", "bin"),
            ("run.log", "log"),
            (".git/HEAD", "ref"),
            (".aho/sessions/1.jsonl", "{}"),
            ("deploy.json", "{}"),
            (".env", "K=v"),
            (".env.example", "K="),
            ("sub/.gitignore", "local.txt\n"),
            ("sub/local.txt", "x"),
        ] {
            fs::create_dir_all(p.join(f).parent().unwrap()).unwrap();
            fs::write(p.join(f), body).unwrap();
        }
        dir
    }

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
}
