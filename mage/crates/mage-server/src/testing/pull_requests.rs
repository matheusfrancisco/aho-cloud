//! `PullRequests` fake over an in-memory GitHub model ([`GitHubWorld`]) and the port's contract
//! suite (fix.md Testing: "`PullRequests` contract"). The adapter tests serve the same
//! `GitHubWorld` over REST (wiremock), so the suite runs against the fake and the real adapter.
//!
//! Object ids are deterministic (SHA-256 of the content, first 20 bytes), so the same plan yields
//! the same tree id, as GitHub does.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use async_trait::async_trait;
use mage_core::domain::fix::{BranchName, CommitChange, CommitPlan};
use mage_core::domain::{CommitSha, OrgId, Repo, RepoPath};
use parking_lot::Mutex;
use sha2::{Digest, Sha256};
use url::Url;

use crate::ports::pull_requests::{
    BaseFile, BranchOutcome, Comparison, FileMode, MediaUpload, MediaUrl, Mergeable, OpenPullRequest, PullRequestRef, PullRequestState,
    PullRequests, PullRequestsError, Reviewers, WriteAccess, parse_trailers,
};

/// One tree entry: git mode string (`100644` / `100755`) and blob id.
pub type TreeEntry = (String, String);

#[derive(Debug, Clone)]
pub struct WorldCommit {
    pub tree: String,
    pub parents: Vec<CommitSha>,
    pub message: String,
}

#[derive(Debug, Clone)]
pub struct WorldPull {
    pub number: u64,
    pub head: String,
    pub base: String,
    pub title: String,
    pub body: String,
    pub draft: bool,
    pub open: bool,
    pub merged: bool,
    pub labels: Vec<String>,
    pub reviewers: Reviewers,
}

#[derive(Debug, Default)]
struct WorldState {
    access: WriteAccess,
    blobs: HashMap<String, Vec<u8>>,
    trees: HashMap<String, BTreeMap<String, TreeEntry>>,
    commits: HashMap<CommitSha, WorldCommit>,
    refs: BTreeMap<String, CommitSha>,
    pulls: Vec<WorldPull>,
    uploads: Vec<(String, String, usize)>,
    upload_ok: bool,
    calls: Vec<&'static str>,
    fail_on: Option<(&'static str, PullRequestsError)>,
}

/// In-memory GitHub repository for one `owner/name`.
#[derive(Clone)]
pub struct GitHubWorld {
    owner: String,
    name: String,
    state: Arc<Mutex<WorldState>>,
}

fn object_id(kind: &str, bytes: &[u8]) -> [u8; 20] {
    let mut hasher = Sha256::new();
    hasher.update(kind.as_bytes());
    hasher.update([0]);
    hasher.update(bytes);
    let digest: [u8; 32] = hasher.finalize().into();
    let mut id = [0u8; 20];
    id.copy_from_slice(&digest[..20]);
    id
}

impl GitHubWorld {
    pub fn new(owner: &str, name: &str) -> Self {
        let state = WorldState {
            access: WriteAccess { contents: true, pull_requests: true, workflows: false, issues: true },
            upload_ok: true,
            ..WorldState::default()
        };
        Self { owner: owner.into(), name: name.into(), state: Arc::new(Mutex::new(state)) }
    }

