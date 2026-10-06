//! `SourceHost` over [`GitHubClient`] (intake.md "`SourceHost`", endpoint table).

use async_trait::async_trait;
use mage_core::domain::ids::{CommitSha, InstallationId};
use mage_core::ports::source_host::{
    FETCH_USERNAME, FetchSpec, InstallationInfo, InstallationToken, MAX_PR_PAGE, PrListQuery, PrPage, PrSort, PrState, RemoteRepo, RepoCoords,
    SortDirection, SourceHost, SourceHostError, TokenScope,
};

use super::client::{ApiResponse, Auth, GitHubClient};
use super::link::page_number;
use super::wire::{Commit, InstallationRepos, PullRequest, Repo};

/// Metadata reads share one full-scope token per installation.
static FULL_SCOPE: TokenScope = TokenScope { repositories: None, permissions: std::collections::BTreeMap::new() };

#[derive(Clone)]
pub struct GitHubSourceHost {
    client: GitHubClient,
}

impl GitHubSourceHost {
    pub fn new(client: GitHubClient) -> Self {
        Self { client }
    }

    pub fn client(&self) -> &GitHubClient {
        &self.client
    }

    fn auth(inst: InstallationId) -> Auth<'static> {
        Auth::Installation { inst, scope: &FULL_SCOPE }
    }
}

/// Percent-encodes one path segment (RFC 3986 unreserved characters stay).
fn encode_segment(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for byte in segment.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// A git ref in a URL path: each `/`-separated part encoded, the separators kept.
fn encode_ref(git_ref: &str) -> String {
    git_ref.split('/').map(encode_segment).collect::<Vec<_>>().join("/")
}

fn repo_path(repo: &RepoCoords) -> String {
    format!("/repos/{}/{}", encode_segment(&repo.owner), encode_segment(&repo.name))
}

#[async_trait]
impl SourceHost for GitHubSourceHost {
    async fn installation(&self, inst: InstallationId) -> Result<InstallationInfo, SourceHostError> {
        self.client.installation(inst).await
    }

    async fn installation_repos(&self, inst: InstallationId) -> Result<Vec<RemoteRepo>, SourceHostError> {
        let mut repos = Vec::new();
        let mut url = "/installation/repositories?per_page=100".to_owned();
        loop {
            let page: ApiResponse<InstallationRepos> = self.client.get(Self::auth(inst), &url).await?;
            repos.extend(page.body.repositories.into_iter().map(Repo::into_remote));
            match page.next {
                Some(next) => url = next,
                None => return Ok(repos),
            }
        }
    }

    async fn repo(&self, inst: InstallationId, repo: &RepoCoords) -> Result<RemoteRepo, SourceHostError> {
        let response: ApiResponse<Repo> = self.client.get(Self::auth(inst), &repo_path(repo)).await?;
        Ok(response.body.into_remote())
    }

    /// 404 and 422 ("No commit found") are `NotFound`, except an ambiguous sha prefix, which is
    /// `Invalid` [unverified: GitHub's wording for an ambiguous prefix].
    async fn resolve_ref(&self, inst: InstallationId, repo: &RepoCoords, git_ref: &str) -> Result<CommitSha, SourceHostError> {
        if git_ref.is_empty() {
            return Err(SourceHostError::Invalid("empty git ref".into()));
        }
        let path = format!("{}/commits/{}", repo_path(repo), encode_ref(git_ref));
        match self.client.get::<Commit>(Self::auth(inst), &path).await {
            Ok(response) => response.body.into_sha(),
            Err(SourceHostError::Invalid(message)) if message.to_ascii_lowercase().contains("ambiguous") => Err(SourceHostError::Invalid(message)),
            Err(SourceHostError::Invalid(_)) => Err(SourceHostError::NotFound),
            Err(other) => Err(other),
        }
    }

    async fn installation_token(&self, inst: InstallationId, scope: &TokenScope) -> Result<InstallationToken, SourceHostError> {
        self.client.installation_token(inst, scope).await
    }

    async fn fetch_spec(&self, inst: InstallationId, repo: &RepoCoords) -> Result<FetchSpec, SourceHostError> {
        let canonical = self.repo(inst, repo).await?.coords;
        let path = format!("{}/{}.git", encode_segment(&canonical.owner), encode_segment(&canonical.name));
        let url = self.client.config().web_base.join(&path).map_err(|e| SourceHostError::Invalid(format!("fetch URL of {canonical}: {e}")))?;
        let token = self.client.installation_token(inst, &TokenScope::fetch(&canonical.name)).await?;
        Ok(FetchSpec { url, username: FETCH_USERNAME.to_owned(), token })
    }

    /// One GitHub page per call; `page_token` is the next page number. `updated_since` is applied
    /// here (inclusive); sorted by update descending, the first older item ends the listing.
    async fn list_pull_requests(&self, inst: InstallationId, repo: &RepoCoords, query: &PrListQuery) -> Result<PrPage, SourceHostError> {
        if query.per_page == 0 || query.per_page > MAX_PR_PAGE {
            return Err(SourceHostError::Invalid(format!("per_page {} outside 1..={MAX_PR_PAGE}", query.per_page)));
        }
        let page: u32 = match &query.page_token {
            None => 1,
            Some(token) => token.parse().ok().filter(|p| *p >= 1).ok_or_else(|| SourceHostError::Invalid(format!("bad page token {token:?}")))?,
        };
        let state = match query.state {
            PrState::Open => "open",
            PrState::Closed => "closed",
            PrState::All => "all",
        };
        let sort = match query.sort {
            PrSort::Created => "created",
            PrSort::Updated => "updated",
        };
        let direction = match query.direction {
            SortDirection::Asc => "asc",
            SortDirection::Desc => "desc",
        };
        let path = format!("{}/pulls?state={state}&sort={sort}&direction={direction}&per_page={}&page={page}", repo_path(repo), query.per_page);
        let response: ApiResponse<Vec<PullRequest>> = self.client.get(Self::auth(inst), &path).await?;

        let mut items = Vec::with_capacity(response.body.len());
        let mut older_seen = false;
        for pr in response.body {
            let info = pr.into_info()?;
            if query.updated_since.is_some_and(|since| info.updated_at < since) {
                older_seen = true;
            } else {
                items.push(info);
            }
        }
        let exhausted = older_seen && query.sort == PrSort::Updated && query.direction == SortDirection::Desc;
        let next = match response.next {
            Some(url) if !exhausted => Some(
                page_number(&url)
                    .ok_or_else(|| SourceHostError::Unavailable("unexpected response from GitHub: next link without a page number".into()))?
                    .to_string(),
            ),
            _ => None,
        };
        Ok(PrPage { items, next })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refs_keep_slashes_and_encode_the_rest() {
        assert_eq!(encode_ref("feature/x"), "feature/x");
        assert_eq!(encode_ref("refs/tags/v1.0.0"), "refs/tags/v1.0.0");
        assert_eq!(encode_ref("a b/c#d?e%"), "a%20b/c%23d%3Fe%25");
        assert_eq!(encode_ref("ü"), "%C3%BC");
        assert_eq!(repo_path(&RepoCoords { owner: "Acme".into(), name: "my.repo".into() }), "/repos/Acme/my.repo");
    }
}
