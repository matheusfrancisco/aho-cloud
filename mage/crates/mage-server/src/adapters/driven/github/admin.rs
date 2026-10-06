//! `SourceHostAdmin` on the shared `mage_infra::github::GitHubClient` (intake.md
//! "`SourceHostAdmin`", Endpoints table rows `complete_install`, `hook_deliveries`, `redeliver`).
//!
//! One client per GitHub App: the read-only app (always) and the Mage Fix App (fix.md F3,
//! optional; every call for [`GithubApp::Fix`] without it → `Invalid("fix app not configured")`).
//! The OAuth client secrets come from the server `SecretVault`, `Platform` scope:
//! - `github/oauth_client_secret`: read app (server.md "Secret inventory");
//! - `github_fix/oauth_client_secret`: Fix App. This name is an addition to server.md's secret
//!   catalog (same shape as the read app's, needed only once F3 is enabled).

use std::sync::Arc;

use async_trait::async_trait;
use mage_core::domain::InstallationId;
use mage_core::ports::source_host::{AccessToken, InstallationInfo, SourceHostError};
use mage_infra::github::{Auth, GitHubClient};
use reqwest::Method;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use url::Url;

use crate::domain::intake::secret_names;
use crate::ports::source_host_admin::{GithubApp, HookDelivery, HookDeliveryPage, SourceHostAdmin};
use crate::ports::vault::{SecretName, SecretScope, SecretVault, VaultError};

/// `GET /user/installations` pages read before declaring the installation invisible (100 per
/// page: 1,000 installations per user).
const MAX_USER_INSTALLATION_PAGES: usize = 10;

/// `[server.github.admin]`: OAuth client ids of the two GitHub Apps (the OAuth code exchange
/// needs them next to the client secrets in the vault).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GithubAdminConfig {
    /// Read-only app's client id (`Iv1.…`).
    pub read_client_id: String,
    /// Mage Fix App's client id; `None` until F3 is set up.
    #[serde(default)]
    pub fix_client_id: Option<String>,
}

/// `SourceHostAdmin` over one client per configured GitHub App.
pub struct GithubAdmin {
    config: GithubAdminConfig,
    read: GitHubClient,
    fix: Option<GitHubClient>,
    vault: Arc<dyn SecretVault>,
}

/// The pieces of one app a call needs.
struct AppHandle<'a> {
    client: &'a GitHubClient,
    client_id: &'a str,
    secret_name: &'static str,
}

impl GithubAdmin {
    /// `fix` is the Fix App's client (its key from `github_fix/app_private_key`), `None` until F3
    /// is set up.
    pub fn new(config: GithubAdminConfig, read: GitHubClient, fix: Option<GitHubClient>, vault: Arc<dyn SecretVault>) -> Self {
        Self { config, read, fix, vault }
    }

