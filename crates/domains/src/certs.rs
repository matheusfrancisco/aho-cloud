//! TLS certificates: PEM pairs in a directory, served by SNI through rustls.
//!
//! Layout: `<dir>/<id>.pem` (chain, leaf first) and `<dir>/<id>.key` (PKCS#8/SEC1/PKCS#1 key),
//! where `<id>` is the certificate's first DNS name with a leading `*` spelled `_wildcard`.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::{Mutex, RwLock};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use serde::{Deserialize, Serialize};

use crate::resolver::normalize_name;

const DAY: u64 = 86_400;

/// What a stored certificate covers and when it is valid (Unix seconds).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CertInfo {
    /// File stem in the store directory.
    pub id: String,
    /// DNS names from the leaf's subjectAltName, lowercase, in certificate order.
    pub names: Vec<String>,
    pub not_before: u64,
    pub not_after: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum CertError {
    #[error("certificate store I/O at {path}: {source}")]
    Io { path: PathBuf, source: io::Error },
    #[error("certificate PEM: {0}")]
    Pem(String),
    #[error("certificate PEM contains no certificate")]
    NoCertificate,
    #[error("certificate: {0}")]
    X509(String),
    #[error("certificate has no DNS subjectAltName")]
    NoNames,
    #[error("certificate name `{0}` cannot be stored")]
    BadName(String),
    #[error("private key: {0}")]
    Key(String),
}

fn io_err(path: &Path) -> impl FnOnce(io::Error) -> CertError + '_ {
    move |source| CertError::Io { path: path.to_owned(), source }
}

#[derive(Debug)]
struct Entry {
    info: CertInfo,
    key: Arc<CertifiedKey>,
}

#[derive(Debug, Default)]
struct Index {
    by_id: BTreeMap<String, Arc<Entry>>,
    /// Exact and `*.` names → the entry with the latest `not_after` covering it.
    by_name: HashMap<String, Arc<Entry>>,
}

impl Index {
    fn rebuild(by_id: BTreeMap<String, Arc<Entry>>) -> Self {
        let mut by_name: HashMap<String, Arc<Entry>> = HashMap::new();
        for entry in by_id.values() {
            for name in &entry.info.names {
                match by_name.get(name) {
                    Some(existing) if existing.info.not_after >= entry.info.not_after => {}
                    _ => {
                        by_name.insert(name.clone(), entry.clone());
                    }
                }
            }
        }
        Self { by_id, by_name }
    }
}

/// Certificates by SNI: exact name first, then a single-label `*.` wildcard.
pub struct CertStore {
    dir: PathBuf,
    index: RwLock<Index>,
    /// Serializes writers so one process never interleaves two pairs' files.
    writer: Mutex<()>,
}

impl std::fmt::Debug for CertStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let count = self.index.read().by_id.len();
        f.debug_struct("CertStore").field("dir", &self.dir).field("certs", &count).finish()
    }
}

impl CertStore {
    /// Opens (creating if needed) `dir` and loads every pair in it.
    pub fn open(dir: impl AsRef<Path>) -> Result<Self, CertError> {
        let dir = dir.as_ref().to_owned();
        fs::create_dir_all(&dir).map_err(io_err(&dir))?;
        let store = Self { dir, index: RwLock::new(Index::default()), writer: Mutex::new(()) };
        store.reload()?;
        Ok(store)
    }

    /// The directory holding the PEM pairs.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Re-reads the directory (for certificates written by another process). A pair that fails
    /// to parse is skipped with a warning; if it was loaded before (e.g. caught between the key
    /// and chain renames of a renewal) the previous version keeps serving. Returns how many
    /// certificates are loaded.
    pub fn reload(&self) -> Result<usize, CertError> {
        let _writer = self.writer.lock();
        let previous = self.index.read().by_id.clone();
        let mut by_id = BTreeMap::new();
        for dirent in fs::read_dir(&self.dir).map_err(io_err(&self.dir))? {
            let path = dirent.map_err(io_err(&self.dir))?.path();
            if path.extension().is_none_or(|e| e != "pem") {
                continue;
            }
            let key_path = path.with_extension("key");
            let loaded = fs::read_to_string(&path)
                .map_err(io_err(&path))
                .and_then(|chain| Ok((chain, fs::read_to_string(&key_path).map_err(io_err(&key_path))?)))
                .and_then(|(chain, key)| parse(&chain, &key));
            match loaded {
                Ok(entry) => {
                    by_id.insert(entry.info.id.clone(), Arc::new(entry));
                }
                Err(error) => {
                    tracing::warn!(path = %path.display(), %error, "skipping certificate");
                    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or_default();
                    if let Some(old) = previous.get(stem) {
                        by_id.insert(stem.to_owned(), old.clone());
                    }
                }
            }
        }
        let count = by_id.len();
        *self.index.write() = Index::rebuild(by_id);
        Ok(count)
    }

    /// Validates the pair (parseable, key matches the leaf), writes it to the directory
    /// atomically and serves it immediately. Replaces a certificate with the same id.
    pub fn insert(&self, chain_pem: &str, key_pem: &str) -> Result<CertInfo, CertError> {
        let entry = parse(chain_pem, key_pem)?;
        let id = entry.info.id.clone();
        let _writer = self.writer.lock();
        write_atomic(&self.dir.join(format!("{id}.key")), key_pem.as_bytes(), true)?;
        write_atomic(&self.dir.join(format!("{id}.pem")), chain_pem.as_bytes(), false)?;
        let info = entry.info.clone();
        let mut index = self.index.write();
        let mut by_id = std::mem::take(&mut index.by_id);
        by_id.insert(id, Arc::new(entry));
        *index = Index::rebuild(by_id);
        Ok(info)
    }

