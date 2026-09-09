//! Principal signing keys: one P-256 key per (project, stage), stored as PKCS#8 PEM at
//! `<dir>/<project>/<stage>.pem`, generated on first use. The `kid` is the RFC 7638 JWK
//! thumbprint, so it is stable across restarts and edge replicas sharing the directory.

use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use cloud_contract::ledger::Stage;
use cloud_contract::principal::PrincipalClaims;
use p256::ecdsa::signature::Signer;
use p256::ecdsa::{Signature, SigningKey};
use p256::pkcs8::{DecodePrivateKey, EncodePrivateKey, LineEnding};
use parking_lot::Mutex;
use rand_core::OsRng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// One JSON Web Key (EC P-256 or RSA public key).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Jwk {
    pub kty: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alg: Option<String>,
    #[serde(default, rename = "use", skip_serializing_if = "Option::is_none")]
    pub use_: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crv: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub x: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub y: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub n: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub e: Option<String>,
}

/// A JWK Set, as served at `/.edge/jwks.json`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Jwks {
    pub keys: Vec<Jwk>,
}

#[derive(Debug, thiserror::Error)]
pub enum KeyError {
    #[error("project slug `{0}` is not a valid key directory name")]
    Project(String),
    #[error("{path}: {source}")]
    Io { path: PathBuf, source: io::Error },
    #[error("{path}: not a P-256 PKCS#8 PEM key: {reason}")]
    Pem { path: PathBuf, reason: String },
    #[error("encoding: {0}")]
    Encode(String),
}

/// The signing key of one (project, stage).
pub struct SiteKey {
    signing: SigningKey,
    jwk: Jwk,
}

impl SiteKey {
    fn new(signing: SigningKey) -> Result<Self, KeyError> {
        let point = signing.verifying_key().to_encoded_point(false);
        let (Some(x), Some(y)) = (point.x(), point.y()) else {
            return Err(KeyError::Encode("public key is the identity point".into()));
        };
        let x = URL_SAFE_NO_PAD.encode(x);
        let y = URL_SAFE_NO_PAD.encode(y);
        let thumb = format!(r#"{{"crv":"P-256","kty":"EC","x":"{x}","y":"{y}"}}"#);
        let kid = URL_SAFE_NO_PAD.encode(Sha256::digest(thumb.as_bytes()));
        let jwk = Jwk {
            kty: "EC".into(),
            kid: Some(kid),
            alg: Some("ES256".into()),
            use_: Some("sig".into()),
            crv: Some("P-256".into()),
            x: Some(x),
            y: Some(y),
            n: None,
            e: None,
        };
        Ok(Self { signing, jwk })
    }

    /// RFC 7638 thumbprint.
    pub fn kid(&self) -> &str {
        self.jwk.kid.as_deref().unwrap_or_default()
    }

    /// Public half, as published.
    pub fn jwk(&self) -> &Jwk {
        &self.jwk
    }

    /// Compact ES256 JWS over `claims`.
    pub fn sign(&self, claims: &PrincipalClaims) -> Result<String, KeyError> {
        let header = serde_json::json!({ "alg": "ES256", "typ": "JWT", "kid": self.kid() });
        let header = serde_json::to_vec(&header).map_err(|e| KeyError::Encode(e.to_string()))?;
        let payload = serde_json::to_vec(claims).map_err(|e| KeyError::Encode(e.to_string()))?;
        let input = format!("{}.{}", URL_SAFE_NO_PAD.encode(header), URL_SAFE_NO_PAD.encode(payload));
        let sig: Signature = self.signing.sign(input.as_bytes());
        Ok(format!("{input}.{}", URL_SAFE_NO_PAD.encode(sig.to_bytes())))
    }
}

/// PEM key directory with an in-memory cache.
pub struct KeyStore {
    dir: PathBuf,
    cache: Mutex<HashMap<(String, Stage), Arc<SiteKey>>>,
}

impl KeyStore {
    /// Opens (creating if needed) the key directory.
    pub fn open(dir: impl Into<PathBuf>) -> Result<Self, KeyError> {
        let dir = dir.into();
        create_private_dir(&dir)?;
        Ok(Self { dir, cache: Mutex::new(HashMap::new()) })
    }

    /// The key for (project, stage), generating and persisting it on first use.
    pub fn key(&self, project: &str, stage: Stage) -> Result<Arc<SiteKey>, KeyError> {
        if !valid_slug(project) {
            return Err(KeyError::Project(project.to_owned()));
        }
        let mut cache = self.cache.lock();
        if let Some(k) = cache.get(&(project.to_owned(), stage)) {
            return Ok(k.clone());
        }
        let key = Arc::new(self.load_or_create(project, stage)?);
        cache.insert((project.to_owned(), stage), key.clone());
        Ok(key)
    }

