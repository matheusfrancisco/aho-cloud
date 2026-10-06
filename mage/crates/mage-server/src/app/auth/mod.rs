//! User authentication and Mage-issued credentials (server.md "Authentication and
//! authorization", "Tokens and credentials", UC1, UC12, UC13).
//!
//! - [`sessions`]: opaque server-side sessions (`__Host-mage_sid`) and the CSRF token.
//! - [`login`]: UC1 login for GitHub and Google, explicit provider linking, logout.
//! - [`tokens`]: mint/verify signed tokens with revocation.
//! - [`keys`]: platform key bootstrap, signing-key rotation, secret rewrap.
//! - [`operator`]: operator CLI use cases (runner bootstrap tokens, secret put).

use std::net::IpAddr;

use crate::error::AppError;
use crate::ports::audit::{Actor, AuditAction, AuditEvent, AuditLog, AuditOutcome, AuditTarget};

pub mod keys;
pub mod login;
pub mod operator;
pub mod sessions;
pub mod tokens;

pub use login::{Callback, Login, LoginConfig, LoginFailure, LoginFinished, LoginStarted, safe_return_to};
pub use sessions::{CSRF_HEADER, OAUTH_COOKIE, SESSION_COOKIE, SessionCookie, SessionId, SessionInfo, SessionPolicy, Sessions};

/// Request facts recorded with audit events.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RequestMeta {
    pub ip: Option<IpAddr>,
    pub user_agent: Option<String>,
    pub request_id: Option<String>,
}

/// Records `event`; a failed audit write fails the request (server.md "Audit log").
pub async fn audit(log: &dyn AuditLog, event: impl Into<AuditEvent>) -> Result<(), AppError> {
    let event = event.into();
    let action = event.action.as_str();
    log.record(event).await.map(|_| ()).map_err(|e| {
        tracing::error!(action, error = %e, "audit write failed");
        AppError::Internal(format!("audit write failed: {e}"))
    })
}

pub(crate) fn role_name(role: mage_core::domain::MemberRole) -> &'static str {
    match role {
        mage_core::domain::MemberRole::Member => "member",
        mage_core::domain::MemberRole::Admin => "admin",
        mage_core::domain::MemberRole::Owner => "owner",
    }
}

/// Builder for a successful audit event carrying the request facts.
pub(crate) struct EventBuilder(pub(crate) AuditEvent);

impl EventBuilder {
    pub(crate) fn detail(mut self, key: &'static str, value: impl Into<String>) -> EventBuilder {
        self.0.details.push((key.into(), value.into()));
        self
    }
}

impl From<EventBuilder> for AuditEvent {
    fn from(b: EventBuilder) -> AuditEvent {
        b.0
    }
}

pub(crate) fn event(org: Option<mage_core::domain::OrgId>, actor: Actor, action: AuditAction, target: AuditTarget, meta: &RequestMeta) -> EventBuilder {
    EventBuilder(AuditEvent {
        org,
        actor,
        action,
        target,
        outcome: AuditOutcome::Success,
        ip: meta.ip,
        request_id: meta.request_id.clone(),
        details: Vec::new(),
    })
}
