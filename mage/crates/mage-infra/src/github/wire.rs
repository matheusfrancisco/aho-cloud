//! GitHub REST JSON shapes (only the fields Mage reads) and their port conversions.

use std::collections::BTreeMap;

use mage_core::domain::ids::{CommitSha, InstallationId};
use mage_core::ports::source_host::{
    Access, AccountKind, InstallationInfo, PullRequestInfo, RemoteRepo, RepoCoords, RepoSelection, SourceHostError, TokenScope,
};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// `body_head` keeps this many bytes of a pull request body.
pub(crate) const BODY_HEAD_BYTES: usize = 2 * 1024;

fn unexpected(what: impl std::fmt::Display) -> SourceHostError {
    SourceHostError::Unavailable(format!("unexpected response from GitHub: {what}"))
}

/// GitHub permission levels: `read`, `write`, `admin` (admin counts as write).
pub(crate) fn access_from_github(level: &str) -> Option<Access> {
    match level {
        "read" => Some(Access::Read),
        "write" | "admin" => Some(Access::Write),
        _ => None,
    }
}

pub(crate) fn access_to_github(access: Access) -> &'static str {
    match access {
        Access::Read => "read",
        Access::Write => "write",
    }
}

/// Permission map of a response; unknown levels and non-string values are skipped.
fn permissions(raw: &BTreeMap<String, serde_json::Value>) -> BTreeMap<String, Access> {
    raw.iter().filter_map(|(name, level)| Some((name.clone(), access_from_github(level.as_str()?)?))).collect()
}

// ---- installations ----

#[derive(Debug, Deserialize)]
pub(crate) struct Installation {
    pub id: u64,
    pub account: Account,
    #[serde(default)]
    pub permissions: BTreeMap<String, serde_json::Value>,
    pub repository_selection: String,
    #[serde(default)]
    pub suspended_at: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct Account {
    pub login: String,
    #[serde(rename = "type")]
    pub kind: String,
}

impl Installation {
    pub fn into_info(self) -> Result<InstallationInfo, SourceHostError> {
        let account_kind = match self.account.kind.as_str() {
            "User" => AccountKind::User,
            "Organization" | "Enterprise" => AccountKind::Organization,
            other => return Err(unexpected(format_args!("account type {other:?}"))),
        };
        let repository_selection = match self.repository_selection.as_str() {
            "all" => RepoSelection::All,
            "selected" => RepoSelection::Selected,
            other => return Err(unexpected(format_args!("repository_selection {other:?}"))),
        };
        Ok(InstallationInfo {
            id: InstallationId(self.id),
            account_login: self.account.login,
            account_kind,
            permissions: permissions(&self.permissions),
            repository_selection,
            suspended: self.suspended_at.is_some(),
        })
    }
}

// ---- installation tokens ----

#[derive(Debug, Serialize)]
pub(crate) struct AccessTokenRequest<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repositories: Option<&'a [String]>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub permissions: BTreeMap<&'a str, &'static str>,
}

