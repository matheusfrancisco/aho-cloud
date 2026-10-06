//! Platform maintenance jobs (server.md "Background jobs: decision", UC12, UC13):
//! `auth.rotate_signing_key`, `auth.sweep_sessions`, `vault.rewrap`. Tickers enqueue them; the
//! rotation job also chains itself while a transition is close (staging window, retired-key
//! destruction).

use std::sync::Arc;

use async_trait::async_trait;
use time::Duration;
use tracing::info;

use mage_core::domain::job::{JobKind, JobPayload};
use mage_core::ports::Clock;
use mage_core::ports::jobs::{LeasedJob, NewJob};

use crate::adapters::driving::worker::{JobDone, JobHandler, Periodic, Ticker};
use crate::app::auth::keys::{RewrapSecrets, RotateSigningKey};
use crate::error::AppError;
use crate::ports::tokens::TokenStore;

const KINDS: [&str; 3] = [JobKind::AuthRotateSigningKey.as_str(), JobKind::AuthSweepSessions.as_str(), JobKind::VaultRewrap.as_str()];

/// Rows per `TokenStore::purge_expired` call.
pub const SWEEP_BATCH: u32 = 1000;
/// A rotation transition due within this horizon is chained as a follow-up job; later ones are
/// left to the daily ticker.
pub const ROTATION_CHAIN_HORIZON: Duration = Duration::days(1);

/// Tickers of the platform jobs, for `Worker::add_periodic`: `auth.sweep_sessions` hourly,
/// `auth.rotate_signing_key` and `vault.rewrap` daily (one job per bucket across instances).
pub fn platform_tickers() -> Vec<Arc<dyn Periodic>> {
    const HOUR: std::time::Duration = std::time::Duration::from_secs(3600);
    const DAY: std::time::Duration = std::time::Duration::from_secs(24 * 3600);
    vec![
        Arc::new(Ticker::new(JobPayload::AuthSweepSessions, HOUR)),
        Arc::new(Ticker::new(JobPayload::AuthRotateSigningKey, DAY)),
        Arc::new(Ticker::new(JobPayload::VaultRewrap, DAY)),
    ]
}

pub struct PlatformJobs {
    rotate: Arc<RotateSigningKey>,
    rewrap: Arc<RewrapSecrets>,
    tokens: Arc<dyn TokenStore>,
    clock: Arc<dyn Clock>,
}

impl PlatformJobs {
    pub fn new(rotate: Arc<RotateSigningKey>, rewrap: Arc<RewrapSecrets>, tokens: Arc<dyn TokenStore>, clock: Arc<dyn Clock>) -> PlatformJobs {
        PlatformJobs { rotate, rewrap, tokens, clock }
    }

    async fn rotate_signing_key(&self) -> Result<JobDone, AppError> {
        let step = self.rotate.step().await?;
        if step.next_at > self.clock.now() + ROTATION_CHAIN_HORIZON {
            return Ok(JobDone::complete());
        }
        let next = NewJob::new(JobPayload::AuthRotateSigningKey)
            .dedupe(format!("{}:{}", JobKind::AuthRotateSigningKey.as_str(), step.next_at.unix_timestamp()))
            .after(step.next_at);
        Ok(JobDone::follow_up(vec![next]))
    }

    async fn sweep_sessions(&self) -> Result<JobDone, AppError> {
        let mut purged = 0u64;
        loop {
            let deleted = self.tokens.purge_expired(SWEEP_BATCH).await?;
            purged += deleted;
            // Up to SWEEP_BATCH rows per kind per call: below that, every kind is exhausted.
            if deleted < u64::from(SWEEP_BATCH) {
                break;
            }
        }
        info!(purged, "expired sessions, states, bootstrap tokens and revocations purged");
        Ok(JobDone::complete())
    }

    async fn rewrap_secrets(&self) -> Result<JobDone, AppError> {
        let report = self.rewrap.run().await?;
        info!(scanned = report.scanned, rewrapped = report.rewrapped, "vault rewrap finished");
        Ok(JobDone::complete())
    }
}

