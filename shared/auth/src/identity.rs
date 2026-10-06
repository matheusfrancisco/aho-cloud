/// Which login provider produced an identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Provider {
    GitHub,
    Google,
}

impl Provider {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::GitHub => "github",
            Self::Google => "google",
        }
    }
}

/// Inputs for an authorize redirect. `state` and `pkce_challenge` come from
/// [`crate::random_state`] and [`crate::Pkce::generate`]; the caller keeps the state and verifier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorizeParams {
    pub state: String,
    pub pkce_challenge: String,
    pub redirect_uri: String,
}

/// A provider-verified user identity. Keyed by `(provider, subject)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalIdentity {
    pub provider: Provider,
    /// Stable id: GitHub numeric user id, Google `sub`.
    pub subject: String,
    /// Display handle: GitHub login, Google email.
    pub login: String,
    pub name: Option<String>,
    /// Primary verified email, if readable.
    pub email: Option<String>,
    pub avatar_url: Option<String>,
}
