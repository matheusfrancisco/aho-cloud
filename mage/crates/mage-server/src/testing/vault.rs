//! In-memory `SecretVault` ([`MemSecretVault`]: the real envelope code with a fixed local key over
//! `MemSecretStore`, so AAD and integrity properties hold) and the generic `SecretVault` contract
//! suite (server.md "Testing"), run against the fake, Postgres (`PgSecretStore`) and Cloud KMS.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use parking_lot::Mutex;
use uuid::Uuid;

use mage_core::domain::OrgId;
use mage_core::ports::Clock;
use mage_core::testing::FakeClock;

use crate::adapters::driven::vault::{EnvelopeVault, Kek, LocalKeyFileKek};
use crate::ports::audit::Actor;
use crate::ports::secrets::{SealedDataKey, SealedSecret, SecretStore};
use crate::ports::vault::{
    Activation, Ciphertext, RewrapReport, SecretBytes, SecretMeta, SecretName, SecretScope, SecretState, SecretVault, SecretVersion,
    VaultError,
};
use crate::testing::MemSecretStore;

/// Key of every [`MemSecretVault`].
pub const MEM_VAULT_KEY: [u8; 32] = [0x6d; 32];

/// `EnvelopeVault<LocalKeyFileKek>` over a [`MemSecretStore`] with [`MEM_VAULT_KEY`].
pub struct MemSecretVault {
    inner: EnvelopeVault<LocalKeyFileKek>,
    store: Arc<MemSecretStore>,
    failure: Mutex<Option<VaultError>>,
}

impl MemSecretVault {
    pub fn new(clock: Arc<dyn Clock>) -> MemSecretVault {
        let store = Arc::new(MemSecretStore::new(clock.clone()));
        let inner = EnvelopeVault::new(store.clone(), LocalKeyFileKek::from_key(MEM_VAULT_KEY), clock);
        MemSecretVault { inner, store, failure: Mutex::new(None) }
    }

    /// The sealed rows (tamper and inspection).
    pub fn store(&self) -> Arc<MemSecretStore> {
        self.store.clone()
    }

    /// While `Some`, every call fails with that error.
    pub fn set_failure(&self, failure: Option<VaultError>) {
        *self.failure.lock() = failure;
    }

    fn check(&self) -> Result<(), VaultError> {
        match &*self.failure.lock() {
            Some(e) => Err(e.clone()),
            None => Ok(()),
        }
    }
}

#[async_trait]
impl SecretVault for MemSecretVault {
    async fn put(
        &self,
        scope: &SecretScope,
        name: &SecretName,
        value: SecretBytes,
        activation: Activation,
        actor: &Actor,
    ) -> Result<SecretMeta, VaultError> {
        self.check()?;
        self.inner.put(scope, name, value, activation, actor).await
    }

    async fn load(&self, scope: &SecretScope, name: &SecretName) -> Result<(SecretMeta, SecretBytes), VaultError> {
        self.check()?;
        self.inner.load(scope, name).await
    }

    async fn load_version(&self, scope: &SecretScope, name: &SecretName, version: SecretVersion) -> Result<SecretBytes, VaultError> {
        self.check()?;
        self.inner.load_version(scope, name, version).await
    }

    async fn versions(&self, scope: &SecretScope, name: &SecretName) -> Result<Vec<SecretMeta>, VaultError> {
        self.check()?;
        self.inner.versions(scope, name).await
    }

    async fn list(&self, scope: &SecretScope) -> Result<Vec<SecretMeta>, VaultError> {
        self.check()?;
        self.inner.list(scope).await
    }

    async fn set_state(
        &self,
        scope: &SecretScope,
        name: &SecretName,
        version: SecretVersion,
        state: SecretState,
        actor: &Actor,
    ) -> Result<SecretMeta, VaultError> {
        self.check()?;
        self.inner.set_state(scope, name, version, state, actor).await
    }

    async fn encrypt(&self, org: OrgId, aad: &[u8], plaintext: &[u8]) -> Result<Ciphertext, VaultError> {
        self.check()?;
        self.inner.encrypt(org, aad, plaintext).await
    }

