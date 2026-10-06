//! `PullRequests` over the GitHub Git Data API (fix.md "GitHub Git Data API flow"), on the shared
//! `mage_infra::github::GitHubClient` of the **Mage Fix** app. No clone: blobs, trees, commits,
//! refs and pulls are all REST calls with the fix installation's token.
//!
//! Rules: refs outside `refs/heads/mage/` are refused before any request; 401/403 map to
//! `PermissionDenied` naming the permission the call needs; rate limits map to `RateLimited`;
//! content-creation calls (blobs, trees, commits, refs, pulls) are serialised per installation.
//! The fix installation is the org's active `GitHubFix` integration (`OrgStore::integrations`).
//!
//! `upload_media` uses GitHub's user-attachments endpoint (`POST
//! {uploads_base}user-attachments/assets?name&content_type&repository_id`, raw bytes,
//! `Authorization: token …`, answer `{"url": …}`), as `gh --attach` does. GitHub currently answers
//! 404 for app installation tokens there; callers fall back to Mage links on any error.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use mage_core::domain::fix::{BranchName, CommitChange, CommitPlan};
use mage_core::domain::{CommitSha, InstallationId, IntegrationKind, IntegrationStatus, OrgId, Repo, RepoPath};
use mage_core::ports::clock::Clock;
use mage_core::ports::source_host::{Access, SourceHostError, TokenScope};
use mage_infra::github::{Auth, GitHubClient};
use parking_lot::Mutex;
use reqwest::Method;
use serde::{Deserialize, Serialize};
use url::Url;

use crate::ports::org::OrgStore;
use crate::ports::pull_requests::{
    BaseFile, BranchOutcome, Comparison, FileMode, MediaUpload, MediaUrl, Mergeable, OpenPullRequest, PullRequestRef, PullRequestState,
    PullRequests, PullRequestsError, Reviewers, WriteAccess, parse_trailers,
};

/// Adapter settings beyond the shared `GitHubConfig`.
#[derive(Debug, Clone, Deserialize)]
pub struct PullRequestsConfig {
    /// Host of the user-attachments upload endpoint.
    #[serde(default = "default_uploads_base")]
    pub uploads_base: Url,
    /// Only asset URLs under this prefix are accepted from the upload endpoint.
    #[serde(default = "default_assets_prefix")]
    pub assets_prefix: String,
    #[serde(default = "default_upload_timeout_ms")]
    pub upload_timeout_ms: u64,
}

impl Default for PullRequestsConfig {
    fn default() -> Self {
        Self { uploads_base: default_uploads_base(), assets_prefix: default_assets_prefix(), upload_timeout_ms: default_upload_timeout_ms() }
    }
}

#[allow(clippy::expect_used)]
fn default_uploads_base() -> Url {
    Url::parse("https://uploads.github.com/").expect("static URL")
}

fn default_assets_prefix() -> String {
    "https://github.com/user-attachments/assets/".into()
}

fn default_upload_timeout_ms() -> u64 {
    120_000
}

pub struct GitHubPullRequests {
    client: GitHubClient,
    orgs: Arc<dyn OrgStore>,
    clock: Arc<dyn Clock>,
    http: reqwest::Client,
    config: PullRequestsConfig,
    /// Content-creation calls are serialised per installation (GitHub secondary-limit guidance).
    writers: Mutex<HashMap<InstallationId, Arc<tokio::sync::Mutex<()>>>>,
}

/// Permission a call needs, named in `PermissionDenied`.
const CONTENTS: &str = "contents";
const PULL_REQUESTS: &str = "pull_requests";
const INSTALLATION: &str = "fix_app_installation";

