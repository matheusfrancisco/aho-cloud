//! Platform keys (server.md "Signing keys and rotation", "Secret inventory", UC12, UC13):
//!
//! - [`EnsurePlatformKeys`]: at startup, create `auth/token_signing_key` and `auth/csrf_key` when
//!   they have no Active version.
//! - [`RotateSigningKey`]: job `auth.rotate_signing_key`, one idempotent transition per step
//!   (stage a new key → activate it after the staging window, retiring the old one → destroy
//!   retired keys after 25 h), plus the emergency [`RotateSigningKey::revoke`].
//! - [`RewrapSecrets`]: job `vault.rewrap`, rewrap DEKs onto the KEK's primary version.

use std::sync::Arc;

use rand::RngCore;
use rand::rngs::OsRng;
use time::{Duration, OffsetDateTime};
use tracing::{info, warn};

use mage_core::ports::Clock;
use mage_core::wire::token::TokenSigningKey;

use super::{RequestMeta, audit, event};
use crate::error::AppError;
use crate::ports::audit::{Actor, AuditAction, AuditLog, AuditTarget};
use crate::ports::auth::TokenSigner;
use crate::ports::vault::{
    Activation, RewrapReport, SecretBytes, SecretMeta, SecretName, SecretScope, SecretState, SecretVault, SecretVersion, VaultError, names,
    zero,
};

/// Bytes of an Ed25519 seed and of the CSRF HMAC key.
const KEY_BYTES: usize = 32;

/// Creates the platform keys the server cannot start without.
pub struct EnsurePlatformKeys {
    vault: Arc<dyn SecretVault>,
    actor: Actor,
}

impl EnsurePlatformKeys {
    pub fn new(vault: Arc<dyn SecretVault>, actor: Actor) -> EnsurePlatformKeys {
        EnsurePlatformKeys { vault, actor }
    }

    /// `auth/token_signing_key` (Ed25519 seed) and `auth/csrf_key` (HMAC key), each 32 random
    /// bytes, put Active unless an Active version exists. Safe to run on every instance at once.
    pub async fn run(&self) -> Result<(), AppError> {
        for name in [names::TOKEN_SIGNING_KEY, names::CSRF_KEY] {
            self.ensure(&SecretName::new(name)?).await?;
        }
        Ok(())
    }

    async fn ensure(&self, name: &SecretName) -> Result<(), AppError> {
        let scope = SecretScope::Platform;
        if has_active(&self.vault.versions(&scope, name).await?) {
            return Ok(());
        }
        match self.vault.put(&scope, name, random_key(), Activation::Now, &self.actor).await {
            Ok(meta) => {
                info!(%name, version = meta.version.0, "platform key created");
                Ok(())
            }
            // Another instance created it at the same time.
            Err(VaultError::Conflict) if has_active(&self.vault.versions(&scope, name).await?) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

fn has_active(versions: &[SecretMeta]) -> bool {
    versions.iter().any(|m| m.state == SecretState::Active)
}

/// Signing-key rotation schedule (server.md: every 30 days, 15 min staging, retired keys
/// destroyed after 25 h).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RotationPolicy {
    /// Age of the Active key at which a successor is staged.
    pub every: Duration,
    /// How long a staged key is published before it signs (verifiers refresh every 5 min).
    pub staging: Duration,
    /// How long a retired key keeps verifying (longest token lifetime + margin).
    pub destroy_retired_after: Duration,
}

impl Default for RotationPolicy {
    fn default() -> Self {
        RotationPolicy { every: Duration::days(30), staging: Duration::minutes(15), destroy_retired_after: Duration::hours(25) }
    }
}

/// What one [`RotateSigningKey::step`] changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RotationTransition {
    /// The ring had no key at all: a new Active key.
    Created { kid: String },
    Staged { kid: String },
    /// Staged → Active; the previous Active was retired in the same transaction.
    Activated { kid: String },
    Destroyed { kid: String },
}

impl RotationTransition {
    fn label(&self) -> &'static str {
        match self {
            RotationTransition::Created { .. } => "created",
            RotationTransition::Staged { .. } => "staged",
            RotationTransition::Activated { .. } => "activated",
            RotationTransition::Destroyed { .. } => "destroyed",
        }
    }

