//! [`EnvelopeVault`]: `SecretVault` over `SecretStore` and a [`Kek`].
//!
//! - Secret version: random 256-bit DEK; value sealed with AES-256-GCM (random 96-bit nonce),
//!   AAD `mage-secret/v1|{scope}|{name}|{version}`; the DEK wrapped by the KEK with the same AAD.
//!   A row copied to another scope, name or version fails both checks → `Integrity`.
//! - Org data key (`encrypt` / `decrypt`): one DEK per org and version, created on the org's first
//!   `encrypt`, wrapped with AAD `mage-datakey/v1|{org}|{version}`. Data is sealed with AAD
//!   `mage-data/v1|{org}|{dek_version}|` ++ caller AAD into
//!   `Ciphertext = [fmt=1][dek_version u32 BE][nonce 12][ct+tag]`.
//! - Plaintext caches per instance: secret values [`SECRET_CACHE_TTL`] keyed by
//!   `(scope, name, version)`, unwrapped data keys [`DATA_KEY_CACHE_TTL`]. Rows are read from the
//!   store on every call (state checks); the caches only skip the KEK.

use std::collections::HashMap;
use std::sync::Arc;

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use async_trait::async_trait;
use parking_lot::Mutex;
use rand::RngCore;
use rand::rngs::OsRng;
use time::OffsetDateTime;
use tracing::{error, info};

use mage_core::domain::OrgId;
use mage_core::ports::Clock;

use super::Kek;
use crate::ports::audit::Actor;
use crate::ports::secrets::{DataKeyState, SealedDataKey, SealedSecret, SecretStore, SecretStoreError, WrapInfo, WrapRef};
use crate::ports::vault::{
    Activation, Ciphertext, RewrapReport, SecretBytes, SecretMeta, SecretName, SecretScope, SecretState, SecretVault, SecretVersion,
    VaultError,
};

/// How long a decrypted secret value is served from memory.
pub const SECRET_CACHE_TTL: time::Duration = time::Duration::seconds(60);
/// How long an unwrapped org data key is served from memory.
pub const DATA_KEY_CACHE_TTL: time::Duration = time::Duration::minutes(10);

pub(crate) const KEY_LEN: usize = 32;
pub(crate) const NONCE_LEN: usize = 12;
pub(crate) const TAG_LEN: usize = 16;
/// `list_wraps` page size inside `rewrap`.
const WRAP_PAGE: u32 = 500;

type SecretKey = (SecretScope, SecretName, SecretVersion);

struct Cached {
    value: SecretBytes,
    expires_at: OffsetDateTime,
}

pub struct EnvelopeVault<K: Kek> {
    store: Arc<dyn SecretStore>,
    kek: K,
    clock: Arc<dyn Clock>,
    secrets: Mutex<HashMap<SecretKey, Cached>>,
    data_keys: Mutex<HashMap<(OrgId, u32), Cached>>,
}

impl<K: Kek> EnvelopeVault<K> {
    pub fn new(store: Arc<dyn SecretStore>, kek: K, clock: Arc<dyn Clock>) -> Self {
        EnvelopeVault { store, kek, clock, secrets: Mutex::new(HashMap::new()), data_keys: Mutex::new(HashMap::new()) }
    }

    pub fn kek(&self) -> &K {
        &self.kek
    }

    /// Plaintext of a live row read at `(scope, name, version)`.
    async fn open_secret(&self, row: &SealedSecret, scope: &SecretScope, name: &SecretName) -> Result<SecretBytes, VaultError> {
        if row.scope != *scope || row.name != *name {
            error!(%scope, %name, version = row.version.0, "secret store returned a row for other coordinates");
            return Err(VaultError::Integrity);
        }
        let (Some(wrapped), Some(ciphertext)) = (row.wrapped_dek.as_deref(), row.ciphertext.as_deref()) else {
            return Err(VaultError::NotFound);
        };
        if row.state == SecretState::Destroyed {
            return Err(VaultError::NotFound);
        }
        let key = (*scope, name.clone(), row.version);
        if let Some(value) = cache_get(&self.secrets, &key, self.clock.now()) {
            return Ok(value);
        }
        let aad = secret_aad(scope, name, row.version);
        let opened = async {
            let dek = self.kek.unwrap(&aad, wrapped, &row.kek_version).await?;
            open(dek.expose(), &row.nonce, &aad, ciphertext)
        }
        .await;
        let value = opened.inspect_err(|e| {
            if *e == VaultError::Integrity {
                error!(%scope, %name, version = row.version.0, "secret failed its integrity check (tampered or moved row)");
            }
        })?;
        cache_put(&self.secrets, key, value.clone(), self.clock.now(), SECRET_CACHE_TTL);
        Ok(value)
    }

