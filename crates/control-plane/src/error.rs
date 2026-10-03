//! One error type for every handler; it renders as [`cloud_api::ApiError`] with the status code
//! its stable `code` implies.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use cloud_api::ApiError;
use cloud_contract::validate::Refusal;
use cloud_domains::DomainError;
use cloud_ledger::LedgerError;

use crate::blobs::BlobError;
use crate::crypto::SecretBoxError;
use crate::ports::PortError;
use crate::store::StoreError;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("{0}")]
    Unauthorized(String),
    #[error("{0}")]
    Forbidden(String),
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Conflict(String),
    #[error("{0}")]
    Invalid(String),
    #[error("{message}")]
    Refused { message: String, refusals: Vec<Refusal> },
    #[error("{0}")]
    QuorumNotMet(String),
    #[error("{0}")]
    Tainted(String),
    #[error("{0}")]
    RateLimited(String),
    /// Over a size limit; the `code` stays `invalid`, the status says why.
    #[error("{0}")]
    TooLarge(String),
    /// A port (builder, runtime, session host, DNS) failed; the request may be retried.
    #[error("{0}")]
    Upstream(String),
    /// A feature this deployment does not run; `code` names it (e.g. `sandboxes_disabled`).
    #[error("{message}")]
    Unavailable { code: &'static str, message: String },
    #[error("{0}")]
    Internal(String),
}

impl AppError {
    /// The stable `ApiError.code`.
    pub fn code(&self) -> &'static str {
        match self {
            AppError::Unauthorized(_) => "unauthorized",
            AppError::Forbidden(_) => "forbidden",
            AppError::NotFound(_) => "not_found",
            AppError::Conflict(_) => "conflict",
            AppError::Invalid(_) | AppError::TooLarge(_) => "invalid",
            AppError::Refused { .. } => "refused",
            AppError::QuorumNotMet(_) => "quorum_not_met",
            AppError::Tainted(_) => "tainted",
            AppError::RateLimited(_) => "rate_limited",
            AppError::Upstream(_) | AppError::Internal(_) => "internal",
            AppError::Unavailable { code, .. } => code,
        }
    }

    pub fn status(&self) -> StatusCode {
        match self {
            AppError::Unauthorized(_) => StatusCode::UNAUTHORIZED,
            AppError::Forbidden(_) => StatusCode::FORBIDDEN,
            AppError::NotFound(_) => StatusCode::NOT_FOUND,
            AppError::Conflict(_) | AppError::QuorumNotMet(_) | AppError::Tainted(_) => StatusCode::CONFLICT,
            AppError::Invalid(_) => StatusCode::BAD_REQUEST,
            AppError::TooLarge(_) => StatusCode::PAYLOAD_TOO_LARGE,
            AppError::Refused { .. } => StatusCode::UNPROCESSABLE_ENTITY,
            AppError::RateLimited(_) => StatusCode::TOO_MANY_REQUESTS,
            AppError::Upstream(_) => StatusCode::BAD_GATEWAY,
            AppError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
            AppError::Unavailable { .. } => StatusCode::SERVICE_UNAVAILABLE,
        }
    }

    pub fn to_api(&self) -> ApiError {
        let mut e = ApiError::new(self.code(), self.to_string());
        if let AppError::Refused { refusals, .. } = self {
            e.refusals = refusals.clone();
        }
        e
    }

    pub fn unauthorized(m: impl Into<String>) -> Self {
        AppError::Unauthorized(m.into())
    }
    pub fn forbidden(m: impl Into<String>) -> Self {
        AppError::Forbidden(m.into())
    }
    pub fn not_found(m: impl Into<String>) -> Self {
        AppError::NotFound(m.into())
    }
    pub fn conflict(m: impl Into<String>) -> Self {
        AppError::Conflict(m.into())
    }
    pub fn invalid(m: impl Into<String>) -> Self {
        AppError::Invalid(m.into())
    }
    pub fn internal(m: impl Into<String>) -> Self {
        AppError::Internal(m.into())
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        if matches!(self, AppError::Internal(_) | AppError::Upstream(_)) {
            tracing::error!(error = %self, "request failed");
        }
        (self.status(), Json(self.to_api())).into_response()
    }
}