    async fn decrypt(&self, org: OrgId, aad: &[u8], ciphertext: &Ciphertext) -> Result<SecretBytes, VaultError> {
        self.check()?;
        self.inner.decrypt(org, aad, ciphertext).await
    }

    async fn rewrap(&self, batch: u32) -> Result<RewrapReport, VaultError> {
        self.check()?;
        self.inner.rewrap(batch).await
    }
}

/// KEK ring for tests: local keys labelled `test-v1`, `test-v2`, …; [`RotatingKek::rotate`] adds
/// a new primary, older versions keep unwrapping. Keys derive from the version number, so every
/// instance unwraps every other instance's wraps.
pub struct RotatingKek {
    keys: Mutex<Vec<Arc<LocalKeyFileKek>>>,
}

impl RotatingKek {
    pub fn new() -> RotatingKek {
        RotatingKek { keys: Mutex::new(vec![Arc::new(RotatingKek::key(1))]) }
    }

    /// Makes `test-v{n+1}` the primary.
    pub fn rotate(&self) {
        let mut keys = self.keys.lock();
        let next = u32::try_from(keys.len()).unwrap_or(u32::MAX).saturating_add(1);
        keys.push(Arc::new(RotatingKek::key(next)));
    }

    fn key(n: u32) -> LocalKeyFileKek {
        let mut key = [0xa5u8; 32];
        key[..4].copy_from_slice(&n.to_be_bytes());
        LocalKeyFileKek::with_version(key, &format!("test-v{n}"))
    }

    fn primary(&self) -> Arc<LocalKeyFileKek> {
        let keys = self.keys.lock();
        keys[keys.len() - 1].clone()
    }
}

impl Default for RotatingKek {
    fn default() -> Self {
        RotatingKek::new()
    }
}

#[async_trait]
impl Kek for RotatingKek {
    async fn wrap(&self, aad: &[u8], dek: &[u8]) -> Result<(Vec<u8>, String), VaultError> {
        self.primary().wrap(aad, dek).await
    }

    async fn unwrap(&self, aad: &[u8], wrapped: &[u8], kek_version: &str) -> Result<SecretBytes, VaultError> {
        let key = self.keys.lock().iter().find(|k| k.version() == kek_version).cloned();
        let key = key.ok_or_else(|| VaultError::Invalid(format!("unknown test KEK version {kek_version:?}")))?;
        key.unwrap(aad, wrapped, kek_version).await
    }

    async fn primary_version(&self) -> Result<String, VaultError> {
        Ok(self.primary().version().to_owned())
    }
}

// ---------------------------------------------------------------------------------------------
// Contract suite
// ---------------------------------------------------------------------------------------------

/// One vault under test, over an empty store.
pub struct VaultFixture {
    pub vault: Arc<dyn SecretVault>,
    /// The store under `vault`: the suite copies sealed rows through it to simulate tampering.
    pub store: Arc<dyn SecretStore>,
    /// Gives the KEK a new primary version (older versions keep unwrapping) so `rewrap` has work;
    /// `None` when the KEK cannot rotate inside a test (real Cloud KMS).
    pub rotate_kek: Option<Box<dyn Fn() + Send + Sync>>,
}

/// The orgs every scenario uses; factories create them in their backing store.
pub fn vault_contract_orgs() -> [OrgId; 3] {
    [0x7a_0001u128, 0x7a_0002, 0x7a_0003].map(|n| OrgId::from_uuid(Uuid::from_u128(n)))
}

/// Runs every `SecretVault` scenario, each against a fresh `make(clock)` fixture. Scenarios move
/// the clock past the plaintext caches before the reads that must hit the KEK.
pub async fn contract_secret_vault<F, Fut>(make: F)
where
    F: Fn(Arc<FakeClock>) -> Fut,
    Fut: Future<Output = VaultFixture>,
{
    put_load_round_trip(&make).await;
    activation_now_retires_previous_active(&make).await;
    staged_versions_are_not_loaded(&make).await;
    retired_loads_by_version_destroyed_does_not(&make).await;
    moved_rows_fail_integrity(&make).await;
    data_encryption_binds_aad_and_org(&make).await;
    rewrap_preserves_plaintext(&make).await;
}

/// Longer than every plaintext cache of the vault.
const PAST_CACHES: Duration = Duration::from_secs(11 * 60);