impl GitHubPullRequests {
    /// `client` must be built with the Fix app's credentials (not the read app's).
    pub fn new(client: GitHubClient, orgs: Arc<dyn OrgStore>, clock: Arc<dyn Clock>, config: PullRequestsConfig) -> Result<Self, PullRequestsError> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_millis(config.upload_timeout_ms))
            .user_agent("ahomage")
            .build()
            .map_err(|e| PullRequestsError::Invalid(format!("upload http client: {e}")))?;
        Ok(Self { client, orgs, clock, http, config, writers: Mutex::new(HashMap::new()) })
    }

    async fn installation(&self, org: OrgId) -> Result<InstallationId, PullRequestsError> {
        let integrations = self.orgs.integrations(org).await.map_err(|e| PullRequestsError::Unavailable(e.to_string()))?;
        let fix = integrations.into_iter().find(|i| i.kind == IntegrationKind::GitHubFix && i.status == IntegrationStatus::Active);
        let Some(fix) = fix else {
            return Err(PullRequestsError::PermissionDenied { missing: vec![INSTALLATION] });
        };
        fix.external_id
            .parse::<u64>()
            .map(InstallationId)
            .map_err(|_| PullRequestsError::Invalid(format!("fix integration id {:?} is not an installation id", fix.external_id)))
    }

    fn scope(repo: &Repo) -> TokenScope {
        // The installation's own permission set, narrowed to this repository.
        TokenScope { repositories: Some(vec![repo.name.clone()]), permissions: BTreeMap::new() }
    }

    fn map(&self, needs: &'static str) -> impl Fn(SourceHostError) -> PullRequestsError + '_ {
        move |err| match err {
            SourceHostError::NotFound => PullRequestsError::NotFound,
            SourceHostError::Conflict => PullRequestsError::Conflict,
            SourceHostError::Invalid(msg) => PullRequestsError::Invalid(msg),
            SourceHostError::Unavailable(msg) => PullRequestsError::Unavailable(msg),
            SourceHostError::Unauthorized => PullRequestsError::PermissionDenied { missing: vec![needs] },
            SourceHostError::RateLimited { retry_at } => {
                let wait = (retry_at - self.clock.now()).max(time::Duration::SECOND);
                PullRequestsError::RateLimited { retry_after: Duration::from_secs(u64::try_from(wait.whole_seconds()).unwrap_or(1)) }
            }
        }
    }

    fn writer(&self, inst: InstallationId) -> Arc<tokio::sync::Mutex<()>> {
        self.writers.lock().entry(inst).or_default().clone()
    }

    fn repo_path(repo: &Repo, rest: &str) -> String {
        format!("/repos/{}/{}/{rest}", segment(&repo.owner), segment(&repo.name))
    }

    async fn get<T: serde::de::DeserializeOwned>(
        &self,
        org: OrgId,
        repo: &Repo,
        path: &str,
        needs: &'static str,
    ) -> Result<(T, Option<String>), PullRequestsError> {
        let inst = self.installation(org).await?;
        let scope = Self::scope(repo);
        let res = self.client.get::<T>(Auth::Installation { inst, scope: &scope }, path).await.map_err(self.map(needs))?;
        Ok((res.body, res.next))
    }

    async fn send<B: Serialize + ?Sized, T: serde::de::DeserializeOwned>(
        &self,
        inst: InstallationId,
        repo: &Repo,
        method: Method,
        path: &str,
        body: &B,
        needs: &'static str,
    ) -> Result<T, PullRequestsError> {
        let scope = Self::scope(repo);
        let res = self.client.send::<B, T>(method, Auth::Installation { inst, scope: &scope }, path, Some(body)).await.map_err(self.map(needs))?;
        Ok(res.body)
    }

    async fn commit_object(&self, org: OrgId, repo: &Repo, sha: CommitSha) -> Result<GitCommit, PullRequestsError> {
        Ok(self.get::<GitCommit>(org, repo, &Self::repo_path(repo, &format!("git/commits/{sha}")), CONTENTS).await?.0)
    }

    async fn tree(&self, org: OrgId, repo: &Repo, tree: &str, recursive: bool) -> Result<GitTree, PullRequestsError> {
        let rest = if recursive { format!("git/trees/{tree}?recursive=1") } else { format!("git/trees/{tree}") };
        Ok(self.get::<GitTree>(org, repo, &Self::repo_path(repo, &rest), CONTENTS).await?.0)
    }

    /// Tree entry of each wanted path: one recursive read, or a walk per directory when GitHub
    /// truncated the recursive listing.
    async fn entries(&self, org: OrgId, repo: &Repo, root: &str, paths: &[RepoPath]) -> Result<HashMap<String, TreeItem>, PullRequestsError> {
        let full = self.tree(org, repo, root, true).await?;
        let wanted: std::collections::HashSet<&str> = paths.iter().map(RepoPath::as_str).collect();
        if !full.truncated {
            return Ok(full.tree.into_iter().filter(|e| wanted.contains(e.path.as_str())).map(|e| (e.path.clone(), e)).collect());
        }
        let mut dirs: HashMap<String, Option<Vec<TreeItem>>> = HashMap::new();
        dirs.insert(String::new(), Some(self.tree(org, repo, root, false).await?.tree));
        let mut out = HashMap::new();
        for path in paths {
            let mut dir = String::new();
            let mut segments = path.as_str().split('/').peekable();
            while let Some(segment) = segments.next() {
                let listing = dirs.get(&dir).cloned().flatten().unwrap_or_default();
                let Some(item) = listing.into_iter().find(|e| e.path == segment) else { break };
                let full_path = if dir.is_empty() { segment.to_owned() } else { format!("{dir}/{segment}") };
                if segments.peek().is_none() {
                    out.insert(path.as_str().to_owned(), TreeItem { path: full_path, ..item });
                    break;
                }
                if item.kind != "tree" {
                    break;
                }
                if !dirs.contains_key(&full_path) {
                    let sub = self.tree(org, repo, &item.sha, false).await?;
                    dirs.insert(full_path.clone(), Some(sub.tree));
                }
                dir = full_path;
            }
        }
        Ok(out)
    }
}

