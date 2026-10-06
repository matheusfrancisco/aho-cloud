//! `MemSecretStore`: in-memory `SecretStore` (sealed rows only; no cryptography here).

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use mage_core::domain::OrgId;
use mage_core::ports::clock::Clock;
use parking_lot::Mutex;
use uuid::Uuid;

use crate::ports::secrets::{
    DataKeyState, SealedDataKey, SealedSecret, SecretStore, SecretStoreError, WrapInfo, WrapRef, check_new_data_key, check_new_secret,
};
use crate::ports::vault::{SecretName, SecretScope, SecretState, SecretVersion};

pub struct MemSecretStore {
    clock: Arc<dyn Clock>,
    state: Mutex<State>,
}

/// `(org or None for platform, name, version)`: the order `list_wraps` walks secrets in.
type SecretKey = (Option<Uuid>, String, u32);

#[derive(Default)]
struct State {
    secrets: BTreeMap<SecretKey, SealedSecret>,
    data_keys: BTreeMap<(Uuid, u32), SealedDataKey>,
}

fn key(scope: &SecretScope, name: &SecretName, version: SecretVersion) -> SecretKey {
    (scope.org().map(|o| o.as_uuid()), name.as_str().to_owned(), version.0)
}

impl MemSecretStore {
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self { clock, state: Mutex::new(State::default()) }
    }
}

impl State {
    fn versions<'a>(&'a self, scope: &SecretScope, name: &'a SecretName) -> impl Iterator<Item = &'a SealedSecret> + 'a {
        let org = scope.org().map(|o| o.as_uuid());
        self.secrets.range((org, name.as_str().to_owned(), 0)..=(org, name.as_str().to_owned(), u32::MAX)).map(|(_, s)| s)
    }

    fn retire_active(&mut self, scope: &SecretScope, name: &SecretName, at: time::OffsetDateTime) {
        let org = scope.org().map(|o| o.as_uuid());
        for s in self.secrets.range_mut((org, name.as_str().to_owned(), 0)..=(org, name.as_str().to_owned(), u32::MAX)).map(|(_, s)| s) {
            if s.state == SecretState::Active {
                s.state = SecretState::Retired;
                s.state_changed_at = at;
            }
        }
    }
}

#[async_trait]
impl SecretStore for MemSecretStore {
    async fn next_secret_version(&self, scope: &SecretScope, name: &SecretName) -> Result<SecretVersion, SecretStoreError> {
        let state = self.state.lock();
        Ok(SecretVersion(state.versions(scope, name).map(|s| s.version.0).max().unwrap_or(0) + 1))
    }

    async fn insert_secret(&self, s: &SealedSecret) -> Result<(), SecretStoreError> {
        check_new_secret(s)?;
        let now = self.clock.now();
        let mut state = self.state.lock();
        let max = state.versions(&s.scope, &s.name).map(|v| v.version.0).max().unwrap_or(0);
        if s.version.0 != max + 1 {
            return Err(SecretStoreError::Conflict);
        }
        if s.state == SecretState::Active {
            state.retire_active(&s.scope, &s.name, now);
        }
        state.secrets.insert(key(&s.scope, &s.name, s.version), s.clone());
        Ok(())
    }

    async fn active_secret(&self, scope: &SecretScope, name: &SecretName) -> Result<Option<SealedSecret>, SecretStoreError> {
        let state = self.state.lock();
        Ok(state.versions(scope, name).find(|s| s.state == SecretState::Active).cloned())
    }

    async fn secret_version(&self, scope: &SecretScope, name: &SecretName, version: SecretVersion) -> Result<SealedSecret, SecretStoreError> {
        self.state.lock().secrets.get(&key(scope, name, version)).cloned().ok_or(SecretStoreError::NotFound)
    }

    async fn secret_versions(&self, scope: &SecretScope, name: &SecretName) -> Result<Vec<SealedSecret>, SecretStoreError> {
        Ok(self.state.lock().versions(scope, name).cloned().collect())
    }

    async fn list_secrets(&self, scope: &SecretScope) -> Result<Vec<SealedSecret>, SecretStoreError> {
        let org = scope.org().map(|o| o.as_uuid());
        Ok(self.state.lock().secrets.iter().filter(|(k, _)| k.0 == org).map(|(_, s)| s.clone()).collect())
    }

