//! Repos, versions, recipes, deployments and customers (server.md catalog; knowledge.md layers;
//! executor.md recipes).

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use ts_rs::TS;

use crate::domain::recipe::RecipeReport;
use crate::domain::{
    CommitSha, Customer, Deployment, ExternalRef, HarnessKind, LayerMap, Recipe, RecipeHash, RecipeStatus, Repo, VersionReason,
};
use crate::wire::api::RunSummary;

/// An indexed commit and the state of each knowledge layer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct VersionView {
    pub sha: CommitSha,
    pub reasons: Vec<VersionReason>,
    pub layers: LayerMap,
    #[serde(with = "time::serde::rfc3339")]
    #[ts(type = "string")]
    pub created_at: OffsetDateTime,
}

/// Status of the repo's current recipe, without its body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct RecipeBrief {
    pub hash: RecipeHash,
    pub status: RecipeStatus,
}

/// A row of the repos page: layers at the default head, recipe status, last Learn run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct RepoSummary {
    pub repo: Repo,
    pub head: Option<VersionView>,
    pub recipe: Option<RecipeBrief>,
    pub last_learn: Option<RunSummary>,
}

/// `GET …/repos`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct RepoList {
    pub repos: Vec<RepoSummary>,
}

/// `GET …/repos/{repo}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct RepoDetail {
    pub repo: Repo,
    /// Newest first.
    pub versions: Vec<VersionView>,
    pub recipe: Option<Recipe>,
    pub deployments: Vec<Deployment>,
    pub last_learn: Option<RunSummary>,
}

/// A repository of the GitHub installation that is not connected yet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct InstallationRepo {
    /// GitHub repository id.
    pub external_id: u64,
    pub owner: String,
    pub name: String,
    pub default_branch: String,
    pub private: bool,
}

/// `GET …/integrations/github/repositories`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct InstallationRepoList {
    pub repositories: Vec<InstallationRepo>,
}

/// `POST …/repos`: connect an installation repository. Response: `Repo`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ConnectRepoRequest {
    pub external_id: u64,
}

/// `POST …/repos/{repo}/versions`: index a branch, tag or sha on demand. Response: `JobQueued`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct IndexVersionRequest {
    pub rev: String,
}

/// `POST …/repos/{repo}/learn` (onboarding wizard). Response: `RunStarted`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct StartLearnRequest {
    /// Default: the default-branch head version.
    #[serde(default)]
    #[ts(optional)]
    pub sha: Option<CommitSha>,
    #[serde(default)]
    #[ts(optional)]
    pub harness: Option<HarnessKind>,
}

/// `GET …/repos/{repo}/recipes`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct RecipeHistory {
    pub current: Option<Recipe>,
    /// Earlier recipes, newest first.
    pub history: Vec<Recipe>,
}

/// `PUT …/repos/{repo}/recipe`: save a Draft (parse, normalize, hash, secret scan).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct SaveRecipeRequest {
    pub toml: String,
}

impl SaveRecipeRequest {
    /// Body limit of this route.
    pub const MAX_BYTES: usize = 256 * 1024;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct SavedRecipe {
    pub recipe: Recipe,
    pub report: RecipeReport,
}

/// `POST …/repos/{repo}/recipe/verify`: Boot gate Verify run. Response: `RunStarted`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct VerifyRecipeRequest {
    pub hash: RecipeHash,
    /// Default: the default-branch head version.
    #[serde(default)]
    #[ts(optional)]
    pub sha: Option<CommitSha>,
}

/// `GET …/repos/{repo}/deployments`: newest observation per environment first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct DeploymentList {
    pub deployments: Vec<Deployment>,
}

/// `PUT …/repos/{repo}/deployments/{environment}`: manual mapping. Response: `Deployment`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct SetDeploymentRequest {
    pub sha: CommitSha,
}

/// `GET …/customers`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct CustomerList {
    pub customers: Vec<Customer>,
}

/// `POST …/customers`, `PUT …/customers/{customer}`. Response: `Customer`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct CustomerInput {
    pub name: String,
    pub external_refs: Vec<ExternalRef>,
    #[serde(default)]
    #[ts(optional)]
    pub environment: Option<String>,
}