    /// The org's Active data key, created (version = first unused) when it has none.
    async fn active_data_key(&self, org: OrgId) -> Result<(u32, SecretBytes), VaultError> {
        if let Some(row) = self.store.active_data_key(org).await.map_err(store_err)? {
            let dek = self.data_key_bytes(&row).await?;
            return Ok((row.version, dek));
        }
        let mut version = 1u32;
        loop {
            match self.store.data_key(org, version).await {
                Ok(_) => version = version.checked_add(1).ok_or_else(|| VaultError::Invalid("data key versions exhausted".into()))?,
                Err(SecretStoreError::NotFound) => break,
                Err(e) => return Err(store_err(e)),
            }
        }
        let dek = random_secret(KEY_LEN);
        let (wrapped, kek_version) = self.kek.wrap(&data_key_aad(org, version), dek.expose()).await?;
        let row = SealedDataKey {
            org,
            version,
            wrapped_dek: Some(wrapped),
            kek_version,
            state: DataKeyState::Active,
            created_at: self.clock.now(),
        };
        match self.store.insert_data_key(&row).await {
            Ok(()) => {
                info!(org = %org.as_uuid(), version, "org data key created");
                cache_put(&self.data_keys, (org, version), dek.clone(), self.clock.now(), DATA_KEY_CACHE_TTL);
                Ok((version, dek))
            }
            // Another instance created it first: use theirs.
            Err(SecretStoreError::Conflict) => {
                let row = self.store.active_data_key(org).await.map_err(store_err)?.ok_or(VaultError::Conflict)?;
                let dek = self.data_key_bytes(&row).await?;
                Ok((row.version, dek))
            }
            Err(e) => Err(store_err(e)),
        }
    }

    async fn data_key_bytes(&self, row: &SealedDataKey) -> Result<SecretBytes, VaultError> {
        let Some(wrapped) = row.wrapped_dek.as_deref().filter(|_| row.state != DataKeyState::Destroyed) else {
            return Err(VaultError::NotFound);
        };
        let key = (row.org, row.version);
        if let Some(dek) = cache_get(&self.data_keys, &key, self.clock.now()) {
            return Ok(dek);
        }
        let dek = self.kek.unwrap(&data_key_aad(row.org, row.version), wrapped, &row.kek_version).await.inspect_err(|e| {
            if *e == VaultError::Integrity {
                error!(org = %row.org.as_uuid(), version = row.version, "org data key failed its integrity check (tampered or moved row)");
            }
        })?;
        if dek.len() != KEY_LEN {
            return Err(VaultError::Integrity);
        }
        cache_put(&self.data_keys, key, dek.clone(), self.clock.now(), DATA_KEY_CACHE_TTL);
        Ok(dek)
    }

    /// Moves one wrap onto the KEK's primary version. `Ok(false)`: nothing to do (already moved,
    /// destroyed, or changed under us).
    async fn rewrap_one(&self, info: &WrapInfo) -> Result<bool, VaultError> {
        let (aad, wrapped, current) = match &info.r {
            WrapRef::Secret { scope, name, version } => {
                let row = match self.store.secret_version(scope, name, *version).await {
                    Ok(row) => row,
                    Err(SecretStoreError::NotFound) => return Ok(false),
                    Err(e) => return Err(store_err(e)),
                };
                let Some(wrapped) = row.wrapped_dek.filter(|_| row.state != SecretState::Destroyed) else {
                    return Ok(false);
                };
                (secret_aad(scope, name, *version), wrapped, row.kek_version)
            }
            WrapRef::DataKey { org, version } => {
                let row = match self.store.data_key(*org, *version).await {
                    Ok(row) => row,
                    Err(SecretStoreError::NotFound) => return Ok(false),
                    Err(e) => return Err(store_err(e)),
                };
                let Some(wrapped) = row.wrapped_dek.filter(|_| row.state != DataKeyState::Destroyed) else {
                    return Ok(false);
                };
                (data_key_aad(*org, *version), wrapped, row.kek_version)
            }
        };
        if current != info.kek_version {
            return Ok(false);
        }
        let dek = self.kek.unwrap(&aad, &wrapped, &current).await?;
        let (rewrapped, kek_version) = self.kek.wrap(&aad, dek.expose()).await?;
        match self.store.rewrap(&info.r, &current, &rewrapped, &kek_version).await {
            Ok(()) => Ok(true),
            Err(SecretStoreError::Conflict | SecretStoreError::NotFound) => Ok(false),
            Err(e) => Err(store_err(e)),
        }
    }
}

