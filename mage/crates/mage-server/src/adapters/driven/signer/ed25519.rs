//! [`Ed25519TokenSigner`]: the key ring is every non-destroyed version of the platform secret
//! `auth/token_signing_key` (32-byte Ed25519 seeds), held as an immutable snapshot behind an
//! `ArcSwap` and rebuilt by `reload`. Signs with the single Active version; verifies against
//! Staged, Active and Retired versions. Codec: `mage_core::wire::token`.

use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use async_trait::async_trait;
use time::OffsetDateTime;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use mage_core::wire::token::{KeyRing, TokenCodec, TokenSigningKey};

use crate::ports::auth::{PublicKeyInfo, RunnerAssertion, SignedToken, TokenClaims, TokenError, TokenKind, TokenSigner};
use crate::ports::vault::{SecretName, SecretScope, SecretState, SecretVault, VaultError, names, zero};

/// Reload period of the key ring (server.md: every 5 min).
pub const RELOAD_EVERY: Duration = Duration::from_secs(5 * 60);
/// Retry period of the reload loop while the signer is not ready.
const RETRY_UNREADY: Duration = Duration::from_secs(5);

#[derive(Default)]
struct Ring {
    loaded: bool,
    active: Option<TokenSigningKey>,
    verify: KeyRing,
    public: Vec<PublicKeyInfo>,
}

pub struct Ed25519TokenSigner {
    vault: Arc<dyn SecretVault>,
    codec: TokenCodec,
    ring: ArcSwap<Ring>,
}

impl Ed25519TokenSigner {
    /// `issuer` is the server's `public_url` without a trailing slash. The ring is empty until
    /// the first successful [`TokenSigner::reload`].
    pub fn new(vault: Arc<dyn SecretVault>, issuer: impl Into<String>) -> Ed25519TokenSigner {
        Ed25519TokenSigner { vault, codec: TokenCodec::new(issuer), ring: ArcSwap::from_pointee(Ring::default()) }
    }

    pub fn issuer(&self) -> &str {
        self.codec.issuer()
    }

    /// Loaded with an Active key: the signer can sign (`/ready`).
    pub fn is_ready(&self) -> bool {
        let ring = self.ring.load();
        ring.loaded && ring.active.is_some()
    }

    /// Reloads the ring every `every` (every few seconds while not ready) until `shutdown`.
    /// The startup load is the caller's (readiness waits for it).
    pub fn spawn_reload_loop(self: Arc<Self>, every: Duration, shutdown: CancellationToken) -> JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                let wait = if self.is_ready() { every } else { every.min(RETRY_UNREADY) };
                tokio::select! {
                    () = shutdown.cancelled() => break,
                    () = tokio::time::sleep(wait) => {}
                }
                if let Err(e) = self.reload().await {
                    warn!(error = %e, ready = self.is_ready(), "signing key ring reload failed; keeping the previous ring");
                }
            }
        })
    }

    async fn load_ring(&self) -> Result<Ring, TokenError> {
        let scope = SecretScope::Platform;
        let name = SecretName::new(names::TOKEN_SIGNING_KEY).map_err(unavailable)?;
        let versions = match self.vault.versions(&scope, &name).await {
            Ok(versions) => versions,
            Err(VaultError::NotFound) => Vec::new(),
            Err(e) => return Err(unavailable(e)),
        };
        let mut ring = Ring { loaded: true, ..Ring::default() };
        let mut active_version = 0;
        for meta in versions.iter().filter(|m| m.state != SecretState::Destroyed) {
            let seed = match self.vault.load_version(&scope, &name, meta.version).await {
                Ok(seed) => seed,
                // Destroyed between `versions` and here.
                Err(VaultError::NotFound) => continue,
                Err(e) => return Err(unavailable(e)),
            };
            let mut seed_bytes: [u8; 32] = seed.expose().try_into().map_err(|_| {
                TokenError::Unavailable(format!("{} version {} is not a 32-byte Ed25519 seed", names::TOKEN_SIGNING_KEY, meta.version))
            })?;
            let key = TokenSigningKey::from_seed(meta.version.0, &seed_bytes);
            zero(&mut seed_bytes);
            let public_key = key.public_key();
            ring.verify.insert(key.kid(), &public_key)?;
            ring.public.push(PublicKeyInfo { kid: key.kid().to_owned(), state: meta.state, public_key });
            if meta.state == SecretState::Active && meta.version.0 > active_version {
                active_version = meta.version.0;
                ring.active = Some(key);
            }
        }
        Ok(ring)
    }
}