fn name(s: &str) -> SecretName {
    SecretName::new(s).expect("valid secret name")
}

fn bytes(s: &[u8]) -> SecretBytes {
    SecretBytes::new(s.to_vec())
}

async fn fixture<F, Fut>(make: &F) -> (Arc<FakeClock>, VaultFixture)
where
    F: Fn(Arc<FakeClock>) -> Fut,
    Fut: Future<Output = VaultFixture>,
{
    let clock = Arc::new(FakeClock::new());
    let fixture = make(clock.clone()).await;
    (clock, fixture)
}

async fn put_load_round_trip<F, Fut>(make: &F)
where
    F: Fn(Arc<FakeClock>) -> Fut,
    Fut: Future<Output = VaultFixture>,
{
    let (clock, f) = fixture(make).await;
    let [org_a, org_b, _] = vault_contract_orgs();
    let key = name("model/anthropic_api_key");
    let api_key = "sk-ant-api03-abcdefghijklmnopqrstuvwxyz";

    let meta = f.vault.put(&SecretScope::Platform, &key, bytes(api_key.as_bytes()), Activation::Now, &Actor::System).await.expect("put");
    assert_eq!((meta.version, meta.state), (SecretVersion(1), SecretState::Active));
    assert_eq!(meta.fingerprint, bytes(api_key.as_bytes()).fingerprint());
    assert!(!meta.fingerprint.contains("abcdefghijklmnop"), "fingerprint must not reveal the value");
    assert_eq!((meta.created_at, meta.state_changed_at), (clock.now(), clock.now()));
    assert_eq!(meta.created_by, Actor::System);

    let binary = [0u8, 255, 1, 254, 0];
    let org_meta =
        f.vault.put(&SecretScope::Org(org_a), &key, bytes(&binary), Activation::Now, &Actor::Operator { os_user: "ops".into() }).await.expect("put org");
    assert_eq!(org_meta.version, SecretVersion(1), "versions count per scope");

    for when in ["cached", "after cache expiry"] {
        let (loaded_meta, value) = f.vault.load(&SecretScope::Platform, &key).await.expect("load platform");
        assert_eq!(loaded_meta, meta, "{when}");
        assert_eq!(value.expose(), api_key.as_bytes(), "{when}");
        let (_, value) = f.vault.load(&SecretScope::Org(org_a), &key).await.expect("load org");
        assert_eq!(value.expose(), &binary, "{when}");
        let value = f.vault.load_version(&SecretScope::Platform, &key, SecretVersion(1)).await.expect("load_version");
        assert_eq!(value.expose(), api_key.as_bytes(), "{when}");
        clock.advance(PAST_CACHES);
    }

    assert_eq!(f.vault.load(&SecretScope::Org(org_b), &key).await.err(), Some(VaultError::NotFound));
    assert_eq!(f.vault.load(&SecretScope::Platform, &name("model/other")).await.err(), Some(VaultError::NotFound));
    assert_eq!(f.vault.load_version(&SecretScope::Platform, &key, SecretVersion(2)).await.err(), Some(VaultError::NotFound));

    let versions = f.vault.versions(&SecretScope::Platform, &key).await.expect("versions");
    assert_eq!(versions, vec![meta.clone()]);
    let listed = f.vault.list(&SecretScope::Org(org_a)).await.expect("list");
    assert_eq!(listed, vec![org_meta]);
}

async fn activation_now_retires_previous_active<F, Fut>(make: &F)
where
    F: Fn(Arc<FakeClock>) -> Fut,
    Fut: Future<Output = VaultFixture>,
{
    let (clock, f) = fixture(make).await;
    let scope = SecretScope::Platform;
    let key = name("github/webhook_secret");
    f.vault.put(&scope, &key, bytes(b"one"), Activation::Now, &Actor::System).await.expect("put v1");
    clock.advance(Duration::from_secs(60));
    let retired_at = clock.now();
    let v2 = f.vault.put(&scope, &key, bytes(b"two"), Activation::Now, &Actor::System).await.expect("put v2");
    assert_eq!((v2.version, v2.state), (SecretVersion(2), SecretState::Active));

    let versions = f.vault.versions(&scope, &key).await.expect("versions");
    let states: Vec<(u32, SecretState)> = versions.iter().map(|m| (m.version.0, m.state)).collect();
    assert_eq!(states, vec![(1, SecretState::Retired), (2, SecretState::Active)]);
    assert_eq!(versions[0].state_changed_at, retired_at, "retired in the same step as the new put");

    clock.advance(PAST_CACHES);
    let (meta, value) = f.vault.load(&scope, &key).await.expect("load");
    assert_eq!((meta.version, value.expose()), (SecretVersion(2), b"two".as_slice()));
}

