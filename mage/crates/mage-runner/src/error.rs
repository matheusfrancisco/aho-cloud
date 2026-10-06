//! Use-case error for this role. Port error enums convert into it with `From` impls placed next
//! to each port.

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("not found: {0}")]
    NotFound(String),
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("invalid: {0}")]
    Invalid(String),
    #[error("unavailable: {0}")]
    Unavailable(String),
    #[error(transparent)]
    Domain(#[from] mage_core::domain::DomainError),
    #[error("internal: {0}")]
    Internal(String),
}