    pub fn owner(&self) -> &str {
        &self.owner
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn set_access(&self, access: WriteAccess) {
        self.state.lock().access = access;
    }

    pub fn access(&self) -> WriteAccess {
        self.state.lock().access
    }

    /// `upload_media` succeeds (default) or is refused like an installation token is today.
    pub fn set_uploads_accepted(&self, ok: bool) {
        self.state.lock().upload_ok = ok;
    }

    pub fn uploads_accepted(&self) -> bool {
        self.state.lock().upload_ok
    }

    /// The next call of port method `call` fails with `err`, once (fake only): simulates a
    /// crash or outage at that step.
    pub fn fail_on(&self, call: &'static str, err: PullRequestsError) {
        self.state.lock().fail_on = Some((call, err));
    }

    fn take_failure(&self, call: &'static str) -> Result<(), PullRequestsError> {
        let mut s = self.state.lock();
        s.calls.push(call);
        match s.fail_on.take() {
            Some((at, err)) if at == call => Err(err),
            other => {
                s.fail_on = other;
                Ok(())
            }
        }
    }

    /// How many times a port method (or REST call family) ran.
    pub fn calls(&self, call: &str) -> usize {
        self.state.lock().calls.iter().filter(|c| **c == call).count()
    }

    pub fn record_call(&self, call: &'static str) {
        self.state.lock().calls.push(call);
    }

    // ---- objects ----

    pub fn create_blob(&self, bytes: Vec<u8>) -> String {
        let id = hex::encode(object_id("blob", &bytes));
        self.state.lock().blobs.insert(id.clone(), bytes);
        id
    }

    pub fn blob(&self, id: &str) -> Option<Vec<u8>> {
        self.state.lock().blobs.get(id).cloned()
    }

    /// New tree = `base` with `entries` applied (`None` blob = delete). Unknown base → `None`.
    pub fn create_tree(&self, base: Option<&str>, entries: &[(String, String, Option<String>)]) -> Option<String> {
        let mut s = self.state.lock();
        let mut tree = match base {
            Some(base) => s.trees.get(base)?.clone(),
            None => BTreeMap::new(),
        };
        for (path, mode, blob) in entries {
            match blob {
                Some(blob) => {
                    tree.insert(path.clone(), (mode.clone(), blob.clone()));
                }
                None => {
                    tree.remove(path);
                }
            }
        }
        let mut canon = Vec::new();
        for (path, (mode, blob)) in &tree {
            canon.extend_from_slice(format!("{mode} {path}\0{blob}\n").as_bytes());
        }
        let id = hex::encode(object_id("tree", &canon));
        s.trees.insert(id.clone(), tree);
        Some(id)
    }

    pub fn tree(&self, id: &str) -> Option<BTreeMap<String, TreeEntry>> {
        self.state.lock().trees.get(id).cloned()
    }

    pub fn create_commit(&self, message: &str, tree: &str, parents: &[CommitSha]) -> Option<CommitSha> {
        let mut s = self.state.lock();
        if !s.trees.contains_key(tree) || parents.iter().any(|p| !s.commits.contains_key(p)) {
            return None;
        }
        // GitHub stamps a time; a counter makes two identical requests distinct commits.
        let n = s.commits.len();
        let mut canon = format!("tree {tree}\n").into_bytes();
        for p in parents {
            canon.extend_from_slice(format!("parent {p}\n").as_bytes());
        }
        canon.extend_from_slice(format!("seq {n}\n\n{message}").as_bytes());
        let sha = CommitSha::from_bytes(object_id("commit", &canon));
        s.commits.insert(sha, WorldCommit { tree: tree.into(), parents: parents.to_vec(), message: message.into() });
        Some(sha)
    }

    pub fn commit(&self, sha: CommitSha) -> Option<WorldCommit> {
        self.state.lock().commits.get(&sha).cloned()
    }

    /// Seed a commit whose tree holds exactly `files` (path, mode, bytes).
    pub fn seed_commit(&self, files: &[(&str, FileMode, &[u8])], message: &str, parents: &[CommitSha]) -> CommitSha {
        let entries: Vec<(String, String, Option<String>)> = files
            .iter()
            .map(|(path, mode, bytes)| ((*path).to_owned(), mode.git_mode().to_owned(), Some(self.create_blob(bytes.to_vec()))))
            .collect();
        let tree = self.create_tree(None, &entries).unwrap_or_default();
        self.create_commit(message, &tree, parents).unwrap_or_else(|| CommitSha::from_bytes([0; 20]))
    }

    /// Files of a commit's tree: path → (mode, bytes).
    pub fn files_at(&self, sha: CommitSha) -> BTreeMap<String, (String, Vec<u8>)> {
        let s = self.state.lock();
        let Some(commit) = s.commits.get(&sha) else { return BTreeMap::new() };
        let Some(tree) = s.trees.get(&commit.tree) else { return BTreeMap::new() };
        tree.iter().map(|(path, (mode, blob))| (path.clone(), (mode.clone(), s.blobs.get(blob).cloned().unwrap_or_default()))).collect()
    }

    // ---- refs ----

    pub fn set_branch(&self, branch: &str, sha: CommitSha) {
        self.state.lock().refs.insert(branch.into(), sha);
    }

    pub fn branch(&self, branch: &str) -> Option<CommitSha> {
        self.state.lock().refs.get(branch).copied()
    }

    pub fn branches(&self) -> Vec<String> {
        self.state.lock().refs.keys().cloned().collect()
    }

    /// `Err(existing)` when the ref exists.
    pub fn create_ref(&self, branch: &str, sha: CommitSha) -> Result<(), CommitSha> {
        let mut s = self.state.lock();
        if let Some(existing) = s.refs.get(branch) {
            return Err(*existing);
        }
        s.refs.insert(branch.into(), sha);
        Ok(())
    }

    /// Paths whose blob or mode differ between two commits, and the ahead/behind counts along
    /// first parents.
    pub fn compare(&self, base: CommitSha, head: CommitSha) -> Option<Comparison> {
        let s = self.state.lock();
        let tree_of = |sha: &CommitSha| s.commits.get(sha).and_then(|c| s.trees.get(&c.tree));
        let (a, b) = (tree_of(&base)?, tree_of(&head)?);
        let mut changed = std::collections::BTreeSet::new();
        for path in a.keys().chain(b.keys()) {
            if a.get(path) != b.get(path) {
                if let Ok(p) = RepoPath::new(path.clone()) {
                    changed.insert(p);
                }
            }
        }
        let ancestors = |mut sha: CommitSha| {
            let mut line = vec![sha];
            while let Some(parent) = s.commits.get(&sha).and_then(|c| c.parents.first()) {
                line.push(*parent);
                sha = *parent;
            }
            line
        };
        let (from_base, from_head) = (ancestors(base), ancestors(head));
        let ahead = from_head.iter().take_while(|c| !from_base.contains(c)).count();
        let behind = from_base.iter().take_while(|c| !from_head.contains(c)).count();
        Some(Comparison {
            ahead_by: u32::try_from(ahead).unwrap_or(u32::MAX),
            behind_by: u32::try_from(behind).unwrap_or(u32::MAX),
            changed_paths: changed,
        })
    }

    // ---- pulls ----

    pub fn pull_url(&self, number: u64) -> String {
        format!("https://github.com/{}/{}/pull/{number}", self.owner, self.name)
    }

    pub fn find_open_pull(&self, head: &str) -> Option<WorldPull> {
        self.state.lock().pulls.iter().find(|p| p.open && p.head == head).cloned()
    }

    /// `Err(existing number)` when an open PR with this head exists.
    pub fn open_pull(&self, head: &str, base: &str, title: &str, body: &str, draft: bool) -> Result<WorldPull, u64> {
        let mut s = self.state.lock();
        if let Some(p) = s.pulls.iter().find(|p| p.open && p.head == head) {
            return Err(p.number);
        }
        let pull = WorldPull {
            number: s.pulls.len() as u64 + 1,
            head: head.into(),
            base: base.into(),
            title: title.into(),
            body: body.into(),
            draft,
            open: true,
            merged: false,
            labels: Vec::new(),
            reviewers: Reviewers::default(),
        };
        s.pulls.push(pull.clone());
        Ok(pull)
    }

    pub fn pull(&self, number: u64) -> Option<WorldPull> {
        self.state.lock().pulls.iter().find(|p| p.number == number).cloned()
    }

    pub fn pulls(&self) -> Vec<WorldPull> {
        self.state.lock().pulls.clone()
    }

    pub fn add_labels(&self, number: u64, labels: &[String]) -> bool {
        let mut s = self.state.lock();
        let Some(p) = s.pulls.iter_mut().find(|p| p.number == number) else { return false };
        for l in labels {
            if !p.labels.contains(l) {
                p.labels.push(l.clone());
            }
        }
        true
    }

    pub fn request_reviewers(&self, number: u64, who: &Reviewers) -> bool {
        let mut s = self.state.lock();
        let Some(p) = s.pulls.iter_mut().find(|p| p.number == number) else { return false };
        p.reviewers.users.extend(who.users.iter().cloned());
        p.reviewers.teams.extend(who.teams.iter().cloned());
        true
    }

    /// Close (optionally merged) a PR, as a human would.
    pub fn close_pull(&self, number: u64, merged: bool) {
        if let Some(p) = self.state.lock().pulls.iter_mut().find(|p| p.number == number) {
            p.open = false;
            p.merged = merged;
        }
    }

    pub fn record_upload(&self, name: &str, content_type: &str, len: usize) -> String {
        let mut s = self.state.lock();
        s.uploads.push((name.into(), content_type.into(), len));
        format!("https://github.com/user-attachments/assets/00000000-0000-4000-8000-{:012}", s.uploads.len())
    }

    pub fn uploads(&self) -> Vec<(String, String, usize)> {
        self.state.lock().uploads.clone()
    }
}

/// `PullRequests` over a [`GitHubWorld`]. Refuses non-`mage/` branches and writes without the
/// needed permission, like the adapter.
#[derive(Clone)]
pub struct FakePullRequests {
    world: GitHubWorld,
}

impl FakePullRequests {
    pub fn new(world: GitHubWorld) -> Self {
        Self { world }
    }