/// Percent-encode one path segment (owner, repo name).
fn segment(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect::<String>().replace('+', "%20")
}

/// A branch name inside a ref path: segments encoded, `/` kept.
fn ref_path(branch: &str) -> String {
    branch.split('/').map(segment).collect::<Vec<_>>().join("/")
}

fn parse_sha(s: &str) -> Result<CommitSha, PullRequestsError> {
    s.parse().map_err(|_| PullRequestsError::Unavailable(format!("GitHub returned a malformed object id {s:?}")))
}

fn refuse_foreign(branch: &str) -> Result<(), PullRequestsError> {
    // `BranchName` already guarantees this; the adapter re-checks so no call path can write a
    // ref outside `refs/heads/mage/`.
    if branch.starts_with("mage/") && !branch.contains("..") {
        Ok(())
    } else {
        Err(PullRequestsError::Invalid(format!("refusing to write ref outside refs/heads/mage/: {branch}")))
    }
}

#[derive(Deserialize)]
struct GitRef {
    object: GitObject,
}

#[derive(Deserialize)]
struct GitObject {
    sha: String,
}

#[derive(Deserialize)]
struct GitCommit {
    tree: GitObject,
    message: String,
}

#[derive(Deserialize)]
struct GitTree {
    tree: Vec<TreeItem>,
    #[serde(default)]
    truncated: bool,
}

#[derive(Debug, Clone, Deserialize)]
struct TreeItem {
    path: String,
    mode: String,
    #[serde(rename = "type")]
    kind: String,
    sha: String,
}

#[derive(Deserialize)]
struct GitBlob {
    content: String,
    encoding: String,
}

#[derive(Deserialize)]
struct Created {
    sha: String,
}

#[derive(Serialize)]
struct NewBlob<'a> {
    content: &'a str,
    encoding: &'static str,
}

#[derive(Serialize)]
struct NewTree<'a> {
    base_tree: &'a str,
    tree: Vec<NewTreeEntry<'a>>,
}

#[derive(Serialize)]
struct NewTreeEntry<'a> {
    path: &'a str,
    mode: &'static str,
    #[serde(rename = "type")]
    kind: &'static str,
    /// `null` deletes the path.
    sha: Option<String>,
}

#[derive(Serialize)]
struct NewCommit<'a> {
    message: &'a str,
    tree: &'a str,
    parents: [String; 1],
}

#[derive(Serialize)]
struct NewRef {
    #[serde(rename = "ref")]
    git_ref: String,
    sha: String,
}

#[derive(Deserialize)]
struct CompareBody {
    ahead_by: u32,
    behind_by: u32,
    #[serde(default)]
    files: Vec<CompareFile>,
}

#[derive(Deserialize)]
struct CompareFile {
    filename: String,
    #[serde(default)]
    previous_filename: Option<String>,
}

#[derive(Deserialize)]
struct PullBody {
    number: u64,
    html_url: Url,
    head: PullHead,
    #[serde(default)]
    state: String,
    #[serde(default)]
    merged: bool,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    mergeable: Option<bool>,
}

#[derive(Deserialize)]
struct PullHead {
    sha: String,
}

#[derive(Serialize)]
struct NewPull<'a> {
    title: &'a str,
    body: &'a str,
    head: &'a str,
    base: &'a str,
    draft: bool,
}

#[derive(Serialize)]
struct Labels<'a> {
    labels: &'a [String],
}

#[derive(Serialize)]
struct ReviewRequest<'a> {
    reviewers: &'a [String],
    team_reviewers: &'a [String],
}

