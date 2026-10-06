//! In-memory fakes of the intake ports: [`FakeTracker`] (`TicketTracker`), [`FakeChat`]
//! (`ChatNotifier`), [`FakeSourceHostAdmin`] (`SourceHostAdmin`), [`FakeDeploymentResolver`]
//! (`DeploymentResolver`). Each can fail the next call of a method, and the two outbound fakes can
//! "lose the response" of a write (the write happens, the caller sees `Unavailable`), which is how
//! tests reproduce an uncertain outcome.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::Arc;

use async_trait::async_trait;
use mage_core::domain::{Deployment, InstallationId, OrgId, Repo, SlackThreadRef};
use mage_core::ports::Clock;
use mage_core::ports::source_host::{InstallationInfo, SourceHostError};
use parking_lot::Mutex;
use time::OffsetDateTime;
use url::Url;
use uuid::Uuid;

use crate::ports::chat::{ChatAuthor, ChatError, ChatGrant, ChatMessageView, ChatNotifier, ChatPost, ChatThread, PostedChat};
use crate::ports::deployments::{DeploymentError, DeploymentResolver};
use crate::ports::source_host_admin::{GithubApp, HookDelivery, HookDeliveryPage, SourceHostAdmin};
use crate::ports::tracker::{
    CommentDraft, IssuePage, IssueQuery, IssueStub, LinkAttachment, PostedComment, TicketTracker, TrackerError, TrackerGrant,
    TrackerIdentity, TrackerIssue, TrackerUpload,
};

// ---------------------------------------------------------------------------------------------
// TicketTracker
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TrackerMethod {
    GetIssue,
    UpdatedIssues,
    Comment,
    AttachLink,
    SetState,
    UploadFile,
    Identity,
    CompleteInstall,
}

/// A comment the fake holds (Linear's id = the `client_id` Mage chose).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FakeComment {
    pub org: OrgId,
    pub issue_id: String,
    pub id: Uuid,
    pub body: String,
}

#[derive(Default)]
struct TrackerState {
    issues: HashMap<(OrgId, String), TrackerIssue>,
    identities: HashMap<OrgId, TrackerIdentity>,
    comments: Vec<FakeComment>,
    /// (org, issue, url) → attachment, insertion-ordered by `attach_order`.
    attachments: BTreeMap<(OrgId, String, String), LinkAttachment>,
    states_set: Vec<(OrgId, String, String)>,
    uploads: Vec<(OrgId, TrackerUpload, Url)>,
    grant: Option<TrackerGrant>,
    fail_next: HashMap<TrackerMethod, VecDeque<TrackerError>>,
    lose_response: HashMap<TrackerMethod, u32>,
    calls: Vec<TrackerMethod>,
}

/// Scripted Linear. `comment` is idempotent by `client_id` like the real adapter.
#[derive(Default)]
pub struct FakeTracker {
    state: Mutex<TrackerState>,
}

impl FakeTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn put_issue(&self, org: OrgId, issue: TrackerIssue) {
        self.state.lock().issues.insert((org, issue.issue_id.clone()), issue);
    }

    pub fn update_issue(&self, org: OrgId, issue_id: &str, change: impl FnOnce(&mut TrackerIssue)) {
        let mut st = self.state.lock();
        let issue = st.issues.get_mut(&(org, issue_id.to_owned())).expect("issue seeded");
        change(issue);
    }

    pub fn remove_issue(&self, org: OrgId, issue_id: &str) {
        self.state.lock().issues.remove(&(org, issue_id.to_owned()));
    }

    pub fn set_identity(&self, org: OrgId, identity: TrackerIdentity) {
        self.state.lock().identities.insert(org, identity);
    }

    pub fn set_grant(&self, grant: TrackerGrant) {
        self.state.lock().grant = Some(grant);
    }

    pub fn fail_next(&self, method: TrackerMethod, error: TrackerError) {
        self.state.lock().fail_next.entry(method).or_default().push_back(error);
    }

    /// The next `comment` / `attach_link` / `set_state` / `upload_file` call takes effect but
    /// reports `Unavailable` (a timeout after the provider committed).
    pub fn lose_next_response(&self, method: TrackerMethod) {
        *self.state.lock().lose_response.entry(method).or_default() += 1;
    }

    pub fn comments(&self) -> Vec<FakeComment> {
        self.state.lock().comments.clone()
    }

    pub fn attachments(&self) -> Vec<LinkAttachment> {
        self.state.lock().attachments.values().cloned().collect()
    }

    pub fn states_set(&self) -> Vec<(OrgId, String, String)> {
        self.state.lock().states_set.clone()
    }

    pub fn uploads(&self) -> Vec<(OrgId, TrackerUpload, Url)> {
        self.state.lock().uploads.clone()
    }

    pub fn calls(&self) -> Vec<TrackerMethod> {
        self.state.lock().calls.clone()
    }

    fn enter(&self, method: TrackerMethod) -> Result<(), TrackerError> {
        let mut st = self.state.lock();
        st.calls.push(method);
        match st.fail_next.get_mut(&method).and_then(VecDeque::pop_front) {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }

    fn lose(&self, method: TrackerMethod) -> bool {
        let mut st = self.state.lock();
        match st.lose_response.get_mut(&method) {
            Some(n) if *n > 0 => {
                *n -= 1;
                true
            }
            _ => false,
        }
    }

    fn after_write<T>(&self, method: TrackerMethod, ok: T) -> Result<T, TrackerError> {
        if self.lose(method) { Err(TrackerError::Unavailable("response lost".into())) } else { Ok(ok) }
    }
}