async fn staged_versions_are_not_loaded<F, Fut>(make: &F)
where
    F: Fn(Arc<FakeClock>) -> Fut,
    Fut: Future<Output = VaultFixture>,
{
    let (clock, f) = fixture(make).await;
    let scope = SecretScope::Platform;
    let key = name("auth/token_signing_key");
    f.vault.put(&scope, &key, bytes(b"active"), Activation::Now, &Actor::System).await.expect("put v1");
    let staged = f.vault.put(&scope, &key, bytes(b"staged"), Activation::Staged, &Actor::System).await.expect("put v2");
    assert_eq!(staged.state, SecretState::Staged);
    clock.advance(PAST_CACHES);

    let (meta, value) = f.vault.load(&scope, &key).await.expect("load");
    assert_eq!((meta.version, value.expose()), (SecretVersion(1), b"active".as_slice()), "load returns the Active version only");
    let value = f.vault.load_version(&scope, &key, SecretVersion(2)).await.expect("staged by version");
    assert_eq!(value.expose(), b"staged");

    let only_staged = name("auth/only_staged");
    f.vault.put(&scope, &only_staged, bytes(b"x"), Activation::Staged, &Actor::System).await.expect("put staged");
    assert_eq!(f.vault.load(&scope, &only_staged).await.err(), Some(VaultError::NotFound));

    clock.advance(Duration::from_secs(60));
    let activated = f.vault.set_state(&scope, &key, SecretVersion(2), SecretState::Active, &Actor::System).await.expect("activate");
    assert_eq!((activated.state, activated.state_changed_at), (SecretState::Active, clock.now()));
    let (meta, value) = f.vault.load(&scope, &key).await.expect("load after activation");
    assert_eq!((meta.version, value.expose()), (SecretVersion(2), b"staged".as_slice()));
    let states: Vec<SecretState> = f.vault.versions(&scope, &key).await.expect("versions").iter().map(|m| m.state).collect();
    assert_eq!(states, vec![SecretState::Retired, SecretState::Active], "activation retires the previous Active");
}

async fn retired_loads_by_version_destroyed_does_not<F, Fut>(make: &F)
where
    F: Fn(Arc<FakeClock>) -> Fut,
    Fut: Future<Output = VaultFixture>,
{
    let (clock, f) = fixture(make).await;
    let [org, _, _] = vault_contract_orgs();
    let scope = SecretScope::Org(org);
    let key = name("integration/slack/t1/bot_token");
    f.vault.put(&scope, &key, bytes(b"xoxb-old"), Activation::Now, &Actor::System).await.expect("put v1");
    f.vault.put(&scope, &key, bytes(b"xoxb-new"), Activation::Now, &Actor::System).await.expect("put v2");

    clock.advance(PAST_CACHES);
    let retired = f.vault.load_version(&scope, &key, SecretVersion(1)).await.expect("retired by version");
    assert_eq!(retired.expose(), b"xoxb-old");

    // The value is in this instance's plaintext cache now; destruction must still win.
    let destroyed = f.vault.set_state(&scope, &key, SecretVersion(1), SecretState::Destroyed, &Actor::System).await.expect("destroy");
    assert_eq!(destroyed.state, SecretState::Destroyed);
    assert_eq!(f.vault.load_version(&scope, &key, SecretVersion(1)).await.err(), Some(VaultError::NotFound));
    clock.advance(PAST_CACHES);
    assert_eq!(f.vault.load_version(&scope, &key, SecretVersion(1)).await.err(), Some(VaultError::NotFound));
    let states: Vec<SecretState> = f.vault.versions(&scope, &key).await.expect("versions").iter().map(|m| m.state).collect();
    assert_eq!(states, vec![SecretState::Destroyed, SecretState::Active], "destroyed versions keep their metadata");

    assert!(f.vault.set_state(&scope, &key, SecretVersion(1), SecretState::Active, &Actor::System).await.is_err(), "Destroyed is final");
    assert!(f.vault.set_state(&scope, &key, SecretVersion(2), SecretState::Staged, &Actor::System).await.is_err(), "Active → Staged");
    let (_, value) = f.vault.load(&scope, &key).await.expect("v2 untouched");
    assert_eq!(value.expose(), b"xoxb-new");
}