#[derive(Deserialize)]
struct UploadedAsset {
    url: String,
}

impl PullBody {
    fn into_ref(self) -> Result<PullRequestRef, PullRequestsError> {
        Ok(PullRequestRef { number: self.number, url: self.html_url, head_commit: parse_sha(&self.head.sha)? })
    }
}

#[async_trait]
impl PullRequests for GitHubPullRequests {
    async fn write_access(&self, org: OrgId, repo: &Repo) -> Result<WriteAccess, PullRequestsError> {
        let inst = self.installation(org).await?;
        // Installation permissions (app JWT, never cached): a token mint would reuse a cached
        // token whose permission snapshot predates `installation.new_permissions_accepted`.
        let info = self.client.installation(inst).await.map_err(self.map(INSTALLATION))?;
        if info.suspended {
            return Err(PullRequestsError::PermissionDenied { missing: vec![INSTALLATION] });
        }
        let _ = repo;
        let write = |name: &str| info.permissions.get(name) == Some(&Access::Write);
        Ok(WriteAccess { contents: write("contents"), pull_requests: write("pull_requests"), workflows: write("workflows"), issues: write("issues") })
    }

    async fn branch_head(&self, org: OrgId, repo: &Repo, branch: &str) -> Result<Option<CommitSha>, PullRequestsError> {
        let path = Self::repo_path(repo, &format!("git/ref/heads/{}", ref_path(branch)));
        match self.get::<GitRef>(org, repo, &path, CONTENTS).await {
            Ok((r, _)) => parse_sha(&r.object.sha).map(Some),
            Err(PullRequestsError::NotFound) => Ok(None),
            Err(e) => Err(e),
        }
    }

    async fn compare(&self, org: OrgId, repo: &Repo, base: CommitSha, head: CommitSha) -> Result<Comparison, PullRequestsError> {
        let mut path = Self::repo_path(repo, &format!("compare/{base}...{head}?per_page=100"));
        let mut out = Comparison::default();
        let mut first = true;
        loop {
            let (page, next) = self.get::<CompareBody>(org, repo, &path, CONTENTS).await?;
            if first {
                out.ahead_by = page.ahead_by;
                out.behind_by = page.behind_by;
                first = false;
            }
            for f in page.files {
                for name in std::iter::once(f.filename).chain(f.previous_filename) {
                    if let Ok(p) = RepoPath::new(name) {
                        out.changed_paths.insert(p);
                    }
                }
            }
            match next {
                Some(next) => path = next,
                None => return Ok(out),
            }
        }
    }

    async fn read_files(&self, org: OrgId, repo: &Repo, at: CommitSha, paths: &[RepoPath]) -> Result<Vec<BaseFile>, PullRequestsError> {
        let commit = self.commit_object(org, repo, at).await?;
        let entries = self.entries(org, repo, &commit.tree.sha, paths).await?;
        let mut out = Vec::with_capacity(paths.len());
        for path in paths {
            let Some(entry) = entries.get(path.as_str()) else {
                out.push(BaseFile::Absent { path: path.clone() });
                continue;
            };
            let mode = match (entry.kind.as_str(), entry.mode.as_str()) {
                ("blob", "100755") => FileMode::Executable,
                ("blob", m) if m.starts_with("100") => FileMode::Regular,
                _ => return Err(PullRequestsError::Invalid(format!("{path} is not a regular file at {at} (mode {})", entry.mode))),
            };
            let (blob, _) = self.get::<GitBlob>(org, repo, &Self::repo_path(repo, &format!("git/blobs/{}", entry.sha)), CONTENTS).await?;
            if blob.encoding != "base64" {
                return Err(PullRequestsError::Unavailable(format!("blob {} has encoding {}", entry.sha, blob.encoding)));
            }
            let cleaned: String = blob.content.chars().filter(|c| !c.is_ascii_whitespace()).collect();
            let bytes = BASE64.decode(cleaned).map_err(|e| PullRequestsError::Unavailable(format!("blob {}: {e}", entry.sha)))?;
            out.push(BaseFile::Present { path: path.clone(), mode, bytes });
        }
        Ok(out)
    }

