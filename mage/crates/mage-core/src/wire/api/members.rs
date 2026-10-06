//! Members, invites, audit log and usage (server.md catalog, "RBAC", "Audit log", "Metering").

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use ts_rs::TS;

use crate::domain::{MemberRole, RunnerId, User, UserId};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct MemberView {
    pub user: User,
    pub role: MemberRole,
}

/// A pending invite (expires after 14 days; accepted on the invitee's next login).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct InviteView {
    /// Opaque invite id for `DELETE …/members/invites/{invite}`.
    pub id: String,
    /// GitHub login as invited (resolved to the stable GitHub id server-side).
    pub login: String,
    pub role: MemberRole,
    pub invited_by: UserId,
    #[serde(with = "time::serde::rfc3339")]
    #[ts(type = "string")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    #[ts(type = "string")]
    pub expires_at: OffsetDateTime,
}

/// `GET …/members`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct MemberList {
    pub members: Vec<MemberView>,
    pub invites: Vec<InviteView>,
}

/// `POST …/members/invites` (Admin; invited role ≤ caller's role). Response: `InviteView`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct InviteRequest {
    pub github_login: String,
    pub role: MemberRole,
}

/// `PUT …/members/{user}` (Owner). Response: `MemberView`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct SetRoleRequest {
    pub role: MemberRole,
}

// ---------------------------------------------------------------------------------------------
// Audit
// ---------------------------------------------------------------------------------------------

/// Who did it (server.md `Actor`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AuditActor {
    User { id: UserId },
    Runner { id: RunnerId },
    Operator { os_user: String },
    System,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum AuditOutcome {
    Success,
    Denied,
    Failed,
}

/// `GET …/audit?action=&before=&limit=` (Admin), newest first.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
pub struct AuditQuery {
    /// `area.verb`, e.g. `member.invited`.
    #[ts(optional)]
    pub action: Option<String>,
    /// Entries with `id < before`.
    #[ts(optional)]
    pub before: Option<i64>,
    #[ts(optional)]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct AuditDetail {
    pub key: String,
    /// Fingerprints and ids only; never secret values.
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct AuditEntry {
    pub id: i64,
    #[serde(with = "time::serde::rfc3339")]
    #[ts(type = "string")]
    pub at: OffsetDateTime,
    pub actor: AuditActor,
    /// `area.verb`.
    pub action: String,
    pub target_kind: String,
    pub target_id: String,
    pub outcome: AuditOutcome,
    pub ip: Option<String>,
    pub request_id: Option<String>,
    pub details: Vec<AuditDetail>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct AuditPage {
    pub entries: Vec<AuditEntry>,
    /// Pass as `before` for the next page.
    pub next_before: Option<i64>,
}

// ---------------------------------------------------------------------------------------------
// Usage
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum UsageGroupBy {
    Run,
    Ticket,
    Model,
}

/// `GET …/usage?from=&to=&group_by=` (Admin): metered cost in `[from, to)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct UsageQuery {
    #[serde(with = "time::serde::rfc3339")]
    #[ts(type = "string")]
    pub from: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    #[ts(type = "string")]
    pub to: OffsetDateTime,
    pub group_by: UsageGroupBy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct UsageRow {
    /// Run id, ticket id or model id, per `group_by`.
    pub key: String,
    /// Human label (ticket identifier, run kind), if any.
    pub label: Option<String>,
    pub runs: u32,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_micros: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct UsageReport {
    #[serde(with = "time::serde::rfc3339")]
    #[ts(type = "string")]
    pub from: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    #[ts(type = "string")]
    pub to: OffsetDateTime,
    pub group_by: UsageGroupBy,
    /// Highest cost first.
    pub rows: Vec<UsageRow>,
    pub total_cost_micros: u64,
}
