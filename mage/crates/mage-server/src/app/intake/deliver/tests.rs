//! ProcessDelivery: tenant resolution from the envelope, dispatch per event, inbox outcomes.

use mage_core::domain::{
    CommitSha, DeploymentSource, InstallationId, IntegrationStatus, JobKind, RepoAccess, RepoVersionKey, VersionReason,
};
use mage_core::ports::RepoStore;
use mage_core::ports::source_host::RepoCoords;

use super::*;
use crate::app::intake::accept::{AcceptDelivery, AcceptOutcome, InboundDelivery};
use crate::app::intake::testkit::{HEAD, INSTALLATION, Kit, MAGE_USER, T0, WORKSPACE, sha};
use crate::ports::hooks::{DeploymentState, InstallationAction, LinearAction};
use crate::ports::source_host_admin::GithubApp;

async fn deliver(kit: &Kit, source: WebhookSource, id: &str, event: HookEvent) -> InboxOutcome {
    let body = format!("{source:?}-{id}").into_bytes();
    kit.decoder(source).script(&body, event);
    let accepted = AcceptDelivery::new(kit.inbox.clone())
        .accept(InboundDelivery { source, delivery_id: id.into(), event: "e".into(), body })
        .await
        .unwrap();
    assert_eq!(accepted, AcceptOutcome::Accepted);
    kit.deliver().run(source, id).await.unwrap()
}

fn linear_issue(actor: &str) -> HookEvent {
    HookEvent::LinearIssue { workspace_id: WORKSPACE.into(), issue_id: "iss-9".into(), action: LinearAction::Update, actor_id: Some(actor.into()) }
}

#[tokio::test]
async fn linear_change_enqueues_a_debounced_coalesced_ingest() {
    let kit = Kit::new().await;
    assert_eq!(deliver(&kit, WebhookSource::Linear, "d1", linear_issue("u-human")).await, InboxOutcome::Processed);
    assert_eq!(deliver(&kit, WebhookSource::Linear, "d2", linear_issue("u-other")).await, InboxOutcome::Processed);
    let jobs = kit.queued(JobKind::IntakeLinearIssue);
    assert_eq!(jobs.len(), 1, "a burst collapses into one queued job");
    assert_eq!(jobs[0].run_after, T0 + time::Duration::seconds(10));
    assert_eq!(jobs[0].org_id, Some(kit.org));
    let row = kit.inbox.get(WebhookSource::Linear, "d1").await.unwrap();
    assert_eq!((row.org_id, row.outcome), (Some(kit.org), Some(InboxOutcome::Processed)));
}

#[tokio::test]
async fn own_actions_unbound_and_undecodable_deliveries() {
    let kit = Kit::new().await;
    assert_eq!(deliver(&kit, WebhookSource::Linear, "own", linear_issue(MAGE_USER)).await, InboxOutcome::Ignored("own action".into()));
    let other_ws = HookEvent::LinearIssue { workspace_id: "ws-x".into(), issue_id: "i".into(), action: LinearAction::Create, actor_id: None };
    assert_eq!(deliver(&kit, WebhookSource::Linear, "unbound", other_ws).await, InboxOutcome::Ignored("unbound".into()));
    assert!(kit.queued(JobKind::IntakeLinearIssue).is_empty());

    // A verified body that no longer parses is kept as a fixture and not retried.
    AcceptDelivery::new(kit.inbox.clone())
        .accept(InboundDelivery { source: WebhookSource::Linear, delivery_id: "bad".into(), event: "Issue".into(), body: b"{\"new\":1}".to_vec() })
        .await
        .unwrap();
    assert_eq!(kit.deliver().run(WebhookSource::Linear, "bad").await.unwrap(), InboxOutcome::Rejected("unexpected payload shape".into()));
    // Re-running a processed delivery changes nothing.
    assert_eq!(kit.deliver().run(WebhookSource::Linear, "bad").await.unwrap(), InboxOutcome::Rejected("unexpected payload shape".into()));
}

#[tokio::test]
async fn push_to_default_branch_moves_head_and_indexes_after_debounce() {
    let kit = Kit::new().await;
    let push = |git_ref: &str| HookEvent::GithubPush {
        installation: INSTALLATION,
        repo_external_id: 100,
        repo: RepoCoords { owner: "acme".into(), name: "shop".into() },
        git_ref: git_ref.into(),
        deleted: false,
    };
    assert_eq!(deliver(&kit, WebhookSource::GitHub, "p1", push("refs/heads/main")).await, InboxOutcome::Processed);
    assert_eq!(kit.repos.default_head(kit.org, kit.repo.id).await.unwrap(), Some(sha(HEAD)));
    let jobs = kit.queued(JobKind::IndexVersion);
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].run_after, T0 + time::Duration::seconds(60));
    // Same head again: nothing new.
    kit.drain().await;
    deliver(&kit, WebhookSource::GitHub, "p2", push("refs/heads/main")).await;
    assert!(kit.queued(JobKind::IndexVersion).is_empty());
    // Release tag: Release(tag) version, indexed now.
    deliver(&kit, WebhookSource::GitHub, "p3", push("refs/tags/v1.0")).await;
    let v = kit.repos.version(kit.org, RepoVersionKey::new(kit.repo.id, sha(0x44))).await.unwrap();
    assert!(v.reasons.contains(&VersionReason::Release("v1.0".into())));
    assert_eq!(kit.queued(JobKind::IndexVersion)[0].run_after, T0);
    // Feature branches are ignored.
    assert!(matches!(deliver(&kit, WebhookSource::GitHub, "p4", push("refs/heads/feature")).await, InboxOutcome::Ignored(_)));
}