async fn moved_rows_fail_integrity<F, Fut>(make: &F)
where
    F: Fn(Arc<FakeClock>) -> Fut,
    Fut: Future<Output = VaultFixture>,
{
    let (clock, f) = fixture(make).await;
    let [org_a, org_b, _] = vault_contract_orgs();
    let original = name("linear/webhook_secret");
    f.vault.put(&SecretScope::Platform, &original, bytes(b"platform value"), Activation::Now, &Actor::System).await.expect("put platform");
    f.vault.put(&SecretScope::Org(org_a), &original, bytes(b"org a value"), Activation::Now, &Actor::System).await.expect("put org a");
    clock.advance(PAST_CACHES);

    let platform_row = f.store.secret_version(&SecretScope::Platform, &original, SecretVersion(1)).await.expect("platform row");
    let org_row = f.store.secret_version(&SecretScope::Org(org_a), &original, SecretVersion(1)).await.expect("org row");
    let moves: Vec<(&str, SealedSecret)> = vec![
        ("other name", SealedSecret { name: name("slack/signing_secret"), ..platform_row.clone() }),
        ("platform → org", SealedSecret { scope: SecretScope::Org(org_b), ..platform_row.clone() }),
        ("org → other org", SealedSecret { scope: SecretScope::Org(org_b), name: name("model/anthropic_api_key"), ..org_row.clone() }),
        ("org → platform", SealedSecret { scope: SecretScope::Platform, name: name("model/anthropic_api_key"), ..org_row }),
        ("other version", SealedSecret { version: SecretVersion(2), ..platform_row }),
    ];
    for (what, row) in moves {
        f.store.insert_secret(&row).await.unwrap_or_else(|e| panic!("insert moved row ({what}): {e}"));
        assert_eq!(f.vault.load(&row.scope, &row.name).await.err(), Some(VaultError::Integrity), "{what}: load");
        assert_eq!(f.vault.load_version(&row.scope, &row.name, row.version).await.err(), Some(VaultError::Integrity), "{what}: load_version");
    }

    let still = f.vault.load_version(&SecretScope::Platform, &original, SecretVersion(1)).await.expect("original row still opens");
    assert_eq!(still.expose(), b"platform value");
}