#[async_trait]
impl TicketTracker for FakeTracker {
    async fn get_issue(&self, org: OrgId, issue_id: &str) -> Result<TrackerIssue, TrackerError> {
        self.enter(TrackerMethod::GetIssue)?;
        self.state.lock().issues.get(&(org, issue_id.to_owned())).cloned().ok_or(TrackerError::NotFound)
    }

    async fn updated_issues(&self, org: OrgId, q: &IssueQuery) -> Result<IssuePage, TrackerError> {
        self.enter(TrackerMethod::UpdatedIssues)?;
        let st = self.state.lock();
        let mut all: Vec<IssueStub> = st
            .issues
            .iter()
            .filter(|((o, _), i)| *o == org && q.team_keys.iter().any(|t| t == &i.team_key) && i.updated_at > q.updated_since)
            .map(|(_, i)| IssueStub { issue_id: i.issue_id.clone(), updated_at: i.updated_at })
            .collect();
        all.sort_by(|a, b| (a.updated_at, &a.issue_id).cmp(&(b.updated_at, &b.issue_id)));
        let start: usize = q.cursor.as_deref().map_or(0, |c| c.parse().unwrap_or(0));
        let size = usize::from(q.page_size.max(1));
        let end = (start + size).min(all.len());
        let next = (end < all.len()).then(|| end.to_string());
        Ok(IssuePage { items: all[start.min(end)..end].to_vec(), next })
    }

    async fn comment(&self, org: OrgId, issue_id: &str, c: &CommentDraft) -> Result<PostedComment, TrackerError> {
        self.enter(TrackerMethod::Comment)?;
        let created = {
            let mut st = self.state.lock();
            if !st.issues.contains_key(&(org, issue_id.to_owned())) {
                return Err(TrackerError::NotFound);
            }
            if st.comments.iter().any(|x| x.id == c.client_id) {
                false
            } else {
                st.comments.push(FakeComment { org, issue_id: issue_id.to_owned(), id: c.client_id, body: c.body_markdown.clone() });
                true
            }
        };
        self.after_write(TrackerMethod::Comment, PostedComment { id: c.client_id.to_string(), url: None, created })
    }

    async fn attach_link(&self, org: OrgId, issue_id: &str, link: &LinkAttachment) -> Result<(), TrackerError> {
        self.enter(TrackerMethod::AttachLink)?;
        self.state.lock().attachments.insert((org, issue_id.to_owned(), link.url.to_string()), link.clone());
        self.after_write(TrackerMethod::AttachLink, ())
    }

    async fn set_state(&self, org: OrgId, issue_id: &str, state_name: &str) -> Result<(), TrackerError> {
        self.enter(TrackerMethod::SetState)?;
        {
            let mut st = self.state.lock();
            let issue = st.issues.get_mut(&(org, issue_id.to_owned())).ok_or(TrackerError::NotFound)?;
            if issue.state_name != state_name {
                issue.state_name = state_name.to_owned();
                st.states_set.push((org, issue_id.to_owned(), state_name.to_owned()));
            }
        }
        self.after_write(TrackerMethod::SetState, ())
    }

    async fn upload_file(&self, org: OrgId, file: &TrackerUpload) -> Result<Url, TrackerError> {
        self.enter(TrackerMethod::UploadFile)?;
        let url = {
            let mut st = self.state.lock();
            let n = st.uploads.len() + 1;
            let url = Url::parse(&format!("https://uploads.linear.app/fake/{n}/{}", file.file_name))
                .map_err(|e| TrackerError::Invalid(e.to_string()))?;
            st.uploads.push((org, file.clone(), url.clone()));
            url
        };
        self.after_write(TrackerMethod::UploadFile, url)
    }