    pub fn world(&self) -> &GitHubWorld {
        &self.world
    }

    fn check_repo(&self, repo: &Repo) -> Result<(), PullRequestsError> {
        if repo.owner == self.world.owner && repo.name == self.world.name {
            Ok(())
        } else {
            Err(PullRequestsError::NotFound)
        }
    }

    fn need(&self, contents: bool, pulls: bool) -> Result<(), PullRequestsError> {
        let access = self.world.access();
        let mut missing = Vec::new();
        if contents && !access.contents {
            missing.push("contents");
        }
        if pulls && !access.pull_requests {
            missing.push("pull_requests");
        }
        if missing.is_empty() { Ok(()) } else { Err(PullRequestsError::PermissionDenied { missing }) }
    }

    fn pull_ref(&self, pull: &WorldPull) -> Result<PullRequestRef, PullRequestsError> {
        let url = Url::parse(&self.world.pull_url(pull.number)).map_err(|e| PullRequestsError::Invalid(e.to_string()))?;
        let head_commit = self.world.branch(&pull.head).ok_or(PullRequestsError::NotFound)?;
        Ok(PullRequestRef { number: pull.number, url, head_commit })
    }
}

#[async_trait]
impl PullRequests for FakePullRequests {
    async fn write_access(&self, _org: OrgId, repo: &Repo) -> Result<WriteAccess, PullRequestsError> {
        self.world.take_failure("write_access")?;
        self.check_repo(repo)?;
        Ok(self.world.access())
    }

