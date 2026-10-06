//! Live `DeploymentResolver` over the GitHub Deployments API (intake.md "`DeploymentResolver`",
//! "`deployed_sha` algorithm"), for repos whose recorded history has gaps (installed recently,
//! missed webhooks). Last in `app::intake::DeploymentChain`, after the pg history resolver.
//!
//! Walks `GET /repos/{o}/{r}/deployments?environment=&per_page=30` newest first, skipping
//! deployments created after `as_of`; for each, `GET …/deployments/{id}/statuses?per_page=10`
//! and the first `success` status with `created_at <= as_of` answers. Three pages without an
//! answer → `Ok(None)`. Calls use an installation token scoped to `deployments: read` on the repo.

use std::collections::BTreeMap;

use async_trait::async_trait;
use mage_core::domain::{CommitSha, Deployment, DeploymentSource, OrgId, Repo};
use mage_core::ports::source_host::{Access, TokenScope};
use mage_infra::github::{Auth, GitHubClient};
use serde::Deserialize;
use time::OffsetDateTime;

use crate::ports::deployments::{DeploymentError, DeploymentResolver};

/// Deployment pages read before giving up.
const MAX_PAGES: usize = 3;
const DEPLOYMENTS_PER_PAGE: u8 = 30;
const STATUSES_PER_PAGE: u8 = 10;

/// `DeploymentResolver` on the read app's client.
pub struct GithubDeploymentResolver {
    client: GitHubClient,
}

impl GithubDeploymentResolver {
    pub fn new(client: GitHubClient) -> Self {
        Self { client }
    }
}

#[async_trait]
impl DeploymentResolver for GithubDeploymentResolver {
    async fn deployed_sha(&self, org: OrgId, repo: &Repo, environment: &str, as_of: OffsetDateTime)
    -> Result<Option<Deployment>, DeploymentError> {
        let scope = TokenScope {
            repositories: Some(vec![repo.name.clone()]),
            permissions: BTreeMap::from([("deployments".to_owned(), Access::Read)]),
        };
        let auth = || Auth::Installation { inst: repo.installation_id, scope: &scope };
        let base = format!("/repos/{}/{}/deployments", repo.owner, repo.name);
        let env: String = url::form_urlencoded::byte_serialize(environment.as_bytes()).collect();
        let mut url = format!("{base}?environment={env}&per_page={DEPLOYMENTS_PER_PAGE}");

        for _ in 0..MAX_PAGES {
            let page = self.client.get::<Vec<DeploymentJson>>(auth(), &url).await?;
            let mut deployments = page.body;
            // Newest first whatever the API's default order turns out to be within a page.
            deployments.sort_by(|a, b| (b.created_at, b.id).cmp(&(a.created_at, a.id)));
            for deployment in deployments.iter().filter(|d| d.created_at <= as_of) {
                let statuses_url = format!("{base}/{}/statuses?per_page={STATUSES_PER_PAGE}", deployment.id);
                let statuses = self.client.get::<Vec<StatusJson>>(auth(), &statuses_url).await?.body;
                let success = statuses.iter().filter(|s| s.state == "success" && s.created_at <= as_of).max_by_key(|s| (s.created_at, s.id));
                if let Some(status) = success {
                    let sha: CommitSha = deployment
                        .sha
                        .parse()
                        .map_err(|e| DeploymentError::Invalid(format!("deployment {} sha {:?}: {e}", deployment.id, deployment.sha)))?;
                    return Ok(Some(Deployment {
                        org_id: org,
                        repo_id: repo.id,
                        environment: environment.to_owned(),
                        sha,
                        observed_at: status.created_at,
                        source: DeploymentSource::GitHubDeployments,
                    }));
                }
            }
            match page.next {
                Some(next) => url = next,
                None => return Ok(None),
            }
        }
        Ok(None)
    }
}

#[derive(Deserialize)]
struct DeploymentJson {
    id: u64,
    sha: String,
    #[serde(with = "time::serde::rfc3339")]
    created_at: OffsetDateTime,
}

