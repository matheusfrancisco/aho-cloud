//! Content-addressed uploads (source archives, session logs). The address is `blake3:<hex>` of
//! the bytes, which is also what a manifest's `source.hash` names.

use std::path::{Path, PathBuf};

use async_trait::async_trait;

#[derive(Debug, thiserror::Error)]
pub enum BlobError {
    #[error("`{0}` is not a content hash (`blake3:` plus 64 lowercase hex)")]
    BadHash(String),
    #[error("blob store: {0}")]
    Io(String),
    #[error("blob {0} does not match its hash")]
    Corrupt(String),
}

/// `blake3:<hex>` of `bytes`.
pub fn content_hash(bytes: &[u8]) -> String {
    format!("blake3:{}", blake3::hash(bytes).to_hex())
}

/// The 64 hex digits of a well-formed content hash.
pub fn hash_hex(hash: &str) -> Result<&str, BlobError> {
    match hash.strip_prefix("blake3:") {
        Some(hex) if hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) => Ok(hex),
        _ => Err(BlobError::BadHash(hash.to_owned())),
    }
}

/// Where uploaded bytes live.
#[async_trait]
pub trait BlobStore: Send + Sync {
    /// Stores `bytes` and returns their content hash. Idempotent.
    async fn put(&self, bytes: &[u8]) -> Result<String, BlobError>;
    /// The bytes, verified against `hash`.
    async fn get(&self, hash: &str) -> Result<Option<Vec<u8>>, BlobError>;
}

/// Blobs as files under `root/<2 hex>/<62 hex>`, written to a temp name and renamed.
pub struct FsBlobStore {
    root: PathBuf,
}

impl FsBlobStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn path(&self, hex: &str) -> PathBuf {
        self.root.join(&hex[..2]).join(&hex[2..])
    }
}

fn io(path: &Path, e: std::io::Error) -> BlobError {
    BlobError::Io(format!("{}: {e}", path.display()))
}

#[async_trait]
impl BlobStore for FsBlobStore {
    async fn put(&self, bytes: &[u8]) -> Result<String, BlobError> {
        let hash = content_hash(bytes);
        let hex = hash_hex(&hash)?.to_owned();
        let path = self.path(&hex);
        if tokio::fs::try_exists(&path).await.map_err(|e| io(&path, e))? {
            return Ok(hash);
        }
        let dir = path.parent().map(Path::to_path_buf).unwrap_or_else(|| self.root.clone());
        tokio::fs::create_dir_all(&dir).await.map_err(|e| io(&dir, e))?;
        let tmp = dir.join(format!(".{}.{}.tmp", &hex[2..], uuid::Uuid::new_v4().simple()));
        tokio::fs::write(&tmp, bytes).await.map_err(|e| io(&tmp, e))?;
        tokio::fs::rename(&tmp, &path).await.map_err(|e| io(&path, e))?;
        Ok(hash)
    }

    async fn get(&self, hash: &str) -> Result<Option<Vec<u8>>, BlobError> {
        let hex = hash_hex(hash)?;
        let path = self.path(hex);
        match tokio::fs::read(&path).await {
            Ok(bytes) if content_hash(&bytes) == hash => Ok(Some(bytes)),
            Ok(_) => Err(BlobError::Corrupt(hash.to_owned())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(io(&path, e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn round_trips_and_verifies_on_read() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsBlobStore::new(dir.path());
        let hash = store.put(b"archive bytes").await.unwrap();
        assert_eq!(hash, content_hash(b"archive bytes"));
        assert_eq!(store.put(b"archive bytes").await.unwrap(), hash, "idempotent");
        assert_eq!(store.get(&hash).await.unwrap().unwrap(), b"archive bytes");
        assert!(store.get(&content_hash(b"other")).await.unwrap().is_none());

        let hex = hash_hex(&hash).unwrap();
        std::fs::write(dir.path().join(&hex[..2]).join(&hex[2..]), b"swapped").unwrap();
        assert!(matches!(store.get(&hash).await, Err(BlobError::Corrupt(_))));
    }

    #[tokio::test]
    async fn refuses_hashes_that_could_escape_the_root() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsBlobStore::new(dir.path());
        for bad in ["blake3:../../etc/passwd", "sha256:00", "blake3:ABCD", &format!("blake3:{}", "0".repeat(63))] {
            assert!(matches!(store.get(bad).await, Err(BlobError::BadHash(_))), "{bad}");
        }
    }
}