    async fn identity(&self, org: OrgId) -> Result<TrackerIdentity, TrackerError> {
        self.enter(TrackerMethod::Identity)?;
        self.state.lock().identities.get(&org).cloned().ok_or(TrackerError::Unauthorized)
    }

    async fn complete_install(&self, code: &str, _redirect_uri: &Url, pkce_verifier: &str) -> Result<TrackerGrant, TrackerError> {
        self.enter(TrackerMethod::CompleteInstall)?;
        if code.is_empty() || pkce_verifier.is_empty() {
            return Err(TrackerError::Invalid("code and verifier required".into()));
        }
        self.state.lock().grant.clone().ok_or_else(|| TrackerError::Invalid("unknown code".into()))
    }
}

// ---------------------------------------------------------------------------------------------
// ChatNotifier
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChatMethod {
    ReadThread,
    PostReply,
    FindReply,
    CompleteInstall,
}

#[derive(Default)]
struct ChatState {
    threads: HashMap<SlackThreadRef, Vec<ChatMessageView>>,
    posts: Vec<(OrgId, SlackThreadRef, ChatPost, String)>,
    next_ts: u64,
    grant: Option<ChatGrant>,
    fail_next: HashMap<ChatMethod, VecDeque<ChatError>>,
    lose_response: u32,
    calls: Vec<ChatMethod>,
}

/// Scripted Slack: seeded thread messages plus our replies (bot, `ours`, metadata key).
pub struct FakeChat {
    clock: Arc<dyn Clock>,
    state: Mutex<ChatState>,
}

impl FakeChat {
    pub const BOT_ID: &'static str = "UMAGEBOT";

    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self { clock, state: Mutex::new(ChatState { next_ts: 1, ..ChatState::default() }) }
    }

    /// Append a message to a thread (creating it).
    pub fn push_message(&self, thread: &SlackThreadRef, message: ChatMessageView) {
        self.state.lock().threads.entry(thread.clone()).or_default().push(message);
    }

    pub fn set_grant(&self, grant: ChatGrant) {
        self.state.lock().grant = Some(grant);
    }

    pub fn fail_next(&self, method: ChatMethod, error: ChatError) {
        self.state.lock().fail_next.entry(method).or_default().push_back(error);
    }

    /// The next `post_reply` posts but reports `Unavailable`.
    pub fn lose_next_post_response(&self) {
        self.state.lock().lose_response += 1;
    }

    /// Every reply posted, with the ts Slack assigned.
    pub fn posts(&self) -> Vec<(OrgId, SlackThreadRef, ChatPost, String)> {
        self.state.lock().posts.clone()
    }

    pub fn calls(&self) -> Vec<ChatMethod> {
        self.state.lock().calls.clone()
    }

    fn enter(&self, method: ChatMethod) -> Result<(), ChatError> {
        let mut st = self.state.lock();
        st.calls.push(method);
        match st.fail_next.get_mut(&method).and_then(VecDeque::pop_front) {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }
}

#[async_trait]
impl ChatNotifier for FakeChat {
    async fn read_thread(&self, _org: OrgId, thread: &SlackThreadRef, max_messages: u16) -> Result<ChatThread, ChatError> {
        self.enter(ChatMethod::ReadThread)?;
        let st = self.state.lock();
        let all = st.threads.get(thread).ok_or(ChatError::NotFound)?;
        let max = usize::from(max_messages);
        Ok(ChatThread { thread: thread.clone(), messages: all.iter().take(max).cloned().collect(), truncated: all.len() > max })
    }

    async fn post_reply(&self, org: OrgId, thread: &SlackThreadRef, post: &ChatPost) -> Result<PostedChat, ChatError> {
        self.enter(ChatMethod::PostReply)?;
        let now = self.clock.now();
        let mut st = self.state.lock();
        let ts = format!("{}.{:06}", now.unix_timestamp(), st.next_ts);
        st.next_ts += 1;
        let text = match &post.link {
            Some((url, label)) => format!("{} <{url}|{label}>", post.text),
            None => post.text.clone(),
        };
        st.threads.entry(thread.clone()).or_default().push(ChatMessageView {
            ts: ts.clone(),
            author: ChatAuthor::Bot { id: Self::BOT_ID.into(), ours: true },
            text,
            at: now,
            file_names: vec![],
            mage_key: Some(post.key.clone()),
        });
        st.posts.push((org, thread.clone(), post.clone(), ts.clone()));
        if st.lose_response > 0 {
            st.lose_response -= 1;
            return Err(ChatError::Unavailable("response lost".into()));
        }
        Ok(PostedChat { ts })
    }

