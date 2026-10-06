//! GitHub `WebhookDecoder` (intake.md "Endpoints", "`WebhookDecoder`"): one decoder type serves
//! `/hooks/github` (read app, [`WebhookSource::GitHub`]) and `/hooks/github-fix` (Mage Fix App,
//! [`WebhookSource::GitHubFix`]). The `X-GitHub-Event` header picks the payload shape; only the
//! fields intake acts on are deserialized, so additive GitHub payload changes stay compatible.
//!
//! The read app decodes `push`, `installation`, `installation_repositories`, `repository`,
//! `deployment_status` and `pull_request` (internal v1 single-app mode). The Fix App decodes
//! `installation` and `pull_request` only. `ping` and every other event → [`HookEvent::Ignored`].
//! Policy (bound org, `mage/*` head refs, success-only deployments) belongs to `app::intake`.

use mage_core::domain::{CommitSha, InstallationId};
use mage_core::ports::source_host::RepoCoords;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use time::OffsetDateTime;

use crate::ports::hooks::{DecodeError, DeploymentState, HookEvent, InstallationAction, RepoAction, WebhookDecoder, WebhookSource};

/// Decoder for one GitHub webhook route; `source` is `GitHub` or `GitHubFix`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GithubDecoder {
    pub source: WebhookSource,
}

impl GithubDecoder {
    pub const fn new(source: WebhookSource) -> Self {
        Self { source }
    }

    fn handles(&self, event: &str) -> bool {
        match self.source {
            WebhookSource::GitHubFix => matches!(event, "installation" | "pull_request"),
            _ => matches!(
                event,
                "push" | "installation" | "installation_repositories" | "repository" | "deployment_status" | "pull_request"
            ),
        }
    }
}

impl WebhookDecoder for GithubDecoder {
    fn source(&self) -> WebhookSource {
        self.source
    }

    fn decode(&self, event: &str, body: &[u8]) -> Result<HookEvent, DecodeError> {
        if !self.handles(event) {
            return Ok(HookEvent::Ignored { reason: format!("github event `{event}` not handled") });
        }
        match event {
            "push" => decode_push(parse(event, body)?),
            "installation" => decode_installation(parse(event, body)?),
            "installation_repositories" => decode_installation_repos(parse(event, body)?),
            "repository" => decode_repository(parse(event, body)?),
            "deployment_status" => decode_deployment_status(parse(event, body)?),
            "pull_request" => decode_pull_request(parse(event, body)?),
            other => Ok(HookEvent::Ignored { reason: format!("github event `{other}` not handled") }),
        }
    }
}

fn parse<T: DeserializeOwned>(event: &str, body: &[u8]) -> Result<T, DecodeError> {
    serde_json::from_slice(body).map_err(|e| DecodeError(format!("github `{event}` payload: {e}")))
}

fn installation_id(event: &str, installation: Option<InstallationRef>) -> Result<InstallationId, DecodeError> {
    installation.map(|i| InstallationId(i.id)).ok_or_else(|| DecodeError(format!("github `{event}` payload has no installation")))
}

#[derive(Deserialize)]
struct InstallationRef {
    id: u64,
}

#[derive(Deserialize)]
struct Account {
    login: String,
}

#[derive(Deserialize)]
struct Owner {
    login: String,
}

#[derive(Deserialize)]
struct RepoIdOnly {
    id: u64,
}

#[derive(Deserialize)]
struct RepoFull {
    id: u64,
    name: String,
    owner: Owner,
    #[serde(default)]
    default_branch: Option<String>,
}

impl RepoFull {
    fn coords(&self) -> RepoCoords {
        RepoCoords { owner: self.owner.login.clone(), name: self.name.clone() }
    }
}

// push

#[derive(Deserialize)]
struct PushPayload {
    #[serde(rename = "ref")]
    git_ref: String,
    #[serde(default)]
    deleted: bool,
    repository: RepoFull,
    installation: Option<InstallationRef>,
}

fn decode_push(p: PushPayload) -> Result<HookEvent, DecodeError> {
    Ok(HookEvent::GithubPush {
        installation: installation_id("push", p.installation)?,
        repo_external_id: p.repository.id,
        repo: p.repository.coords(),
        git_ref: p.git_ref,
        deleted: p.deleted,
    })
}

// installation

#[derive(Deserialize)]
struct InstallationObject {
    id: u64,
    account: Option<Account>,
}

