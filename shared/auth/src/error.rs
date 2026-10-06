/// Login failure, shaped like `mage_server::ports::auth::IdentityError` so callers map 1:1.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AuthError {
    /// The provider or the user refused the login: denied consent, rejected or reused code,
    /// revoked token, or an account that does not meet the login requirements (unverified email).
    #[error("login denied")]
    Denied,
    /// Malformed input or provider response, misconfiguration, or an `id_token` that fails
    /// verification (signature, issuer, audience, expiry, nonce).
    #[error("invalid login: {0}")]
    Invalid(String),
    /// The provider could not be reached or answered with a transient failure.
    #[error("identity provider unavailable: {0}")]
    Unavailable(String),
}

impl AuthError {
    pub(crate) fn invalid(message: impl Into<String>) -> Self {
        Self::Invalid(message.into())
    }

    pub(crate) fn unavailable(message: impl Into<String>) -> Self {
        Self::Unavailable(message.into())
    }

    /// Map an OAuth 2.0 / OIDC token-endpoint error code (RFC 6749 §5.2 plus GitHub's codes).
    pub(crate) fn from_oauth_error(code: &str, description: Option<&str>) -> Self {
        match code {
            "access_denied" | "invalid_grant" | "bad_verification_code" | "unverified_user_email" => {
                Self::Denied
            }
            "temporarily_unavailable" | "server_error" => {
                Self::Unavailable(describe(code, description))
            }
            _ => Self::Invalid(describe(code, description)),
        }
    }
}

fn describe(code: &str, description: Option<&str>) -> String {
    match description {
        Some(description) if !description.is_empty() => format!("{code}: {description}"),
        _ => code.to_owned(),
    }
}