#[async_trait]
impl<K: Kek> SecretVault for EnvelopeVault<K> {
    async fn put(
        &self,
        scope: &SecretScope,
        name: &SecretName,
        value: SecretBytes,
        activation: Activation,
        actor: &Actor,
    ) -> Result<SecretMeta, VaultError> {
        let version = self.store.next_secret_version(scope, name).await.map_err(store_err)?;
        let aad = secret_aad(scope, name, version);
        let dek = random_secret(KEY_LEN);
        let nonce = random_nonce();
        let ciphertext = seal(dek.expose(), &nonce, &aad, value.expose())?;
        let (wrapped, kek_version) = self.kek.wrap(&aad, dek.expose()).await?;
        let now = self.clock.now();
        let row = SealedSecret {
            scope: *scope,
            name: name.clone(),
            version,
            state: match activation {
                Activation::Now => SecretState::Active,
                Activation::Staged => SecretState::Staged,
            },
            wrapped_dek: Some(wrapped),
            kek_version,
            nonce,
            ciphertext: Some(ciphertext),
            fingerprint: value.fingerprint(),
            created_by: actor.clone(),
            created_at: now,
            state_changed_at: now,
        };
        self.store.insert_secret(&row).await.map_err(store_err)?;
        cache_put(&self.secrets, (*scope, name.clone(), version), value, now, SECRET_CACHE_TTL);
        Ok(meta_of(&row))
    }

    async fn load(&self, scope: &SecretScope, name: &SecretName) -> Result<(SecretMeta, SecretBytes), VaultError> {
        let row = self.store.active_secret(scope, name).await.map_err(store_err)?.ok_or(VaultError::NotFound)?;
        if row.state != SecretState::Active {
            error!(%scope, %name, version = row.version.0, state = %row.state, "secret store returned a non-active row as active");
            return Err(VaultError::Integrity);
        }
        let value = self.open_secret(&row, scope, name).await?;
        Ok((meta_of(&row), value))
    }

    async fn load_version(&self, scope: &SecretScope, name: &SecretName, version: SecretVersion) -> Result<SecretBytes, VaultError> {
        let row = self.store.secret_version(scope, name, version).await.map_err(store_err)?;
        if row.version != version {
            return Err(VaultError::Integrity);
        }
        self.open_secret(&row, scope, name).await
    }

    async fn versions(&self, scope: &SecretScope, name: &SecretName) -> Result<Vec<SecretMeta>, VaultError> {
        let rows = self.store.secret_versions(scope, name).await.map_err(store_err)?;
        Ok(rows.iter().map(meta_of).collect())
    }

    async fn list(&self, scope: &SecretScope) -> Result<Vec<SecretMeta>, VaultError> {
        let rows = self.store.list_secrets(scope).await.map_err(store_err)?;
        Ok(rows.iter().map(meta_of).collect())
    }

    async fn set_state(
        &self,
        scope: &SecretScope,
        name: &SecretName,
        version: SecretVersion,
        state: SecretState,
        actor: &Actor,
    ) -> Result<SecretMeta, VaultError> {
        let row = self.store.set_secret_state(scope, name, version, state).await.map_err(store_err)?;
        if state == SecretState::Destroyed {
            self.secrets.lock().remove(&(*scope, name.clone(), version));
        }
        info!(%scope, %name, version = version.0, %state, actor = %actor.encode(), "secret state changed");
        Ok(meta_of(&row))
    }

    async fn encrypt(&self, org: OrgId, aad: &[u8], plaintext: &[u8]) -> Result<Ciphertext, VaultError> {
        let (version, dek) = self.active_data_key(org).await?;
        let nonce = random_nonce();
        let sealed = seal(dek.expose(), &nonce, &data_aad(org, version, aad), plaintext)?;
        Ok(Ciphertext::assemble(version, &nonce, &sealed))
    }

    async fn decrypt(&self, org: OrgId, aad: &[u8], ciphertext: &Ciphertext) -> Result<SecretBytes, VaultError> {
        let (version, nonce, sealed) = ciphertext.parts()?;
        let row = match self.store.data_key(org, version).await {
            Ok(row) => row,
            // The ciphertext names a data key this org never had: sealed for another org.
            Err(SecretStoreError::NotFound) => return Err(VaultError::Integrity),
            Err(e) => return Err(store_err(e)),
        };
        let dek = self.data_key_bytes(&row).await?;
        open(dek.expose(), &nonce, &data_aad(org, version, aad), sealed)
    }