    fn app(&self, app: GithubApp) -> Result<AppHandle<'_>, SourceHostError> {
        match app {
            GithubApp::Read => Ok(AppHandle {
                client: &self.read,
                client_id: &self.config.read_client_id,
                secret_name: secret_names::GITHUB_OAUTH_CLIENT_SECRET,
            }),
            GithubApp::Fix => match (&self.fix, &self.config.fix_client_id) {
                (Some(client), Some(client_id)) => {
                    Ok(AppHandle { client, client_id, secret_name: secret_names::GITHUB_FIX_OAUTH_CLIENT_SECRET })
                }
                _ => Err(SourceHostError::Invalid("fix app not configured".into())),
            },
        }
    }

    /// `code` → user access token (`POST {web_base}login/oauth/access_token`).
    async fn exchange_code(&self, app: &AppHandle<'_>, code: &str) -> Result<AccessToken, SourceHostError> {
        let name = SecretName::new(app.secret_name).map_err(|e| SourceHostError::Invalid(e.to_string()))?;
        let (_, secret) = self.vault.load(&SecretScope::Platform, &name).await.map_err(|e| vault_error(app.secret_name, e))?;
        let client_secret =
            secret.expose_str().ok_or_else(|| SourceHostError::Invalid(format!("secret `{}` is not UTF-8", app.secret_name)))?;
        let url = format!("{}login/oauth/access_token", app.client.config().web_base);
        let body = CodeExchange { client_id: app.client_id, client_secret, code };
        let resp = app.client.send::<_, OAuthTokenResponse>(Method::POST, Auth::Anonymous, &url, Some(&body)).await?;
        // GitHub answers a bad or reused code with 200 and an `error` field.
        match resp.body {
            OAuthTokenResponse { access_token: Some(token), .. } if !token.is_empty() => Ok(AccessToken::new(token)),
            OAuthTokenResponse { error, error_description, .. } => Err(SourceHostError::Invalid(format!(
                "oauth code exchange rejected: {}{}",
                error.as_deref().unwrap_or("no access_token"),
                error_description.map(|d| format!(" ({d})")).unwrap_or_default()
            ))),
        }
    }

    /// Whether the token's user can see `inst` (`GET /user/installations`, every page).
    async fn user_sees(&self, client: &GitHubClient, token: &AccessToken, inst: InstallationId) -> Result<bool, SourceHostError> {
        let mut url = "/user/installations?per_page=100".to_owned();
        for _ in 0..MAX_USER_INSTALLATION_PAGES {
            let page = client.get::<UserInstallations>(Auth::Bearer(token), &url).await?;
            if page.body.installations.iter().any(|i| i.id == inst.0) {
                return Ok(true);
            }
            match page.next {
                Some(next) => url = next,
                None => return Ok(false),
            }
        }
        Ok(false)
    }
}

#[async_trait]
impl SourceHostAdmin for GithubAdmin {
    async fn complete_install(&self, app: GithubApp, inst: InstallationId, code: &str) -> Result<InstallationInfo, SourceHostError> {
        let handle = self.app(app)?;
        let token = self.exchange_code(&handle, code).await?;
        if !self.user_sees(handle.client, &token, inst).await? {
            return Err(SourceHostError::Invalid("installation not visible to user".into()));
        }
        handle.client.installation(inst).await
    }

    async fn hook_deliveries(&self, app: GithubApp, cursor: Option<String>, per_page: u8) -> Result<HookDeliveryPage, SourceHostError> {
        let handle = self.app(app)?;
        let mut path = format!("/app/hook/deliveries?per_page={}", per_page.clamp(1, 100));
        if let Some(cursor) = &cursor {
            path.push_str("&cursor=");
            path.extend(url::form_urlencoded::byte_serialize(cursor.as_bytes()));
        }
        let resp = handle.client.get::<Vec<DeliveryJson>>(Auth::App, &path).await?;
        let next = resp.next.as_deref().map(cursor_of).transpose()?;
        let items = resp
            .body
            .into_iter()
            .map(|d| HookDelivery {
                id: d.id,
                guid: d.guid,
                delivered_at: d.delivered_at,
                redelivery: d.redelivery,
                status_code: d.status_code,
                event: d.event,
                action: d.action,
            })
            .collect();
        Ok(HookDeliveryPage { items, next })
    }

    async fn redeliver(&self, app: GithubApp, delivery_id: u64) -> Result<(), SourceHostError> {
        let handle = self.app(app)?;
        let path = format!("/app/hook/deliveries/{delivery_id}/attempts");
        handle.client.send::<(), serde::de::IgnoredAny>(Method::POST, Auth::App, &path, None).await?;
        Ok(())
    }

    fn evict_installation(&self, app: GithubApp, inst: InstallationId) {
        let client = match app {
            GithubApp::Read => Some(&self.read),
            GithubApp::Fix => self.fix.as_ref(),
        };
        if let Some(client) = client {
            client.evict_installation(inst);
        }
    }
}

/// The `cursor` query parameter of a `Link: rel="next"` URL.
fn cursor_of(next: &str) -> Result<String, SourceHostError> {
    let url = Url::parse(next).map_err(|e| SourceHostError::Unavailable(format!("bad hook deliveries next link {next:?}: {e}")))?;
    url.query_pairs()
        .find(|(k, _)| k == "cursor")
        .map(|(_, v)| v.into_owned())
        .ok_or_else(|| SourceHostError::Unavailable(format!("hook deliveries next link without cursor: {next}")))
}