    async fn create_commit(&self, org: OrgId, repo: &Repo, plan: &CommitPlan) -> Result<CommitSha, PullRequestsError> {
        let inst = self.installation(org).await?;
        let parent = self.commit_object(org, repo, plan.parent()).await?;
        let writer = self.writer(inst);
        let _serial = writer.lock().await;
        let mut entries = Vec::with_capacity(plan.files().len());
        for file in plan.files() {
            let entry = match &file.change {
                CommitChange::Write { mode, bytes } => {
                    let content = BASE64.encode(bytes);
                    let blob: Created = self
                        .send(inst, repo, Method::POST, &Self::repo_path(repo, "git/blobs"), &NewBlob { content: &content, encoding: "base64" }, CONTENTS)
                        .await?;
                    NewTreeEntry { path: file.path.as_str(), mode: mode.git_mode(), kind: "blob", sha: Some(blob.sha) }
                }
                CommitChange::Delete => NewTreeEntry { path: file.path.as_str(), mode: FileMode::Regular.git_mode(), kind: "blob", sha: None },
            };
            entries.push(entry);
        }
        let tree: Created = self
            .send(inst, repo, Method::POST, &Self::repo_path(repo, "git/trees"), &NewTree { base_tree: &parent.tree.sha, tree: entries }, CONTENTS)
            .await?;
        let commit: Created = self
            .send(
                inst,
                repo,
                Method::POST,
                &Self::repo_path(repo, "git/commits"),
                &NewCommit { message: plan.message(), tree: &tree.sha, parents: [plan.parent().to_string()] },
                CONTENTS,
            )
            .await?;
        parse_sha(&commit.sha)
    }

    async fn create_branch(&self, org: OrgId, repo: &Repo, branch: &BranchName, commit: CommitSha) -> Result<BranchOutcome, PullRequestsError> {
        refuse_foreign(branch.as_str())?;
        let inst = self.installation(org).await?;
        let writer = self.writer(inst);
        let created: Result<serde::de::IgnoredAny, PullRequestsError> = {
            let _serial = writer.lock().await;
            let body = NewRef { git_ref: format!("refs/heads/{branch}"), sha: commit.to_string() };
            self.send(inst, repo, Method::POST, &Self::repo_path(repo, "git/refs"), &body, CONTENTS).await
        };
        match created {
            Ok(_) => Ok(BranchOutcome::Created),
            Err(PullRequestsError::Invalid(msg)) if msg.to_ascii_lowercase().contains("already exists") => {
                match self.branch_head(org, repo, branch.as_str()).await? {
                    Some(head) if head == commit => Ok(BranchOutcome::AlreadyAt),
                    Some(head) => Ok(BranchOutcome::ExistsAt(head)),
                    None => Err(PullRequestsError::Conflict),
                }
            }
            Err(e) => Err(e),
        }
    }

    async fn commit_trailers(&self, org: OrgId, repo: &Repo, commit: CommitSha) -> Result<Vec<(String, String)>, PullRequestsError> {
        Ok(parse_trailers(&self.commit_object(org, repo, commit).await?.message))
    }

    async fn find_open(&self, org: OrgId, repo: &Repo, branch: &BranchName) -> Result<Option<PullRequestRef>, PullRequestsError> {
        let head: String = url::form_urlencoded::byte_serialize(format!("{}:{branch}", repo.owner).as_bytes()).collect();
        let path = Self::repo_path(repo, &format!("pulls?head={head}&state=open&per_page=10"));
        let (pulls, _) = self.get::<Vec<PullBody>>(org, repo, &path, PULL_REQUESTS).await?;
        pulls.into_iter().next().map(PullBody::into_ref).transpose()
    }

    async fn open(&self, org: OrgId, repo: &Repo, req: &OpenPullRequest) -> Result<PullRequestRef, PullRequestsError> {
        refuse_foreign(req.head.as_str())?;
        let inst = self.installation(org).await?;
        let writer = self.writer(inst);
        let opened: Result<PullBody, PullRequestsError> = {
            let _serial = writer.lock().await;
            let body = NewPull { title: &req.title, body: &req.body, head: req.head.as_str(), base: &req.base_branch, draft: req.draft };
            self.send(inst, repo, Method::POST, &Self::repo_path(repo, "pulls"), &body, PULL_REQUESTS).await
        };
        match opened {
            Ok(pull) => pull.into_ref(),
            Err(PullRequestsError::Invalid(msg)) if msg.to_ascii_lowercase().contains("already exists") => {
                self.find_open(org, repo, &req.head).await?.ok_or(PullRequestsError::Invalid(msg))
            }
            Err(e) => Err(e),
        }
    }