#[async_trait]
impl JobHandler for PlatformJobs {
    fn kinds(&self) -> &'static [&'static str] {
        &KINDS
    }

    async fn handle(&self, job: &LeasedJob) -> Result<JobDone, AppError> {
        match &job.payload {
            JobPayload::AuthRotateSigningKey => self.rotate_signing_key().await,
            JobPayload::AuthSweepSessions => self.sweep_sessions().await,
            JobPayload::VaultRewrap => self.rewrap_secrets().await,
            other => Err(AppError::Invalid(format!("platform jobs do not handle {}", other.kind()))),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use mage_core::domain::{JobId, OrgId};
    use mage_core::ports::jobs::JobLease;
    use mage_core::testing::FakeClock;
    use uuid::Uuid;

    use super::*;
    use crate::adapters::driven::signer::Ed25519TokenSigner;
    use crate::adapters::driven::vault::EnvelopeVault;
    use crate::app::auth::keys::{EnsurePlatformKeys, RotationPolicy};
    use crate::ports::audit::Actor;
    use crate::ports::auth::TokenSigner;
    use crate::ports::secrets::SecretStore;
    use crate::ports::tokens::{Jti, Revocation};
    use crate::ports::vault::{Activation, SecretBytes, SecretName, SecretScope, SecretVault};
    use crate::testing::{MemAuditLog, MemSecretStore, MemSecretVault, MemTokenStore, RotatingKek};

    fn leased(payload: JobPayload, clock: &FakeClock) -> LeasedJob {
        let now = clock.now();
        LeasedJob {
            lease: JobLease { job_id: JobId::from_uuid(Uuid::from_u128(1)), lease_seq: 1 },
            kind: payload.kind(),
            org_id: None,
            payload,
            attempt: 1,
            max_attempts: 5,
            leased_until: now + Duration::minutes(5),
            enqueued_at: now,
        }
    }

    struct World {
        clock: Arc<FakeClock>,
        tokens: Arc<MemTokenStore>,
        jobs: PlatformJobs,
    }

    async fn world(vault: Arc<dyn SecretVault>, clock: Arc<FakeClock>) -> World {
        EnsurePlatformKeys::new(vault.clone(), Actor::System).run().await.expect("ensure keys");
        let signer = Arc::new(Ed25519TokenSigner::new(vault.clone(), "https://mage.test"));
        signer.reload().await.expect("reload");
        let audit = Arc::new(MemAuditLog::new(clock.clone()));
        let rotate = Arc::new(RotateSigningKey::new(vault.clone(), signer, audit, clock.clone(), RotationPolicy::default()));
        let tokens = Arc::new(MemTokenStore::new(clock.clone()));
        let jobs = PlatformJobs::new(rotate, Arc::new(RewrapSecrets::new(vault)), tokens.clone(), clock.clone());
        World { clock, tokens, jobs }
    }

    #[tokio::test]
    async fn rotation_chains_itself_only_while_a_transition_is_near() {
        let clock = Arc::new(FakeClock::new());
        let w = world(Arc::new(MemSecretVault::new(clock.clone())), clock.clone()).await;
        let job = leased(JobPayload::AuthRotateSigningKey, &w.clock);
        assert_eq!(w.jobs.handle(&job).await.expect("idle"), JobDone::complete(), "next rotation is 30 days away");

        w.clock.advance(std::time::Duration::from_secs(30 * 24 * 3600));
        let staged_at = w.clock.now();
        let done = w.jobs.handle(&job).await.expect("stage");
        let next = staged_at + Duration::minutes(15);
        let expected = NewJob::new(JobPayload::AuthRotateSigningKey)
            .dedupe(format!("auth.rotate_signing_key:{}", next.unix_timestamp()))
            .after(next);
        assert_eq!(done, JobDone::follow_up(vec![expected]));

        w.clock.advance(std::time::Duration::from_secs(15 * 60));
        let activated = w.jobs.handle(&job).await.expect("activate");
        assert_eq!(activated, JobDone::complete(), "retired-key destruction is 25 h away: left to the ticker");

        // A ticker run inside the last day before the destruction chains it precisely.
        w.clock.advance(std::time::Duration::from_secs(2 * 3600));
        let destroy_at = w.clock.now() + Duration::hours(23);
        let expected = NewJob::new(JobPayload::AuthRotateSigningKey)
            .dedupe(format!("auth.rotate_signing_key:{}", destroy_at.unix_timestamp()))
            .after(destroy_at);
        assert_eq!(w.jobs.handle(&job).await.expect("ticker run"), JobDone::follow_up(vec![expected]));
    }

    #[tokio::test]
    async fn sweep_purges_past_one_batch() {
        let clock = Arc::new(FakeClock::new());
        let w = world(Arc::new(MemSecretVault::new(clock.clone())), clock.clone()).await;
        let expires_at = w.clock.now() + Duration::minutes(1);
        let count = 2 * SWEEP_BATCH + 10;
        for n in 0..count {
            let jti = Jti(u128::from(n).to_be_bytes());
            w.tokens.revoke(&Revocation { jti, org_id: None, reason: "test".into(), expires_at }).await.expect("revoke");
        }
        let live = Jti(u128::MAX.to_be_bytes());
        w.tokens
            .revoke(&Revocation { jti: live, org_id: Some(OrgId::from_uuid(Uuid::from_u128(9))), reason: "live".into(), expires_at: expires_at + Duration::hours(1) })
            .await
            .expect("revoke live");
        w.clock.advance(std::time::Duration::from_secs(120));

        assert_eq!(w.jobs.handle(&leased(JobPayload::AuthSweepSessions, &w.clock)).await.expect("sweep"), JobDone::complete());
        for n in [0, SWEEP_BATCH, count - 1] {
            assert!(!w.tokens.is_revoked(&Jti(u128::from(n).to_be_bytes())).await.expect("is_revoked"), "row {n} purged");
        }
        assert!(w.tokens.is_revoked(&live).await.expect("is_revoked"), "unexpired revocation kept");
    }

    #[tokio::test]
    async fn rewrap_job_moves_every_wrap_to_the_new_kek_version() {
        let clock = Arc::new(FakeClock::new());
        let store = Arc::new(MemSecretStore::new(clock.clone()));
        let kek = Arc::new(RotatingKek::new());
        let vault: Arc<dyn SecretVault> = Arc::new(EnvelopeVault::new(store.clone(), kek.clone(), clock.clone()));
        let w = world(vault.clone(), clock.clone()).await;
        let byok = SecretName::new("model/anthropic_api_key").expect("name");
        let org = OrgId::from_uuid(Uuid::from_u128(7));
        vault.put(&SecretScope::Org(org), &byok, SecretBytes::new(b"sk-ant-key".to_vec()), Activation::Now, &Actor::System).await.expect("put");
        let sealed = vault.encrypt(org, b"aad", b"tenant data").await.expect("encrypt");

        kek.rotate();
        assert_eq!(w.jobs.handle(&leased(JobPayload::VaultRewrap, &w.clock)).await.expect("rewrap"), JobDone::complete());
        let wraps = store.list_wraps(None, 100).await.expect("wraps");
        assert_eq!(wraps.len(), 4, "signing key, csrf key, BYOK key, org data key");
        assert!(wraps.iter().all(|w| w.kek_version == "test-v2"), "{wraps:?}");

        w.clock.advance(std::time::Duration::from_secs(11 * 60));
        assert_eq!(vault.load(&SecretScope::Org(org), &byok).await.expect("load").1.expose(), b"sk-ant-key");
        assert_eq!(vault.decrypt(org, b"aad", &sealed).await.expect("decrypt").expose(), b"tenant data");
    }

    #[tokio::test]
    async fn other_kinds_are_refused() {
        let clock = Arc::new(FakeClock::new());
        let w = world(Arc::new(MemSecretVault::new(clock.clone())), clock.clone()).await;
        let job = leased(JobPayload::OpsPurge, &w.clock);
        assert!(matches!(w.jobs.handle(&job).await, Err(AppError::Invalid(_))));
    }
}