#[derive(Deserialize)]
struct InstallationPayload {
    action: String,
    installation: Option<InstallationObject>,
}

fn decode_installation(p: InstallationPayload) -> Result<HookEvent, DecodeError> {
    let installation = p.installation.ok_or_else(|| DecodeError("github `installation` payload has no installation".into()))?;
    let account = installation.account.ok_or_else(|| DecodeError("github `installation` payload has no account".into()))?;
    let action = match p.action.as_str() {
        "created" => InstallationAction::Created,
        "deleted" => InstallationAction::Deleted,
        "suspend" => InstallationAction::Suspend,
        "unsuspend" => InstallationAction::Unsuspend,
        "new_permissions_accepted" => InstallationAction::NewPermissionsAccepted,
        other => InstallationAction::Other(other.to_owned()),
    };
    Ok(HookEvent::GithubInstallation { installation: InstallationId(installation.id), action, account_login: account.login })
}

// installation_repositories

#[derive(Deserialize)]
struct ListedRepo {
    id: u64,
    full_name: String,
}

#[derive(Deserialize)]
struct InstallationReposPayload {
    installation: Option<InstallationRef>,
    #[serde(default)]
    repositories_added: Vec<ListedRepo>,
    #[serde(default)]
    repositories_removed: Vec<ListedRepo>,
}

fn split_full_name(full_name: &str) -> Result<RepoCoords, DecodeError> {
    match full_name.split_once('/') {
        Some((owner, name)) if !owner.is_empty() && !name.is_empty() && !name.contains('/') => {
            Ok(RepoCoords { owner: owner.to_owned(), name: name.to_owned() })
        }
        _ => Err(DecodeError(format!("github repository full_name {full_name:?} is not owner/name"))),
    }
}

fn decode_installation_repos(p: InstallationReposPayload) -> Result<HookEvent, DecodeError> {
    let added = p.repositories_added.iter().map(|r| Ok((r.id, split_full_name(&r.full_name)?))).collect::<Result<Vec<_>, DecodeError>>()?;
    Ok(HookEvent::GithubInstallationRepos {
        installation: installation_id("installation_repositories", p.installation)?,
        added,
        removed: p.repositories_removed.iter().map(|r| r.id).collect(),
    })
}

// repository

#[derive(Deserialize)]
struct RepositoryPayload {
    action: String,
    repository: RepoFull,
    installation: Option<InstallationRef>,
}

fn decode_repository(p: RepositoryPayload) -> Result<HookEvent, DecodeError> {
    let action = match p.action.as_str() {
        "renamed" => RepoAction::Renamed,
        "edited" => RepoAction::Edited,
        "archived" => RepoAction::Archived,
        "unarchived" => RepoAction::Unarchived,
        "deleted" => RepoAction::Deleted,
        "transferred" => RepoAction::Transferred,
        other => RepoAction::Other(other.to_owned()),
    };
    let repo = p.repository.coords();
    let default_branch = p.repository.default_branch.ok_or_else(|| DecodeError("github `repository` payload has no default_branch".into()))?;
    Ok(HookEvent::GithubRepository {
        installation: installation_id("repository", p.installation)?,
        repo_external_id: p.repository.id,
        action,
        repo,
        default_branch,
    })
}

// deployment_status

#[derive(Deserialize)]
struct DeploymentObject {
    id: u64,
    sha: String,
    #[serde(default)]
    environment: Option<String>,
}

#[derive(Deserialize)]
struct DeploymentStatusObject {
    id: u64,
    state: String,
    #[serde(default)]
    environment: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    created_at: OffsetDateTime,
}

#[derive(Deserialize)]
struct DeploymentStatusPayload {
    deployment_status: DeploymentStatusObject,
    deployment: DeploymentObject,
    repository: RepoIdOnly,
    installation: Option<InstallationRef>,
}

/// GitHub deployment status `state` (REST and webhook spelling).
pub(crate) fn deployment_state(state: &str) -> Option<DeploymentState> {
    Some(match state {
        "success" => DeploymentState::Success,
        "failure" => DeploymentState::Failure,
        "error" => DeploymentState::Error,
        "inactive" => DeploymentState::Inactive,
        "in_progress" => DeploymentState::InProgress,
        "queued" => DeploymentState::Queued,
        "pending" => DeploymentState::Pending,
        _ => return None,
    })
}