    /// Deletes the pair `id` from disk and memory. Returns whether it existed in either.
    pub fn remove(&self, id: &str) -> Result<bool, CertError> {
        let _writer = self.writer.lock();
        let mut existed = {
            let mut index = self.index.write();
            let mut by_id = std::mem::take(&mut index.by_id);
            let existed = by_id.remove(id).is_some();
            *index = Index::rebuild(by_id);
            existed
        };
        // Only ids this store could have written touch the filesystem (no `../`, no `/`).
        let name = id.strip_prefix("_wildcard.").map_or_else(|| id.to_owned(), |rest| format!("*.{rest}"));
        if file_id(&name).ok().as_deref() != Some(id) {
            return Ok(existed);
        }
        // Chain first, so a concurrent reload never sees a chain without its key.
        for ext in ["pem", "key"] {
            let path = self.dir.join(format!("{id}.{ext}"));
            match fs::remove_file(&path) {
                Ok(()) => existed = true,
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(io_err(&path)(e)),
            }
        }
        Ok(existed)
    }

    /// The certificate for a TLS server name: exact match, else `*.<parent>`.
    pub fn get(&self, server_name: &str) -> Option<Arc<CertifiedKey>> {
        let name = normalize_name(server_name);
        let index = self.index.read();
        if let Some(entry) = index.by_name.get(&name) {
            return Some(entry.key.clone());
        }
        let (_, parent) = name.split_once('.')?;
        index.by_name.get(&format!("*.{parent}")).map(|e| e.key.clone())
    }

    pub fn list(&self) -> Vec<CertInfo> {
        let index = self.index.read();
        index.by_id.values().map(|e| e.info.clone()).collect()
    }

    /// Certificates that expire within `days_before` days of `now` (or already have).
    pub fn renewal_due(&self, now: u64, days_before: u64) -> Vec<CertInfo> {
        let horizon = now.saturating_add(days_before.saturating_mul(DAY));
        self.list().into_iter().filter(|c| c.not_after <= horizon).collect()
    }
}

impl ResolvesServerCert for CertStore {
    fn resolve(&self, client_hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        self.get(client_hello.server_name()?)
    }
}

/// Names and validity of the leaf certificate in `chain_pem`.
pub fn inspect(chain_pem: &str) -> Result<CertInfo, CertError> {
    let chain = chain_ders(chain_pem)?;
    leaf_info(&chain[0])
}

fn chain_ders(chain_pem: &str) -> Result<Vec<CertificateDer<'static>>, CertError> {
    let chain =
        CertificateDer::pem_slice_iter(chain_pem.as_bytes()).collect::<Result<Vec<_>, _>>().map_err(|e| CertError::Pem(e.to_string()))?;
    if chain.is_empty() {
        return Err(CertError::NoCertificate);
    }
    Ok(chain)
}

fn leaf_info(der: &CertificateDer<'_>) -> Result<CertInfo, CertError> {
    let (_, cert) = x509_parser::parse_x509_certificate(der).map_err(|e| CertError::X509(e.to_string()))?;
    let san = cert.subject_alternative_name().map_err(|e| CertError::X509(e.to_string()))?;
    let names: Vec<String> = san
        .map(|ext| {
            ext.value
                .general_names
                .iter()
                .filter_map(|gn| match gn {
                    x509_parser::extensions::GeneralName::DNSName(n) => Some(normalize_name(n)),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();
    let Some(first) = names.first() else {
        return Err(CertError::NoNames);
    };
    let id = file_id(first)?;
    let secs = |t: i64| u64::try_from(t).unwrap_or(0);
    Ok(CertInfo {
        id,
        not_before: secs(cert.validity().not_before.timestamp()),
        not_after: secs(cert.validity().not_after.timestamp()),
        names,
    })
}

fn parse(chain_pem: &str, key_pem: &str) -> Result<Entry, CertError> {
    let chain = chain_ders(chain_pem)?;
    let info = leaf_info(&chain[0])?;
    let key = PrivateKeyDer::from_pem_slice(key_pem.as_bytes()).map_err(|e| CertError::Key(e.to_string()))?;
    let certified =
        CertifiedKey::from_der(chain, key, &rustls::crypto::ring::default_provider()).map_err(|e| CertError::Key(e.to_string()))?;
    Ok(Entry { info, key: Arc::new(certified) })
}

/// A safe file stem for a certificate name: hostname characters only, `*` → `_wildcard`.
fn file_id(name: &str) -> Result<String, CertError> {
    if !cloud_contract::validate::is_hostname(name, true) {
        return Err(CertError::BadName(name.to_owned()));
    }
    Ok(match name.strip_prefix("*.") {
        Some(rest) => format!("_wildcard.{rest}"),
        None => name.to_owned(),
    })
}

fn write_atomic(path: &Path, bytes: &[u8], private: bool) -> Result<(), CertError> {
    let tmp = path.with_extension(format!("{}.tmp", path.extension().and_then(|e| e.to_str()).unwrap_or("")));
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(if private { 0o600 } else { 0o644 });
    }
    #[cfg(not(unix))]
    let _ = private;
    let mut file = options.open(&tmp).map_err(io_err(&tmp))?;
    file.write_all(bytes).and_then(|()| file.sync_all()).map_err(io_err(&tmp))?;
    fs::rename(&tmp, path).map_err(io_err(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_ids_are_safe_stems() {
        assert_eq!(file_id("*.shop.acme.com").unwrap(), "_wildcard.shop.acme.com");
        assert_eq!(file_id("shop.acme.com").unwrap(), "shop.acme.com");
        assert!(file_id("../etc/passwd").is_err());
        assert!(file_id("a/b.com").is_err());
    }
}