    async fn branch_head(&self, _org: OrgId, repo: &Repo, branch: &str) -> Result<Option<CommitSha>, PullRequestsError> {
        self.world.take_failure("branch_head")?;
        self.check_repo(repo)?;
        Ok(self.world.branch(branch))
    }

    async fn compare(&self, _org: OrgId, repo: &Repo, base: CommitSha, head: CommitSha) -> Result<Comparison, PullRequestsError> {
        self.world.take_failure("compare")?;
        self.check_repo(repo)?;
        self.world.compare(base, head).ok_or(PullRequestsError::NotFound)
    }

    async fn read_files(&self, _org: OrgId, repo: &Repo, at: CommitSha, paths: &[RepoPath]) -> Result<Vec<BaseFile>, PullRequestsError> {
        self.world.take_failure("read_files")?;
        self.check_repo(repo)?;
        if self.world.commit(at).is_none() {
            return Err(PullRequestsError::NotFound);
        }
        let files = self.world.files_at(at);
        Ok(paths
            .iter()
            .map(|path| match files.get(path.as_str()) {
                Some((mode, bytes)) => BaseFile::Present {
                    path: path.clone(),
                    mode: if mode == "100755" { FileMode::Executable } else { FileMode::Regular },
                    bytes: bytes.clone(),
                },
                None => BaseFile::Absent { path: path.clone() },
            })
            .collect())
    }

    async fn create_commit(&self, _org: OrgId, repo: &Repo, plan: &CommitPlan) -> Result<CommitSha, PullRequestsError> {
        self.world.take_failure("create_commit")?;
        self.check_repo(repo)?;
        self.need(true, false)?;
        let parent = self.world.commit(plan.parent()).ok_or(PullRequestsError::NotFound)?;
        let entries: Vec<(String, String, Option<String>)> = plan
            .files()
            .iter()
            .map(|f| match &f.change {
                CommitChange::Write { mode, bytes } => {
                    (f.path.as_str().to_owned(), mode.git_mode().to_owned(), Some(self.world.create_blob(bytes.clone())))
                }
                CommitChange::Delete => (f.path.as_str().to_owned(), String::new(), None),
            })
            .collect();
        let tree = self.world.create_tree(Some(&parent.tree), &entries).ok_or(PullRequestsError::NotFound)?;
        self.world.create_commit(plan.message(), &tree, &[plan.parent()]).ok_or(PullRequestsError::NotFound)
    }

