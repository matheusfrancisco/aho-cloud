//! `PgDeploymentResolver` (intake.md "`DeploymentResolver`", storage.md port map): the recorded
//! deployment history (`deployments`: sources `Manual`, `Tag`, `GitHubDeployments`), read through
//! `RepoStore::latest_deployment` (`mage_infra::pg::PgRepoStore`: `observed_at <= as_of ORDER BY
//! observed_at DESC LIMIT 1`). First link of `app::intake::DeploymentChain`.

use std::sync::Arc;

use async_trait::async_trait;
use mage_core::domain::{Deployment, OrgId, Repo};
use mage_core::ports::RepoStore;
use time::OffsetDateTime;

use crate::ports::deployments::{DeploymentError, DeploymentResolver};

pub struct PgDeploymentResolver {
    repos: Arc<dyn RepoStore>,
}

impl PgDeploymentResolver {
    pub fn new(repos: Arc<dyn RepoStore>) -> Self {
        Self { repos }
    }
}

#[async_trait]
impl DeploymentResolver for PgDeploymentResolver {
    async fn deployed_sha(&self, org: OrgId, repo: &Repo, environment: &str, as_of: OffsetDateTime) -> Result<Option<Deployment>, DeploymentError> {
        Ok(self.repos.latest_deployment(org, repo.id, environment, as_of).await?)
    }
}

#[cfg(test)]
mod tests {
    use mage_core::domain::{Deployment, DeploymentSource};
    use mage_core::testing::{FakeClock, SeqIdGen};
    use mage_infra::pg::PgRepoStore;

    use super::*;
    use crate::testing::contract_deployment_resolver;
    use crate::testing::pg::with_pg;
    use crate::testing::seed::Seed;

    /// Opt-in: `DATABASE_URL` pointing at Postgres 16 (Docker `postgres:16`).
    #[tokio::test]
    async fn pg_deployment_resolver_passes_the_contract() {
        with_pg("pg_deployment_resolver_passes_the_contract", |env| async move {
            contract_deployment_resolver(|org, repo, seeds| {
                let env = env.clone();
                async move {
                    let clock = Arc::new(FakeClock::new());
                    let db = env.fresh().await;
                    db.seed(clock.clone()).org(org).await;
                    let repos = Arc::new(PgRepoStore::new(db.pools.clone(), clock, Arc::new(SeqIdGen::new())));
                    repos.upsert_repo(&repo, &[]).await.expect("repo");
                    for s in seeds.into_iter().filter(|s| s.success) {
                        let d = Deployment {
                            org_id: org,
                            repo_id: repo.id,
                            environment: s.environment,
                            sha: s.sha,
                            observed_at: s.observed_at,
                            source: DeploymentSource::Manual,
                        };
                        repos.record_deployment(org, &d).await.expect("record deployment");
                    }
                    PgDeploymentResolver::new(repos)
                }
            })
            .await;
        })
        .await;
    }
}