    async fn find_reply(&self, _org: OrgId, thread: &SlackThreadRef, key: &str, since: OffsetDateTime) -> Result<Option<PostedChat>, ChatError> {
        self.enter(ChatMethod::FindReply)?;
        let st = self.state.lock();
        Ok(st.threads.get(thread).and_then(|msgs| {
            msgs.iter()
                .find(|m| matches!(m.author, ChatAuthor::Bot { ours: true, .. }) && m.mage_key.as_deref() == Some(key) && m.at >= since)
                .map(|m| PostedChat { ts: m.ts.clone() })
        }))
    }

    async fn complete_install(&self, code: &str, _redirect_uri: &Url) -> Result<ChatGrant, ChatError> {
        self.enter(ChatMethod::CompleteInstall)?;
        if code.is_empty() {
            return Err(ChatError::Invalid("code required".into()));
        }
        self.state.lock().grant.clone().ok_or_else(|| ChatError::Invalid("unknown code".into()))
    }
}

// ---------------------------------------------------------------------------------------------
// SourceHostAdmin
// ---------------------------------------------------------------------------------------------

#[derive(Default)]
struct AdminState {
    /// (app, code) → installations the code's user can see.
    visible: HashMap<(GithubApp, String), Vec<InstallationInfo>>,
    /// Newest first.
    deliveries: HashMap<GithubApp, Vec<HookDelivery>>,
    redelivered: Vec<(GithubApp, u64)>,
    evicted: Vec<(GithubApp, InstallationId)>,
    fail_next: VecDeque<SourceHostError>,
}

#[derive(Default)]
pub struct FakeSourceHostAdmin {
    state: Mutex<AdminState>,
}

impl FakeSourceHostAdmin {
    pub fn new() -> Self {
        Self::default()
    }

    /// `code` exchanges to a user who can see `info`.
    pub fn grant(&self, app: GithubApp, code: &str, info: InstallationInfo) {
        self.state.lock().visible.entry((app, code.to_owned())).or_default().push(info);
    }

    /// Deliveries of `app`, newest first.
    pub fn set_deliveries(&self, app: GithubApp, deliveries: Vec<HookDelivery>) {
        self.state.lock().deliveries.insert(app, deliveries);
    }

    pub fn fail_next(&self, error: SourceHostError) {
        self.state.lock().fail_next.push_back(error);
    }

    pub fn redelivered(&self) -> Vec<(GithubApp, u64)> {
        self.state.lock().redelivered.clone()
    }

    pub fn evicted(&self) -> Vec<(GithubApp, InstallationId)> {
        self.state.lock().evicted.clone()
    }

    fn enter(&self) -> Result<(), SourceHostError> {
        match self.state.lock().fail_next.pop_front() {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }
}

#[async_trait]
impl SourceHostAdmin for FakeSourceHostAdmin {
    async fn complete_install(&self, app: GithubApp, inst: InstallationId, code: &str) -> Result<InstallationInfo, SourceHostError> {
        self.enter()?;
        let st = self.state.lock();
        let visible = st.visible.get(&(app, code.to_owned())).ok_or_else(|| SourceHostError::Invalid("bad verification code".into()))?;
        visible.iter().find(|i| i.id == inst).cloned().ok_or_else(|| SourceHostError::Invalid("installation not visible to user".into()))
    }

    async fn hook_deliveries(&self, app: GithubApp, cursor: Option<String>, per_page: u8) -> Result<HookDeliveryPage, SourceHostError> {
        self.enter()?;
        let st = self.state.lock();
        let all = st.deliveries.get(&app).cloned().unwrap_or_default();
        let start: usize = cursor.as_deref().map_or(0, |c| c.parse().unwrap_or(0));
        let end = (start + usize::from(per_page.max(1))).min(all.len());
        let next = (end < all.len()).then(|| end.to_string());
        Ok(HookDeliveryPage { items: all[start.min(end)..end].to_vec(), next })
    }

    async fn redeliver(&self, app: GithubApp, delivery_id: u64) -> Result<(), SourceHostError> {
        self.enter()?;
        self.state.lock().redelivered.push((app, delivery_id));
        Ok(())
    }

