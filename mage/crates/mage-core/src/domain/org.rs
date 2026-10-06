//! Org, User, Membership, Integration (domain.md; enum names from storage.md Additions).

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::domain::ids::{OrgId, UserId};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
pub struct Org {
    pub id: OrgId,
    /// Immutable; used in UI URLs.
    pub slug: String,
    pub name: String,
    #[serde(with = "time::serde::rfc3339")]
    #[ts(type = "string")]
    pub created_at: OffsetDateTime,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
pub struct User {
    pub id: UserId,
    pub email: String,
    pub name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "snake_case")]
pub enum MemberRole {
    Member,
    Admin,
    Owner,
}

impl MemberRole {
    /// Role hierarchy: Owner ⊇ Admin ⊇ Member.
    pub fn at_least(self, required: MemberRole) -> bool {
        self >= required
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ts_rs::TS)]
pub struct Membership {
    pub org_id: OrgId,
    pub user_id: UserId,
    pub role: MemberRole,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ts_rs::TS)]
pub enum IntegrationKind {
    /// Read-only GitHub App (`SourceHost`).
    #[serde(rename = "github")]
    GitHub,
    /// Separate write App used only to open fix PRs (fix.md).
    #[serde(rename = "github_fix")]
    GitHubFix,
    #[serde(rename = "linear")]
    Linear,
    #[serde(rename = "slack")]
    Slack,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "snake_case")]
pub enum IntegrationStatus {
    Active,
    Revoked,
}

/// `external_id` = GitHub installation id / Linear workspace id / Slack team id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
pub struct Integration {
    pub org_id: OrgId,
    pub kind: IntegrationKind,
    pub external_id: String,
    pub status: IntegrationStatus,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_hierarchy() {
        use MemberRole::*;
        assert!(Owner.at_least(Admin) && Owner.at_least(Member) && Owner.at_least(Owner));
        assert!(Admin.at_least(Member) && !Admin.at_least(Owner));
        assert!(Member.at_least(Member) && !Member.at_least(Admin));
    }

    #[test]
    fn integration_kind_storage_names() {
        let names: Vec<String> = [IntegrationKind::GitHub, IntegrationKind::GitHubFix, IntegrationKind::Linear, IntegrationKind::Slack]
            .iter()
            .map(|k| serde_json::to_string(k).unwrap())
            .collect();
        assert_eq!(names, ["\"github\"", "\"github_fix\"", "\"linear\"", "\"slack\""]);
    }
}