#[async_trait]
impl TokenSigner for Ed25519TokenSigner {
    fn sign(&self, claims: &TokenClaims) -> Result<SignedToken, TokenError> {
        let ring = self.ring.load();
        let key = ring.active.as_ref().ok_or_else(|| TokenError::Unavailable("no active token signing key".into()))?;
        Ok(SignedToken::new(self.codec.sign(key, claims)?))
    }

    fn verify(&self, token: &str, expected: TokenKind, now: OffsetDateTime) -> Result<TokenClaims, TokenError> {
        let ring = self.ring.load();
        if !ring.loaded {
            return Err(TokenError::Unavailable("token signing key ring not loaded".into()));
        }
        Ok(self.codec.verify(&ring.verify, token, expected, now)?)
    }

    fn verify_assertion(&self, assertion: &str, runner_public_key: &[u8; 32], now: OffsetDateTime) -> Result<RunnerAssertion, TokenError> {
        Ok(self.codec.verify_assertion(assertion, runner_public_key, now)?)
    }

    async fn reload(&self) -> Result<(), TokenError> {
        let ring = self.load_ring().await?;
        let kids: Vec<String> = ring.public.iter().map(|k| format!("{}:{}", k.kid, k.state)).collect();
        let changed = {
            let old = self.ring.load();
            !old.loaded || old.public != ring.public
        };
        if ring.active.is_none() {
            warn!(keys = ?kids, "token signing key ring has no active key");
        } else if changed {
            info!(keys = ?kids, "token signing key ring loaded");
        }
        self.ring.store(Arc::new(ring));
        Ok(())
    }

    fn public_keys(&self) -> Vec<PublicKeyInfo> {
        self.ring.load().public.clone()
    }
}

