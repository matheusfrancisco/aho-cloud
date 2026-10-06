//! Customer and Deployment (domain.md; `ExternalRef` variants from intake.md Additions).

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::domain::ids::{CommitSha, CustomerId, OrgId, RepoId};

/// A reference that names a customer in an external system. A ref names at most one customer.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, ts_rs::TS)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum ExternalRef {
    LinearCustomer(String),
    LinearLabel(String),
    SlackChannel { team: String, channel: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
pub struct Customer {
    pub id: CustomerId,
    pub org_id: OrgId,
    pub name: String,
    pub external_refs: Vec<ExternalRef>,
    /// Deployment environment this customer runs on (used to resolve a ticket's sha).
    pub environment: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ts_rs::TS)]
pub enum DeploymentSource {
    #[serde(rename = "github_deployments")]
    GitHubDeployments,
    #[serde(rename = "manual")]
    Manual,
    #[serde(rename = "tag")]
    Tag,
}

/// One observation of `sha` deployed to `environment` (append-only history).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
pub struct Deployment {
    pub org_id: OrgId,
    pub repo_id: RepoId,
    pub environment: String,
    pub sha: CommitSha,
    #[serde(with = "time::serde::rfc3339")]
    #[ts(type = "string")]
    pub observed_at: OffsetDateTime,
    pub source: DeploymentSource,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn external_ref_serde_shapes() {
        let r = ExternalRef::SlackChannel { team: "T1".into(), channel: "C2".into() };
        assert_eq!(
            serde_json::to_value(&r).unwrap(),
            serde_json::json!({"type": "slack_channel", "data": {"team": "T1", "channel": "C2"}})
        );
        let l = ExternalRef::LinearLabel("acme".into());
        assert_eq!(serde_json::to_value(&l).unwrap(), serde_json::json!({"type": "linear_label", "data": "acme"}));
        assert_eq!(serde_json::to_string(&DeploymentSource::GitHubDeployments).unwrap(), "\"github_deployments\"");
    }
}
