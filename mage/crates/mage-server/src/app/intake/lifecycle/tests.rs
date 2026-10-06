//! Use case 6: BindIntegration (provider exchange, secrets, integration, first sync), revocation,
//! installation repository and repository events.

use std::collections::BTreeMap;

use mage_core::domain::{JobKind, OrgId, RepoAccess, UserId};
use mage_core::ports::source_host::{AccessToken, AccountKind, RepoSelection};
use uuid::Uuid;

use super::*;
use crate::app::intake::testkit::{INSTALLATION, Kit, T0, remote_repo};
use crate::domain::intake::IntakeConfig;
use crate::ports::chat::ChatGrant;
use crate::ports::tracker::TrackerGrant;

fn actor() -> Actor {
    Actor::User(UserId::from_uuid(Uuid::from_u128(0xa1)))
}

fn info(id: u64) -> mage_core::ports::source_host::InstallationInfo {
    mage_core::ports::source_host::InstallationInfo {
        id: InstallationId(id),
        account_login: "acme-co".into(),
        account_kind: AccountKind::Organization,
        permissions: BTreeMap::new(),
        repository_selection: RepoSelection::All,
        suspended: false,
    }
}

async fn fresh_org(kit: &Kit, n: u128) -> OrgId {
    let org = OrgId::from_uuid(Uuid::from_u128(n));
    let o = mage_core::domain::Org { id: org, slug: format!("org{n}"), name: "x".into(), created_at: T0 };
    kit.orgs.create_org(&o, UserId::from_uuid(Uuid::from_u128(0xa1)), &crate::ports::org::RetentionPolicy::default()).await.unwrap();
    kit.orgs.put_intake_config(org, &IntakeConfig::default()).await.unwrap();
    org
}

#[tokio::test]
async fn github_install_binds_only_visible_installations() {
    let kit = Kit::new().await;
    let org = fresh_org(&kit, 0xf0).await;
    kit.admin.grant(GithubApp::Read, "code-1", info(55));
    // Spoofed installation id: rejected, nothing bound.
    let err = kit.bind().github(org, GithubApp::Read, InstallationId(56), "code-1", &actor()).await.unwrap_err();
    assert!(matches!(err, AppError::Invalid(_)), "{err:?}");
    assert!(kit.orgs.integrations(org).await.unwrap().is_empty());
    // Visible: Active integration, account login recorded, first resync enqueued.
    let i = kit.bind().github(org, GithubApp::Read, InstallationId(55), "code-1", &actor()).await.unwrap();
    assert_eq!((i.kind, i.external_id.as_str(), i.status), (IntegrationKind::GitHub, "55", IntegrationStatus::Active));
    let s = kit.orgs.integration_settings(org, IntegrationKind::GitHub, "55").await.unwrap();
    assert_eq!(s.github_account_login.as_deref(), Some("acme-co"));
    assert!(kit.queued(JobKind::IntakeGithubResync).iter().any(|j| j.org_id == Some(org)));
    // The same installation cannot be bound to a second org.
    let other = fresh_org(&kit, 0xf1).await;
    kit.admin.grant(GithubApp::Read, "code-2", info(55));
    assert!(matches!(kit.bind().github(other, GithubApp::Read, InstallationId(55), "code-2", &actor()).await, Err(AppError::Conflict(_))));
    // The Fix App binds the GitHubFix kind without a repo sync.
    kit.drain().await;
    kit.admin.grant(GithubApp::Fix, "code-3", info(77));
    let fix = kit.bind().github(org, GithubApp::Fix, InstallationId(77), "code-3", &actor()).await.unwrap();
    assert_eq!(fix.kind, IntegrationKind::GitHubFix);
    assert!(kit.queued(JobKind::IntakeGithubResync).is_empty());
}