    pub fn kid(&self) -> &str {
        match self {
            RotationTransition::Created { kid }
            | RotationTransition::Staged { kid }
            | RotationTransition::Activated { kid }
            | RotationTransition::Destroyed { kid } => kid,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RotationStep {
    /// `None`: nothing was due.
    pub transition: Option<RotationTransition>,
    /// When the next transition is due (`<= now`: more work is pending now).
    pub next_at: OffsetDateTime,
}

enum Due {
    Create,
    Stage,
    Activate(SecretVersion),
    Destroy(SecretVersion),
}

pub struct RotateSigningKey {
    vault: Arc<dyn SecretVault>,
    signer: Arc<dyn TokenSigner>,
    audit: Arc<dyn AuditLog>,
    clock: Arc<dyn Clock>,
    policy: RotationPolicy,
}

impl RotateSigningKey {
    pub fn new(
        vault: Arc<dyn SecretVault>,
        signer: Arc<dyn TokenSigner>,
        audit: Arc<dyn AuditLog>,
        clock: Arc<dyn Clock>,
        policy: RotationPolicy,
    ) -> RotateSigningKey {
        RotateSigningKey { vault, signer, audit, clock, policy }
    }

    /// Inspects the key states, performs the one transition that is due (if any), reloads this
    /// instance's signer and audits `keys.rotated`. Running it again before `next_at` changes
    /// nothing.
    pub async fn step(&self) -> Result<RotationStep, AppError> {
        let now = self.clock.now();
        let versions = self.versions().await?;
        let Some(due) = self.due(&versions, now) else {
            return Ok(RotationStep { transition: None, next_at: self.next_at(&versions, now) });
        };
        let transition = match due {
            Due::Create => RotationTransition::Created { kid: self.put_key(Activation::Now, &Actor::System).await? },
            Due::Stage => RotationTransition::Staged { kid: self.put_key(Activation::Staged, &Actor::System).await? },
            Due::Activate(version) => {
                let kid = self.kid_of(version).await?;
                self.vault.set_state(&SecretScope::Platform, &key_name()?, version, SecretState::Active, &Actor::System).await?;
                RotationTransition::Activated { kid }
            }
            Due::Destroy(version) => {
                let kid = self.kid_of(version).await?;
                self.vault.set_state(&SecretScope::Platform, &key_name()?, version, SecretState::Destroyed, &Actor::System).await?;
                RotationTransition::Destroyed { kid }
            }
        };
        info!(transition = transition.label(), kid = transition.kid(), "token signing key rotation");
        self.signer.reload().await?;
        let target = AuditTarget::new("signing_key", transition.kid());
        audit(&*self.audit, event(None, Actor::System, AuditAction::KeysRotated, target, &RequestMeta::default()).detail("transition", transition.label()))
            .await?;
        let versions = self.versions().await?;
        Ok(RotationStep { transition: Some(transition), next_at: self.next_at(&versions, self.clock.now()) })
    }

    /// Emergency (`mage admin keys revoke <kid>`): a new key becomes Active now and `kid` is
    /// destroyed now; tokens it signed stop verifying on every instance's next reload (this one
    /// reloads at once). Returns the new Active kid.
    pub async fn revoke(&self, kid: &str, actor: &Actor) -> Result<String, AppError> {
        let not_found = || AppError::NotFound(format!("signing key {kid}"));
        let version = kid
            .strip_prefix('v')
            .and_then(|rest| rest.split_once('-'))
            .and_then(|(v, _)| v.parse::<u32>().ok())
            .map(SecretVersion)
            .ok_or_else(not_found)?;
        let versions = self.versions().await?;
        if !versions.iter().any(|m| m.version == version && m.state != SecretState::Destroyed) || self.kid_of(version).await? != kid {
            return Err(not_found());
        }
        let new_kid = self.put_key(Activation::Now, actor).await?;
        self.vault.set_state(&SecretScope::Platform, &key_name()?, version, SecretState::Destroyed, actor).await?;
        warn!(revoked = kid, active = %new_kid, "token signing key revoked");
        self.signer.reload().await?;
        let target = AuditTarget::new("signing_key", kid);
        audit(&*self.audit, event(None, actor.clone(), AuditAction::KeysRevoked, target, &RequestMeta::default()).detail("new_kid", new_kid.clone()))
            .await?;
        Ok(new_kid)
    }

    async fn versions(&self) -> Result<Vec<SecretMeta>, AppError> {
        match self.vault.versions(&SecretScope::Platform, &key_name()?).await {
            Ok(versions) => Ok(versions),
            Err(VaultError::NotFound) => Ok(Vec::new()),
            Err(e) => Err(e.into()),
        }
    }

    fn due(&self, versions: &[SecretMeta], now: OffsetDateTime) -> Option<Due> {
        let active = versions.iter().filter(|m| m.state == SecretState::Active).max_by_key(|m| m.version);
        let staged = versions.iter().filter(|m| m.state == SecretState::Staged).min_by_key(|m| m.version);
        let Some(active) = active else {
            // Nothing signs: activate a staged key at once, or create one.
            return Some(staged.map_or(Due::Create, |s| Due::Activate(s.version)));
        };
        if let Some(staged) = staged.filter(|s| s.state_changed_at + self.policy.staging <= now) {
            return Some(Due::Activate(staged.version));
        }
        let expired = versions
            .iter()
            .filter(|m| m.state == SecretState::Retired && m.state_changed_at + self.policy.destroy_retired_after <= now)
            .min_by_key(|m| m.version);
        if let Some(retired) = expired {
            return Some(Due::Destroy(retired.version));
        }
        if staged.is_none() && active.state_changed_at + self.policy.every <= now {
            return Some(Due::Stage);
        }
        None
    }

    fn next_at(&self, versions: &[SecretMeta], now: OffsetDateTime) -> OffsetDateTime {
        let active = versions.iter().filter(|m| m.state == SecretState::Active).max_by_key(|m| m.version);
        let staged = versions.iter().filter(|m| m.state == SecretState::Staged).min_by_key(|m| m.version);
        let Some(active) = active else {
            return now;
        };
        let promote = match staged {
            Some(staged) => staged.state_changed_at + self.policy.staging,
            None => active.state_changed_at + self.policy.every,
        };
        let next = versions
            .iter()
            .filter(|m| m.state == SecretState::Retired)
            .map(|m| m.state_changed_at + self.policy.destroy_retired_after)
            .fold(promote, OffsetDateTime::min);
        next.max(now)
    }

    /// Puts a fresh seed; returns its kid.
    async fn put_key(&self, activation: Activation, actor: &Actor) -> Result<String, AppError> {
        let mut seed = [0u8; KEY_BYTES];
        OsRng.fill_bytes(&mut seed);
        let value = SecretBytes::new(seed.to_vec());
        let meta = self.vault.put(&SecretScope::Platform, &key_name()?, value, activation, actor).await;
        let kid = meta.map(|meta| TokenSigningKey::from_seed(meta.version.0, &seed).kid().to_owned());
        zero(&mut seed);
        Ok(kid?)
    }

    async fn kid_of(&self, version: SecretVersion) -> Result<String, AppError> {
        let seed = self.vault.load_version(&SecretScope::Platform, &key_name()?, version).await?;
        let mut seed: [u8; KEY_BYTES] = seed
            .expose()
            .try_into()
            .map_err(|_| AppError::Internal(format!("{} version {} is not a 32-byte seed", names::TOKEN_SIGNING_KEY, version.0)))?;
        let kid = TokenSigningKey::from_seed(version.0, &seed).kid().to_owned();
        zero(&mut seed);
        Ok(kid)
    }
}

fn key_name() -> Result<SecretName, AppError> {
    Ok(SecretName::new(names::TOKEN_SIGNING_KEY)?)
}

fn random_key() -> SecretBytes {
    let mut bytes = vec![0u8; KEY_BYTES];
    OsRng.fill_bytes(&mut bytes);
    SecretBytes::new(bytes)
}

/// Rewraps every DEK onto the KEK's primary version (job `vault.rewrap`).
pub struct RewrapSecrets {
    vault: Arc<dyn SecretVault>,
}

impl RewrapSecrets {
    /// Wraps moved per `SecretVault::rewrap` call.
    pub const BATCH: u32 = 500;

    pub fn new(vault: Arc<dyn SecretVault>) -> RewrapSecrets {
        RewrapSecrets { vault }
    }

    /// Batches until nothing remains. Fails when a batch makes no progress (wraps that fail
    /// their integrity check stay behind; the vault logs each).
    pub async fn run(&self) -> Result<RewrapReport, AppError> {
        let mut total = RewrapReport::default();
        loop {
            let batch = self.vault.rewrap(RewrapSecrets::BATCH).await?;
            total.scanned = batch.scanned;
            total.rewrapped += batch.rewrapped;
            total.remaining = batch.remaining;
            if batch.remaining == 0 {
                return Ok(total);
            }
            if batch.rewrapped == 0 {
                return Err(AppError::Internal(format!("{} wrapped keys cannot be rewrapped (integrity failures, see logs)", batch.remaining)));
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use mage_core::domain::RunnerId;
    use mage_core::testing::FakeClock;
    use uuid::Uuid;

    use super::*;
    use crate::adapters::driven::signer::Ed25519TokenSigner;
    use crate::ports::auth::{TokenClaims, TokenError, TokenKind, TokenScope};
    use crate::testing::{MemAuditLog, MemSecretVault};

    const ISSUER: &str = "https://mage.example.com";

    struct World {
        clock: Arc<FakeClock>,
        vault: Arc<MemSecretVault>,
        signer: Arc<Ed25519TokenSigner>,
        audit: Arc<MemAuditLog>,
        rotate: RotateSigningKey,
    }

    async fn world() -> World {
        let clock = Arc::new(FakeClock::new());
        let vault = Arc::new(MemSecretVault::new(clock.clone()));
        EnsurePlatformKeys::new(vault.clone(), Actor::System).run().await.expect("ensure keys");
        let signer = Arc::new(Ed25519TokenSigner::new(vault.clone(), ISSUER));
        signer.reload().await.expect("reload");
        let audit = Arc::new(MemAuditLog::new(clock.clone()));
        let rotate = RotateSigningKey::new(vault.clone(), signer.clone(), audit.clone(), clock.clone(), RotationPolicy::default());
        World { clock, vault, signer, audit, rotate }
    }

    fn claims(now: OffsetDateTime) -> TokenClaims {
        TokenClaims {
            scope: TokenScope::Runner { runner: RunnerId::from_uuid(Uuid::from_u128(1)), generation: 1, instance: Uuid::from_u128(2) },
            iat: now,
            exp: now + Duration::minutes(15),
            jti: Uuid::from_u128(3),
        }
    }

    fn states(versions: &[SecretMeta]) -> Vec<(u32, SecretState)> {
        versions.iter().map(|m| (m.version.0, m.state)).collect()
    }

    async fn key_states(w: &World) -> Vec<(u32, SecretState)> {
        states(&w.vault.versions(&SecretScope::Platform, &key_name().expect("name")).await.expect("versions"))
    }

    #[tokio::test]
    async fn ensure_creates_both_keys_once() {
        let clock = Arc::new(FakeClock::new());
        let vault = Arc::new(MemSecretVault::new(clock.clone()));
        let ensure = EnsurePlatformKeys::new(vault.clone(), Actor::Operator { os_user: "ops".into() });
        ensure.run().await.expect("first run");
        ensure.run().await.expect("second run");
        for name in [names::TOKEN_SIGNING_KEY, names::CSRF_KEY] {
            let name = SecretName::new(name).expect("name");
            let versions = vault.versions(&SecretScope::Platform, &name).await.expect("versions");
            assert_eq!(states(&versions), vec![(1, SecretState::Active)], "{name}");
            assert_eq!(vault.load(&SecretScope::Platform, &name).await.expect("load").1.len(), 32);
        }
        let seed = vault.load(&SecretScope::Platform, &key_name().expect("name")).await.expect("seed").1;
        let csrf = vault.load(&SecretScope::Platform, &SecretName::new(names::CSRF_KEY).expect("name")).await.expect("csrf").1;
        assert!(seed != csrf, "independent random keys");
    }

    #[tokio::test]
    async fn concurrent_ensures_leave_one_active_key() {
        let clock = Arc::new(FakeClock::new());
        let vault = Arc::new(MemSecretVault::new(clock));
        let a = EnsurePlatformKeys::new(vault.clone(), Actor::System);
        let b = EnsurePlatformKeys::new(vault.clone(), Actor::System);
        let (ra, rb) = tokio::join!(a.run(), b.run());
        ra.expect("instance a");
        rb.expect("instance b");
        let versions = vault.versions(&SecretScope::Platform, &key_name().expect("name")).await.expect("versions");
        assert_eq!(versions.iter().filter(|m| m.state == SecretState::Active).count(), 1);
    }

    /// staged → active after 15 min → old key retired but still verifies → destroyed after 25 h.
    #[tokio::test]
    async fn rotation_end_to_end_under_a_fake_clock() {
        let w = world().await;
        let t0 = w.clock.now();
        let old_token = w.signer.sign(&claims(t0)).expect("sign with v1");
        let old_kid = w.signer.public_keys()[0].kid.clone();

        let idle = w.rotate.step().await.expect("idle step");
        assert_eq!(idle, RotationStep { transition: None, next_at: t0 + Duration::days(30) });

        w.clock.advance(std::time::Duration::from_secs(30 * 24 * 3600));
        let staged = w.rotate.step().await.expect("stage");
        let Some(RotationTransition::Staged { kid: new_kid }) = staged.transition.clone() else {
            panic!("expected Staged, got {staged:?}");
        };
        assert!(new_kid.starts_with("v2-"));
        assert_eq!(staged.next_at, w.clock.now() + Duration::minutes(15));
        assert_eq!(key_states(&w).await, vec![(1, SecretState::Active), (2, SecretState::Staged)]);
        assert!(w.signer.public_keys().iter().any(|k| k.kid == new_kid && k.state == SecretState::Staged), "signer reloaded");
        let active: Vec<String> = w.signer.public_keys().into_iter().filter(|k| k.state == SecretState::Active).map(|k| k.kid).collect();
        assert_eq!(active, vec![old_kid.clone()], "the staged key does not sign yet");

        w.clock.advance(std::time::Duration::from_secs(14 * 60));
        assert_eq!(w.rotate.step().await.expect("too early").transition, None, "staging window not over");

        w.clock.advance(std::time::Duration::from_secs(60));
        let activated = w.rotate.step().await.expect("activate");
        assert_eq!(activated.transition, Some(RotationTransition::Activated { kid: new_kid.clone() }));
        assert_eq!(activated.next_at, w.clock.now() + Duration::hours(25), "next: destroy the retired key");
        assert_eq!(key_states(&w).await, vec![(1, SecretState::Retired), (2, SecretState::Active)]);
        let now = w.clock.now();
        let new_token = w.signer.sign(&claims(now)).expect("sign with v2");
        assert!(w.signer.verify(old_token.as_str(), TokenKind::Runner, t0).is_ok(), "retired key still verifies");
        assert!(w.signer.verify(new_token.as_str(), TokenKind::Runner, now).is_ok());
        let kids: Vec<String> = w.signer.public_keys().into_iter().filter(|k| k.state == SecretState::Active).map(|k| k.kid).collect();
        assert_eq!(kids, vec![new_kid.clone()], "the new key signs");

        w.clock.advance(std::time::Duration::from_secs(25 * 3600 - 1));
        assert_eq!(w.rotate.step().await.expect("before 25 h").transition, None);
        w.clock.advance(std::time::Duration::from_secs(1));
        let destroyed = w.rotate.step().await.expect("destroy");
        assert_eq!(destroyed.transition, Some(RotationTransition::Destroyed { kid: old_kid.clone() }));
        assert_eq!(key_states(&w).await, vec![(1, SecretState::Destroyed), (2, SecretState::Active)]);
        assert_eq!(w.signer.verify(old_token.as_str(), TokenKind::Runner, t0).err(), Some(TokenError::UnknownKey));
        assert!(w.signer.verify(new_token.as_str(), TokenKind::Runner, now).is_ok());
        assert_eq!(destroyed.next_at, now + Duration::days(30), "next: the 30-day rotation of v2");

        let records = w.audit.records();
        let rotated: Vec<(String, String)> = records
            .iter()
            .filter(|r| r.event.action == AuditAction::KeysRotated)
            .map(|r| (r.event.target.id.clone(), r.event.details[0].1.clone()))
            .collect();
        assert_eq!(
            rotated,
            vec![(new_kid.clone(), "staged".into()), (new_kid, "activated".into()), (old_kid, "destroyed".into())]
        );
        assert!(records.iter().all(|r| r.event.org.is_none() && r.event.actor == Actor::System));
    }

    #[tokio::test]
    async fn a_ring_without_keys_or_active_key_heals_in_one_step() {
        let clock = Arc::new(FakeClock::new());
        let vault = Arc::new(MemSecretVault::new(clock.clone()));
        let signer = Arc::new(Ed25519TokenSigner::new(vault.clone(), ISSUER));
        let audit = Arc::new(MemAuditLog::new(clock.clone()));
        let rotate = RotateSigningKey::new(vault.clone(), signer.clone(), audit, clock.clone(), RotationPolicy::default());
        let step = rotate.step().await.expect("create");
        assert!(matches!(step.transition, Some(RotationTransition::Created { .. })));
        assert!(signer.is_ready());

        // Active destroyed by hand, a staged successor waiting: activate it at once.
        let name = key_name().expect("name");
        vault.put(&SecretScope::Platform, &name, random_key(), Activation::Staged, &Actor::System).await.expect("stage");
        vault.set_state(&SecretScope::Platform, &name, SecretVersion(1), SecretState::Destroyed, &Actor::System).await.expect("destroy");
        let step = rotate.step().await.expect("activate");
        assert!(matches!(step.transition, Some(RotationTransition::Activated { .. })), "{step:?}");
        assert!(signer.is_ready());
    }

    #[tokio::test]
    async fn revoke_destroys_the_key_and_activates_a_new_one_now() {
        let w = world().await;
        let now = w.clock.now();
        let token = w.signer.sign(&claims(now)).expect("sign");
        let kid = w.signer.public_keys()[0].kid.clone();
        let operator = Actor::Operator { os_user: "alice".into() };

        assert!(matches!(w.rotate.revoke("v9-00000000", &operator).await, Err(AppError::NotFound(_))));
        assert!(matches!(w.rotate.revoke("v1-00000000", &operator).await, Err(AppError::NotFound(_))), "version matches, key does not");
        assert!(matches!(w.rotate.revoke("garbage", &operator).await, Err(AppError::NotFound(_))));

        let new_kid = w.rotate.revoke(&kid, &operator).await.expect("revoke");
        assert!(new_kid.starts_with("v2-"));
        assert_eq!(key_states(&w).await, vec![(1, SecretState::Destroyed), (2, SecretState::Active)]);
        assert_eq!(w.signer.verify(token.as_str(), TokenKind::Runner, now).err(), Some(TokenError::UnknownKey));
        let fresh = w.signer.sign(&claims(now)).expect("sign with the new key");
        assert!(w.signer.verify(fresh.as_str(), TokenKind::Runner, now).is_ok());

        let revoked: Vec<_> = w.audit.records().into_iter().filter(|r| r.event.action == AuditAction::KeysRevoked).collect();
        assert_eq!(revoked.len(), 1);
        assert_eq!((revoked[0].event.target.id.as_str(), &revoked[0].event.actor), (kid.as_str(), &operator));
        assert!(matches!(w.rotate.revoke(&kid, &operator).await, Err(AppError::NotFound(_))), "already destroyed");
    }

    #[tokio::test]
    async fn rewrap_with_a_fixed_kek_has_nothing_to_do() {
        let clock = Arc::new(FakeClock::new());
        let vault = Arc::new(MemSecretVault::new(clock));
        EnsurePlatformKeys::new(vault.clone(), Actor::System).run().await.expect("ensure");
        let report = RewrapSecrets::new(vault).run().await.expect("rewrap");
        assert_eq!(report, RewrapReport { scanned: 2, rewrapped: 0, remaining: 0 });
    }
}
