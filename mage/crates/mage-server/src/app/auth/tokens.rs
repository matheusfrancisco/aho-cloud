//! [`TokenService`]: mint and verify Mage-signed tokens (server.md "Tokens and credentials").
//! Verification = the signer's stateless checks, then the `jti` revocation list. Run state,
//! attempt and runner generation are the calling use case's checks.

use std::sync::Arc;

use time::OffsetDateTime;
use tracing::{info, warn};

use mage_core::domain::OrgId;
use mage_core::ports::{Clock, IdGen};
use mage_core::wire::token::LEEWAY;

use crate::error::AppError;
use crate::ports::auth::{SignedToken, TokenClaims, TokenError, TokenKind, TokenScope, TokenSigner};
use crate::ports::tokens::{Jti, Revocation, TokenStore};

pub struct TokenService {
    signer: Arc<dyn TokenSigner>,
    tokens: Arc<dyn TokenStore>,
    clock: Arc<dyn Clock>,
    ids: Arc<dyn IdGen>,
}

impl TokenService {
    pub fn new(signer: Arc<dyn TokenSigner>, tokens: Arc<dyn TokenStore>, clock: Arc<dyn Clock>, ids: Arc<dyn IdGen>) -> TokenService {
        TokenService { signer, tokens, clock, ids }
    }

    /// A token for `scope`: `iat` = now (whole seconds), `exp` = `iat + ttl`, `jti` = UUID v7.
    pub fn mint(&self, scope: TokenScope, ttl: time::Duration) -> Result<(SignedToken, TokenClaims), AppError> {
        if ttl <= time::Duration::ZERO {
            return Err(AppError::Invalid("token lifetime must be positive".into()));
        }
        let now = self.clock.now();
        let iat = now.replace_nanosecond(0).unwrap_or(now);
        let claims = TokenClaims { scope, iat, exp: iat + ttl, jti: self.ids.uuid_v7() };
        let token = self.signer.sign(&claims)?;
        Ok((token, claims))
    }

    /// Signer checks at `clock.now()`, then revocation. Every rejection is `Unauthorized`
    /// (logged with its reason, never the token); a signer or store outage is `Unavailable`.
    pub async fn verify(&self, token: &str, kind: TokenKind) -> Result<TokenClaims, AppError> {
        let claims = self.signer.verify(token, kind, self.clock.now()).map_err(|e| {
            match &e {
                TokenError::Unavailable(m) => warn!(?kind, error = %m, "token verification unavailable"),
                rejected => info!(?kind, reason = rejected.reason(), "token rejected"),
            }
            AppError::from(e)
        })?;
        if self.tokens.is_revoked(&jti_of(&claims)).await? {
            info!(?kind, reason = "revoked", jti = %claims.jti, "token rejected");
            return Err(AppError::Unauthorized);
        }
        Ok(claims)
    }

    /// Revokes the token's `jti` until it can no longer verify (`exp` + verification leeway).
    pub async fn revoke(&self, claims: &TokenClaims, org: Option<OrgId>, reason: &str) -> Result<(), AppError> {
        let revocation = Revocation { jti: jti_of(claims), org_id: org, reason: reason.to_owned(), expires_at: revoked_until(claims) };
        self.tokens.revoke(&revocation).await?;
        info!(kind = ?claims.scope.kind(), jti = %claims.jti, reason, "token revoked");
        Ok(())
    }
}

fn jti_of(claims: &TokenClaims) -> Jti {
    Jti(*claims.jti.as_bytes())
}