impl From<StoreError> for AppError {
    fn from(e: StoreError) -> Self {
        match e {
            StoreError::Conflict(what) => AppError::Conflict(format!("{what} already exists")),
            StoreError::Limit(message) => AppError::Conflict(message),
            StoreError::Backend(_) => AppError::Internal(e.to_string()),
        }
    }
}

impl From<BlobError> for AppError {
    fn from(e: BlobError) -> Self {
        match e {
            BlobError::BadHash(_) => AppError::Invalid(e.to_string()),
            BlobError::Io(_) | BlobError::Corrupt(_) => AppError::Internal(e.to_string()),
        }
    }
}

impl From<SecretBoxError> for AppError {
    fn from(e: SecretBoxError) -> Self {
        AppError::Internal(e.to_string())
    }
}

/// A service's "no such thing", "too large" and "refused" answers are the caller's to see; any
/// other port failure is ours.
impl From<PortError> for AppError {
    fn from(e: PortError) -> Self {
        match e {
            PortError::NotFound { message, .. } => AppError::NotFound(message),
            PortError::TooLarge { message, .. } => AppError::TooLarge(message),
            PortError::Rejected { message, .. } => AppError::Invalid(message),
            PortError::Transport { .. } | PortError::Status { .. } | PortError::Decode { .. } => AppError::Upstream(e.to_string()),
        }
    }
}

impl From<DomainError> for AppError {
    fn from(e: DomainError) -> Self {
        match e {
            DomainError::InvalidHostname(_) => AppError::Invalid(e.to_string()),
            DomainError::Taken { .. } | DomainError::ModeMismatch { .. } => AppError::Conflict(e.to_string()),
            DomainError::NotFound(_) => AppError::NotFound(e.to_string()),
            DomainError::NoEdgeTarget => AppError::Internal(e.to_string()),
            DomainError::Dns(_) => AppError::Upstream(e.to_string()),
        }
    }
}

impl From<LedgerError> for AppError {
    fn from(e: LedgerError) -> Self {
        use LedgerError as L;
        let message = e.to_string();
        match e {
            L::UnknownRelease(_) => AppError::NotFound(message),
            L::InvalidManifest(refusals) => AppError::Refused { message, refusals },
            L::InvalidReleaseId(_) | L::InvalidDigest(_) | L::RollbackTargetMissing | L::UnexpectedRelease { .. } | L::NoMembers => {
                AppError::Invalid(message)
            }
            L::DuplicateRelease(_)
            | L::AlreadyBuilt { .. }
            | L::NotBuilt { .. }
            | L::ApprovalNotNeeded { .. }
            | L::DuplicateApprover { .. }
            | L::Rejected { .. }
            | L::AlreadyActive { .. }
            | L::WrongStageOrder { .. }
            | L::InvalidActivation { .. }
            | L::RollbackTargetNeverLive { .. }
            | L::StandingRollbackWidens { .. } => AppError::Conflict(message),
            L::QuorumNotMet { .. } | L::RollbackWidens { .. } | L::OwnerRequired { .. } => AppError::QuorumNotMet(message),
            L::NotOwner { .. } | L::NoStandingOrders | L::StandingOrdersExpired { .. } | L::NotPermitted { .. } => {
                AppError::Forbidden(message)
            }
            L::Tainted { .. } => AppError::Tainted(message),
            L::ChainBroken { .. }
            | L::Storage(_)
            | L::Malformed { .. }
            | L::Invariant { .. }
            | L::WrongPrincipal { .. }
            | L::BadSignature { .. }
            | L::ManifestHashMismatch { .. }
            | L::StandingActivationUnrecorded { .. } => AppError::Internal(message),
        }
    }
}