    async fn rewrap(&self, batch: u32) -> Result<RewrapReport, VaultError> {
        let primary = self.kek.primary_version().await?;
        let mut report = RewrapReport::default();
        let mut after: Option<WrapRef> = None;
        loop {
            let page = self.store.list_wraps(after.as_ref(), WRAP_PAGE).await.map_err(store_err)?;
            for info in &page {
                report.scanned += 1;
                if info.kek_version == primary {
                    continue;
                }
                if report.rewrapped >= batch {
                    report.remaining += 1;
                    continue;
                }
                match self.rewrap_one(info).await {
                    Ok(true) => report.rewrapped += 1,
                    Ok(false) => {}
                    // A row that cannot be unwrapped stays on its old version; it is reported as
                    // remaining and the rest of the batch goes on.
                    Err(VaultError::Integrity) => {
                        error!(wrap = ?info.r, "rewrap skipped a wrap that fails its integrity check");
                        report.remaining += 1;
                    }
                    Err(e) => return Err(e),
                }
            }
            match page.last() {
                Some(last) if page.len() as u32 == WRAP_PAGE => after = Some(last.r.clone()),
                _ => break,
            }
        }
        if report.rewrapped > 0 || report.remaining > 0 {
            info!(scanned = report.scanned, rewrapped = report.rewrapped, remaining = report.remaining, %primary, "vault rewrap");
        }
        Ok(report)
    }
}

pub(crate) fn secret_aad(scope: &SecretScope, name: &SecretName, version: SecretVersion) -> Vec<u8> {
    format!("mage-secret/v1|{scope}|{name}|{version}").into_bytes()
}

pub(crate) fn data_key_aad(org: OrgId, version: u32) -> Vec<u8> {
    format!("mage-datakey/v1|{}|{version}", org.as_uuid()).into_bytes()
}

fn data_aad(org: OrgId, dek_version: u32, aad: &[u8]) -> Vec<u8> {
    let mut out = format!("mage-data/v1|{}|{dek_version}|", org.as_uuid()).into_bytes();
    out.extend_from_slice(aad);
    out
}

fn meta_of(row: &SealedSecret) -> SecretMeta {
    SecretMeta {
        scope: row.scope,
        name: row.name.clone(),
        version: row.version,
        state: row.state,
        fingerprint: row.fingerprint.clone(),
        created_by: row.created_by.clone(),
        created_at: row.created_at,
        state_changed_at: row.state_changed_at,
    }
}

fn store_err(e: SecretStoreError) -> VaultError {
    match e {
        SecretStoreError::NotFound => VaultError::NotFound,
        SecretStoreError::Conflict => VaultError::Conflict,
        SecretStoreError::Invalid(m) => VaultError::Invalid(m),
        SecretStoreError::Unavailable(m) => VaultError::Unavailable(m),
    }
}

fn cache_get<Q: Eq + std::hash::Hash>(cache: &Mutex<HashMap<Q, Cached>>, key: &Q, now: OffsetDateTime) -> Option<SecretBytes> {
    cache.lock().get(key).filter(|c| c.expires_at > now).map(|c| c.value.clone())
}

fn cache_put<Q: Eq + std::hash::Hash>(cache: &Mutex<HashMap<Q, Cached>>, key: Q, value: SecretBytes, now: OffsetDateTime, ttl: time::Duration) {
    let mut cache = cache.lock();
    // Expired plaintext is dropped (and zeroed) as soon as anything new is cached.
    cache.retain(|_, c| c.expires_at > now);
    cache.insert(key, Cached { value, expires_at: now + ttl });
}

/// `len` bytes from the OS RNG, held where they are zeroed on drop.
pub(crate) fn random_secret(len: usize) -> SecretBytes {
    let mut bytes = vec![0u8; len];
    OsRng.fill_bytes(&mut bytes);
    SecretBytes::new(bytes)
}

pub(crate) fn random_nonce() -> [u8; NONCE_LEN] {
    let mut nonce = [0u8; NONCE_LEN];
    OsRng.fill_bytes(&mut nonce);
    nonce
}

/// AES-256-GCM: `ct+tag` of `plaintext` under `key`, `nonce`, `aad`.
pub(crate) fn seal(key: &[u8], nonce: &[u8; NONCE_LEN], aad: &[u8], plaintext: &[u8]) -> Result<Vec<u8>, VaultError> {
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|_| VaultError::Invalid("AES-256-GCM keys are 32 bytes".into()))?;
    cipher
        .encrypt(Nonce::from_slice(nonce), Payload { msg: plaintext, aad })
        .map_err(|_| VaultError::Invalid("plaintext too long for AES-256-GCM".into()))
}

/// Inverse of [`seal`]; a wrong key, nonce, AAD or a modified `sealed` → `Integrity`.
pub(crate) fn open(key: &[u8], nonce: &[u8; NONCE_LEN], aad: &[u8], sealed: &[u8]) -> Result<SecretBytes, VaultError> {
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|_| VaultError::Integrity)?;
    cipher.decrypt(Nonce::from_slice(nonce), Payload { msg: sealed, aad }).map(SecretBytes::new).map_err(|_| VaultError::Integrity)
}