fn decode_deployment_status(p: DeploymentStatusPayload) -> Result<HookEvent, DecodeError> {
    let installation = installation_id("deployment_status", p.installation)?;
    let status = p.deployment_status;
    let state = deployment_state(&status.state).ok_or_else(|| DecodeError(format!("unknown deployment status state {:?}", status.state)))?;
    let sha: CommitSha = p.deployment.sha.parse().map_err(|e| DecodeError(format!("deployment sha {:?}: {e}", p.deployment.sha)))?;
    let environment = status
        .environment
        .or(p.deployment.environment)
        .filter(|env| !env.is_empty())
        .ok_or_else(|| DecodeError("github `deployment_status` payload has no environment".into()))?;
    Ok(HookEvent::GithubDeploymentStatus {
        installation,
        repo_external_id: p.repository.id,
        deployment_id: p.deployment.id,
        status_id: status.id,
        environment,
        sha,
        state,
        created_at: status.created_at,
    })
}

// pull_request

#[derive(Deserialize)]
struct HeadRef {
    #[serde(rename = "ref")]
    git_ref: String,
}

#[derive(Deserialize)]
struct PullRequestObject {
    /// `null` on some actions; absent means not merged.
    #[serde(default)]
    merged: Option<bool>,
    head: HeadRef,
}

#[derive(Deserialize)]
struct PullRequestPayload {
    action: String,
    number: u32,
    pull_request: PullRequestObject,
    repository: RepoIdOnly,
    installation: Option<InstallationRef>,
}