async fn data_encryption_binds_aad_and_org<F, Fut>(make: &F)
where
    F: Fn(Arc<FakeClock>) -> Fut,
    Fut: Future<Output = VaultFixture>,
{
    let (clock, f) = fixture(make).await;
    let [org_a, org_b, org_c] = vault_contract_orgs();
    let ct = f.vault.encrypt(org_a, b"ticket:1", b"customer payload").await.expect("encrypt");
    let again = f.vault.encrypt(org_a, b"ticket:1", b"customer payload").await.expect("encrypt again");
    assert_ne!(ct, again, "every encryption uses a fresh nonce");
    let (dek_version, _, _) = ct.parts().expect("well-formed ciphertext");
    assert_eq!(again.parts().expect("well-formed").0, dek_version, "one active data key per org");

    for when in ["cached", "after cache expiry"] {
        let plain = f.vault.decrypt(org_a, b"ticket:1", &ct).await.expect("decrypt");
        assert_eq!(plain.expose(), b"customer payload", "{when}");
        assert_eq!(f.vault.decrypt(org_a, b"ticket:2", &ct).await.err(), Some(VaultError::Integrity), "{when}: wrong AAD");
        assert_eq!(f.vault.decrypt(org_a, b"", &ct).await.err(), Some(VaultError::Integrity), "{when}: empty AAD");
        clock.advance(PAST_CACHES);
    }

    assert_eq!(f.vault.decrypt(org_b, b"ticket:1", &ct).await.err(), Some(VaultError::Integrity), "org without a data key");
    let own = f.vault.encrypt(org_b, b"ticket:1", b"org b payload").await.expect("encrypt org b");
    assert_eq!(f.vault.decrypt(org_b, b"ticket:1", &ct).await.err(), Some(VaultError::Integrity), "org with its own data key");
    assert_eq!(f.vault.decrypt(org_a, b"ticket:1", &own).await.err(), Some(VaultError::Integrity), "org b data under org a");
    assert_eq!(f.vault.decrypt(org_b, b"ticket:1", &own).await.expect("org b decrypts its own").expose(), b"org b payload");

    let mut tampered = ct.clone().into_bytes();
    let last = tampered.len() - 1;
    tampered[last] ^= 0x01;
    assert_eq!(f.vault.decrypt(org_a, b"ticket:1", &Ciphertext::from_bytes(tampered)).await.err(), Some(VaultError::Integrity));
    let mut other_version = ct.clone().into_bytes();
    other_version[1..5].copy_from_slice(&(dek_version + 1).to_be_bytes());
    assert_eq!(
        f.vault.decrypt(org_a, b"ticket:1", &Ciphertext::from_bytes(other_version)).await.err(),
        Some(VaultError::Integrity),
        "header names a data key version the org does not have"
    );
    assert!(matches!(f.vault.decrypt(org_a, b"ticket:1", &Ciphertext::from_bytes(vec![1, 0, 0])).await, Err(VaultError::Invalid(_))));

    // org a's wrapped data key copied to org c.
    let key_a = f.store.data_key(org_a, dek_version).await.expect("org a data key");
    f.store.insert_data_key(&SealedDataKey { org: org_c, ..key_a }).await.expect("insert moved data key");
    assert_eq!(f.vault.decrypt(org_c, b"ticket:1", &ct).await.err(), Some(VaultError::Integrity), "moved data key");
}

async fn rewrap_preserves_plaintext<F, Fut>(make: &F)
where
    F: Fn(Arc<FakeClock>) -> Fut,
    Fut: Future<Output = VaultFixture>,
{
    let (clock, f) = fixture(make).await;
    let [org_a, org_b, _] = vault_contract_orgs();
    let platform = name("github/oauth_client_secret");
    let tenant = name("integration/linear/w1/oauth");
    f.vault.put(&SecretScope::Platform, &platform, bytes(b"old client secret"), Activation::Now, &Actor::System).await.expect("put v1");
    f.vault.put(&SecretScope::Platform, &platform, bytes(b"new client secret"), Activation::Now, &Actor::System).await.expect("put v2");
    f.vault.put(&SecretScope::Org(org_a), &tenant, bytes(b"lin_oauth_token"), Activation::Now, &Actor::System).await.expect("put org");
    let data_a = f.vault.encrypt(org_a, b"a", b"data of a").await.expect("encrypt a");
    let data_b = f.vault.encrypt(org_b, b"b", b"data of b").await.expect("encrypt b");
    // 3 secret versions + 2 org data keys.
    let wraps = 5;

    let before = f.store.list_wraps(None, 100).await.expect("wraps before");
    assert_eq!(before.len(), wraps);

    if let Some(rotate) = &f.rotate_kek {
        rotate();
        let first = f.vault.rewrap(1).await.expect("rewrap 1");
        assert_eq!(first, RewrapReport { scanned: 5, rewrapped: 1, remaining: 4 });
        let rest = f.vault.rewrap(500).await.expect("rewrap rest");
        assert_eq!(rest, RewrapReport { scanned: 5, rewrapped: 4, remaining: 0 });
        let idle = f.vault.rewrap(500).await.expect("rewrap idle");
        assert_eq!(idle, RewrapReport { scanned: 5, rewrapped: 0, remaining: 0 });

        let after = f.store.list_wraps(None, 100).await.expect("wraps after");
        assert_eq!(after.len(), wraps);
        let new_version = &after[0].kek_version;
        assert!(after.iter().all(|w| &w.kek_version == new_version), "every wrap on the new primary: {after:?}");
        assert!(before.iter().all(|w| &w.kek_version != new_version), "the primary changed");
    } else {
        let mut remaining = u32::MAX;
        for _ in 0..10 {
            remaining = f.vault.rewrap(500).await.expect("rewrap").remaining;
            if remaining == 0 {
                break;
            }
        }
        assert_eq!(remaining, 0, "rewrap converges");
    }

    clock.advance(PAST_CACHES);
    let (_, value) = f.vault.load(&SecretScope::Platform, &platform).await.expect("load v2");
    assert_eq!(value.expose(), b"new client secret");
    let value = f.vault.load_version(&SecretScope::Platform, &platform, SecretVersion(1)).await.expect("load retired v1");
    assert_eq!(value.expose(), b"old client secret");
    let (_, value) = f.vault.load(&SecretScope::Org(org_a), &tenant).await.expect("load org secret");
    assert_eq!(value.expose(), b"lin_oauth_token");
    assert_eq!(f.vault.decrypt(org_a, b"a", &data_a).await.expect("decrypt a").expose(), b"data of a");
    assert_eq!(f.vault.decrypt(org_b, b"b", &data_b).await.expect("decrypt b").expose(), b"data of b");
    let fresh = f.vault.encrypt(org_a, b"a", b"after rewrap").await.expect("encrypt after rewrap");
    assert_eq!(f.vault.decrypt(org_a, b"a", &fresh).await.expect("decrypt fresh").expose(), b"after rewrap");
}

