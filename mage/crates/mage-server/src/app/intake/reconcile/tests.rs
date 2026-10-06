//! Use case 7: periodic chains carry their successor; resync converges repos, heads and
//! deployments; redeliver retries only failed deliveries Mage never stored.

use mage_core::domain::{CommitSha, Deployment, DeploymentSource, JobKind, Repo, RepoId};
use uuid::Uuid;

use super::*;
use crate::app::intake::accept::{AcceptDelivery, InboundDelivery};
use crate::app::intake::testkit::{HEAD, Kit, T0, sha};
use crate::ports::source_host_admin::HookDelivery;

#[tokio::test]
async fn github_resync_converges_repos_heads_and_deployments() {
    let kit = Kit::new().await;
    // A repo the installation no longer grants.
    let gone = Repo { id: RepoId::from_uuid(Uuid::from_u128(0xb9)), external_id: 999, name: "old".into(), ..kit.repo.clone() };
    kit.repos.upsert_repo(&gone, &[]).await.unwrap();
    kit.deployments.add(Deployment {
        org_id: kit.org,
        repo_id: kit.repo.id,
        environment: "production".into(),
        sha: CommitSha::from_bytes([0x55; 20]),
        observed_at: T0 - time::Duration::hours(3),
        source: DeploymentSource::GitHubDeployments,
    });
    let run = kit.reconcile(false).github_resync(kit.org).await.unwrap();
    assert_eq!((run.report.repos_synced, run.report.repos_lost, run.report.deployments_recorded), (1, 1, 1));
    assert_eq!(kit.repos.repo(kit.org, gone.id).await.unwrap().access, RepoAccess::Lost);
    assert_eq!(kit.repos.default_head(kit.org, kit.repo.id).await.unwrap(), Some(sha(HEAD)));
    assert!(kit.repos.latest_deployment(kit.org, kit.repo.id, "production", T0).await.unwrap().is_some());
    let successor = run.successor.unwrap();
    assert_eq!((successor.payload, successor.run_after), (JobPayload::IntakeGithubResync { org_id: kit.org }, Some(T0 + time::Duration::hours(1))));
    // Second run: nothing new to record.
    let again = kit.reconcile(false).github_resync(kit.org).await.unwrap();
    assert_eq!(again.report.deployments_recorded, 0);

    // A rate-limited installation listing fails the run at the provider's reset.
    let reset = T0 + time::Duration::minutes(9);
    kit.source.fail_next(mage_core::testing::SourceHostMethod::InstallationRepos, SourceHostError::RateLimited { retry_at: reset });
    assert_eq!(kit.reconcile(false).github_resync(kit.org).await.unwrap_err(), IntakeError::Retry { reason: "GitHub rate limited".into(), at: Some(reset) });
}

#[tokio::test]
async fn chains_end_when_the_integration_is_gone() {
    let kit = Kit::new().await;
    kit.orgs.set_integration_status(kit.org, IntegrationKind::GitHub, "7", IntegrationStatus::Revoked, &[]).await.unwrap();
    assert_eq!(kit.reconcile(false).github_resync(kit.org).await.unwrap().successor, None);
    kit.orgs.set_integration_status(kit.org, IntegrationKind::Linear, crate::app::intake::testkit::WORKSPACE, IntegrationStatus::Revoked, &[]).await.unwrap();
    assert_eq!(kit.reconcile(false).linear_resync(kit.org, T0).await.unwrap().successor, None);
}