/// A token is accepted until `exp + LEEWAY`; its revocation must outlive that.
fn revoked_until(claims: &TokenClaims) -> OffsetDateTime {
    claims.exp + LEEWAY
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use mage_core::domain::{ArtifactId, RunId, RunnerId};
    use mage_core::testing::{FakeClock, SeqIdGen};
    use uuid::Uuid;

    use super::*;
    use crate::testing::{FakeTokenSigner, MemTokenStore};

    struct World {
        clock: Arc<FakeClock>,
        tokens: Arc<MemTokenStore>,
        service: TokenService,
    }

    fn world() -> World {
        let clock = Arc::new(FakeClock::new());
        let tokens = Arc::new(MemTokenStore::new(clock.clone()));
        let service = TokenService::new(Arc::new(FakeTokenSigner::new("https://mage.test")), tokens.clone(), clock.clone(), Arc::new(SeqIdGen::new()));
        World { clock, tokens, service }
    }

    fn artifact_scope() -> TokenScope {
        TokenScope::Artifact { org: OrgId::from_uuid(Uuid::from_u128(1)), artifact: ArtifactId::from_uuid(Uuid::from_u128(2)) }
    }

    #[tokio::test]
    async fn minted_tokens_verify_until_revoked() {
        let w = world();
        w.clock.advance(std::time::Duration::from_millis(1_700));
        let (token, claims) = w.service.mint(artifact_scope(), time::Duration::minutes(10)).expect("mint");
        assert_eq!(claims.iat.nanosecond(), 0, "whole seconds");
        assert_eq!(claims.iat.unix_timestamp(), w.clock.now().unix_timestamp());
        assert_eq!(claims.exp, claims.iat + time::Duration::minutes(10));
        assert_eq!(claims.jti.get_version_num(), 7);
        let (_, second) = w.service.mint(artifact_scope(), time::Duration::minutes(10)).expect("mint again");
        assert_ne!(second.jti, claims.jti);

        assert_eq!(w.service.verify(token.as_str(), TokenKind::Artifact).await.expect("verify"), claims);
        assert!(matches!(w.service.verify(token.as_str(), TokenKind::Blob).await, Err(AppError::Unauthorized)), "wrong kind");

        let org = OrgId::from_uuid(Uuid::from_u128(1));
        w.service.revoke(&claims, Some(org), "artifact share withdrawn").await.expect("revoke");
        assert!(matches!(w.service.verify(token.as_str(), TokenKind::Artifact).await, Err(AppError::Unauthorized)), "revoked");
        assert!(w.tokens.is_revoked(&Jti(*claims.jti.as_bytes())).await.expect("is_revoked"));

        let (other, _) = w.service.mint(artifact_scope(), time::Duration::minutes(10)).expect("mint other");
        assert!(w.service.verify(other.as_str(), TokenKind::Artifact).await.is_ok(), "revocation is per jti");
    }

    #[tokio::test]
    async fn expiry_uses_the_service_clock() {
        let w = world();
        let scope = TokenScope::Runner { runner: RunnerId::from_uuid(Uuid::from_u128(3)), generation: 1, instance: Uuid::from_u128(4) };
        let (token, _) = w.service.mint(scope, time::Duration::minutes(15)).expect("mint");
        w.clock.advance(std::time::Duration::from_secs(15 * 60 + 29));
        assert!(w.service.verify(token.as_str(), TokenKind::Runner).await.is_ok(), "within the leeway");
        w.clock.advance(std::time::Duration::from_secs(1));
        assert!(matches!(w.service.verify(token.as_str(), TokenKind::Runner).await, Err(AppError::Unauthorized)), "expired");
    }

    #[tokio::test]
    async fn revocations_outlive_the_leeway_window() {
        let w = world();
        let scope = TokenScope::Gateway { org: OrgId::from_uuid(Uuid::from_u128(5)), run: RunId::from_uuid(Uuid::from_u128(6)), attempt: 1, models: vec![] };
        let (token, claims) = w.service.mint(scope, time::Duration::minutes(5)).expect("mint");
        w.service.revoke(&claims, None, "run cancelled").await.expect("revoke");
        // Just past exp, the token still passes the signer's leeway; the revocation must still be there.
        w.clock.set(claims.exp + time::Duration::seconds(10));
        w.tokens.purge_expired(1000).await.expect("purge");
        assert!(matches!(w.service.verify(token.as_str(), TokenKind::Gateway).await, Err(AppError::Unauthorized)));
        assert!(w.tokens.is_revoked(&Jti(*claims.jti.as_bytes())).await.expect("is_revoked"), "kept until exp + leeway");
    }

    #[test]
    fn non_positive_lifetimes_are_refused() {
        let w = world();
        assert!(matches!(w.service.mint(artifact_scope(), time::Duration::ZERO), Err(AppError::Invalid(_))));
    }
}
