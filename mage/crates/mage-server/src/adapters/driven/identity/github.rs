//! GitHub login: the read app's OAuth web flow with PKCE (S256) and `state`; identity from
//! `GET /user` + `GET /user/emails`; invites resolve logins with `GET /users/{login}`.

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use shared_auth::{GitHubConfig, GitHubOAuth};

use super::{USER_AGENT, from_shared_identity, to_shared_params};
use crate::ports::auth::{AuthorizeParams, ExternalIdentity, IdentityError, IdentityProvider, IdentityProviderKind};

/// Source of the token `lookup_login` sends (a GitHub App installation token); `Ok(None)` calls
/// anonymously, which GitHub rate-limits to 60 requests/hour per IP.
#[async_trait]
pub trait LookupToken: Send + Sync {
    async fn token(&self) -> Result<Option<String>, IdentityError>;
}

/// `[login.github]`: the read app's OAuth client. URL overrides serve GitHub Enterprise and tests;
/// the client secret is `github/oauth_client_secret` (SecretVault, Platform).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitHubLoginConfig {
    pub client_id: String,
    /// REST API base; default `https://api.github.com`.
    #[serde(default)]
    pub api_base: Option<String>,
    /// Default `https://github.com/login/oauth/authorize`.
    #[serde(default)]
    pub authorize_url: Option<String>,
    /// Default `https://github.com/login/oauth/access_token`.
    #[serde(default)]
    pub token_url: Option<String>,
}

impl GitHubLoginConfig {
    pub fn build(&self, client_secret: &str, http: reqwest::Client) -> GitHubIdentity {
        let mut config = GitHubConfig::new(self.client_id.clone(), client_secret);
        config.user_agent = USER_AGENT.to_owned();
        if let Some(api_base) = &self.api_base {
            config.api_base = api_base.clone();
        }
        if let Some(authorize_url) = &self.authorize_url {
            config.authorize_url = authorize_url.clone();
        }
        if let Some(token_url) = &self.token_url {
            config.token_url = token_url.clone();
        }
        GitHubIdentity::new(GitHubOAuth::new(config, http))
    }
}

/// `IdentityProvider` for GitHub. The user token never leaves `GitHubOAuth::exchange`.
pub struct GitHubIdentity {
    oauth: GitHubOAuth,
    lookup_token: Option<Arc<dyn LookupToken>>,
}

impl GitHubIdentity {
    /// Without a lookup token source: `lookup_login` calls anonymously.
    pub fn new(oauth: GitHubOAuth) -> GitHubIdentity {
        GitHubIdentity { oauth, lookup_token: None }
    }

    /// Token for `lookup_login` (an installation token source).
    pub fn with_lookup_token(mut self, source: Arc<dyn LookupToken>) -> GitHubIdentity {
        self.lookup_token = Some(source);
        self
    }
}

impl fmt::Debug for GitHubIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GitHubIdentity")
            .field("oauth", &self.oauth)
            .field("lookup_token", &self.lookup_token.is_some())
            .finish()
    }
}

#[async_trait]
impl IdentityProvider for GitHubIdentity {
    fn kind(&self) -> IdentityProviderKind {
        IdentityProviderKind::GitHub
    }

    fn authorize_url(&self, params: &AuthorizeParams) -> String {
        self.oauth.authorize_url(&to_shared_params(params))
    }

    async fn exchange(&self, code: &str, pkce_verifier: &str, redirect_uri: &str) -> Result<ExternalIdentity, IdentityError> {
        let identity = self.oauth.exchange(code, pkce_verifier, redirect_uri).await?;
        Ok(from_shared_identity(identity))
    }

    async fn lookup_login(&self, login: &str) -> Result<Option<ExternalIdentity>, IdentityError> {
        let token = match &self.lookup_token {
            Some(source) => source.token().await?,
            None => None,
        };
        let identity = self.oauth.lookup_login(login, token.as_deref()).await?;
        Ok(identity.map(from_shared_identity))
    }
}
