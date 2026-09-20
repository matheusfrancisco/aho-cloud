//! `$AHOCLOUD_HOME/credentials.json`: the API token and the device's Ed25519 secret key, readable
//! by the owner only.

use std::io::Write;
use std::path::{Path, PathBuf};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::SigningKey;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

const FILE: &str = "credentials.json";

/// Deliberately not `Debug`: nothing should print it.
#[derive(Clone, Serialize, Deserialize)]
pub struct Credentials {
    pub api_url: String,
    pub token: String,
    /// The server's id for this device's public key.
    pub key_id: String,
    /// The 32-byte Ed25519 seed, base64url without padding.
    pub secret_key: String,
}

impl Credentials {
    pub fn new(api_url: &str, token: String, key_id: String, key: &SigningKey) -> Self {
        Self { api_url: api_url.to_owned(), token, key_id, secret_key: URL_SAFE_NO_PAD.encode(key.to_bytes()) }
    }

    pub fn signing_key(&self) -> Result<SigningKey> {
        let bytes = URL_SAFE_NO_PAD.decode(&self.secret_key).map_err(|e| Error::Invalid(format!("credentials: secret key: {e}")))?;
        let seed: [u8; 32] = bytes.try_into().map_err(|_| Error::Invalid("credentials: secret key is not 32 bytes".into()))?;
        Ok(SigningKey::from_bytes(&seed))
    }
}

/// `--home` / `AHOCLOUD_HOME`, else `~/.config/ahocloud`.
pub fn home(flag: Option<&Path>) -> Result<PathBuf> {
    if let Some(h) = flag {
        return Ok(h.to_owned());
    }
    let home = std::env::var_os("HOME").ok_or_else(|| Error::Invalid("HOME is not set; pass --home or set AHOCLOUD_HOME".into()))?;
    Ok(PathBuf::from(home).join(".config").join("ahocloud"))
}

pub fn load(home: &Path) -> Result<Credentials> {
    let path = home.join(FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(Error::NotLoggedIn),
        Err(e) => return Err(Error::io(&path)(e)),
    };
    serde_json::from_str(&text).map_err(|e| Error::Invalid(format!("{}: {e}", path.display())))
}

/// Writes atomically (temp file + rename) with mode 0600 in a 0700 directory.
pub fn store(home: &Path, creds: &Credentials) -> Result<PathBuf> {
    create_private_dir(home)?;
    let path = home.join(FILE);
    let tmp = home.join(format!(".{FILE}.tmp"));
    let body = serde_json::to_vec_pretty(creds).map_err(|e| Error::Invalid(e.to_string()))?;
    let _ = std::fs::remove_file(&tmp);
    let mut f = private_file(&tmp).map_err(Error::io(&tmp))?;
    f.write_all(&body).and_then(|()| f.sync_all()).map_err(Error::io(&tmp))?;
    drop(f);
    std::fs::rename(&tmp, &path).map_err(Error::io(&path))?;
    Ok(path)
}

/// Removes the credentials; `false` when there were none.
pub fn remove(home: &Path) -> Result<bool> {
    let path = home.join(FILE);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(Error::io(&path)(e)),
    }
}

#[cfg(unix)]
fn create_private_dir(dir: &Path) -> Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    if !dir.is_dir() {
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir).map_err(Error::io(dir))?;
    }
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).map_err(Error::io(dir))
}

#[cfg(not(unix))]
fn create_private_dir(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).map_err(Error::io(dir))
}

#[cfg(unix)]
fn private_file(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path)
}

#[cfg(not(unix))]
fn private_file(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new().write(true).create_new(true).open(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_keeps_the_key() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("h");
        let key = SigningKey::from_bytes(&[7; 32]);
        store(&home, &Credentials::new("http://x", "t".into(), "k".into(), &key)).unwrap();
        let back = load(&home).unwrap();
        assert_eq!(back.signing_key().unwrap().to_bytes(), key.to_bytes());
        assert!(remove(&home).unwrap());
        assert!(matches!(load(&home), Err(Error::NotLoggedIn)));
        assert!(!remove(&home).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn overwriting_keeps_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        std::fs::write(&path, "{}").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let key = SigningKey::from_bytes(&[1; 32]);
        store(dir.path(), &Credentials::new("http://x", "t".into(), "k".into(), &key)).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    }
}