#[tokio::test]
async fn linear_and_slack_store_tokens_and_settings() {
    let kit = Kit::new().await;
    let org = fresh_org(&kit, 0xf2).await;
    let expires = T0 + time::Duration::hours(10);
    kit.tracker.set_grant(TrackerGrant {
        workspace_id: "WS-New".into(),
        workspace_name: "Acme Linear".into(),
        access_token: AccessToken::new("lin_oauth_a"),
        refresh_token: Some(AccessToken::new("lin_oauth_r")),
        expires_at: Some(expires),
        scopes: vec!["read".into()],
    });
    let redirect = Url::parse("https://mage.example/integrations/linear/callback").unwrap();
    let i = kit.bind().linear(org, "code", &redirect, "verifier", &actor()).await.unwrap();
    assert_eq!((i.kind, i.external_id.as_str()), (IntegrationKind::Linear, "WS-New"));
    let (_, secret) = kit.vault.load(&SecretScope::Org(org), &SecretName::new("integration/linear/ws-new/oauth").unwrap()).await.unwrap();
    let stored: StoredLinearOAuth = serde_json::from_slice(secret.expose()).unwrap();
    assert_eq!(stored, StoredLinearOAuth { access_token: "lin_oauth_a".into(), refresh_token: Some("lin_oauth_r".into()), expires_at: Some(expires) });
    let s = kit.orgs.integration_settings(org, IntegrationKind::Linear, "WS-New").await.unwrap();
    assert_eq!((s.linear_auth, s.linear_workspace_name.as_deref()), (Some(LinearAuthMode::OAuth), Some("Acme Linear")));
    let resync = kit.queued(JobKind::IntakeLinearResync);
    assert!(resync.iter().any(|j| j.payload == JobPayload::IntakeLinearResync { org_id: org, since: T0 - time::Duration::hours(24) }));

    kit.chat.set_grant(ChatGrant {
        team_id: "T9ABC".into(),
        team_domain: "acme-team".into(),
        bot_user_id: "UBOT".into(),
        bot_token: AccessToken::new("xoxb-123"),
        scopes: vec!["chat:write".into()],
    });
    kit.bind().slack(org, "code", &redirect, &actor()).await.unwrap();
    let name = SecretName::new("integration/slack/t9abc/bot_token").unwrap();
    assert_eq!(kit.vault.load(&SecretScope::Org(org), &name).await.unwrap().1.expose_str(), Some("xoxb-123"));
    let s = kit.orgs.integration_settings(org, IntegrationKind::Slack, "T9ABC").await.unwrap();
    assert_eq!((s.slack_team_domain.as_deref(), s.slack_bot_user_id.as_deref()), (Some("acme-team"), Some("UBOT")));

    // Admin disconnect: revoked and the token crypto-shredded.
    kit.bind().revoke(org, IntegrationKind::Slack, "T9ABC", &actor()).await.unwrap();
    assert!(matches!(kit.vault.load(&SecretScope::Org(org), &name).await, Err(VaultError::NotFound)));
    let slack = kit.orgs.integrations(org).await.unwrap().into_iter().find(|i| i.kind == IntegrationKind::Slack).unwrap();
    assert_eq!(slack.status, IntegrationStatus::Revoked);
}