    async fn set_secret_state(&self, scope: &SecretScope, name: &SecretName, version: SecretVersion, next: SecretState)
    -> Result<SealedSecret, SecretStoreError> {
        let now = self.clock.now();
        let mut state = self.state.lock();
        let current = state.secrets.get(&key(scope, name, version)).ok_or(SecretStoreError::NotFound)?.state;
        if current == next {
            return state.secrets.get(&key(scope, name, version)).cloned().ok_or(SecretStoreError::NotFound);
        }
        if !current.can_become(next) {
            return Err(SecretStoreError::Invalid(format!("secret state {current} cannot become {next}")));
        }
        if next == SecretState::Active {
            state.retire_active(scope, name, now);
        }
        let s = state.secrets.get_mut(&key(scope, name, version)).ok_or(SecretStoreError::NotFound)?;
        s.state = next;
        s.state_changed_at = now;
        if next == SecretState::Destroyed {
            s.wrapped_dek = None;
            s.ciphertext = None;
        }
        Ok(s.clone())
    }

    async fn insert_data_key(&self, k: &SealedDataKey) -> Result<(), SecretStoreError> {
        check_new_data_key(k)?;
        let org = k.org.as_uuid();
        let mut state = self.state.lock();
        if state.data_keys.contains_key(&(org, k.version)) {
            return Err(SecretStoreError::Conflict);
        }
        if k.state == DataKeyState::Active {
            for (_, dk) in state.data_keys.range_mut((org, 0)..=(org, u32::MAX)) {
                if dk.state == DataKeyState::Active {
                    dk.state = DataKeyState::Retired;
                }
            }
        }
        state.data_keys.insert((org, k.version), k.clone());
        Ok(())
    }

    async fn active_data_key(&self, org: OrgId) -> Result<Option<SealedDataKey>, SecretStoreError> {
        let org = org.as_uuid();
        let state = self.state.lock();
        Ok(state.data_keys.range((org, 0)..=(org, u32::MAX)).map(|(_, k)| k).find(|k| k.state == DataKeyState::Active).cloned())
    }

    async fn data_key(&self, org: OrgId, version: u32) -> Result<SealedDataKey, SecretStoreError> {
        self.state.lock().data_keys.get(&(org.as_uuid(), version)).cloned().ok_or(SecretStoreError::NotFound)
    }

    async fn list_wraps(&self, after: Option<&WrapRef>, limit: u32) -> Result<Vec<WrapInfo>, SecretStoreError> {
        let limit = limit as usize;
        let state = self.state.lock();
        let secrets = state.secrets.iter().filter(|(k, s)| {
            s.state != SecretState::Destroyed
                && match after {
                    None => true,
                    Some(WrapRef::Secret { scope, name, version }) => **k > key(scope, name, *version),
                    Some(WrapRef::DataKey { .. }) => false,
                }
        });
        let data_keys = state.data_keys.iter().filter(|(k, dk)| {
            dk.state != DataKeyState::Destroyed
                && match after {
                    Some(WrapRef::DataKey { org, version }) => **k > (org.as_uuid(), *version),
                    _ => true,
                }
        });
        let out = secrets
            .map(|(_, s)| WrapInfo {
                r: WrapRef::Secret { scope: s.scope, name: s.name.clone(), version: s.version },
                kek_version: s.kek_version.clone(),
            })
            .chain(data_keys.map(|(_, dk)| WrapInfo { r: WrapRef::DataKey { org: dk.org, version: dk.version }, kek_version: dk.kek_version.clone() }))
            .take(limit)
            .collect();
        Ok(out)
    }

    async fn rewrap(&self, r: &WrapRef, expect_kek_version: &str, wrapped_dek: &[u8], kek_version: &str) -> Result<(), SecretStoreError> {
        let mut state = self.state.lock();
        let (current, dek) = match r {
            WrapRef::Secret { scope, name, version } => {
                let s = state.secrets.get_mut(&key(scope, name, *version)).filter(|s| s.state != SecretState::Destroyed).ok_or(SecretStoreError::NotFound)?;
                (&mut s.kek_version, &mut s.wrapped_dek)
            }
            WrapRef::DataKey { org, version } => {
                let k = state.data_keys.get_mut(&(org.as_uuid(), *version)).filter(|k| k.state != DataKeyState::Destroyed).ok_or(SecretStoreError::NotFound)?;
                (&mut k.kek_version, &mut k.wrapped_dek)
            }
        };
        if current != expect_kek_version {
            return Err(SecretStoreError::Conflict);
        }
        *current = kek_version.to_owned();
        *dek = Some(wrapped_dek.to_vec());
        Ok(())
    }

    async fn destroy_org(&self, org: OrgId) -> Result<u64, SecretStoreError> {
        let org = org.as_uuid();
        let mut state = self.state.lock();
        let before = state.secrets.len() + state.data_keys.len();
        state.secrets.retain(|k, _| k.0 != Some(org));
        state.data_keys.retain(|k, _| k.0 != org);
        Ok((before - state.secrets.len() - state.data_keys.len()) as u64)
    }
}