fn vault_error(name: &str, err: VaultError) -> SourceHostError {
    match err {
        VaultError::NotFound => SourceHostError::Invalid(format!("secret `{name}` is not configured")),
        VaultError::Unavailable(msg) => SourceHostError::Unavailable(msg),
        other => SourceHostError::Unavailable(format!("secret `{name}`: {other}")),
    }
}

#[derive(Serialize)]
struct CodeExchange<'a> {
    client_id: &'a str,
    client_secret: &'a str,
    code: &'a str,
}

#[derive(Deserialize)]
struct OAuthTokenResponse {
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    error_description: Option<String>,
}

#[derive(Deserialize)]
struct UserInstallations {
    installations: Vec<UserInstallation>,
}

#[derive(Deserialize)]
struct UserInstallation {
    id: u64,
}

#[derive(Deserialize)]
struct DeliveryJson {
    id: u64,
    guid: String,
    #[serde(with = "time::serde::rfc3339")]
    delivered_at: OffsetDateTime,
    #[serde(default)]
    redelivery: bool,
    status_code: u16,
    event: String,
    #[serde(default)]
    action: Option<String>,
}

#[cfg(test)]
mod tests {
    use mage_core::ports::source_host::{Access, AccountKind, RepoSelection};
    use mage_core::testing::FakeClock;
    use serde_json::json;
    use wiremock::matchers::{body_json, header, method, path, query_param, query_param_is_missing};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::adapters::driven::github::deployments::tests::{NOW, client};
    use crate::ports::audit::Actor;
    use crate::ports::vault::{Activation, SecretBytes};
    use crate::testing::MemSecretVault;

    const INST: InstallationId = InstallationId(52_118_812);

    async fn vault_with(secrets: &[(&str, &str)]) -> Arc<dyn SecretVault> {
        let vault = MemSecretVault::new(Arc::new(FakeClock::at(NOW)));
        for (name, value) in secrets {
            let name = SecretName::new(*name).unwrap();
            vault.put(&SecretScope::Platform, &name, SecretBytes::from(value.to_string()), Activation::Now, &Actor::System).await.unwrap();
        }
        Arc::new(vault)
    }

    fn config() -> GithubAdminConfig {
        GithubAdminConfig { read_client_id: "Iv1.read".into(), fix_client_id: Some("Iv1.fix".into()) }
    }

    async fn read_admin(server: &MockServer) -> GithubAdmin {
        let vault = vault_with(&[(secret_names::GITHUB_OAUTH_CLIENT_SECRET, "read-secret")]).await;
        GithubAdmin::new(config(), client(server), None, vault)
    }