fn unavailable(e: VaultError) -> TokenError {
    TokenError::Unavailable(format!("token signing keys: {e}"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use mage_core::domain::RunnerId;
    use mage_core::ports::Clock;
    use mage_core::testing::FakeClock;
    use uuid::Uuid;

    use super::*;
    use crate::ports::audit::Actor;
    use crate::ports::auth::TokenScope;
    use crate::ports::vault::{Activation, SecretBytes, SecretVersion};
    use crate::testing::{MemSecretVault, SignerFixture, contract_token_signer};

    const ISSUER: &str = "https://mage.example.com";

    fn key_name() -> SecretName {
        SecretName::new(names::TOKEN_SIGNING_KEY).expect("name")
    }

    #[tokio::test]
    async fn ed25519_signer_over_mem_vault_passes_the_signer_contract() {
        contract_token_signer(|clock| async move {
            let vault: Arc<dyn SecretVault> = Arc::new(MemSecretVault::new(clock));
            vault
                .put(&SecretScope::Platform, &key_name(), SecretBytes::new(vec![0x11; 32]), Activation::Now, &Actor::System)
                .await
                .expect("put signing key");
            let signer = Arc::new(Ed25519TokenSigner::new(vault.clone(), ISSUER));
            signer.reload().await.expect("reload");
            SignerFixture { signer, issuer: ISSUER.into(), vault: Some(vault) }
        })
        .await;
    }

    fn runner_claims(now: OffsetDateTime) -> TokenClaims {
        TokenClaims {
            scope: TokenScope::Runner { runner: RunnerId::from_uuid(Uuid::from_u128(1)), generation: 1, instance: Uuid::from_u128(2) },
            iat: now,
            exp: now + time::Duration::minutes(15),
            jti: Uuid::from_u128(3),
        }
    }

    #[tokio::test]
    async fn not_ready_until_an_active_key_is_loaded() {
        let clock = Arc::new(FakeClock::new());
        let vault = Arc::new(MemSecretVault::new(clock.clone()));
        let signer = Ed25519TokenSigner::new(vault.clone(), ISSUER);
        let claims = runner_claims(clock.now());
        assert!(!signer.is_ready());
        assert!(matches!(signer.sign(&claims), Err(TokenError::Unavailable(_))));
        assert!(matches!(signer.verify("mgr_a.b.c", TokenKind::Runner, clock.now()), Err(TokenError::Unavailable(_))), "ring not loaded");

        signer.reload().await.expect("reload of an empty ring");
        assert!(!signer.is_ready(), "no key at all");
        assert!(signer.public_keys().is_empty());

        vault
            .put(&SecretScope::Platform, &key_name(), SecretBytes::new(vec![0x22; 32]), Activation::Staged, &Actor::System)
            .await
            .expect("put staged");
        signer.reload().await.expect("reload");
        assert!(!signer.is_ready(), "a staged key does not sign");
        assert!(matches!(signer.sign(&claims), Err(TokenError::Unavailable(_))));
        assert_eq!(signer.public_keys().len(), 1);

        vault
            .set_state(&SecretScope::Platform, &key_name(), SecretVersion(1), SecretState::Active, &Actor::System)
            .await
            .expect("activate");
        signer.reload().await.expect("reload");
        assert!(signer.is_ready());
        let token = signer.sign(&claims).expect("sign");
        assert_eq!(signer.verify(token.as_str(), TokenKind::Runner, clock.now()).expect("verify"), claims);
    }

    #[tokio::test]
    async fn a_failed_reload_keeps_the_previous_ring() {
        let clock = Arc::new(FakeClock::new());
        let vault = Arc::new(MemSecretVault::new(clock.clone()));
        vault
            .put(&SecretScope::Platform, &key_name(), SecretBytes::new(vec![0x33; 32]), Activation::Now, &Actor::System)
            .await
            .expect("put");
        let signer = Ed25519TokenSigner::new(vault.clone(), ISSUER);
        signer.reload().await.expect("reload");
        vault.set_failure(Some(VaultError::Unavailable("kms down".into())));
        assert!(matches!(signer.reload().await, Err(TokenError::Unavailable(_))));
        assert!(signer.is_ready());
        assert!(signer.sign(&runner_claims(clock.now())).is_ok());

        vault.set_failure(None);
        vault
            .put(&SecretScope::Platform, &key_name(), SecretBytes::new(vec![1; 31]), Activation::Now, &Actor::System)
            .await
            .expect("put malformed seed");
        assert!(matches!(signer.reload().await, Err(TokenError::Unavailable(m)) if m.contains("32-byte")));
        assert!(signer.is_ready(), "previous ring kept");
    }

    #[tokio::test]
    async fn reload_loop_picks_up_new_keys_and_stops_on_shutdown() {
        let clock = Arc::new(FakeClock::new());
        let vault = Arc::new(MemSecretVault::new(clock.clone()));
        let signer = Arc::new(Ed25519TokenSigner::new(vault.clone(), ISSUER));
        let shutdown = CancellationToken::new();
        let handle = signer.clone().spawn_reload_loop(Duration::from_millis(20), shutdown.clone());
        vault
            .put(&SecretScope::Platform, &key_name(), SecretBytes::new(vec![0x44; 32]), Activation::Now, &Actor::System)
            .await
            .expect("put");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !signer.is_ready() {
            assert!(tokio::time::Instant::now() < deadline, "reload loop never loaded the key");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(5), handle).await.expect("loop stops").expect("loop task");
    }
}
