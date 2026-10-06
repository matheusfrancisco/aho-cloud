//! `GET …/repos` and `GET …/repos/{repo}`.

use std::collections::BTreeSet;

use mage_core::domain::{Deployment, OrgId, Repo, RepoId, RepoVersion, RepoVersionKey, RunKind};
use mage_core::ports::RepoStoreError;
use mage_core::wire::api::{RecipeBrief, RepoDetail, RepoList, RepoSummary, RunSummary, VersionView};

use super::{ReposService, collect_pages, repo_err};
use crate::error::AppError;
use crate::ports::runs::RunQuery;

impl ReposService {
    /// Every repo of the org (disconnected ones show `access: lost`) with layers at the default
    /// head, recipe status and last Learn run.
    pub async fn list_repos(&self, org: OrgId) -> Result<RepoList, AppError> {
        let repos = self.repos.list_repos(org).await.map_err(repo_err)?;
        let mut out = Vec::with_capacity(repos.len());
        for repo in repos {
            out.push(self.summary(org, repo).await?);
        }
        Ok(RepoList { repos: out })
    }

    pub async fn repo_detail(&self, org: OrgId, id: RepoId) -> Result<RepoDetail, AppError> {
        let repo = self.repo(org, id).await?;
        let versions = self.repos.versions(org, id).await.map_err(repo_err)?;
        let recipe = self.repos.latest_recipe(org, id).await.map_err(repo_err)?;
        let deployments = self.current_deployments(org, id).await?;
        let last_learn = self.last_learn(org, id).await?;
        // The store orders oldest first.
        let versions = versions.into_iter().rev().map(version_view).collect();
        Ok(RepoDetail { repo, versions, recipe, deployments, last_learn })
    }

    async fn summary(&self, org: OrgId, repo: Repo) -> Result<RepoSummary, AppError> {
        let head = match self.repos.default_head(org, repo.id).await.map_err(repo_err)? {
            Some(sha) => match self.repos.version(org, RepoVersionKey::new(repo.id, sha)).await {
                Ok(version) => Some(version_view(version)),
                // Removed between the two reads (`index.gc`).
                Err(RepoStoreError::NotFound) => None,
                Err(err) => return Err(repo_err(err)),
            },
            None => None,
        };
        let recipe = self
            .repos
            .latest_recipe(org, repo.id)
            .await
            .map_err(repo_err)?
            .map(|r| RecipeBrief { hash: r.hash, status: r.status.clone() });
        let last_learn = self.last_learn(org, repo.id).await?;
        Ok(RepoSummary { repo, head, recipe, last_learn })
    }

    async fn last_learn(&self, org: OrgId, repo: RepoId) -> Result<Option<RunSummary>, AppError> {
        let query = RunQuery { kind: Some(RunKind::Learn), repo: Some(repo), newest_first: true, limit: 1, ..Default::default() };
        let runs = self.runs.list(org, &query).await?;
        Ok(runs.first().map(|rec| RunSummary::of(&rec.run)))
    }

    /// Newest observation per environment, newest first (`list_deployments` is newest first).
    pub(super) async fn current_deployments(&self, org: OrgId, repo: RepoId) -> Result<Vec<Deployment>, AppError> {
        let history = collect_pages(|page| self.repos.list_deployments(org, repo, page)).await.map_err(repo_err)?;
        let mut seen = BTreeSet::new();
        Ok(history.into_iter().filter(|d| seen.insert(d.environment.clone())).collect())
    }
}

fn version_view(v: RepoVersion) -> VersionView {
    VersionView { sha: v.sha, reasons: v.reasons.into_iter().collect(), layers: v.layers, created_at: v.created_at }
}