    async fn create_branch(
        &self,
        _org: OrgId,
        repo: &Repo,
        branch: &BranchName,
        commit: CommitSha,
    ) -> Result<BranchOutcome, PullRequestsError> {
        self.world.take_failure("create_branch")?;
        self.check_repo(repo)?;
        if !branch.as_str().starts_with("mage/") {
            return Err(PullRequestsError::Invalid(format!("refusing ref outside refs/heads/mage/: {branch}")));
        }
        self.need(true, false)?;
        if self.world.commit(commit).is_none() {
            return Err(PullRequestsError::Invalid("unknown commit".into()));
        }
        match self.world.create_ref(branch.as_str(), commit) {
            Ok(()) => Ok(BranchOutcome::Created),
            Err(existing) if existing == commit => Ok(BranchOutcome::AlreadyAt),
            Err(existing) => Ok(BranchOutcome::ExistsAt(existing)),
        }
    }

    async fn commit_trailers(&self, _org: OrgId, repo: &Repo, commit: CommitSha) -> Result<Vec<(String, String)>, PullRequestsError> {
        self.world.take_failure("commit_trailers")?;
        self.check_repo(repo)?;
        let commit = self.world.commit(commit).ok_or(PullRequestsError::NotFound)?;
        Ok(parse_trailers(&commit.message))
    }

    async fn find_open(&self, _org: OrgId, repo: &Repo, branch: &BranchName) -> Result<Option<PullRequestRef>, PullRequestsError> {
        self.world.take_failure("find_open")?;
        self.check_repo(repo)?;
        self.world.find_open_pull(branch.as_str()).map(|p| self.pull_ref(&p)).transpose()
    }

    async fn open(&self, _org: OrgId, repo: &Repo, req: &OpenPullRequest) -> Result<PullRequestRef, PullRequestsError> {
        self.world.take_failure("open")?;
        self.check_repo(repo)?;
        self.need(false, true)?;
        if self.world.branch(req.head.as_str()).is_none() {
            return Err(PullRequestsError::Invalid("head branch does not exist".into()));
        }
        let pull = match self.world.open_pull(req.head.as_str(), &req.base_branch, &req.title, &req.body, req.draft) {
            Ok(pull) => pull,
            Err(existing) => self.world.pull(existing).ok_or(PullRequestsError::NotFound)?,
        };
        self.pull_ref(&pull)
    }

    async fn add_labels(&self, _org: OrgId, repo: &Repo, number: u64, labels: &[String]) -> Result<(), PullRequestsError> {
        self.world.take_failure("add_labels")?;
        self.check_repo(repo)?;
        self.need(false, true)?;
        if self.world.add_labels(number, labels) { Ok(()) } else { Err(PullRequestsError::NotFound) }
    }

    async fn upload_media(&self, _org: OrgId, repo: &Repo, media: &MediaUpload) -> Result<MediaUrl, PullRequestsError> {
        self.world.take_failure("upload_media")?;
        self.check_repo(repo)?;
        if !self.world.uploads_accepted() {
            return Err(PullRequestsError::NotFound);
        }
        if media.bytes.len() as u64 > media.kind.max_bytes() {
            return Err(PullRequestsError::Invalid("file too large".into()));
        }
        let url = self.world.record_upload(&media.name, media.kind.content_type(), media.bytes.len());
        Url::parse(&url).map(MediaUrl).map_err(|e| PullRequestsError::Invalid(e.to_string()))
    }

    async fn request_reviewers(&self, _org: OrgId, repo: &Repo, number: u64, who: &Reviewers) -> Result<(), PullRequestsError> {
        self.world.take_failure("request_reviewers")?;
        self.check_repo(repo)?;
        self.need(false, true)?;
        if self.world.request_reviewers(number, who) { Ok(()) } else { Err(PullRequestsError::NotFound) }
    }

