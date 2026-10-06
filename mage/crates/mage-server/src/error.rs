//! Use-case error for the server role. Driving adapters map it to HTTP/MCP errors.
//! Port error enums convert into it with `From` impls placed next to each port.

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("not found: {0}")]
    NotFound(String),
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("invalid: {0}")]
    Invalid(String),
    #[error("unauthorized")]
    Unauthorized,
    #[error("forbidden: {0}")]
    Forbidden(String),
    #[error("rate limited")]
    RateLimited { retry_after_secs: Option<u64> },
    #[error("unavailable: {0}")]
    Unavailable(String),
    #[error(transparent)]
    Domain(#[from] mage_core::domain::DomainError),
    #[error("internal: {0}")]
    Internal(String),
}

impl AppError {
    /// Stable machine-readable code used in API error bodies.
    pub fn code(&self) -> &'static str {
        match self {
            AppError::NotFound(_) => "not_found",
            AppError::Conflict(_) => "conflict",
            AppError::Invalid(_) => "invalid",
            AppError::Unauthorized => "unauthorized",
            AppError::Forbidden(_) => "forbidden",
            AppError::RateLimited { .. } => "rate_limited",
            AppError::Unavailable(_) => "unavailable",
            AppError::Domain(_) => "invalid",
            AppError::Internal(_) => "internal",
        }
    }
}
