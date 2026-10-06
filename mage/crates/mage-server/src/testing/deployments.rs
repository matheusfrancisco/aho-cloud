//! Generic `DeploymentResolver` contract suite (intake.md "Port contract tests"): run against
//! [`super::FakeDeploymentResolver`], the pg resolver (`RepoStore` history) and the GitHub
//! Deployments API resolver (recorded HTTP).

use mage_core::domain::{CommitSha, Deployment, InstallationId, OrgId, Repo, RepoAccess, RepoId};
use time::OffsetDateTime;
use time::macros::datetime;
use uuid::Uuid;

use crate::ports::deployments::{DeploymentError, DeploymentResolver};

/// One deployment status the implementation under test must know about. `success = false`
/// models a `failure`/`error`/`inactive` status; history-backed implementations simply do not
/// record it (IngestDeployment never does).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeedDeployment {
    pub environment: String,
    pub sha: CommitSha,
    pub observed_at: OffsetDateTime,
    pub success: bool,
}

const T1: OffsetDateTime = datetime!(2026-09-01 10:00 UTC);
const T2: OffsetDateTime = datetime!(2026-09-03 10:00 UTC);

fn sha(byte: u8) -> CommitSha {
    CommitSha::from_bytes([byte; 20])
}

fn seed(environment: &str, sha_byte: u8, observed_at: OffsetDateTime, success: bool) -> SeedDeployment {
    SeedDeployment { environment: environment.into(), sha: sha(sha_byte), observed_at, success }
}

/// The org and repo every scenario uses; factories create them in their backing store.
pub fn contract_org() -> OrgId {
    OrgId::from_uuid(Uuid::from_u128(0xd0_0001))
}

pub fn contract_repo() -> Repo {
    Repo {
        id: RepoId::from_uuid(Uuid::from_u128(0xd0_0002)),
        org_id: contract_org(),
        external_id: 4242,
        owner: "acme".into(),
        name: "shop".into(),
        default_branch: "main".into(),
        installation_id: InstallationId(77),
        access: RepoAccess::Granted,
        created_at: datetime!(2026-08-01 00:00 UTC),
    }
}

async fn production<R: DeploymentResolver>(r: &R, org: OrgId, repo: &Repo, as_of: OffsetDateTime) -> Result<Option<Deployment>, DeploymentError> {
    r.deployed_sha(org, repo, "production", as_of).await
}

/// `make(org, repo, seeds)` returns a resolver that knows exactly `seeds` for `repo`.
pub async fn contract_deployment_resolver<F, Fut, R>(make: F)
where
    F: Fn(OrgId, Repo, Vec<SeedDeployment>) -> Fut,
    Fut: Future<Output = R>,
    R: DeploymentResolver,
{
    let org = contract_org();
    let repo = contract_repo();

    // Deployments at T1 (A) and T2 (B): as_of between → A, before T1 → None, after T2 → B,
    // exactly T2 → B (observed_at <= as_of).
    let r = make(org, repo.clone(), vec![seed("production", 0xa1, T1, true), seed("production", 0xb2, T2, true)]).await;
    let between = production(&r, org, &repo, datetime!(2026-09-02 10:00 UTC)).await.expect("between");
    assert_eq!(between.as_ref().map(|d| d.sha), Some(sha(0xa1)), "as_of between T1 and T2 must give A");
    let d = between.unwrap();
    assert_eq!((d.org_id, d.repo_id, d.environment.as_str(), d.observed_at), (org, repo.id, "production", T1));
    assert_eq!(production(&r, org, &repo, datetime!(2026-08-31 10:00 UTC)).await.expect("before"), None, "as_of before T1 must give None");
    let after = production(&r, org, &repo, datetime!(2026-09-04 10:00 UTC)).await.expect("after");
    assert_eq!(after.map(|d| d.sha), Some(sha(0xb2)), "after T2 → B");
    assert_eq!(production(&r, org, &repo, T2).await.expect("exact").map(|d| d.sha), Some(sha(0xb2)), "observed_at == as_of counts");

    // Non-success statuses are ignored.
    let r = make(org, repo.clone(), vec![seed("production", 0xa1, T1, true), seed("production", 0xb2, T2, false)]).await;
    let latest = production(&r, org, &repo, datetime!(2026-09-04 10:00 UTC)).await.expect("failed latest");
    assert_eq!(latest.map(|d| d.sha), Some(sha(0xa1)));

    // Environments do not mix.
    let r = make(org, repo.clone(), vec![seed("staging", 0xc3, T1, true)]).await;
    assert_eq!(production(&r, org, &repo, T2).await.expect("other env"), None);
    let staging = r.deployed_sha(org, &repo, "staging", T2).await.expect("staging");
    assert_eq!(staging.map(|d| d.sha), Some(sha(0xc3)));
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use mage_core::domain::{Deployment, DeploymentSource};
    use mage_core::ports::RepoStore;
    use mage_core::testing::{FakeClock, MemJobQueue, MemRepoStore, SeqIdGen};

    use super::*;
    use crate::adapters::driven::pg::deployments_manual::PgDeploymentResolver;
    use crate::testing::FakeDeploymentResolver;

    #[tokio::test]
    async fn fake_resolver_passes_contract() {
        contract_deployment_resolver(|org, repo, seeds| async move {
            let fake = FakeDeploymentResolver::new();
            for s in seeds.into_iter().filter(|s| s.success) {
                fake.add(Deployment {
                    org_id: org,
                    repo_id: repo.id,
                    environment: s.environment,
                    sha: s.sha,
                    observed_at: s.observed_at,
                    source: DeploymentSource::GitHubDeployments,
                });
            }
            fake
        })
        .await;
    }

    /// The history resolver over the in-memory `RepoStore` (the same delegate the pg adapter
    /// uses; `mage_infra::pg::PgRepoStore` is covered by `mage_core`'s RepoStore contract).
    #[tokio::test]
    async fn history_resolver_passes_contract() {
        contract_deployment_resolver(|org, repo, seeds| async move {
            let clock = Arc::new(FakeClock::new());
            let jobs = Arc::new(MemJobQueue::new(clock.clone(), Arc::new(SeqIdGen::new())));
            let repos = Arc::new(MemRepoStore::new(clock, jobs));
            repos.upsert_repo(&repo, &[]).await.unwrap();
            for s in seeds.into_iter().filter(|s| s.success) {
                let d = Deployment {
                    org_id: org,
                    repo_id: repo.id,
                    environment: s.environment,
                    sha: s.sha,
                    observed_at: s.observed_at,
                    source: DeploymentSource::Manual,
                };
                repos.record_deployment(org, &d).await.unwrap();
            }
            PgDeploymentResolver::new(repos)
        })
        .await;
    }
}