impl<'a> AccessTokenRequest<'a> {
    pub fn new(scope: &'a TokenScope) -> Self {
        Self {
            repositories: scope.repositories.as_deref(),
            permissions: scope.permissions.iter().map(|(name, access)| (name.as_str(), access_to_github(*access))).collect(),
        }
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct AccessTokenResponse {
    pub token: String,
    #[serde(with = "time::serde::rfc3339")]
    pub expires_at: OffsetDateTime,
    #[serde(default)]
    pub permissions: BTreeMap<String, serde_json::Value>,
}

impl AccessTokenResponse {
    pub fn granted_permissions(&self) -> BTreeMap<String, Access> {
        permissions(&self.permissions)
    }
}

// ---- repos ----

#[derive(Debug, Deserialize)]
pub(crate) struct Repo {
    pub id: u64,
    pub name: String,
    pub owner: Owner,
    pub default_branch: String,
    pub private: bool,
    #[serde(default)]
    pub archived: bool,
}

#[derive(Debug, Deserialize)]
pub(crate) struct Owner {
    pub login: String,
}

impl Repo {
    pub fn into_remote(self) -> RemoteRepo {
        RemoteRepo {
            external_id: self.id,
            coords: RepoCoords { owner: self.owner.login, name: self.name },
            default_branch: self.default_branch,
            private: self.private,
            archived: self.archived,
        }
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct InstallationRepos {
    pub repositories: Vec<Repo>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct Commit {
    pub sha: String,
}

impl Commit {
    pub fn into_sha(self) -> Result<CommitSha, SourceHostError> {
        self.sha.parse().map_err(|e| unexpected(format_args!("commit sha {:?}: {e}", self.sha)))
    }
}

// ---- pull requests ----

#[derive(Debug, Deserialize)]
pub(crate) struct PullRequest {
    pub number: u32,
    pub title: String,
    #[serde(default)]
    pub body: Option<String>,
    pub head: BranchRef,
    pub base: BranchRef,
    #[serde(default)]
    pub merge_commit_sha: Option<String>,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub merged_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
    pub html_url: String,
    #[serde(default)]
    pub user: Option<Owner>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct BranchRef {
    #[serde(rename = "ref")]
    pub name: String,
}

impl PullRequest {
    /// `merge_commit_sha` only for merged pull requests (GitHub also reports a test merge commit
    /// for open ones); `ghost` when the author account is gone.
    pub fn into_info(self) -> Result<PullRequestInfo, SourceHostError> {
        let merge_commit_sha = match (self.merged_at, &self.merge_commit_sha) {
            (Some(_), Some(sha)) => Some(sha.parse().map_err(|e| unexpected(format_args!("merge_commit_sha {sha:?}: {e}")))?),
            _ => None,
        };
        Ok(PullRequestInfo {
            number: self.number,
            title: self.title,
            body_head: head_of(self.body.as_deref().unwrap_or_default(), BODY_HEAD_BYTES).to_owned(),
            head_ref: self.head.name,
            base_ref: self.base.name,
            merge_commit_sha,
            merged_at: self.merged_at,
            updated_at: self.updated_at,
            url: self.html_url,
            author_login: self.user.map_or_else(|| "ghost".to_owned(), |u| u.login),
        })
    }
}

/// The longest prefix of `text` of at most `max` bytes that ends on a char boundary.
pub(crate) fn head_of(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut cut = max;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    &text[..cut]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn head_of_cuts_on_a_char_boundary() {
        assert_eq!(head_of("abc", 10), "abc");
        assert_eq!(head_of("abcdef", 3), "abc");
        // 'é' is 2 bytes: a cut at byte 2 would split it.
        assert_eq!(head_of("aé", 2), "a");
        assert_eq!(head_of("ééé", 5), "éé");
    }

    #[test]
    fn permission_levels() {
        let raw = BTreeMap::from([
            ("contents".to_owned(), serde_json::json!("read")),
            ("administration".to_owned(), serde_json::json!("admin")),
            ("issues".to_owned(), serde_json::json!("write")),
            ("odd".to_owned(), serde_json::json!("none")),
            ("weird".to_owned(), serde_json::json!(1)),
        ]);
        assert_eq!(
            permissions(&raw),
            BTreeMap::from([
                ("administration".to_owned(), Access::Write),
                ("contents".to_owned(), Access::Read),
                ("issues".to_owned(), Access::Write)
            ])
        );
    }

    #[test]
    fn token_request_omits_unset_fields() {
        let full = TokenScope::default();
        assert_eq!(serde_json::to_value(AccessTokenRequest::new(&full)).ok(), Some(serde_json::json!({})));
        let fetch = TokenScope::fetch("web");
        assert_eq!(
            serde_json::to_value(AccessTokenRequest::new(&fetch)).ok(),
            Some(serde_json::json!({"repositories": ["web"], "permissions": {"contents": "read", "pull_requests": "read"}}))
        );
    }

    #[test]
    fn merge_sha_only_when_merged() {
        let pr = |merged_at: Option<&str>| {
            serde_json::from_value::<PullRequest>(serde_json::json!({
                "number": 7, "title": "t", "body": null, "head": {"ref": "h"}, "base": {"ref": "main"},
                "merge_commit_sha": "0101010101010101010101010101010101010101", "merged_at": merged_at,
                "updated_at": "2026-01-01T00:00:00Z", "html_url": "https://github.com/o/r/pull/7", "user": null
            }))
            .map_err(|e| e.to_string())
            .and_then(|p| p.into_info().map_err(|e| e.to_string()))
        };
        let open = pr(None);
        assert_eq!(open.as_ref().map(|p| (p.merge_commit_sha, p.body_head.as_str(), p.author_login.as_str())), Ok((None, "", "ghost")));
        let merged = pr(Some("2026-01-01T00:00:00Z"));
        assert_eq!(merged.map(|p| p.merge_commit_sha), Ok(Some(CommitSha::from_bytes([1; 20]))));
    }
}