#[derive(Deserialize)]
struct StatusJson {
    id: u64,
    state: String,
    #[serde(with = "time::serde::rfc3339")]
    created_at: OffsetDateTime,
}

#[cfg(test)]
pub(super) mod tests {
    use std::sync::Arc;

    use mage_core::domain::{InstallationId, RepoAccess, RepoId};
    use mage_core::testing::FakeClock;
    use mage_infra::github::{AppCredentials, GitHubConfig};
    use serde_json::{Value, json};
    use time::format_description::well_known::Rfc3339;
    use time::macros::datetime;
    use url::Url;
    use uuid::Uuid;
    use wiremock::matchers::{body_partial_json, method, path, query_param, query_param_is_missing};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::testing::{SeedDeployment, contract_deployment_resolver};

    /// Test RSA key shared with `mage_infra`'s GitHub client tests.
    const APP_KEY_PEM: &str = include_str!("../../../../../mage-infra/tests/fixtures/github_app_key.pem");
    pub(in crate::adapters::driven::github) const NOW: OffsetDateTime = datetime!(2026-10-05 12:00 UTC);

    /// A client for one GitHub App whose API and web bases both point at `server`.
    pub(in crate::adapters::driven::github) fn client(server: &MockServer) -> GitHubClient {
        let base = Url::parse(&format!("{}/", server.uri())).unwrap();
        let config = GitHubConfig { api_base: base.clone(), web_base: base, ..GitHubConfig::default() };
        let creds = AppCredentials { client_id: "Iv1.test".into(), private_key_pem: APP_KEY_PEM.into() };
        GitHubClient::new(&config, creds, Arc::new(FakeClock::at(NOW))).unwrap()
    }

    fn ts(at: OffsetDateTime) -> String {
        at.format(&Rfc3339).unwrap()
    }

    fn repo() -> Repo {
        Repo {
            id: RepoId::from_uuid(Uuid::from_u128(9)),
            org_id: org(),
            external_id: 4242,
            owner: "acme".into(),
            name: "shop".into(),
            default_branch: "main".into(),
            installation_id: InstallationId(77),
            access: RepoAccess::Granted,
            created_at: datetime!(2026-08-01 00:00 UTC),
        }
    }

    fn org() -> OrgId {
        OrgId::from_uuid(Uuid::from_u128(1))
    }

    fn sha(byte: u8) -> CommitSha {
        CommitSha::from_bytes([byte; 20])
    }