#[tokio::test]
async fn deployment_status_records_history_and_backfills_assumed_tickets() {
    let kit = Kit::new().await;
    // An open ticket with an assumed target.
    kit.tracker.put_issue(kit.org, kit.issue("a1"));
    kit.ingest().run(kit.org, "a1").await.unwrap();
    kit.drain().await;

    let status = |state, env: &str, at| HookEvent::GithubDeploymentStatus {
        installation: INSTALLATION,
        repo_external_id: 100,
        deployment_id: 1,
        status_id: 2,
        environment: env.into(),
        sha: CommitSha::from_bytes([0x55; 20]),
        state,
        created_at: at,
    };
    assert!(matches!(deliver(&kit, WebhookSource::GitHub, "s0", status(DeploymentState::InProgress, "production", T0)).await, InboxOutcome::Ignored(_)));
    assert!(matches!(deliver(&kit, WebhookSource::GitHub, "s1", status(DeploymentState::Success, "preview-7", T0)).await, InboxOutcome::Ignored(_)));
    let late = T0 - time::Duration::hours(2);
    assert_eq!(deliver(&kit, WebhookSource::GitHub, "s2", status(DeploymentState::Success, "production", late)).await, InboxOutcome::Processed);
    let d = kit.repos.latest_deployment(kit.org, kit.repo.id, "production", T0).await.unwrap().unwrap();
    assert_eq!((d.sha, d.observed_at, d.source), (sha(0x55), late, DeploymentSource::GitHubDeployments));
    let v = kit.repos.version(kit.org, RepoVersionKey::new(kit.repo.id, sha(0x55))).await.unwrap();
    assert!(v.reasons.contains(&VersionReason::Deployed("production".into())));
    assert_eq!(kit.queued(JobKind::IndexVersion).len(), 1);
    // Arrived late: the assumed ticket is re-ingested so it can upgrade.
    let reingest = kit.queued(JobKind::IntakeLinearIssue);
    assert_eq!(reingest.len(), 1);
    assert_eq!(reingest[0].payload, JobPayload::IntakeLinearIssue { org_id: kit.org, issue_id: "a1".into() });
}

#[tokio::test]
async fn pull_requests_on_mage_branches_become_fix_events() {
    let kit = Kit::new().await;
    let pr = |head: &str| HookEvent::GithubPullRequest {
        installation: INSTALLATION,
        repo_external_id: 100,
        number: 12,
        action: "closed".into(),
        merged: true,
        head_ref: head.into(),
    };
    assert!(matches!(deliver(&kit, WebhookSource::GitHub, "pr0", pr("feature/x")).await, InboxOutcome::Ignored(_)));
    assert_eq!(deliver(&kit, WebhookSource::GitHub, "pr1", pr("mage/eng-1-abcdef0")).await, InboxOutcome::Processed);
    let jobs = kit.queued(JobKind::FixPrEvent);
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].payload, JobPayload::FixPrEvent { org_id: kit.org, repo_id: kit.repo.id, number: 12, action: "closed".into(), merged: true });
    assert_eq!(jobs[0].dedupe_key.as_deref(), Some(format!("fix.pr_event:{}:12:pr1", kit.repo.id).as_str()));
}

#[tokio::test]
async fn installation_deleted_revokes_and_loses_repos() {
    let kit = Kit::new().await;
    let ev = HookEvent::GithubInstallation { installation: INSTALLATION, action: InstallationAction::Deleted, account_login: "acme".into() };
    assert_eq!(deliver(&kit, WebhookSource::GitHub, "i1", ev).await, InboxOutcome::Processed);
    let gh = kit.orgs.integrations(kit.org).await.unwrap().into_iter().find(|i| i.kind == IntegrationKind::GitHub).unwrap();
    assert_eq!(gh.status, IntegrationStatus::Revoked);
    assert_eq!(kit.repos.repo(kit.org, kit.repo.id).await.unwrap().access, RepoAccess::Lost);
    assert_eq!(kit.admin.evicted(), vec![(GithubApp::Read, INSTALLATION)]);
    // Pushes for a revoked installation are answered and ignored.
    let push = HookEvent::GithubPush {
        installation: INSTALLATION,
        repo_external_id: 100,
        repo: RepoCoords { owner: "acme".into(), name: "shop".into() },
        git_ref: "refs/heads/main".into(),
        deleted: false,
    };
    assert_eq!(deliver(&kit, WebhookSource::GitHub, "i2", push).await, InboxOutcome::Ignored("revoked".into()));
    // Unsuspend reactivates and resyncs.
    let ev = HookEvent::GithubInstallation { installation: INSTALLATION, action: InstallationAction::Unsuspend, account_login: "acme".into() };
    deliver(&kit, WebhookSource::GitHub, "i3", ev).await;
    assert_eq!(kit.queued(JobKind::IntakeGithubResync).len(), 1);
    // An installation nobody bound is ignored until the callback binds it.
    let ev = HookEvent::GithubInstallation { installation: InstallationId(999), action: InstallationAction::Created, account_login: "x".into() };
    assert_eq!(deliver(&kit, WebhookSource::GitHub, "i4", ev).await, InboxOutcome::Ignored("unbound".into()));
}