    async fn add_labels(&self, org: OrgId, repo: &Repo, number: u64, labels: &[String]) -> Result<(), PullRequestsError> {
        let inst = self.installation(org).await?;
        let path = Self::repo_path(repo, &format!("issues/{number}/labels"));
        let _: serde::de::IgnoredAny = self.send(inst, repo, Method::POST, &path, &Labels { labels }, PULL_REQUESTS).await?;
        Ok(())
    }

    async fn upload_media(&self, org: OrgId, repo: &Repo, media: &MediaUpload) -> Result<MediaUrl, PullRequestsError> {
        if media.bytes.len() as u64 > media.kind.max_bytes() {
            return Err(PullRequestsError::Invalid(format!("{} is {} bytes (limit {})", media.name, media.bytes.len(), media.kind.max_bytes())));
        }
        let inst = self.installation(org).await?;
        let token = self.client.installation_token(inst, &Self::scope(repo)).await.map_err(self.map(CONTENTS))?;
        let mut url = self
            .config
            .uploads_base
            .join("user-attachments/assets")
            .map_err(|e| PullRequestsError::Invalid(format!("uploads base: {e}")))?;
        url.query_pairs_mut()
            .append_pair("name", &media.name)
            .append_pair("content_type", media.kind.content_type())
            .append_pair("repository_id", &repo.external_id.to_string());
        let response = self
            .http
            .post(url)
            .header(reqwest::header::AUTHORIZATION, format!("token {}", token.token.expose()))
            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
            .header(reqwest::header::ACCEPT, "application/vnd.github+json")
            .body(media.bytes.clone())
            .send()
            .await
            .map_err(|e| PullRequestsError::Unavailable(format!("user-attachments upload failed: {}", e.without_url())))?;
        let status = response.status().as_u16();
        let retry_after = response.headers().get(reqwest::header::RETRY_AFTER).and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<u64>().ok());
        let body = response.bytes().await.map_err(|e| PullRequestsError::Unavailable(format!("user-attachments upload: {}", e.without_url())))?;
        match status {
            200..=299 => {}
            401 | 403 if retry_after.is_none() => return Err(PullRequestsError::PermissionDenied { missing: vec![CONTENTS] }),
            403 | 429 => return Err(PullRequestsError::RateLimited { retry_after: Duration::from_secs(retry_after.unwrap_or(60)) }),
            // The endpoint answers 404 when the credential cannot write (or is an app token).
            404 => return Err(PullRequestsError::NotFound),
            400..=499 => return Err(PullRequestsError::Invalid(format!("user-attachments upload refused ({status})"))),
            _ => return Err(PullRequestsError::Unavailable(format!("user-attachments upload failed ({status})"))),
        }
        let asset: UploadedAsset =
            serde_json::from_slice(&body).map_err(|e| PullRequestsError::Unavailable(format!("user-attachments answer: {e}")))?;
        if !asset.url.starts_with(&self.config.assets_prefix) {
            return Err(PullRequestsError::Unavailable("user-attachments answer is not an asset URL".into()));
        }
        Url::parse(&asset.url).map(MediaUrl).map_err(|e| PullRequestsError::Unavailable(format!("asset URL: {e}")))
    }

    async fn request_reviewers(&self, org: OrgId, repo: &Repo, number: u64, who: &Reviewers) -> Result<(), PullRequestsError> {
        if who.is_empty() {
            return Ok(());
        }
        let inst = self.installation(org).await?;
        let path = Self::repo_path(repo, &format!("pulls/{number}/requested_reviewers"));
        let body = ReviewRequest { reviewers: &who.users, team_reviewers: &who.teams };
        let _: serde::de::IgnoredAny = self.send(inst, repo, Method::POST, &path, &body, PULL_REQUESTS).await?;
        Ok(())
    }

    async fn state(&self, org: OrgId, repo: &Repo, number: u64) -> Result<PullRequestState, PullRequestsError> {
        let (pull, _) = self.get::<PullBody>(org, repo, &Self::repo_path(repo, &format!("pulls/{number}")), PULL_REQUESTS).await?;
        Ok(PullRequestState {
            open: pull.state == "open",
            merged: pull.merged,
            draft: pull.draft,
            mergeable: match pull.mergeable {
                Some(true) => Mergeable::Clean,
                Some(false) => Mergeable::Conflicting,
                None => Mergeable::Unknown,
            },
            head_commit: parse_sha(&pull.head.sha)?,
        })
    }
}

#[cfg(test)]
mod tests;