fn decode_pull_request(p: PullRequestPayload) -> Result<HookEvent, DecodeError> {
    Ok(HookEvent::GithubPullRequest {
        installation: installation_id("pull_request", p.installation)?,
        repo_external_id: p.repository.id,
        number: p.number,
        action: p.action,
        merged: p.pull_request.merged.unwrap_or(false),
        head_ref: p.pull_request.head.git_ref,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    const SHA: &str = "9f2c1a7e4b3d5c6f7a8b9c0d1e2f3a4b5c6d7e8f";

    fn coords(owner: &str, name: &str) -> RepoCoords {
        RepoCoords { owner: owner.into(), name: name.into() }
    }

    fn read() -> GithubDecoder {
        GithubDecoder::new(WebhookSource::GitHub)
    }

    fn fix() -> GithubDecoder {
        GithubDecoder::new(WebhookSource::GitHubFix)
    }

    fn fixture(name: &str) -> Vec<u8> {
        let path = format!("{}/src/adapters/driven/github/testdata/{name}", env!("CARGO_MANIFEST_DIR"));
        std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
    }

    /// Recorded-shape GitHub payloads (trimmed captures of real deliveries) → expected events.
    #[test]
    fn decodes_recorded_github_payloads() {
        let cases: Vec<(&str, &str, HookEvent)> = vec![
            (
                "push",
                "push.json",
                HookEvent::GithubPush {
                    installation: InstallationId(52_118_812),
                    repo_external_id: 781_220_334,
                    repo: coords("acme", "billing-api"),
                    git_ref: "refs/heads/main".into(),
                    deleted: false,
                },
            ),
            (
                "push",
                "push_tag_deleted.json",
                HookEvent::GithubPush {
                    installation: InstallationId(52_118_812),
                    repo_external_id: 781_220_334,
                    repo: coords("acme", "billing-api"),
                    git_ref: "refs/tags/v1.4.0".into(),
                    deleted: true,
                },
            ),
            (
                "installation",
                "installation_created.json",
                HookEvent::GithubInstallation {
                    installation: InstallationId(52_118_812),
                    action: InstallationAction::Created,
                    account_login: "acme".into(),
                },
            ),
            (
                "installation",
                "installation_suspend.json",
                HookEvent::GithubInstallation {
                    installation: InstallationId(52_118_812),
                    action: InstallationAction::Suspend,
                    account_login: "acme".into(),
                },
            ),
            (
                "installation_repositories",
                "installation_repositories.json",
                HookEvent::GithubInstallationRepos {
                    installation: InstallationId(52_118_812),
                    added: vec![(790_001_002, coords("acme", "web-app")), (790_001_003, coords("acme", "infra.tools"))],
                    removed: vec![781_220_335],
                },
            ),
            (
                "repository",
                "repository_renamed.json",
                HookEvent::GithubRepository {
                    installation: InstallationId(52_118_812),
                    repo_external_id: 781_220_334,
                    action: RepoAction::Renamed,
                    repo: coords("acme", "billing-service"),
                    default_branch: "main".into(),
                },
            ),
            (
                "repository",
                "repository_privatized.json",
                HookEvent::GithubRepository {
                    installation: InstallationId(52_118_812),
                    repo_external_id: 781_220_334,
                    action: RepoAction::Other("privatized".into()),
                    repo: coords("acme", "billing-api"),
                    default_branch: "trunk".into(),
                },
            ),
            (
                "deployment_status",
                "deployment_status_success.json",
                HookEvent::GithubDeploymentStatus {
                    installation: InstallationId(52_118_812),
                    repo_external_id: 781_220_334,
                    deployment_id: 1_903_442_118,
                    status_id: 2_771_065_430,
                    environment: "production".into(),
                    sha: SHA.parse().unwrap(),
                    state: DeploymentState::Success,
                    created_at: datetime!(2026-09-30 14:02:11 UTC),
                },
            ),
            (
                "deployment_status",
                "deployment_status_in_progress.json",
                HookEvent::GithubDeploymentStatus {
                    installation: InstallationId(52_118_812),
                    repo_external_id: 781_220_334,
                    deployment_id: 1_903_442_118,
                    status_id: 2_771_065_001,
                    environment: "production".into(),
                    sha: SHA.parse().unwrap(),
                    state: DeploymentState::InProgress,
                    created_at: datetime!(2026-09-30 14:00:58 UTC),
                },
            ),
            (
                "pull_request",
                "pull_request_closed_merged.json",
                HookEvent::GithubPullRequest {
                    installation: InstallationId(52_118_812),
                    repo_external_id: 781_220_334,
                    number: 412,
                    action: "closed".into(),
                    merged: true,
                    head_ref: "mage/fix-tkt-81".into(),
                },
            ),
            (
                "pull_request",
                "pull_request_opened.json",
                HookEvent::GithubPullRequest {
                    installation: InstallationId(52_118_812),
                    repo_external_id: 781_220_334,
                    number: 413,
                    action: "opened".into(),
                    merged: false,
                    head_ref: "feature/login".into(),
                },
            ),
        ];
        for (event, file, expected) in cases {
            assert_eq!(read().decode(event, &fixture(file)).unwrap(), expected, "{event} {file}");
        }
    }

    #[test]
    fn fix_app_decodes_only_installation_and_pull_requests() {
        assert!(matches!(fix().decode("pull_request", &fixture("pull_request_closed_merged.json")), Ok(HookEvent::GithubPullRequest { number: 412, .. })));
        assert!(matches!(
            fix().decode("installation", &fixture("installation_created.json")),
            Ok(HookEvent::GithubInstallation { action: InstallationAction::Created, .. })
        ));
        for (event, file) in [("push", "push.json"), ("deployment_status", "deployment_status_success.json"), ("repository", "repository_renamed.json")] {
            assert!(matches!(fix().decode(event, &fixture(file)), Ok(HookEvent::Ignored { .. })), "{event}");
        }
        assert_eq!(fix().source(), WebhookSource::GitHubFix);
        assert_eq!(read().source(), WebhookSource::GitHub);
    }

    #[test]
    fn ping_and_unknown_events_are_ignored_without_parsing() {
        assert!(matches!(read().decode("ping", &fixture("ping.json")), Ok(HookEvent::Ignored { .. })));
        assert!(matches!(fix().decode("ping", &fixture("ping.json")), Ok(HookEvent::Ignored { .. })));
        assert!(matches!(read().decode("check_suite", b"not json"), Ok(HookEvent::Ignored { .. })));
    }

    #[test]
    fn rejects_payloads_intake_cannot_act_on() {
        let no_installation = br#"{"ref":"refs/heads/main","deleted":false,"repository":{"id":1,"name":"r","owner":{"login":"o"}}}"#;
        let err = read().decode("push", no_installation).unwrap_err();
        assert!(err.0.contains("no installation"), "{err}");

        let bad_full_name = br#"{"action":"added","installation":{"id":7},"repositories_added":[{"id":3,"full_name":"no-slash"}],"repositories_removed":[]}"#;
        assert!(read().decode("installation_repositories", bad_full_name).is_err());

        let body = String::from_utf8(fixture("deployment_status_success.json")).unwrap();
        assert!(read().decode("deployment_status", body.replace("\"success\"", "\"waiting\"").as_bytes()).is_err());
        assert!(read().decode("deployment_status", body.replace(SHA, "abc123").as_bytes()).is_err());

        assert!(read().decode("push", b"{truncated").is_err());
    }
}
