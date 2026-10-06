//! Installation repositories, connect, disconnect and on-demand indexing.

use std::collections::BTreeSet;

use mage_core::domain::{CommitSha, InstallationId, JobPayload, OrgId, Repo, RepoAccess, RepoId, VersionReason};
use mage_core::ports::NewJob;
use mage_core::ports::source_host::{RemoteRepo, SourceHostError};
use mage_core::wire::api::{InstallationRepo, InstallationRepoList, JobQueued};

use super::{ReposService, coords, repo_err};
use crate::error::AppError;

/// Longest rev accepted by `POST …/versions` (git refs are far shorter in practice).
const MAX_REV_BYTES: usize = 255;

impl ReposService {
    /// Repositories of the org's GitHub installation(s) that are not connected (not stored, or
    /// stored with `access: lost`). Archived repositories are left out.
    pub async fn installation_repos(&self, org: OrgId) -> Result<InstallationRepoList, AppError> {
        let connected: BTreeSet<u64> = self
            .repos
            .list_repos(org)
            .await
            .map_err(repo_err)?
            .into_iter()
            .filter(|r| r.access == RepoAccess::Granted)
            .map(|r| r.external_id)
            .collect();
        let mut repositories = Vec::new();
        for inst in self.installations(org).await? {
            for remote in self.source.installation_repos(inst).await? {
                if remote.archived || connected.contains(&remote.external_id) {
                    continue;
                }
                repositories.push(InstallationRepo {
                    external_id: remote.external_id,
                    owner: remote.coords.owner,
                    name: remote.coords.name,
                    default_branch: remote.default_branch,
                    private: remote.private,
                });
            }
        }
        repositories.sort_by_cached_key(|r| (r.owner.to_lowercase(), r.name.to_lowercase(), r.external_id));
        Ok(InstallationRepoList { repositories })
    }

    /// Connect an installation repository: store it `Granted` (a reconnect keeps its id) and
    /// enqueue `index.version` for the default-branch head in the same transaction.
    pub async fn connect(&self, org: OrgId, external_id: u64) -> Result<Repo, AppError> {
        let (inst, remote) = self.find_installation_repo(org, external_id).await?;
        if remote.archived {
            return Err(AppError::Invalid(format!("repository {} is archived", remote.coords)));
        }
        let existing = self.repos.get_by_external_id(org, external_id).await.map_err(repo_err)?;
        let (id, created_at) = match &existing {
            Some(repo) => (repo.id, repo.created_at),
            None => (self.ids.new_id::<RepoId>(), self.clock.now()),
        };
        let repo = Repo {
            id,
            org_id: org,
            external_id,
            owner: remote.coords.owner.clone(),
            name: remote.coords.name.clone(),
            default_branch: remote.default_branch.clone(),
            installation_id: inst,
            access: RepoAccess::Granted,
            created_at,
        };
        // An empty repository has no default-branch head yet; the first push indexes it.
        let head = match self.source.resolve_ref(inst, &remote.coords, &format!("refs/heads/{}", remote.default_branch)).await {
            Ok(sha) => Some(sha),
            Err(SourceHostError::NotFound) => None,
            Err(err) => return Err(err.into()),
        };
        let jobs: Vec<NewJob> = head
            .map(|sha| NewJob::new(JobPayload::IndexVersion { org_id: org, repo_id: id, sha, reason: VersionReason::DefaultHead }))
            .into_iter()
            .collect();
        let stored = self.repos.upsert_repo(&repo, &jobs).await.map_err(repo_err)?;
        if let Some(sha) = head {
            self.repos.set_default_head(org, stored.id, sha).await.map_err(repo_err)?;
        }
        Ok(stored)
    }

    /// Disconnect: `access = Lost`. Rows stay because tickets and citations refer to them
    /// (intake.md); versions age out under knowledge.md's retention table (`index.gc`).
    pub async fn disconnect(&self, org: OrgId, id: RepoId) -> Result<(), AppError> {
        self.repos.set_access(org, id, RepoAccess::Lost).await.map_err(repo_err)
    }

    /// On-demand index of `rev`. Without a ticket only two reasons fit (knowledge.md): the default
    /// branch (`DefaultHead`) and a tag (`Release(tag)`); other refs are `Invalid`.
    pub async fn index_rev(&self, org: OrgId, id: RepoId, rev: &str) -> Result<JobQueued, AppError> {
        let rev = rev.trim();
        if rev.is_empty() || rev.len() > MAX_REV_BYTES || rev.chars().any(char::is_whitespace) {
            return Err(AppError::Invalid(format!("rev must be 1..={MAX_REV_BYTES} bytes without whitespace")));
        }
        let repo = self.connected_repo(org, id).await?;
        let (sha, reason) = if rev.strip_prefix("refs/heads/").unwrap_or(rev) == repo.default_branch {
            let sha = self.resolve(&repo, &format!("refs/heads/{}", repo.default_branch)).await?;
            (sha, VersionReason::DefaultHead)
        } else {
            let tag = rev.strip_prefix("refs/tags/").unwrap_or(rev);
            match self.resolve(&repo, &format!("refs/tags/{tag}")).await {
                Ok(sha) => (sha, VersionReason::Release(tag.to_owned())),
                Err(AppError::NotFound(_)) => {
                    return Err(AppError::Invalid(format!(
                        "`{rev}` is neither the default branch `{}` nor a tag; other refs are indexed on demand only for a ticket",
                        repo.default_branch
                    )));
                }
                Err(err) => return Err(err),
            }
        };
        self.index(org, id, sha, reason).await
    }

    /// `SourceHost::resolve_ref` on the repo's installation.
    pub(super) async fn resolve(&self, repo: &Repo, git_ref: &str) -> Result<CommitSha, AppError> {
        self.source.resolve_ref(repo.installation_id, &coords(repo), git_ref).await.map_err(|err| match err {
            SourceHostError::NotFound => AppError::NotFound(format!("`{git_ref}` in {}/{}", repo.owner, repo.name)),
            other => other.into(),
        })
    }

    async fn find_installation_repo(&self, org: OrgId, external_id: u64) -> Result<(InstallationId, RemoteRepo), AppError> {
        for inst in self.installations(org).await? {
            if let Some(remote) = self.source.installation_repos(inst).await?.into_iter().find(|r| r.external_id == external_id) {
                return Ok((inst, remote));
            }
        }
        Err(AppError::Invalid(format!("repository {external_id} is not granted to the org's GitHub installation")))
    }
}
