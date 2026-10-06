//! Login providers shared by Mage and aho-cloud: GitHub OAuth (PKCE + state) and Google OIDC.
//! No product logic. See `mage/docs/internals/{deployment,server}.md`.
//!
//! This crate only talks to the providers: it builds authorize URLs, exchanges codes, verifies
//! identities and returns an [`ExternalIdentity`]. Login state (state/verifier/nonce storage),
//! sessions, users and invites belong to the caller. Provider access tokens never leave this
//! crate: they are dropped inside `exchange`.

mod error;
pub mod github;
pub mod google;
mod http;
mod identity;
pub mod pkce;

pub use error::AuthError;
pub use github::{GitHubConfig, GitHubOAuth};
pub use google::{GoogleConfig, GoogleOidc};
pub use identity::{AuthorizeParams, ExternalIdentity, Provider};
pub use pkce::{Pkce, random_nonce, random_state};
