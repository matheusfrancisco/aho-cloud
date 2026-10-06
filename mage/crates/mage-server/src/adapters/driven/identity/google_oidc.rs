//! Google login: OIDC authorization code flow with PKCE (S256), `state` and `nonce`; the
//! `id_token` is verified against Google's JWKS by `shared_auth::GoogleOidc` (issuer, audience,
//! expiry, nonce; `email_verified` must be true).

use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use shared_auth::pkce::{challenge_s256, nonce_for_verifier};
use shared_auth::{GoogleConfig, GoogleOidc};

use super::{from_shared_identity, to_shared_params};
use crate::ports::auth::{AuthorizeParams, ExternalIdentity, IdentityError, IdentityProvider, IdentityProviderKind};

/// `[login.google]`: the OAuth client id; the secret is `google/oauth_client_secret`
/// (SecretVault, Platform).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoogleLoginConfig {
    pub client_id: String,
}

impl GoogleLoginConfig {
    pub fn build(&self, client_secret: &str, http: reqwest::Client) -> GoogleIdentity {
        let config = GoogleConfig::new(self.client_id.clone(), client_secret);
        GoogleIdentity::new(Arc::new(GoogleOidc::new(config, http)))
    }
}

/// `IdentityProvider` for Google. `GoogleOidc` holds the JWKS cache; share one instance.
#[derive(Debug, Clone)]
pub struct GoogleIdentity {
    oidc: Arc<GoogleOidc>,
}

impl GoogleIdentity {
    pub fn new(oidc: Arc<GoogleOidc>) -> GoogleIdentity {
        GoogleIdentity { oidc }
    }
}

/// The OIDC `nonce` of one login. The port carries no nonce (the login use case stores only
/// `state` and the PKCE verifier), so it is derived from the PKCE pair: the authorize redirect
/// derives it from the challenge, the exchange from `challenge_s256(verifier)`, which yields the
/// same value only for this login's verifier. That binds the `id_token` to the verifier without
/// storing a nonce, and the nonce is as unpredictable and single-use as the 256-bit verifier.
fn nonce_for_challenge(challenge: &str) -> String {
    nonce_for_verifier(challenge)
}

#[async_trait]
impl IdentityProvider for GoogleIdentity {
    fn kind(&self) -> IdentityProviderKind {
        IdentityProviderKind::Google
    }

    fn authorize_url(&self, params: &AuthorizeParams) -> String {
        let nonce = nonce_for_challenge(&params.pkce_challenge);
        self.oidc.authorize_url(&to_shared_params(params), &nonce)
    }

    async fn exchange(&self, code: &str, pkce_verifier: &str, redirect_uri: &str) -> Result<ExternalIdentity, IdentityError> {
        let nonce = nonce_for_challenge(&challenge_s256(pkce_verifier));
        let identity = self.oidc.exchange(code, pkce_verifier, redirect_uri, &nonce).await?;
        Ok(from_shared_identity(identity))
    }

    /// Google has no login directory to resolve; invites are GitHub logins in v1 (server.md
    /// "Invites"), so every login is unknown here.
    async fn lookup_login(&self, _login: &str) -> Result<Option<ExternalIdentity>, IdentityError> {
        Ok(None)
    }
}