#[cfg(test)]
mod tests {
    use mage_core::testing::SeqIdGen;

    use super::*;
    use crate::adapters::driven::pg::PgSecretStore;
    use crate::testing::pg::with_pg;
    use crate::testing::seed::Seed;

    #[tokio::test]
    async fn mem_secret_vault_passes_the_vault_contract() {
        contract_secret_vault(|clock| async move {
            let vault = Arc::new(MemSecretVault::new(clock));
            let store: Arc<dyn SecretStore> = vault.store();
            VaultFixture { vault, store, rotate_kek: None }
        })
        .await;
    }

    #[tokio::test]
    async fn envelope_vault_with_a_rotating_kek_passes_the_vault_contract() {
        contract_secret_vault(|clock| async move {
            let store = Arc::new(MemSecretStore::new(clock.clone()));
            let kek = Arc::new(RotatingKek::new());
            let vault = Arc::new(EnvelopeVault::new(store.clone(), kek.clone(), clock));
            VaultFixture { vault, store, rotate_kek: Some(Box::new(move || kek.rotate())) }
        })
        .await;
    }

    /// Postgres in Docker (`DATABASE_URL`, see `testing::pg`): local key files over `PgSecretStore`.
    #[tokio::test]
    async fn envelope_vault_over_postgres_passes_the_vault_contract() {
        with_pg("envelope_vault_over_postgres_passes_the_vault_contract", |env| async move {
            contract_secret_vault(|clock| {
                let env = env.clone();
                async move {
                    let db = env.fresh().await;
                    let seed = db.seed(clock.clone());
                    for org in vault_contract_orgs() {
                        seed.org(org).await;
                    }
                    let store = Arc::new(PgSecretStore::new(db.pools.clone(), clock.clone(), Arc::new(SeqIdGen::new())));
                    let kek = Arc::new(RotatingKek::new());
                    let vault = Arc::new(EnvelopeVault::new(store.clone(), kek.clone(), clock));
                    VaultFixture { vault, store, rotate_kek: Some(Box::new(move || kek.rotate())) }
                }
            })
            .await;
        })
        .await;
    }

    #[tokio::test]
    async fn mem_secret_vault_failure_injection_applies_to_every_call() {
        let vault = MemSecretVault::new(Arc::new(FakeClock::new()));
        let key = name("auth/csrf_key");
        vault.put(&SecretScope::Platform, &key, bytes(b"k"), Activation::Now, &Actor::System).await.expect("put");
        vault.set_failure(Some(VaultError::Unavailable("kms down".into())));
        assert_eq!(vault.load(&SecretScope::Platform, &key).await.err(), Some(VaultError::Unavailable("kms down".into())));
        assert!(vault.encrypt(vault_contract_orgs()[0], b"", b"x").await.is_err());
        vault.set_failure(None);
        assert_eq!(vault.load(&SecretScope::Platform, &key).await.expect("load").1.expose(), b"k");
    }
}