    /// The JWKS apps of (project, stage) verify against.
    pub fn jwks(&self, project: &str, stage: Stage) -> Result<Jwks, KeyError> {
        Ok(Jwks { keys: vec![self.key(project, stage)?.jwk().clone()] })
    }

    /// Signs claims with the key of their (project, stage).
    pub fn mint(&self, project: &str, stage: Stage, claims: &PrincipalClaims) -> Result<String, KeyError> {
        self.key(project, stage)?.sign(claims)
    }

    fn load_or_create(&self, project: &str, stage: Stage) -> Result<SiteKey, KeyError> {
        let dir = self.dir.join(project);
        create_private_dir(&dir)?;
        let path = dir.join(format!("{}.pem", stage.as_str()));
        match fs::read_to_string(&path) {
            Ok(pem) => return parse_pem(&path, &pem),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(source) => return Err(KeyError::Io { path, source }),
        }
        let signing = SigningKey::random(&mut OsRng);
        let pem = signing.to_pkcs8_pem(LineEnding::LF).map_err(|e| KeyError::Encode(e.to_string()))?;
        // Write a private temp file, then hard-link it into place: the link fails if another
        // process won the race, and nobody ever reads a half-written key.
        let tmp = dir.join(format!(".{}.pem.{}", stage.as_str(), crate::session::random_token()));
        write_private(&tmp, pem.as_bytes())?;
        let linked = fs::hard_link(&tmp, &path);
        let _ = fs::remove_file(&tmp);
        match linked {
            Ok(()) => SiteKey::new(signing),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                let pem = fs::read_to_string(&path).map_err(|source| KeyError::Io { path: path.clone(), source })?;
                parse_pem(&path, &pem)
            }
            Err(source) => Err(KeyError::Io { path, source }),
        }
    }
}

fn parse_pem(path: &Path, pem: &str) -> Result<SiteKey, KeyError> {
    let signing = SigningKey::from_pkcs8_pem(pem).map_err(|e| KeyError::Pem { path: path.to_owned(), reason: e.to_string() })?;
    SiteKey::new(signing)
}

fn valid_slug(s: &str) -> bool {
    let mut bytes = s.bytes();
    matches!(bytes.next(), Some(b'a'..=b'z')) && s.len() <= 40 && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn create_private_dir(dir: &Path) -> Result<(), KeyError> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir).map_err(|source| KeyError::Io { path: dir.to_owned(), source })
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<(), KeyError> {
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let io_err = |source| KeyError::Io { path: path.to_owned(), source };
    let mut f = opts.open(path).map_err(io_err)?;
    f.write_all(bytes).map_err(io_err)?;
    f.sync_all().map_err(io_err)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_is_generated_once_and_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let a = KeyStore::open(dir.path()).unwrap();
        let kid = a.key("shop", Stage::Live).unwrap().kid().to_owned();
        assert!(dir.path().join("shop/live.pem").is_file());
        let b = KeyStore::open(dir.path()).unwrap();
        assert_eq!(b.key("shop", Stage::Live).unwrap().kid(), kid);
        assert_ne!(b.key("shop", Stage::Staging).unwrap().kid(), kid, "stages never share keys");
        assert_ne!(b.key("other", Stage::Live).unwrap().kid(), kid, "projects never share keys");
    }

    #[cfg(unix)]
    #[test]
    fn key_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        KeyStore::open(dir.path()).unwrap().key("shop", Stage::Live).unwrap();
        let mode = fs::metadata(dir.path().join("shop/live.pem")).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0);
    }

    #[test]
    fn kid_is_the_rfc7638_thumbprint() {
        // RFC 7638 §3.1 uses RSA; recompute the EC form independently.
        let dir = tempfile::tempdir().unwrap();
        let key = KeyStore::open(dir.path()).unwrap().key("shop", Stage::Live).unwrap();
        let j = key.jwk();
        let canonical = serde_json::json!({ "crv": "P-256", "kty": "EC", "x": j.x, "y": j.y }).to_string();
        assert_eq!(key.kid(), URL_SAFE_NO_PAD.encode(Sha256::digest(canonical.as_bytes())));
    }

    #[test]
    fn traversal_and_bad_slugs_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let ks = KeyStore::open(dir.path()).unwrap();
        for bad in ["../x", "a/b", "", "Shop", "1abc", &"a".repeat(41)] {
            assert!(matches!(ks.key(bad, Stage::Live), Err(KeyError::Project(_))), "{bad}");
        }
    }

    #[test]
    fn corrupt_key_file_is_an_error_not_a_new_key() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("shop")).unwrap();
        fs::write(dir.path().join("shop/live.pem"), "garbage").unwrap();
        let ks = KeyStore::open(dir.path()).unwrap();
        assert!(matches!(ks.key("shop", Stage::Live), Err(KeyError::Pem { .. })));
    }
}