    fn evict_installation(&self, app: GithubApp, inst: InstallationId) {
        self.state.lock().evicted.push((app, inst));
    }
}

// ---------------------------------------------------------------------------------------------
// DeploymentResolver
// ---------------------------------------------------------------------------------------------

#[derive(Default)]
struct DeploymentState {
    known: Vec<Deployment>,
    fail_next: VecDeque<DeploymentError>,
    calls: Vec<(OrgId, String, OffsetDateTime)>,
}

/// Answers from a list of successful deployments; scripted failures first.
#[derive(Default)]
pub struct FakeDeploymentResolver {
    state: Mutex<DeploymentState>,
}

impl FakeDeploymentResolver {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&self, deployment: Deployment) {
        self.state.lock().known.push(deployment);
    }

    pub fn fail_next(&self, error: DeploymentError) {
        self.state.lock().fail_next.push_back(error);
    }

    /// `(org, environment, as_of)` per call.
    pub fn calls(&self) -> Vec<(OrgId, String, OffsetDateTime)> {
        self.state.lock().calls.clone()
    }
}

#[async_trait]
impl DeploymentResolver for FakeDeploymentResolver {
    async fn deployed_sha(&self, org: OrgId, repo: &Repo, environment: &str, as_of: OffsetDateTime) -> Result<Option<Deployment>, DeploymentError> {
        let mut st = self.state.lock();
        st.calls.push((org, environment.to_owned(), as_of));
        if let Some(err) = st.fail_next.pop_front() {
            return Err(err);
        }
        Ok(st
            .known
            .iter()
            .filter(|d| d.org_id == org && d.repo_id == repo.id && d.environment == environment && d.observed_at <= as_of)
            .max_by_key(|d| d.observed_at)
            .cloned())
    }
}

#[cfg(test)]
mod tests {
    use mage_core::testing::FakeClock;
    use time::macros::datetime;

    use super::*;
    use crate::ports::tracker::{TrackerStateKind, TrackerComment};

    fn org() -> OrgId {
        OrgId::from_uuid(Uuid::from_u128(1))
    }

    fn issue() -> TrackerIssue {
        TrackerIssue {
            issue_id: "iss-1".into(),
            identifier: "ENG-1".into(),
            url: Url::parse("https://linear.app/acme/issue/ENG-1").unwrap(),
            workspace_id: "ws".into(),
            team_key: "ENG".into(),
            project: None,
            labels: vec![],
            state_name: "Triage".into(),
            state_kind: TrackerStateKind::Triage,
            title: "t".into(),
            description: String::new(),
            created_at: datetime!(2026-10-01 10:00 UTC),
            updated_at: datetime!(2026-10-01 10:00 UTC),
            archived: false,
            comments: Vec::<TrackerComment>::new(),
            attachments: vec![],
            customer_ids: vec![],
        }
    }

    /// intake.md "Port contract tests": `comment` with a repeated `client_id` returns
    /// `created: false` and leaves one comment.
    #[tokio::test]
    async fn tracker_comment_is_idempotent_by_client_id() {
        let tracker = FakeTracker::new();
        tracker.put_issue(org(), issue());
        let draft = CommentDraft { client_id: Uuid::from_u128(7), body_markdown: "hello".into() };
        let first = tracker.comment(org(), "iss-1", &draft).await.unwrap();
        let again = tracker.comment(org(), "iss-1", &CommentDraft { body_markdown: "changed".into(), ..draft.clone() }).await.unwrap();
        assert!(first.created && !again.created);
        assert_eq!(first.id, again.id);
        let comments = tracker.comments();
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].body, "hello");
    }

    /// `find_reply` returns a posted reply by key, and only one posted at or after `since`.
    #[tokio::test]
    async fn chat_find_reply_by_key() {
        let clock = Arc::new(FakeClock::at(datetime!(2026-10-01 10:00 UTC)));
        let chat = FakeChat::new(clock.clone());
        let thread = SlackThreadRef { team: "T1".into(), channel: "C1".into(), ts: "1.000001".into() };
        let posted = chat.post_reply(org(), &thread, &ChatPost { key: "k1".into(), text: "hi".into(), link: None }).await.unwrap();
        let found = chat.find_reply(org(), &thread, "k1", datetime!(2026-10-01 09:59 UTC)).await.unwrap();
        assert_eq!(found, Some(posted));
        assert_eq!(chat.find_reply(org(), &thread, "k2", datetime!(2026-10-01 09:59 UTC)).await.unwrap(), None);
        assert_eq!(chat.find_reply(org(), &thread, "k1", datetime!(2026-10-01 10:01 UTC)).await.unwrap(), None);
    }
}
