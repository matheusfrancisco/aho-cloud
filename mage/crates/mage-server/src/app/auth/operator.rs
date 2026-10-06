//! Operator CLI use cases (`mage admin runner issue`, `mage admin secret put`; server.md "Runner
//! credential lifecycle", "Envelope design"). Both are audited with the operator as actor.

use std::sync::Arc;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::RngCore;
use rand::rngs::OsRng;
use sha2::{Digest, Sha256};
use time::{Duration, OffsetDateTime};

use mage_core::ports::Clock;
use mage_core::wire::token::BOOTSTRAP_PREFIX;

use super::{RequestMeta, audit, event};
use crate::error::AppError;
use crate::ports::audit::{Actor, AuditAction, AuditLog, AuditTarget};
use crate::ports::tokens::{NewBootstrapToken, RunnerScope, TokenError, TokenStore};
use crate::ports::vault::{Activation, SecretBytes, SecretMeta, SecretName, SecretScope, SecretVault, names};

/// Lifetime of an unused bootstrap token (server.md: 24 h).
pub const BOOTSTRAP_TTL: Duration = Duration::hours(24);
/// Longest runner name.
const RUNNER_NAME_MAX: usize = 64;

/// A freshly issued bootstrap token; `token` is shown once and never stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssuedBootstrap {
    pub token: String,
    pub expires_at: OffsetDateTime,
}

/// `mage admin runner issue --name <name> [--scope org,…]`.
pub struct IssueRunnerBootstrap {
    tokens: Arc<dyn TokenStore>,
    audit: Arc<dyn AuditLog>,
    clock: Arc<dyn Clock>,
}

impl IssueRunnerBootstrap {
    pub fn new(tokens: Arc<dyn TokenStore>, audit: Arc<dyn AuditLog>, clock: Arc<dyn Clock>) -> IssueRunnerBootstrap {
        IssueRunnerBootstrap { tokens, audit, clock }
    }

    /// A one-time `mgb_` token (256 random bits) for runner `name`; only its SHA-256 is stored,
    /// with the scope the runner will lease under.
    pub async fn issue(&self, name: &str, scope: RunnerScope, actor: &Actor) -> Result<IssuedBootstrap, AppError> {
        let valid = !name.is_empty() && name.len() <= RUNNER_NAME_MAX && name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'));
        if !valid {
            return Err(AppError::Invalid(format!("runner name must be 1–{RUNNER_NAME_MAX} of [A-Za-z0-9._-]")));
        }
        if matches!(&scope, RunnerScope::Orgs(orgs) if orgs.is_empty()) {
            return Err(AppError::Invalid("runner scope names no org".into()));
        }
        let mut random = [0u8; 32];
        OsRng.fill_bytes(&mut random);
        let token = format!("{BOOTSTRAP_PREFIX}{}", URL_SAFE_NO_PAD.encode(random));
        let expires_at = self.clock.now() + BOOTSTRAP_TTL;
        let record = NewBootstrapToken { token_hash: Sha256::digest(token.as_bytes()).into(), name: name.to_owned(), scope: scope.clone(), expires_at, created_by: actor.clone() };
        self.tokens.create_bootstrap(&record).await.map_err(|e| match e {
            TokenError::Conflict => AppError::Conflict("bootstrap token collision; issue again".into()),
            other => other.into(),
        })?;
        let scope_detail = match &scope {
            RunnerScope::Shared => "shared".to_owned(),
            RunnerScope::Orgs(orgs) => orgs.iter().map(ToString::to_string).collect::<Vec<_>>().join(","),
        };
        audit(
            self.audit.as_ref(),
            event(None, actor.clone(), AuditAction::RunnerBootstrapIssued, AuditTarget::new("runner", name), &RequestMeta::default())
                .detail("scope", scope_detail)
                .detail("expires_at", expires_at.unix_timestamp().to_string()),
        )
        .await?;
        Ok(IssuedBootstrap { token, expires_at })
    }
}

/// `mage admin secret put <platform|org:slug> <name> --stdin`.
pub struct PutSecret {
    vault: Arc<dyn SecretVault>,
    audit: Arc<dyn AuditLog>,
}

impl PutSecret {
    pub fn new(vault: Arc<dyn SecretVault>, audit: Arc<dyn AuditLog>) -> PutSecret {
        PutSecret { vault, audit }
    }