#[test]
fn connect_urls_carry_state_and_pkce() {
    let kit_cfg = ConnectConfig { linear_client_id: Some("lin".into()), slack_client_id: Some("sl".into()), ..ConnectConfig::default() };
    let redirect = Url::parse("https://mage.example/integrations/linear/callback").unwrap();
    let bind = |cfg: ConnectConfig| -> BindIntegration {
        let clock = std::sync::Arc::new(mage_core::testing::FakeClock::new());
        let ids = std::sync::Arc::new(mage_core::testing::SeqIdGen::new());
        let jobs = std::sync::Arc::new(mage_core::testing::MemJobQueue::new(clock.clone(), ids));
        let feed = std::sync::Arc::new(crate::testing::MemChangeFeed::new(clock.clone()));
        BindIntegration::new(
            std::sync::Arc::new(crate::testing::MemOrgStore::new(clock.clone(), jobs, feed)),
            std::sync::Arc::new(crate::testing::FakeSourceHostAdmin::new()),
            std::sync::Arc::new(crate::testing::FakeTracker::new()),
            std::sync::Arc::new(crate::testing::FakeChat::new(clock.clone())),
            std::sync::Arc::new(crate::testing::MemSecretVault::new(clock.clone())),
            clock,
            cfg,
        )
    };
    let b = bind(kit_cfg);
    let gh = Url::parse(&b.connect_url(IntegrationKind::GitHub, "st&1", "ch", &redirect).unwrap()).unwrap();
    assert_eq!((gh.host_str(), gh.path()), (Some("github.com"), "/apps/ahomage/installations/new"));
    assert_eq!(gh.query_pairs().collect::<Vec<_>>(), vec![("state".into(), "st&1".into())]);
    let lin = Url::parse(&b.connect_url(IntegrationKind::Linear, "st", "ch", &redirect).unwrap()).unwrap();
    let q: BTreeMap<String, String> = lin.query_pairs().map(|(k, v)| (k.into_owned(), v.into_owned())).collect();
    assert_eq!(q["code_challenge"], "ch");
    assert_eq!(q["code_challenge_method"], "S256");
    assert_eq!(q["redirect_uri"], redirect.as_str());
    assert_eq!(q["state"], "st");
    assert!(b.connect_url(IntegrationKind::Slack, "st", "", &redirect).unwrap().contains("scope=channels%3Ahistory"));
    // Not configured → unavailable, never a broken URL.
    assert!(matches!(b.connect_url(IntegrationKind::GitHubFix, "st", "", &redirect), Err(AppError::Unavailable(_))));
}

#[tokio::test]
async fn repository_events_rename_and_lose_access() {
    let kit = Kit::new().await;
    let lifecycle = kit.lifecycle();
    let renamed = RepoCoords { owner: "acme".into(), name: "storefront".into() };
    lifecycle.repository(kit.org, 100, &RepoAction::Renamed, &renamed, "main").await.unwrap();
    let r = kit.repos.repo(kit.org, kit.repo.id).await.unwrap();
    assert_eq!((r.name.as_str(), r.id), ("storefront", kit.repo.id), "rows keep their id across renames");
    lifecycle.repository(kit.org, 100, &RepoAction::Deleted, &renamed, "main").await.unwrap();
    assert_eq!(kit.repos.repo(kit.org, kit.repo.id).await.unwrap().access, RepoAccess::Lost);

    // installation_repositories: added comes back Granted with its head synced; removed → Lost.
    let coords = remote_repo().coords;
    lifecycle.installation_repos(kit.org, INSTALLATION, &[(100, coords)], &[]).await.unwrap();
    let r = kit.repos.repo(kit.org, kit.repo.id).await.unwrap();
    assert_eq!((r.access, r.name.as_str()), (RepoAccess::Granted, "shop"));
    assert!(kit.repos.default_head(kit.org, kit.repo.id).await.unwrap().is_some());
    lifecycle.installation_repos(kit.org, INSTALLATION, &[], &[100]).await.unwrap();
    assert_eq!(kit.repos.repo(kit.org, kit.repo.id).await.unwrap().access, RepoAccess::Lost);
}

#[tokio::test]
async fn seeding_is_idempotent() {
    let kit = Kit::new().await;
    let mut section = IntakeSection::default();
    let mut cfg = IntakeConfig::default();
    cfg.linear.teams = vec!["SUP".into()];
    section.orgs.insert("acme".into(), cfg.clone());
    let installs = [GithubInstallSeed { id: 7, org: "acme".into() }];
    for _ in 0..2 {
        assert_eq!(seed(kit.orgs.as_ref(), &section, &installs).await.unwrap(), vec![kit.org]);
    }
    assert_eq!(kit.orgs.intake_config(kit.org).await.unwrap(), cfg);
    assert_eq!(kit.queued(JobKind::IntakeGithubResync).len(), 1);
    section.orgs.insert("nope".into(), IntakeConfig::default());
    assert!(matches!(seed(kit.orgs.as_ref(), &section, &[]).await, Err(AppError::Invalid(_))));
}
