//! `sqlx::Error` → port errors: the one mapping of storage.md "Common types".
//!
//! | Postgres / sqlx | [`DbError`] |
//! |---|---|
//! | `RowNotFound` | `NotFound` |
//! | `23505` unique_violation | `Conflict { constraint }` |
//! | `23503` foreign_key_violation | `Invalid("dangling reference: <constraint>")` |
//! | `23502`, `23514`, class `22` (`22P02`, …) | `Invalid` |
//! | `40001`, `40P01` | `Retry` (rerun by [`super::tx::retry_tx`], then `Unavailable`) |
//! | `57014` statement timeout | `Timeout` |
//! | pool timeout, I/O, TLS, protocol, `57P0x`, class `08`, `53` | `Unavailable` |
//! | row decode failure | `Invalid` (stored data does not match the code) |
//!
//! Each port error implements `From<DbError>`; the core ports' impls are here, server ports
//! implement theirs in `mage-server`.

use mage_core::ports::jobs::JobError;
use mage_core::ports::knowledge::KnowledgeError;
use mage_core::ports::repos::RepoStoreError;

/// A classified database failure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DbError {
    #[error("row not found")]
    NotFound,
    /// Unique violation (with the violated constraint or index when Postgres names it), or a CAS
    /// miss reported by the adapter.
    #[error("conflict{}", constraint.as_deref().map(|c| format!(" on {c}")).unwrap_or_default())]
    Conflict { constraint: Option<String> },
    #[error("invalid: {0}")]
    Invalid(String),
    #[error("database unavailable: {0}")]
    Unavailable(String),
    /// The statement ran past `statement_timeout`.
    #[error("statement timeout")]
    Timeout,
    /// Serialization failure or deadlock: the whole transaction may be rerun.
    #[error("transaction must be retried: {0}")]
    Retry(String),
}

impl DbError {
    /// A CAS miss or other adapter-detected conflict without a constraint name.
    pub const fn conflict() -> Self {
        Self::Conflict { constraint: None }
    }

    /// The violated unique constraint / index, for adapters that react to one in particular.
    pub fn constraint(&self) -> Option<&str> {
        match self {
            Self::Conflict { constraint } => constraint.as_deref(),
            _ => None,
        }
    }
}

impl From<sqlx::Error> for DbError {
    fn from(err: sqlx::Error) -> Self {
        match err {
            sqlx::Error::RowNotFound => Self::NotFound,
            sqlx::Error::Database(db) => {
                let code = db.code().map(|c| c.into_owned()).unwrap_or_default();
                let constraint = db.constraint().map(str::to_owned);
                let message = db.message().to_owned();
                classify_sqlstate(&code, constraint, message)
            }
            sqlx::Error::PoolTimedOut => Self::Unavailable("timed out acquiring a database connection".into()),
            sqlx::Error::PoolClosed => Self::Unavailable("database pool closed".into()),
            sqlx::Error::WorkerCrashed => Self::Unavailable("database worker crashed".into()),
            sqlx::Error::Io(e) => Self::Unavailable(format!("database i/o: {e}")),
            sqlx::Error::Tls(e) => Self::Unavailable(format!("database tls: {e}")),
            sqlx::Error::Protocol(e) => Self::Unavailable(format!("database protocol: {e}")),
            sqlx::Error::ColumnDecode { index, source } => Self::Invalid(format!("stored column {index} does not decode: {source}")),
            sqlx::Error::Decode(e) => Self::Invalid(format!("stored value does not decode: {e}")),
            sqlx::Error::ColumnNotFound(c) => Self::Invalid(format!("column {c} missing from result")),
            sqlx::Error::TypeNotFound { type_name } => Self::Invalid(format!("database type {type_name} not found")),
            other => Self::Unavailable(other.to_string()),
        }
    }
}

fn classify_sqlstate(code: &str, constraint: Option<String>, message: String) -> DbError {
    match code {
        "23505" => DbError::Conflict { constraint },
        "23503" => DbError::Invalid(format!("dangling reference: {}", constraint.as_deref().unwrap_or(&message))),
        "23502" | "23514" => DbError::Invalid(message),
        "40001" | "40P01" => DbError::Retry(message),
        "57014" => DbError::Timeout,
        _ if code.starts_with("22") => DbError::Invalid(message),
        _ if code.starts_with("08") || code.starts_with("53") || code.starts_with("57P") => DbError::Unavailable(format!("{code}: {message}")),
        _ => DbError::Unavailable(format!("database error {code}: {message}")),
    }
}

impl From<DbError> for JobError {
    fn from(err: DbError) -> Self {
        match err {
            DbError::NotFound => Self::NotFound,
            DbError::Conflict { .. } => Self::Conflict,
            DbError::Invalid(msg) => Self::Invalid(msg),
            DbError::Unavailable(msg) | DbError::Retry(msg) => Self::Unavailable(msg),
            DbError::Timeout => Self::Unavailable("statement timeout".into()),
        }
    }
}

impl From<DbError> for RepoStoreError {
    fn from(err: DbError) -> Self {
        match err {
            DbError::NotFound => Self::NotFound,
            DbError::Conflict { .. } => Self::Conflict,
            DbError::Invalid(msg) => Self::Invalid(msg),
            DbError::Unavailable(msg) | DbError::Retry(msg) => Self::Unavailable(msg),
            DbError::Timeout => Self::Unavailable("statement timeout".into()),
        }
    }
}

impl From<DbError> for KnowledgeError {
    fn from(err: DbError) -> Self {
        match err {
            DbError::NotFound => Self::NotFound,
            DbError::Conflict { .. } => Self::Conflict,
            DbError::Invalid(msg) => Self::Invalid(msg),
            DbError::Unavailable(msg) | DbError::Retry(msg) => Self::Unavailable(msg),
            DbError::Timeout => Self::Timeout,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sqlstates_map_per_the_storage_table() {
        let c = |code: &str| classify_sqlstate(code, Some("jobs_dedupe_active".into()), "msg".into());
        assert_eq!(c("23505"), DbError::Conflict { constraint: Some("jobs_dedupe_active".into()) });
        assert_eq!(c("23503"), DbError::Invalid("dangling reference: jobs_dedupe_active".into()));
        assert_eq!(c("23514"), DbError::Invalid("msg".into()));
        assert_eq!(c("22P02"), DbError::Invalid("msg".into()));
        assert_eq!(c("40001"), DbError::Retry("msg".into()));
        assert_eq!(c("40P01"), DbError::Retry("msg".into()));
        assert_eq!(c("57014"), DbError::Timeout);
        assert!(matches!(c("57P01"), DbError::Unavailable(_)));
        assert!(matches!(c("08006"), DbError::Unavailable(_)));
        assert_eq!(DbError::from(sqlx::Error::RowNotFound), DbError::NotFound);
        assert!(matches!(DbError::from(sqlx::Error::PoolTimedOut), DbError::Unavailable(_)));
    }

    #[test]
    fn retry_and_timeout_surface_as_unavailable_or_timeout() {
        assert_eq!(JobError::from(DbError::Retry("x".into())), JobError::Unavailable("x".into()));
        assert_eq!(KnowledgeError::from(DbError::Timeout), KnowledgeError::Timeout);
        assert_eq!(RepoStoreError::from(DbError::conflict()), RepoStoreError::Conflict);
    }
}
