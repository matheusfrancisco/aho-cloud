//! What the app receives from the edge: `X-Principal-JWT`, an ES256 JWT whose keys the project's
//! JWKS publishes. The edge authenticates; the app authorizes, keyed on `(iss, sub)`.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::ledger::Stage;

pub const HEADER: &str = "x-principal-jwt";
/// Seconds a principal token stays valid; the edge mints one per request.
pub const TTL_SECS: u64 = 60;
/// Paths the edge owns on every project hostname.
pub const ME_PATH: &str = "/.edge/me";
pub const LOGOUT_PATH: &str = "/.edge/logout";
pub const JWKS_PATH: &str = "/.edge/jwks.json";
/// Accepted clock skew when checking `iat`/`exp`.
pub const LEEWAY_SECS: u64 = 5;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
pub struct PrincipalClaims {
    /// [`issuer`] of the project.
    pub iss: String,
    /// [`audience`] of the project and stage.
    pub aud: String,
    /// Stable per user and upstream identity provider: `<idp>:<upstream subject>`.
    pub sub: String,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub email_verified: bool,
    #[serde(default)]
    pub groups: Vec<String>,
    /// How the user authenticated: `pwd`, `otp` (magic link), `oidc`, `svc` (bearer token).
    pub amr: Vec<String>,
    /// Edge session id; `/.edge/logout` revokes it.
    pub sid: String,
    pub iat: u64,
    pub exp: u64,
}

pub fn issuer(project: &str) -> String {
    format!("edge/p/{project}")
}

pub fn audience(project: &str, stage: Stage) -> String {
    format!("{project}/{}", stage.as_str())
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ClaimsError {
    #[error("issuer is `{0}`, expected `{1}`")]
    Issuer(String, String),
    #[error("audience is `{0}`, expected `{1}`")]
    Audience(String, String),
    #[error("token expired")]
    Expired,
    #[error("token issued in the future")]
    NotYetValid,
    #[error("token lifetime exceeds {TTL_SECS}s")]
    TooLong,
}

impl PrincipalClaims {
    /// The claim checks every verifier runs after the signature check.
    pub fn check(&self, project: &str, stage: Stage, now: u64) -> Result<(), ClaimsError> {
        let iss = issuer(project);
        if self.iss != iss {
            return Err(ClaimsError::Issuer(self.iss.clone(), iss));
        }
        let aud = audience(project, stage);
        if self.aud != aud {
            return Err(ClaimsError::Audience(self.aud.clone(), aud));
        }
        if self.iat > now + LEEWAY_SECS {
            return Err(ClaimsError::NotYetValid);
        }
        if now >= self.exp + LEEWAY_SECS {
            return Err(ClaimsError::Expired);
        }
        if self.exp.saturating_sub(self.iat) > TTL_SECS {
            return Err(ClaimsError::TooLong);
        }
        Ok(())
    }
}