#[tokio::test]
async fn linear_resync_enqueues_changed_issues_and_moves_the_checkpoint() {
    let kit = Kit::new().await;
    let since = T0 - time::Duration::hours(1);
    let mut a = kit.issue("r1");
    a.updated_at = since - time::Duration::minutes(3); // inside the 5 min overlap
    let mut b = kit.issue("r2");
    b.updated_at = T0 - time::Duration::minutes(1);
    let mut old = kit.issue("r3");
    old.updated_at = since - time::Duration::hours(1);
    let mut other_team = kit.issue("r4");
    other_team.team_key = "OPS".into();
    for i in [a, b, old, other_team] {
        kit.tracker.put_issue(kit.org, i);
    }
    let run = kit.reconcile(false).linear_resync(kit.org, since).await.unwrap();
    assert_eq!(run.report.issues_enqueued, 2);
    let mut ids: Vec<String> = kit
        .queued(JobKind::IntakeLinearIssue)
        .into_iter()
        .map(|j| match j.payload {
            JobPayload::IntakeLinearIssue { issue_id, .. } => issue_id,
            other => panic!("{other:?}"),
        })
        .collect();
    ids.sort();
    assert_eq!(ids, ["r1", "r2"]);
    let successor = run.successor.unwrap();
    assert_eq!(successor.payload, JobPayload::IntakeLinearResync { org_id: kit.org, since: T0 - time::Duration::minutes(1) });
    assert_eq!(successor.run_after, Some(T0 + time::Duration::minutes(15)));
}

fn attempt(id: u64, guid: &str, minutes_ago: i64, status: u16) -> HookDelivery {
    HookDelivery { id, guid: guid.into(), delivered_at: T0 - time::Duration::minutes(minutes_ago), redelivery: false, status_code: status, event: "push".into(), action: None }
}

#[tokio::test]
async fn redeliver_retries_failed_deliveries_without_inbox_rows() {
    let kit = Kit::new().await;
    AcceptDelivery::new(kit.inbox.clone())
        .accept(InboundDelivery { source: WebhookSource::GitHub, delivery_id: "stored".into(), event: "push".into(), body: b"x".to_vec() })
        .await
        .unwrap();
    // Newest first.
    kit.admin.set_deliveries(GithubApp::Read, vec![
        attempt(9, "lost", 5, 503),
        attempt(8, "ok-after-retry", 6, 200),
        attempt(7, "stored", 7, 502),
        attempt(6, "ok-after-retry", 8, 500),
        attempt(5, "lost", 9, 500),
        attempt(4, "ancient", 600, 500),
    ]);
    kit.admin.set_deliveries(GithubApp::Fix, vec![attempt(20, "fix-lost", 2, 500)]);
    let since = T0 - time::Duration::hours(2);
    let run = kit.reconcile(true).github_redeliver(since).await.unwrap();
    assert_eq!(kit.admin.redelivered(), vec![(GithubApp::Read, 9), (GithubApp::Fix, 20)]);
    assert_eq!(run.report.redelivered, 2);
    let successor = run.successor.unwrap();
    assert_eq!(successor.payload, JobPayload::IntakeGithubRedeliver { since: T0 - time::Duration::minutes(2) - time::Duration::minutes(15) });
    assert_eq!(successor.run_after, Some(T0 + time::Duration::minutes(10)));
    // Without the Fix App only the read app is reconciled.
    let kit2 = Kit::new().await;
    kit2.admin.set_deliveries(GithubApp::Fix, vec![attempt(20, "fix-lost", 2, 500)]);
    kit2.reconcile(false).github_redeliver(since).await.unwrap();
    assert!(kit2.admin.redelivered().is_empty());
}

#[tokio::test]
async fn bootstrap_and_purge_keep_one_chain_each() {
    let kit = Kit::new().await;
    let r = kit.reconcile(false);
    r.bootstrap(&[kit.org]).await.unwrap();
    r.bootstrap(&[kit.org]).await.unwrap();
    for kind in [JobKind::IntakeGithubRedeliver, JobKind::IntakeInboxPurge, JobKind::IntakeGithubResync, JobKind::IntakeLinearResync] {
        assert_eq!(kit.queued(kind).len(), 1, "{kind}");
    }
    let purge = r.inbox_purge().await.unwrap();
    assert_eq!(purge.successor.unwrap().run_after, Some(T0 + time::Duration::hours(24)));
}
