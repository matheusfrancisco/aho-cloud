//! `GET /api/v1/session`, `GET /api/v1/resolve/{id}`, `POST /api/v1/orgs`.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::domain::{MemberRole, User};

/// `GET /api/v1/session`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct SessionView {
    pub user: User,
    pub memberships: Vec<SessionMembership>,
    /// Sent back as `X-CSRF-Token` on every mutation; held in memory only.
    pub csrf_token: String,
    /// Enabled feature flags (e.g. `google_login`).
    pub features: Vec<String>,
}

/// One org of the session user: `{org, name, role}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct SessionMembership {
    /// Immutable slug, used in every `/api/v1/orgs/{org}` path.
    pub org: String,
    pub name: String,
    pub role: MemberRole,
}

/// Kind of a prefixed id accepted by `GET /api/v1/resolve/{id}` (short links).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ResolvedKind {
    Ticket,
    Run,
    Cause,
    Artifact,
}

/// `GET /api/v1/resolve/{id}`: the org (slug) owning the id, if the caller is a member (else 404).
/// `ticket` is set for kind `cause` (the brief and causes live on the ticket page).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ResolvedId {
    pub org: String,
    pub kind: ResolvedKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub ticket: Option<crate::domain::TicketId>,
}

/// `POST /api/v1/orgs` (public product only; v1 answers 404 and orgs are created by the CLI).
/// Response: `Org`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct CreateOrgRequest {
    pub slug: String,
    pub name: String,
}
