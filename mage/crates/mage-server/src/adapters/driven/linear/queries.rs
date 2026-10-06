//! GraphQL operations (intake.md "Operations") and their response shapes. Every connection sets
//! `first` explicitly to keep query complexity bounded.

use serde::Deserialize;
use serde_json::{Map, Value};
use time::OffsetDateTime;

pub(super) const ISSUE: &str = r#"query MageIssue($id: String!) {
  issue(id: $id) {
    id identifier url title description createdAt updatedAt archivedAt
    team { key }
    project { name }
    state { name type }
    labels(first: 50) { nodes { name } }
    attachments(first: 20) { nodes { url title sourceType metadata } }
    comments(first: 100, orderBy: createdAt) { nodes { id body createdAt user { id } botActor { id } } }
    needs(first: 10) { nodes { customer { id } } }
  }
}"#;

pub(super) const UPDATED_ISSUES: &str = r#"query MageUpdatedIssues($teams: [String!]!, $since: DateTimeOrDuration!, $first: Int!, $after: String) {
  issues(
    filter: { team: { key: { in: $teams } }, updatedAt: { gt: $since } }
    orderBy: updatedAt
    first: $first
    after: $after
    includeArchived: true
  ) {
    nodes { id updatedAt }
    pageInfo { hasNextPage endCursor }
  }
}"#;

pub(super) const COMMENT_CREATE: &str = r#"mutation MageCommentCreate($input: CommentCreateInput!) {
  commentCreate(input: $input) { success comment { id url } }
}"#;

pub(super) const COMMENT: &str = r#"query MageComment($id: String!) {
  comment(id: $id) { id url }
}"#;

pub(super) const ATTACHMENT_CREATE: &str = r#"mutation MageAttachmentCreate($input: AttachmentCreateInput!) {
  attachmentCreate(input: $input) { success }
}"#;

pub(super) const ISSUE_STATE: &str = r#"query MageIssueState($id: String!) {
  issue(id: $id) { id state { id name } team { id } }
}"#;

pub(super) const TEAM_STATES: &str = r#"query MageTeamStates($id: String!) {
  team(id: $id) { states(first: 100) { nodes { id name } } }
}"#;

pub(super) const ISSUE_UPDATE: &str = r#"mutation MageIssueUpdate($id: String!, $input: IssueUpdateInput!) {
  issueUpdate(id: $id, input: $input) { success }
}"#;

pub(super) const FILE_UPLOAD: &str = r#"mutation MageFileUpload($contentType: String!, $filename: String!, $size: Int!) {
  fileUpload(contentType: $contentType, filename: $filename, size: $size) {
    success
    uploadFile { uploadUrl assetUrl headers { key value } }
  }
}"#;

pub(super) const VIEWER: &str = r#"query MageViewer {
  viewer { id }
}"#;

pub(super) const VIEWER_APP: &str = r#"query MageViewerApp($clientId: String!) {
  viewer { id }
  applicationInfo(clientId: $clientId) { id }
}"#;

pub(super) const VIEWER_ORGANIZATION: &str = r#"query MageViewerOrganization {
  viewer { organization { id name } }
}"#;

#[derive(Debug, Deserialize)]
pub(super) struct Nodes<T> {
    pub nodes: Vec<T>,
}

#[derive(Debug, Deserialize)]
pub(super) struct Id {
    pub id: String,
}

#[derive(Debug, Deserialize)]
pub(super) struct Name {
    pub name: String,
}

#[derive(Debug, Deserialize)]
pub(super) struct IssueData {
    pub issue: Option<Issue>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Issue {
    pub id: String,
    pub identifier: String,
    pub url: String,
    pub title: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub archived_at: Option<OffsetDateTime>,
    pub team: TeamKey,
    #[serde(default)]
    pub project: Option<Name>,
    pub state: State,
    pub labels: Nodes<Name>,
    pub attachments: Nodes<Attachment>,
    pub comments: Nodes<Comment>,
    #[serde(default)]
    pub needs: Option<Nodes<Need>>,
}

#[derive(Debug, Deserialize)]
pub(super) struct TeamKey {
    pub key: String,
}

#[derive(Debug, Deserialize)]
pub(super) struct State {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Attachment {
    pub url: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub source_type: Option<String>,
    #[serde(default)]
    pub metadata: Option<Map<String, Value>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Comment {
    pub id: String,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(default)]
    pub user: Option<Id>,
    #[serde(default)]
    pub bot_actor: Option<OptionalId>,
}

#[derive(Debug, Deserialize)]
pub(super) struct OptionalId {
    #[serde(default)]
    pub id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(super) struct Need {
    #[serde(default)]
    pub customer: Option<Id>,
}

#[derive(Debug, Deserialize)]
pub(super) struct IssuesData {
    pub issues: IssueConnection,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct IssueConnection {
    pub nodes: Vec<IssueStubNode>,
    pub page_info: PageInfo,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct IssueStubNode {
    pub id: String,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct PageInfo {
    pub has_next_page: bool,
    #[serde(default)]
    pub end_cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct CommentCreateData {
    pub comment_create: CommentPayload,
}

#[derive(Debug, Deserialize)]
pub(super) struct CommentPayload {
    pub success: bool,
    #[serde(default)]
    pub comment: Option<CommentRef>,
}

#[derive(Debug, Deserialize)]
pub(super) struct CommentRef {
    pub id: String,
    #[serde(default)]
    pub url: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(super) struct CommentData {
    pub comment: Option<CommentRef>,
}

#[derive(Debug, Deserialize)]
pub(super) struct Success {
    pub success: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct AttachmentCreateData {
    pub attachment_create: Success,
}

#[derive(Debug, Deserialize)]
pub(super) struct IssueStateData {
    pub issue: Option<IssueState>,
}

#[derive(Debug, Deserialize)]
pub(super) struct IssueState {
    pub state: StateRef,
    pub team: Id,
}

#[derive(Debug, Deserialize)]
pub(super) struct StateRef {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Deserialize)]
pub(super) struct TeamStatesData {
    pub team: Option<TeamStates>,
}

#[derive(Debug, Deserialize)]
pub(super) struct TeamStates {
    pub states: Nodes<StateRef>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct IssueUpdateData {
    pub issue_update: Success,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct FileUploadData {
    pub file_upload: FileUploadPayload,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct FileUploadPayload {
    pub success: bool,
    #[serde(default)]
    pub upload_file: Option<UploadFile>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct UploadFile {
    pub upload_url: String,
    pub asset_url: String,
    #[serde(default)]
    pub headers: Vec<UploadHeader>,
}

#[derive(Debug, Deserialize)]
pub(super) struct UploadHeader {
    pub key: String,
    pub value: String,
}

#[derive(Debug, Deserialize)]
pub(super) struct ViewerData {
    pub viewer: Id,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ViewerAppData {
    pub viewer: Id,
    #[serde(default)]
    pub application_info: Option<Id>,
}

#[derive(Debug, Deserialize)]
pub(super) struct ViewerOrganizationData {
    pub viewer: ViewerOrganization,
}

#[derive(Debug, Deserialize)]
pub(super) struct ViewerOrganization {
    pub organization: Organization,
}

#[derive(Debug, Deserialize)]
pub(super) struct Organization {
    pub id: String,
    pub name: String,
}