    async fn state(&self, _org: OrgId, repo: &Repo, number: u64) -> Result<PullRequestState, PullRequestsError> {
        self.world.take_failure("state")?;
        self.check_repo(repo)?;
        let pull = self.world.pull(number).ok_or(PullRequestsError::NotFound)?;
        let head_commit = self.world.branch(&pull.head).ok_or(PullRequestsError::NotFound)?;
        Ok(PullRequestState { open: pull.open, merged: pull.merged, draft: pull.draft, mergeable: Mergeable::Unknown, head_commit })
    }
}

/// Builds a gated `CommitPlan` on `parent` writing `writes`, deleting `deletes`, with `message`.
pub type PlanFn = dyn Fn(CommitSha, &[(&str, &[u8])], &[&str], &str) -> CommitPlan + Sync;

/// Scenario inputs of [`contract_pull_requests`]: a world the implementation under test serves,
/// the repo it maps to, and a way to build gated plans against it.
pub struct PullRequestsFixture<'a> {
    pub prs: &'a dyn PullRequests,
    pub world: &'a GitHubWorld,
    pub org: OrgId,
    pub repo: Repo,
    /// Builds a `CommitPlan` on `parent` that writes `writes` and deletes `deletes` (the suite
    /// cannot mint `FixGatePassed` itself).
    pub plan: &'a PlanFn,
}

/// Generic suite for any `PullRequests` implementation serving `fixture.world`.
pub async fn contract_pull_requests(fx: PullRequestsFixture<'_>) {
    let (org, repo, prs, world) = (fx.org, &fx.repo, fx.prs, fx.world);
    let base = world.seed_commit(
        &[("src/a.ts", FileMode::Regular, b"one\n"), ("bin/run.sh", FileMode::Executable, b"#!/bin/sh\n"), ("old.txt", FileMode::Regular, b"x\n")],
        "base",
        &[],
    );
    world.set_branch(&repo.default_branch, base);

    // branch_head / read_files.
    assert_eq!(prs.branch_head(org, repo, &repo.default_branch).await.ok().flatten(), Some(base), "branch_head of default");
    assert_eq!(prs.branch_head(org, repo, "mage/missing").await.ok(), Some(None), "missing branch → None");
    let paths = [RepoPath::new("src/a.ts").unwrap_or_else(|e| panic!("{e}")), RepoPath::new("bin/run.sh").unwrap_or_else(|e| panic!("{e}")), RepoPath::new("nope.md").unwrap_or_else(|e| panic!("{e}"))];
    let files = prs.read_files(org, repo, base, &paths).await.unwrap_or_else(|e| panic!("read_files: {e}"));
    assert_eq!(
        files,
        vec![
            BaseFile::Present { path: paths[0].clone(), mode: FileMode::Regular, bytes: b"one\n".to_vec() },
            BaseFile::Present { path: paths[1].clone(), mode: FileMode::Executable, bytes: b"#!/bin/sh\n".to_vec() },
            BaseFile::Absent { path: paths[2].clone() },
        ],
        "read_files contents, modes and absent paths"
    );

    // create_commit: writes + deletion entry, parent preserved, no ref moved.
    let message = "fix: thing (ENG-1)\n\nBody.\n\nMage-Change-Set: cafe\n";
    let plan = (fx.plan)(base, &[("src/a.ts", b"two\n"), ("src/new.ts", b"new\n")], &["old.txt"], message);
    let commit = prs.create_commit(org, repo, &plan).await.unwrap_or_else(|e| panic!("create_commit: {e}"));
    let after = world.files_at(commit);
    assert_eq!(after.get("src/a.ts").map(|f| f.1.as_slice()), Some(&b"two\n"[..]), "modified file");
    assert_eq!(after.get("src/new.ts").map(|f| (f.0.as_str(), f.1.as_slice())), Some(("100644", &b"new\n"[..])), "added file");
    assert_eq!(after.get("bin/run.sh").map(|f| f.0.as_str()), Some("100755"), "untouched mode kept");
    assert!(!after.contains_key("old.txt"), "deleted file removed from the tree");
    assert_eq!(world.commit(commit).map(|c| c.parents), Some(vec![base]), "parent");
    assert_eq!(world.branch(&repo.default_branch), Some(base), "create_commit moves no ref");
    let again = prs.create_commit(org, repo, &plan).await.unwrap_or_else(|e| panic!("create_commit again: {e}"));
    assert_eq!(world.commit(again).map(|c| c.tree), world.commit(commit).map(|c| c.tree), "same plan → same tree");

    // commit_trailers.
    let trailers = prs.commit_trailers(org, repo, commit).await.unwrap_or_else(|e| panic!("trailers: {e}"));
    assert_eq!(trailers, vec![("Mage-Change-Set".to_owned(), "cafe".to_owned())], "trailers");

    // create_branch: Created → AlreadyAt → ExistsAt; never force-updates; refuses non-mage refs.
    let branch = BranchName::new("mage/eng-1-cafe000").unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(prs.create_branch(org, repo, &branch, commit).await.ok(), Some(BranchOutcome::Created), "create");
    assert_eq!(prs.create_branch(org, repo, &branch, commit).await.ok(), Some(BranchOutcome::AlreadyAt), "idempotent");
    assert_eq!(prs.create_branch(org, repo, &branch, base).await.ok(), Some(BranchOutcome::ExistsAt(commit)), "exists elsewhere");
    assert_eq!(world.branch(branch.as_str()), Some(commit), "never force-updated");

    // open / find_open: an existing open PR is reused, not duplicated.
    assert_eq!(prs.find_open(org, repo, &branch).await.ok(), Some(None), "no PR yet");
    let req = OpenPullRequest {
        title: "fix: thing (ENG-1)".into(),
        body: "body".into(),
        head: branch.clone(),
        base_branch: repo.default_branch.clone(),
        draft: true,
    };
    let opened = prs.open(org, repo, &req).await.unwrap_or_else(|e| panic!("open: {e}"));
    assert_eq!(opened.head_commit, commit, "PR head");
    let reopened = prs.open(org, repo, &req).await.unwrap_or_else(|e| panic!("open existing: {e}"));
    assert_eq!(reopened.number, opened.number, "open on an existing PR returns it");
    assert_eq!(world.pulls().len(), 1, "one PR");
    assert_eq!(prs.find_open(org, repo, &branch).await.ok().flatten().map(|p| p.number), Some(opened.number), "find_open");
    let pull = world.pull(opened.number).unwrap_or_else(|| panic!("pull in world"));
    assert!(pull.draft && pull.base == repo.default_branch, "draft against the default branch");

    // labels, reviewers, state.
    prs.add_labels(org, repo, opened.number, &["mage".into(), "mage:fix".into()]).await.unwrap_or_else(|e| panic!("labels: {e}"));
    let who = Reviewers { users: vec!["octo".into()], teams: vec!["core".into()] };
    prs.request_reviewers(org, repo, opened.number, &who).await.unwrap_or_else(|e| panic!("reviewers: {e}"));
    let pull = world.pull(opened.number).unwrap_or_else(|| panic!("pull in world"));
    assert_eq!(pull.labels, vec!["mage".to_owned(), "mage:fix".to_owned()], "labels");
    assert_eq!(pull.reviewers, who, "reviewers");
    let state = prs.state(org, repo, opened.number).await.unwrap_or_else(|e| panic!("state: {e}"));
    assert!(state.open && !state.merged && state.draft && state.head_commit == commit, "state");
    world.close_pull(opened.number, true);
    let state = prs.state(org, repo, opened.number).await.unwrap_or_else(|e| panic!("state: {e}"));
    assert!(!state.open && state.merged, "merged state");

    // compare.
    let cmp = prs.compare(org, repo, base, commit).await.unwrap_or_else(|e| panic!("compare: {e}"));
    assert_eq!((cmp.ahead_by, cmp.behind_by), (1, 0), "ahead/behind");
    let changed: Vec<&str> = cmp.changed_paths.iter().map(RepoPath::as_str).collect();
    assert_eq!(changed, vec!["old.txt", "src/a.ts", "src/new.ts"], "changed paths");

    // write_access mirrors the installation; writes without permission are PermissionDenied.
    world.set_access(WriteAccess { contents: false, pull_requests: true, workflows: false, issues: true });
    assert_eq!(prs.write_access(org, repo).await.ok().map(|a| a.contents), Some(false), "write_access");
    let denied = prs.create_commit(org, repo, &plan).await;
    assert!(matches!(denied, Err(PullRequestsError::PermissionDenied { ref missing }) if missing.contains(&"contents")), "{denied:?}");
    world.set_access(WriteAccess { contents: true, pull_requests: true, workflows: false, issues: true });

    // upload_media: success returns a user-attachments URL; refusal is an error (callers fall back).
    let media = MediaUpload { name: "after.gif".into(), kind: crate::ports::pull_requests::MediaKind::Gif, bytes: b"GIF89a".to_vec() };
    let url = prs.upload_media(org, repo, &media).await.unwrap_or_else(|e| panic!("upload: {e}"));
    assert!(url.0.as_str().starts_with("https://github.com/user-attachments/assets/"), "{url:?}");
    world.set_uploads_accepted(false);
    assert!(prs.upload_media(org, repo, &media).await.is_err(), "refused upload is an error");
    world.set_uploads_accepted(true);
}