    /// A new Active version of `name` in `scope` (the previous Active one is Retired). The keys
    /// the server generates and rotates itself are refused.
    pub async fn put(&self, scope: SecretScope, name: &str, value: SecretBytes, actor: &Actor) -> Result<SecretMeta, AppError> {
        if matches!(name, names::TOKEN_SIGNING_KEY | names::CSRF_KEY) {
            return Err(AppError::Invalid(format!("{name} is generated and rotated by the server")));
        }
        if value.is_empty() {
            return Err(AppError::Invalid("secret value is empty".into()));
        }
        let name = SecretName::new(name)?;
        let meta = self.vault.put(&scope, &name, value, Activation::Now, actor).await?;
        audit(
            self.audit.as_ref(),
            event(scope.org(), actor.clone(), AuditAction::SecretPut, AuditTarget::new("secret", format!("{scope}/{name}")), &RequestMeta::default())
                .detail("version", meta.version.to_string())
                .detail("fingerprint", meta.fingerprint.clone()),
        )
        .await?;
        Ok(meta)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use mage_core::domain::{HarnessKind, RunnerId};
    use mage_core::testing::FakeClock;
    use mage_core::wire::runner::{Arch, Capabilities, CredentialModeKind, ExecutorKind, HarnessAvailability, Os, SlotCounts};
    use uuid::Uuid;

    use super::*;
    use crate::ports::tokens::RunnerRegistration;
    use crate::testing::{MemAuditLog, MemSecretVault, MemTokenStore};

    fn operator() -> Actor {
        Actor::Operator { os_user: "ops".into() }
    }

    #[tokio::test]
    async fn issued_token_registers_its_runner_once_with_the_recorded_scope() {
        let clock = Arc::new(FakeClock::new());
        let tokens = Arc::new(MemTokenStore::new(clock.clone()));
        let audit_log = Arc::new(MemAuditLog::new(clock.clone()));
        let issue = IssueRunnerBootstrap::new(tokens.clone(), audit_log.clone(), clock.clone());

        let issued = issue.issue("gce-r1", RunnerScope::Shared, &operator()).await.unwrap();
        assert!(issued.token.starts_with(BOOTSTRAP_PREFIX));
        assert_eq!(issued.expires_at, clock.now() + BOOTSTRAP_TTL);

        let hash: [u8; 32] = Sha256::digest(issued.token.as_bytes()).into();
        let reg = |name: &str| RunnerRegistration {
            id: RunnerId::from_uuid(Uuid::from_u128(7)),
            name: name.into(),
            public_key: [3; 32],
            capabilities: Capabilities {
                arch: Arch::Aarch64,
                os: Os::MacOs,
                executor: ExecutorKind::Host,
                harnesses: vec![HarnessAvailability { kind: HarnessKind::Omp, version: "1".into(), credential_modes: vec![CredentialModeKind::Gateway] }],
                browser: false,
                slots: SlotCounts { light: 1, work: 1, verify: 1 },
            },
        };
        assert!(tokens.register_runner(&hash, &reg("other")).await.is_err(), "the token names one runner");
        let runner = tokens.register_runner(&hash, &reg("gce-r1")).await.unwrap();
        assert_eq!((runner.name.as_str(), runner.scope), ("gce-r1", RunnerScope::Shared));

        let records = audit_log.records();
        assert!(records.iter().any(|r| r.event.action == AuditAction::RunnerBootstrapIssued && r.event.actor == operator()));
        assert!(issue.issue("bad name", RunnerScope::Shared, &operator()).await.is_err());
        assert!(issue.issue("r2", RunnerScope::Orgs(Vec::new()), &operator()).await.is_err());
    }

    #[tokio::test]
    async fn put_secret_activates_a_new_version_and_refuses_server_keys() {
        let clock = Arc::new(FakeClock::new());
        let vault = Arc::new(MemSecretVault::new(clock.clone()));
        let put = PutSecret::new(vault.clone(), Arc::new(MemAuditLog::new(clock.clone())));
        let first = put.put(SecretScope::Platform, "github/webhook_secret", SecretBytes::from("one".to_owned()), &operator()).await.unwrap();
        let second = put.put(SecretScope::Platform, "github/webhook_secret", SecretBytes::from("two".to_owned()), &operator()).await.unwrap();
        assert!(second.version > first.version);
        let (_, active) = vault.load(&SecretScope::Platform, &SecretName::new("github/webhook_secret").unwrap()).await.unwrap();
        assert_eq!(active.expose(), b"two");
        assert!(put.put(SecretScope::Platform, names::CSRF_KEY, SecretBytes::from("x".to_owned()), &operator()).await.is_err());
        assert!(put.put(SecretScope::Platform, "github/webhook_secret", SecretBytes::new(Vec::new()), &operator()).await.is_err());
    }
}