    /// Installation token for `repo` scoped to `deployments: read` on that repo only.
    async fn mount_token(server: &MockServer, repo: &Repo) {
        Mock::given(method("POST"))
            .and(path(format!("/app/installations/{}/access_tokens", repo.installation_id.0)))
            .and(body_partial_json(json!({"repositories": [repo.name], "permissions": {"deployments": "read"}})))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({
                "token": "ghs_deploymentsreadtoken0000000000000",
                "expires_at": ts(NOW + time::Duration::hours(1)),
                "permissions": {"deployments": "read"},
                "repository_selection": "selected"
            })))
            .mount(server)
            .await;
    }

    fn deployment_json(repo: &Repo, id: u64, sha_byte: u8, environment: &str, created_at: OffsetDateTime) -> Value {
        json!({
            "url": format!("https://api.github.com/repos/{}/{}/deployments/{id}", repo.owner, repo.name),
            "id": id, "sha": sha(sha_byte).to_hex(), "ref": "main", "task": "deploy",
            "environment": environment, "original_environment": environment,
            "created_at": ts(created_at), "updated_at": ts(created_at),
            "creator": {"login": "github-actions[bot]", "type": "Bot"}, "payload": {},
            "transient_environment": false, "production_environment": environment == "production"
        })
    }

    fn status_json(id: u64, state: &str, created_at: OffsetDateTime) -> Value {
        json!({"id": id, "state": state, "description": "", "environment": "production",
               "created_at": ts(created_at), "updated_at": ts(created_at), "target_url": ""})
    }

    async fn mount_deployments(server: &MockServer, repo: &Repo, environment: &str, page: Option<u32>, next_page: Option<u32>, body: Vec<Value>) {
        let mut response = ResponseTemplate::new(200).set_body_json(Value::Array(body));
        if let Some(next) = next_page {
            let link = format!(
                "<{}/repos/{}/{}/deployments?environment={environment}&per_page=30&page={next}>; rel=\"next\"",
                server.uri(),
                repo.owner,
                repo.name
            );
            response = response.insert_header("link", link.as_str());
        }
        let mock = Mock::given(method("GET"))
            .and(path(format!("/repos/{}/{}/deployments", repo.owner, repo.name)))
            .and(query_param("environment", environment));
        let mock = match page {
            None => mock.and(query_param_is_missing("page")),
            Some(p) => mock.and(query_param("page", p.to_string())),
        };
        mock.respond_with(response).mount(server).await;
    }

    async fn mount_statuses(server: &MockServer, repo: &Repo, deployment_id: u64, statuses: Vec<Value>, expected_calls: Option<u64>) {
        let mock = Mock::given(method("GET"))
            .and(path(format!("/repos/{}/{}/deployments/{deployment_id}/statuses", repo.owner, repo.name)))
            .and(query_param("per_page", "10"))
            .respond_with(ResponseTemplate::new(200).set_body_json(Value::Array(statuses)));
        match expected_calls {
            Some(n) => mock.expect(n).mount(server).await,
            None => mock.mount(server).await,
        }
    }

    const T1: OffsetDateTime = datetime!(2026-09-01 10:00 UTC);
    const T2: OffsetDateTime = datetime!(2026-09-03 10:00 UTC);
    const T3: OffsetDateTime = datetime!(2026-09-05 10:00 UTC);

    #[tokio::test]
    async fn as_of_picks_the_older_deployment_and_skips_later_ones() {
        let server = MockServer::start().await;
        let repo = repo();
        mount_token(&server, &repo).await;
        mount_deployments(&server, &repo, "production", None, None, vec![
            deployment_json(&repo, 30, 0xc3, "production", T3),
            deployment_json(&repo, 20, 0xb2, "production", T2),
            deployment_json(&repo, 10, 0xa1, "production", T1),
        ])
        .await;
        // Created after as_of: its statuses are never read.
        mount_statuses(&server, &repo, 30, vec![status_json(300, "success", T3)], Some(0)).await;
        // Created before as_of, but its only success came after as_of; the failure is ignored.
        mount_statuses(&server, &repo, 20, vec![
            status_json(202, "success", T2 + time::Duration::days(2)),
            status_json(201, "failure", T2 + time::Duration::minutes(5)),
            status_json(200, "in_progress", T2 + time::Duration::minutes(1)),
        ], Some(1))
        .await;
        mount_statuses(&server, &repo, 10, vec![
            status_json(101, "success", T1 + time::Duration::minutes(3)),
            status_json(100, "pending", T1 + time::Duration::minutes(1)),
        ], Some(1))
        .await;

        let as_of = T2 + time::Duration::days(1);
        let found = GithubDeploymentResolver::new(client(&server)).deployed_sha(org(), &repo, "production", as_of).await.unwrap();
        assert_eq!(
            found,
            Some(Deployment {
                org_id: org(),
                repo_id: repo.id,
                environment: "production".into(),
                sha: sha(0xa1),
                observed_at: T1 + time::Duration::minutes(3),
                source: DeploymentSource::GitHubDeployments,
            })
        );
    }

    #[tokio::test]
    async fn gives_up_after_three_pages() {
        let server = MockServer::start().await;
        let repo = repo();
        mount_token(&server, &repo).await;
        mount_deployments(&server, &repo, "production", None, Some(2), vec![deployment_json(&repo, 3, 0x03, "production", T3)]).await;
        mount_deployments(&server, &repo, "production", Some(2), Some(3), vec![deployment_json(&repo, 2, 0x02, "production", T2)]).await;
        mount_deployments(&server, &repo, "production", Some(3), Some(4), vec![deployment_json(&repo, 1, 0x01, "production", T1)]).await;
        // A successful deployment exists on page 4, but the walk never gets there.
        Mock::given(method("GET"))
            .and(path("/repos/acme/shop/deployments"))
            .and(query_param("page", "4"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([deployment_json(&repo, 0, 0x09, "production", T1)])))
            .expect(0)
            .mount(&server)
            .await;
        for id in 1..=3 {
            mount_statuses(&server, &repo, id, vec![status_json(id * 10, "error", T1)], Some(1)).await;
        }

        let found = GithubDeploymentResolver::new(client(&server)).deployed_sha(org(), &repo, "production", T3).await.unwrap();
        assert_eq!(found, None);
    }

    #[tokio::test]
    async fn server_errors_are_unavailable_and_rate_limits_keep_the_reset_time() {
        let server = MockServer::start().await;
        let repo = repo();
        mount_token(&server, &repo).await;
        Mock::given(method("GET"))
            .and(path("/repos/acme/shop/deployments"))
            .and(query_param("environment", "production"))
            .respond_with(ResponseTemplate::new(502).set_body_string("bad gateway"))
            .mount(&server)
            .await;
        let reset = NOW + time::Duration::minutes(17);
        Mock::given(method("GET"))
            .and(path("/repos/acme/shop/deployments"))
            .and(query_param("environment", "staging"))
            .respond_with(
                ResponseTemplate::new(403)
                    .insert_header("x-ratelimit-remaining", "0")
                    .insert_header("x-ratelimit-reset", reset.unix_timestamp().to_string().as_str())
                    .set_body_json(json!({"message": "API rate limit exceeded for installation ID 77."})),
            )
            .mount(&server)
            .await;

        let resolver = GithubDeploymentResolver::new(client(&server));
        let err = resolver.deployed_sha(org(), &repo, "production", NOW).await.unwrap_err();
        assert!(matches!(err, DeploymentError::Unavailable(_)), "{err:?}");
        let err = resolver.deployed_sha(org(), &repo, "staging", NOW).await.unwrap_err();
        assert_eq!(err, DeploymentError::RateLimited { retry_at: reset });
    }

    /// Owns the mock server so it outlives the resolver under test.
    struct Served {
        _server: MockServer,
        resolver: GithubDeploymentResolver,
    }

    #[async_trait]
    impl DeploymentResolver for Served {
        async fn deployed_sha(&self, org: OrgId, repo: &Repo, environment: &str, as_of: OffsetDateTime)
        -> Result<Option<Deployment>, DeploymentError> {
            self.resolver.deployed_sha(org, repo, environment, as_of).await
        }
    }

    /// Each seed becomes one GitHub deployment created a minute before its status, whose single
    /// status is `success` or `failure` at `observed_at`.
    #[tokio::test]
    async fn github_resolver_passes_contract() {
        contract_deployment_resolver(|_org, repo, seeds: Vec<SeedDeployment>| async move {
            let server = MockServer::start().await;
            mount_token(&server, &repo).await;
            let mut by_env: BTreeMap<String, Vec<(u64, SeedDeployment)>> = BTreeMap::new();
            for (i, seed) in seeds.into_iter().enumerate() {
                by_env.entry(seed.environment.clone()).or_default().push((i as u64 + 1, seed));
            }
            for (env, mut list) in by_env {
                list.sort_by(|a, b| b.1.observed_at.cmp(&a.1.observed_at));
                let body = list
                    .iter()
                    .map(|(id, s)| {
                        let mut d = deployment_json(&repo, *id, 0, &env, s.observed_at - time::Duration::minutes(1));
                        d["sha"] = json!(s.sha.to_hex());
                        d
                    })
                    .collect();
                mount_deployments(&server, &repo, &env, None, None, body).await;
                for (id, s) in &list {
                    let state = if s.success { "success" } else { "failure" };
                    mount_statuses(&server, &repo, *id, vec![status_json(id * 100, state, s.observed_at)], None).await;
                }
            }
            // Environments without deployments.
            Mock::given(method("GET"))
                .and(path(format!("/repos/{}/{}/deployments", repo.owner, repo.name)))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
                .with_priority(10)
                .mount(&server)
                .await;
            let resolver = GithubDeploymentResolver::new(client(&server));
            Served { _server: server, resolver }
        })
        .await;
    }
}