    async fn mount_code_exchange(server: &MockServer, client_id: &str, secret: &str, user_token: &str) {
        Mock::given(method("POST"))
            .and(path("/login/oauth/access_token"))
            .and(body_json(json!({"client_id": client_id, "client_secret": secret, "code": "code-123"})))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"access_token": user_token, "token_type": "bearer", "scope": ""})),
            )
            .expect(1)
            .mount(server)
            .await;
    }

    /// The user sees installation 1 on page 1 and `page2_ids` on page 2.
    async fn mount_user_installations(server: &MockServer, user_token: &str, page2_ids: &[u64]) {
        let bearer = format!("Bearer {user_token}");
        let next = format!("<{}/user/installations?per_page=100&page=2>; rel=\"next\"", server.uri());
        Mock::given(method("GET"))
            .and(path("/user/installations"))
            .and(header("authorization", bearer.as_str()))
            .and(query_param_is_missing("page"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("link", next.as_str())
                    .set_body_json(json!({"total_count": 1 + page2_ids.len(), "installations": [{"id": 1, "account": {"login": "someone-else"}}]})),
            )
            .mount(server)
            .await;
        let page2: Vec<_> = page2_ids.iter().map(|id| json!({"id": id, "account": {"login": "acme"}})).collect();
        Mock::given(method("GET"))
            .and(path("/user/installations"))
            .and(header("authorization", bearer.as_str()))
            .and(query_param("page", "2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"total_count": 1 + page2_ids.len(), "installations": page2})))
            .mount(server)
            .await;
    }

    fn installation_json() -> serde_json::Value {
        json!({
            "id": INST.0, "client_id": "Iv1.read", "app_id": 1012345, "app_slug": "ahomage",
            "account": {"login": "acme", "id": 91234567, "type": "Organization"},
            "repository_selection": "selected",
            "permissions": {"contents": "read", "metadata": "read", "deployments": "read", "pull_requests": "read"},
            "events": ["push", "repository", "deployment_status"],
            "target_type": "Organization", "suspended_at": null, "suspended_by": null,
            "created_at": "2026-09-01T08:00:00.000Z", "updated_at": "2026-09-01T08:00:01.000Z"
        })
    }

    #[tokio::test]
    async fn complete_install_accepts_an_installation_the_user_can_see() {
        let server = MockServer::start().await;
        mount_code_exchange(&server, "Iv1.read", "read-secret", "ghu_visible").await;
        mount_user_installations(&server, "ghu_visible", &[7, INST.0]).await;
        Mock::given(method("GET"))
            .and(path(format!("/app/installations/{}", INST.0)))
            .respond_with(ResponseTemplate::new(200).set_body_json(installation_json()))
            .expect(1)
            .mount(&server)
            .await;

        let info = read_admin(&server).await.complete_install(GithubApp::Read, INST, "code-123").await.unwrap();
        assert_eq!(info.id, INST);
        assert_eq!(info.account_login, "acme");
        assert_eq!(info.account_kind, AccountKind::Organization);
        assert_eq!(info.repository_selection, RepoSelection::Selected);
        assert!(!info.suspended);
        assert_eq!(info.permissions.get("deployments"), Some(&Access::Read));
    }

    #[tokio::test]
    async fn complete_install_rejects_a_spoofed_installation_id() {
        let server = MockServer::start().await;
        mount_code_exchange(&server, "Iv1.read", "read-secret", "ghu_spoofer").await;
        mount_user_installations(&server, "ghu_spoofer", &[7, 8]).await;
        Mock::given(method("GET"))
            .and(path(format!("/app/installations/{}", INST.0)))
            .respond_with(ResponseTemplate::new(200).set_body_json(installation_json()))
            .expect(0)
            .mount(&server)
            .await;

        let err = read_admin(&server).await.complete_install(GithubApp::Read, INST, "code-123").await.unwrap_err();
        assert_eq!(err, SourceHostError::Invalid("installation not visible to user".into()));
    }

    #[tokio::test]
    async fn complete_install_rejects_a_bad_code() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/login/oauth/access_token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "error": "bad_verification_code",
                "error_description": "The code passed is incorrect or expired."
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET")).and(path("/user/installations")).respond_with(ResponseTemplate::new(200)).expect(0).mount(&server).await;

        let err = read_admin(&server).await.complete_install(GithubApp::Read, INST, "code-123").await.unwrap_err();
        assert!(matches!(&err, SourceHostError::Invalid(msg) if msg.contains("bad_verification_code")), "{err:?}");
    }

    #[tokio::test]
    async fn fix_app_uses_its_own_client_and_secret_and_is_refused_when_absent() {
        let read_server = MockServer::start().await;
        let fix_server = MockServer::start().await;
        mount_code_exchange(&fix_server, "Iv1.fix", "fix-secret", "ghu_fixuser").await;
        mount_user_installations(&fix_server, "ghu_fixuser", &[INST.0]).await;
        Mock::given(method("GET"))
            .and(path(format!("/app/installations/{}", INST.0)))
            .respond_with(ResponseTemplate::new(200).set_body_json(installation_json()))
            .mount(&fix_server)
            .await;
        let vault = vault_with(&[
            (secret_names::GITHUB_OAUTH_CLIENT_SECRET, "read-secret"),
            (secret_names::GITHUB_FIX_OAUTH_CLIENT_SECRET, "fix-secret"),
        ])
        .await;
        let admin = GithubAdmin::new(config(), client(&read_server), Some(client(&fix_server)), vault.clone());
        assert_eq!(admin.complete_install(GithubApp::Fix, INST, "code-123").await.unwrap().id, INST);
        assert!(read_server.received_requests().await.unwrap().is_empty());

        let without_fix = GithubAdmin::new(config(), client(&read_server), None, vault);
        let not_configured = SourceHostError::Invalid("fix app not configured".into());
        assert_eq!(without_fix.complete_install(GithubApp::Fix, INST, "code-123").await.unwrap_err(), not_configured);
        assert_eq!(without_fix.hook_deliveries(GithubApp::Fix, None, 50).await.unwrap_err(), not_configured);
        assert_eq!(without_fix.redeliver(GithubApp::Fix, 1).await.unwrap_err(), not_configured);
    }

    #[tokio::test]
    async fn complete_install_without_the_client_secret_is_invalid() {
        let server = MockServer::start().await;
        let admin = GithubAdmin::new(config(), client(&server), None, vault_with(&[]).await);
        let err = admin.complete_install(GithubApp::Read, INST, "code-123").await.unwrap_err();
        assert!(matches!(&err, SourceHostError::Invalid(msg) if msg.contains("github/oauth_client_secret")), "{err:?}");
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    fn delivery(id: u64, guid: &str, status_code: u16, redelivery: bool, action: Option<&str>) -> serde_json::Value {
        json!({
            "id": id, "guid": guid, "delivered_at": "2026-10-05T11:58:02Z", "redelivery": redelivery,
            "duration": 0.31, "status": if status_code == 200 { "OK" } else { "Bad Gateway" },
            "status_code": status_code, "event": "push", "action": action,
            "installation_id": INST.0, "repository_id": 781220334, "throttled_at": null
        })
    }

    #[tokio::test]
    async fn hook_deliveries_page_with_the_next_links_cursor() {
        let server = MockServer::start().await;
        let next = format!("<{}/app/hook/deliveries?per_page=2&cursor=v1_12077215967>; rel=\"next\"", server.uri());
        Mock::given(method("GET"))
            .and(path("/app/hook/deliveries"))
            .and(query_param("per_page", "2"))
            .and(query_param_is_missing("cursor"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("link", next.as_str())
                    .set_body_json(json!([delivery(12077215969, "g-1", 502, false, None), delivery(12077215968, "g-2", 200, true, Some("opened"))])),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/app/hook/deliveries"))
            .and(query_param("per_page", "2"))
            .and(query_param("cursor", "v1_12077215967"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([delivery(12077215967, "g-3", 200, false, None)])))
            .mount(&server)
            .await;

        let admin = read_admin(&server).await;
        let first = admin.hook_deliveries(GithubApp::Read, None, 2).await.unwrap();
        assert_eq!(first.next.as_deref(), Some("v1_12077215967"));
        assert_eq!(
            first.items[1],
            HookDelivery {
                id: 12077215968,
                guid: "g-2".into(),
                delivered_at: time::macros::datetime!(2026-10-05 11:58:02 UTC),
                redelivery: true,
                status_code: 200,
                event: "push".into(),
                action: Some("opened".into()),
            }
        );
        assert_eq!((first.items[0].status_code, first.items[0].action.as_deref()), (502, None));

        let second = admin.hook_deliveries(GithubApp::Read, first.next, 2).await.unwrap();
        assert_eq!(second.items.iter().map(|d| d.id).collect::<Vec<_>>(), vec![12077215967]);
        assert_eq!(second.next, None);
    }

    #[tokio::test]
    async fn redeliver_posts_an_attempt_and_maps_errors() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/app/hook/deliveries/12077215969/attempts"))
            .respond_with(ResponseTemplate::new(202).set_body_json(json!({})))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/app/hook/deliveries/404/attempts"))
            .respond_with(ResponseTemplate::new(404).set_body_json(json!({"message": "Not Found"})))
            .mount(&server)
            .await;

        let admin = read_admin(&server).await;
        admin.redeliver(GithubApp::Read, 12077215969).await.unwrap();
        assert_eq!(admin.redeliver(GithubApp::Read, 404).await.unwrap_err(), SourceHostError::NotFound);
    }
}
