//! Login providers (`ports::auth::IdentityProvider`) on top of `shared-auth`: GitHub OAuth with
//! PKCE (server.md "Login (GitHub, self-host)", "Identity") and Google OIDC (server.md "Login
//! (Google, self-host)"). Client secrets come from the `SecretVault` (`github/oauth_client_secret`,
//! `google/oauth_client_secret`) and are passed to the config `build` functions by the caller.

pub mod github;
pub mod google_oidc;

pub use github::{GitHubIdentity, GitHubLoginConfig, LookupToken};
pub use google_oidc::{GoogleIdentity, GoogleLoginConfig};

use crate::ports::auth::{AuthorizeParams, ExternalIdentity, IdentityProviderKind};

/// `User-Agent` sent to GitHub (required by its API).
const USER_AGENT: &str = concat!("mage-server/", env!("CARGO_PKG_VERSION"));

fn to_shared_params(params: &AuthorizeParams) -> shared_auth::AuthorizeParams {
    shared_auth::AuthorizeParams {
        state: params.state.clone(),
        pkce_challenge: params.pkce_challenge.clone(),
        redirect_uri: params.redirect_uri.clone(),
    }
}

fn from_shared_identity(identity: shared_auth::ExternalIdentity) -> ExternalIdentity {
    let provider = match identity.provider {
        shared_auth::Provider::GitHub => IdentityProviderKind::GitHub,
        shared_auth::Provider::Google => IdentityProviderKind::Google,
    };
    ExternalIdentity {
        provider,
        subject: identity.subject,
        login: identity.login,
        name: identity.name,
        email: identity.email,
        avatar_url: identity.avatar_url,
    }
}

#[cfg(test)]
mod tests;
