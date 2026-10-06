//! `DomainError` (domain.md "Errors") and the tenancy check (domain.md "Tenancy").

use crate::domain::budget::BudgetLimit;
use crate::domain::ids::OrgId;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DomainError {
    /// Two aggregates of different orgs were linked.
    #[error("cross-tenant operation refused")]
    CrossTenant,
    /// A status enum's `apply` received an event that has no row for the current state.
    #[error("invalid transition: {event} from {from}")]
    InvalidTransition { from: &'static str, event: &'static str },
    /// No checked evidence (a cause without a citation that passed the evidence gate).
    #[error("missing evidence")]
    MissingEvidence,
    #[error("budget exceeded: {0}")]
    BudgetExceeded(BudgetLimit),
    #[error("invalid: {0}")]
    Invalid(String),
}

impl DomainError {
    pub(crate) fn invalid(msg: impl Into<String>) -> Self {
        Self::Invalid(msg.into())
    }
}

/// Every operation that links two aggregates calls this first.
pub fn ensure_same_org(a: OrgId, b: OrgId) -> Result<(), DomainError> {
    if a == b { Ok(()) } else { Err(DomainError::CrossTenant) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn same_org_passes_other_org_is_cross_tenant() {
        let a = OrgId::from_uuid(Uuid::from_u128(1));
        let b = OrgId::from_uuid(Uuid::from_u128(2));
        assert_eq!(ensure_same_org(a, a), Ok(()));
        assert_eq!(ensure_same_org(a, b), Err(DomainError::CrossTenant));
    }
}
