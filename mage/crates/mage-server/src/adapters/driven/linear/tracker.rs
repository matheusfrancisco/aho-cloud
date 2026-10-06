//! [`LinearTracker`]: `TicketTracker` over Linear's GraphQL API (intake.md "Operations").

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use async_trait::async_trait;
use mage_core::domain::OrgId;
use mage_core::ports::Clock;
use parking_lot::Mutex;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, CACHE_CONTROL, CONTENT_TYPE};
use serde::de::DeserializeOwned;
use serde_json::{json, Map, Value};
use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime};
use url::Url;

use super::auth::{Credentials, Session, TokenError};
use super::client::{transport_error, Caller, GraphqlClient};
use super::queries as q;
use super::LinearConfig;
use crate::domain::intake::LinearAuthMode;
use crate::ports::org::OrgStore;
use crate::ports::tracker::{
    AccessToken, Author, CommentDraft, IssuePage, IssueQuery, IssueStub, LinkAttachment, PostedComment, TicketTracker, TrackerAttachment, TrackerComment,
    TrackerError, TrackerGrant, TrackerIdentity, TrackerIssue, TrackerStateKind, TrackerUpload,
};
use crate::ports::vault::SecretVault;

/// Identity and team workflow states change rarely.
const IDENTITY_TTL: Duration = Duration::hours(1);
const STATES_TTL: Duration = Duration::hours(1);
/// Linear's maximum page size.
const MAX_PAGE_SIZE: u8 = 250;
/// Rate-limit bucket of the one-off request `complete_install` makes with a brand-new token.
const INSTALL_BUCKET: &str = "oauth-install";

/// One team's workflow states (`(name, id)`), fetched at `at`.
struct TeamStates {
    states: Vec<(String, String)>,
    at: OffsetDateTime,
}

pub struct LinearTracker {
    gql: GraphqlClient,
    creds: Credentials,
    http: reqwest::Client,
    clock: Arc<dyn Clock>,
    upload_timeout: std::time::Duration,
    identities: Mutex<HashMap<OrgId, (TrackerIdentity, OffsetDateTime)>>,
    /// Keyed by `(workspace id, team id)`.
    team_states: Mutex<HashMap<(String, String), TeamStates>>,
}

impl LinearTracker {
    pub fn new(config: &LinearConfig, vault: Arc<dyn SecretVault>, orgs: Arc<dyn OrgStore>, clock: Arc<dyn Clock>) -> Result<Self, TrackerError> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("mage-server/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(std::time::Duration::from_millis(config.connect_timeout_ms))
            .timeout(std::time::Duration::from_millis(config.request_timeout_ms))
            .build()
            .map_err(|e| TrackerError::Unavailable(format!("build Linear HTTP client: {e}")))?;
        Ok(LinearTracker {
            gql: GraphqlClient::new(http.clone(), config.api_url.clone(), clock.clone()),
            creds: Credentials::new(vault, orgs, clock.clone(), http.clone(), config.oauth_token_url.clone(), config.oauth_client_id.clone()),
            http,
            clock,
            upload_timeout: std::time::Duration::from_millis(config.upload_timeout_ms),
            identities: Mutex::new(HashMap::new()),
            team_states: Mutex::new(HashMap::new()),
        })
    }

    /// Runs one operation as the org's workspace credential. A 401 refreshes an OAuth token once
    /// and retries; an API key (or a second 401) evicts the cached credential.
    async fn call<T: DeserializeOwned>(&self, org: OrgId, op: &'static str, query: &str, variables: Value) -> Result<(T, Session), TrackerError> {
        let session = self.creds.session(org).await?;
        match self.gql.execute(&session.caller()?, op, query, &variables).await {
            Err(TrackerError::Unauthorized) => {
                let fresh = self.creds.refresh(&session).await?;
                match self.gql.execute(&fresh.caller()?, op, query, &variables).await {
                    Ok(data) => Ok((data, fresh)),
                    Err(TrackerError::Unauthorized) => {
                        self.creds.evict(org);
                        Err(TrackerError::Unauthorized)
                    }
                    Err(err) => Err(err),
                }
            }
            result => result.map(|data| (data, session)),
        }
    }

    /// Id of the team workflow state named `name` (case-insensitive), from a 1 h cache that is
    /// refetched when stale or when it does not know the name.
    async fn state_id(&self, org: OrgId, workspace_id: &str, team_id: &str, name: &str) -> Result<String, TrackerError> {
        let key = (workspace_id.to_owned(), team_id.to_owned());
        let now = self.clock.now();
        if let Some(cached) = self.team_states.lock().get(&key).filter(|c| now - c.at < STATES_TTL)
            && let Some(id) = find_state(&cached.states, name)
        {
            return Ok(id);
        }
        let (data, _) = self.call::<q::TeamStatesData>(org, "MageTeamStates", q::TEAM_STATES, json!({ "id": team_id })).await?;
        let team = data.team.ok_or(TrackerError::NotFound)?;
        let states: Vec<(String, String)> = team.states.nodes.into_iter().map(|s| (s.name, s.id)).collect();
        let found = find_state(&states, name);
        self.team_states.lock().insert(key, TeamStates { states, at: now });
        found.ok_or_else(|| TrackerError::Invalid(format!("the Linear team has no workflow state named {name:?}")))
    }
}

fn find_state(states: &[(String, String)], name: &str) -> Option<String> {
    let name = name.trim();
    states.iter().find(|(n, _)| n.trim().eq_ignore_ascii_case(name)).map(|(_, id)| id.clone())
}

fn parse_url(raw: &str, what: &str) -> Result<Url, TrackerError> {
    Url::parse(raw).map_err(|e| TrackerError::Unavailable(format!("Linear returned an invalid {what} URL: {e}")))
}

/// Linear attachment metadata is arbitrary JSON; strings stay as they are, other values keep
/// their JSON spelling, nulls are dropped.
fn flatten_metadata(metadata: Option<Map<String, Value>>) -> BTreeMap<String, String> {
    metadata
        .into_iter()
        .flatten()
        .filter_map(|(k, v)| match v {
            Value::Null => None,
            Value::String(s) => Some((k, s)),
            other => Some((k, other.to_string())),
        })
        .collect()
}

fn map_comment(c: q::Comment) -> TrackerComment {
    let author = match (c.bot_actor.and_then(|b| b.id), c.user) {
        (Some(bot), _) => Author::Bot { id: bot },
        (None, Some(user)) => Author::Human { id: user.id },
        // Neither a user nor an identified bot (e.g. an integration without actor id).
        (None, None) => Author::Bot { id: String::new() },
    };
    TrackerComment { id: c.id, author, body: c.body.unwrap_or_default(), created_at: c.created_at }
}

fn map_issue(issue: q::Issue, workspace_id: String) -> Result<TrackerIssue, TrackerError> {
    let state_kind = TrackerStateKind::parse(&issue.state.kind).ok_or_else(|| TrackerError::Invalid(format!("unknown Linear workflow state type {:?}", issue.state.kind)))?;
    let mut comments: Vec<TrackerComment> = issue.comments.nodes.into_iter().map(map_comment).collect();
    comments.sort_by_key(|c| c.created_at);
    let attachments = issue
        .attachments
        .nodes
        .into_iter()
        .filter_map(|a| match Url::parse(&a.url) {
            Ok(url) => Some(TrackerAttachment { url, title: a.title.unwrap_or_default(), source_kind: a.source_type, metadata: flatten_metadata(a.metadata) }),
            Err(err) => {
                tracing::warn!(issue = %issue.id, %err, "skipping Linear attachment with an invalid url");
                None
            }
        })
        .collect();
    let mut customer_ids: Vec<String> = Vec::new();
    for id in issue.needs.map(|n| n.nodes).unwrap_or_default().into_iter().filter_map(|n| n.customer.map(|c| c.id)) {
        if !customer_ids.contains(&id) {
            customer_ids.push(id);
        }
    }
    Ok(TrackerIssue {
        url: parse_url(&issue.url, "issue")?,
        issue_id: issue.id,
        identifier: issue.identifier,
        workspace_id,
        team_key: issue.team.key,
        project: issue.project.map(|p| p.name),
        labels: issue.labels.nodes.into_iter().map(|l| l.name).collect(),
        state_name: issue.state.name,
        state_kind,
        title: issue.title,
        description: issue.description.unwrap_or_default(),
        created_at: issue.created_at,
        updated_at: issue.updated_at,
        archived: issue.archived_at.is_some(),
        comments,
        attachments,
        customer_ids,
    })
}

fn posted(c: q::CommentRef, created: bool) -> Result<PostedComment, TrackerError> {
    let url = c.url.as_deref().map(|u| parse_url(u, "comment")).transpose()?;
    Ok(PostedComment { id: c.id, url, created })
}

fn require_success(success: bool, op: &str) -> Result<(), TrackerError> {
    if success { Ok(()) } else { Err(TrackerError::Unavailable(format!("Linear {op} reported success = false"))) }
}

fn scopes(scope: Option<&str>) -> Vec<String> {
    scope.unwrap_or("").split([' ', ',']).filter(|s| !s.is_empty()).map(str::to_owned).collect()
}

#[async_trait]
impl TicketTracker for LinearTracker {
    async fn get_issue(&self, org: OrgId, issue_id: &str) -> Result<TrackerIssue, TrackerError> {
        let (data, session) = self.call::<q::IssueData>(org, "MageIssue", q::ISSUE, json!({ "id": issue_id })).await?;
        map_issue(data.issue.ok_or(TrackerError::NotFound)?, session.workspace_id)
    }

    async fn updated_issues(&self, org: OrgId, query: &IssueQuery) -> Result<IssuePage, TrackerError> {
        // An empty team list accepts no team (intake.md "Configuration").
        if query.team_keys.is_empty() {
            return Ok(IssuePage { items: Vec::new(), next: None });
        }
        let since = query.updated_since.format(&Rfc3339).map_err(|e| TrackerError::Invalid(format!("updated_since: {e}")))?;
        let variables = json!({
            "teams": query.team_keys,
            "since": since,
            "first": query.page_size.clamp(1, MAX_PAGE_SIZE),
            "after": query.cursor,
        });
        let (data, _) = self.call::<q::IssuesData>(org, "MageUpdatedIssues", q::UPDATED_ISSUES, variables).await?;
        let issues = data.issues;
        Ok(IssuePage {
            items: issues.nodes.into_iter().map(|n| IssueStub { issue_id: n.id, updated_at: n.updated_at }).collect(),
            next: if issues.page_info.has_next_page { issues.page_info.end_cursor } else { None },
        })
    }

    async fn comment(&self, org: OrgId, issue_id: &str, c: &CommentDraft) -> Result<PostedComment, TrackerError> {
        let client_id = c.client_id.to_string();
        let input = json!({ "id": client_id, "issueId": issue_id, "body": c.body_markdown });
        match self.call::<q::CommentCreateData>(org, "MageCommentCreate", q::COMMENT_CREATE, json!({ "input": input })).await {
            Ok((data, _)) => {
                let payload = data.comment_create;
                require_success(payload.success, "commentCreate")?;
                posted(payload.comment.ok_or_else(|| TrackerError::Unavailable("Linear commentCreate returned no comment".into()))?, true)
            }
            // The duplicate-id error shape is unverified: on any conflict or input error, a
            // comment that already exists under `client_id` means an earlier attempt landed.
            Err(err @ (TrackerError::Conflict | TrackerError::Invalid(_))) => {
                match self.call::<q::CommentData>(org, "MageComment", q::COMMENT, json!({ "id": client_id })).await {
                    Ok((q::CommentData { comment: Some(existing) }, _)) => posted(existing, false),
                    Ok((q::CommentData { comment: None }, _)) | Err(TrackerError::NotFound) => Err(err),
                    Err(other) => Err(other),
                }
            }
            Err(err) => Err(err),
        }
    }

    async fn attach_link(&self, org: OrgId, issue_id: &str, link: &LinkAttachment) -> Result<(), TrackerError> {
        let input = json!({
            "issueId": issue_id,
            "url": link.url.as_str(),
            "title": link.title,
            "subtitle": link.subtitle,
            "metadata": link.metadata,
        });
        let (data, _) = self.call::<q::AttachmentCreateData>(org, "MageAttachmentCreate", q::ATTACHMENT_CREATE, json!({ "input": input })).await?;
        require_success(data.attachment_create.success, "attachmentCreate")
    }

    async fn set_state(&self, org: OrgId, issue_id: &str, state_name: &str) -> Result<(), TrackerError> {
        let (data, session) = self.call::<q::IssueStateData>(org, "MageIssueState", q::ISSUE_STATE, json!({ "id": issue_id })).await?;
        let issue = data.issue.ok_or(TrackerError::NotFound)?;
        if issue.state.name.trim().eq_ignore_ascii_case(state_name.trim()) {
            return Ok(());
        }
        let state_id = self.state_id(org, &session.workspace_id, &issue.team.id, state_name).await?;
        if state_id == issue.state.id {
            return Ok(());
        }
        let variables = json!({ "id": issue_id, "input": { "stateId": state_id } });
        let (data, _) = self.call::<q::IssueUpdateData>(org, "MageIssueUpdate", q::ISSUE_UPDATE, variables).await?;
        require_success(data.issue_update.success, "issueUpdate")
    }

    async fn upload_file(&self, org: OrgId, file: &TrackerUpload) -> Result<Url, TrackerError> {
        let size = i32::try_from(file.bytes.len()).map_err(|_| TrackerError::Invalid(format!("{} is too large for Linear", file.file_name)))?;
        let content_type = HeaderValue::from_str(&file.content_type).map_err(|_| TrackerError::Invalid(format!("invalid content type {:?}", file.content_type)))?;
        let variables = json!({ "contentType": file.content_type, "filename": file.file_name, "size": size });
        let (data, _) = self.call::<q::FileUploadData>(org, "MageFileUpload", q::FILE_UPLOAD, variables).await?;
        let payload = data.file_upload;
        require_success(payload.success, "fileUpload")?;
        let target = payload.upload_file.ok_or_else(|| TrackerError::Unavailable("Linear fileUpload returned no upload target".into()))?;
        let upload_url = parse_url(&target.upload_url, "upload")?;
        let asset_url = parse_url(&target.asset_url, "asset")?;

        let mut headers = HeaderMap::new();
        for h in &target.headers {
            let name = HeaderName::from_bytes(h.key.as_bytes()).map_err(|_| TrackerError::Unavailable(format!("Linear returned an invalid upload header name {:?}", h.key)))?;
            let value = HeaderValue::from_str(&h.value).map_err(|_| TrackerError::Unavailable(format!("Linear returned an invalid value for upload header {:?}", h.key)))?;
            headers.insert(name, value);
        }
        headers.insert(CONTENT_TYPE, content_type);
        headers.insert(CACHE_CONTROL, HeaderValue::from_static("public, max-age=31536000"));
        let response = self
            .http
            .put(upload_url)
            .headers(headers)
            .timeout(self.upload_timeout)
            .body(file.bytes.clone())
            .send()
            .await
            .map_err(|e| transport_error(&e))?;
        let status = response.status();
        if status.is_success() {
            Ok(asset_url)
        } else if status.is_client_error() && !matches!(status.as_u16(), 408 | 429) {
            Err(TrackerError::Invalid(format!("Linear storage refused the upload of {}: HTTP {status}", file.file_name)))
        } else {
            Err(TrackerError::Unavailable(format!("Linear storage upload failed: HTTP {status}")))
        }
    }

    async fn identity(&self, org: OrgId) -> Result<TrackerIdentity, TrackerError> {
        let now = self.clock.now();
        if let Some((identity, at)) = self.identities.lock().get(&org)
            && now - *at < IDENTITY_TTL
        {
            return Ok(identity.clone());
        }
        let session = self.creds.session(org).await?;
        let identity = match session.mode {
            LinearAuthMode::ApiKey => {
                let (data, _) = self.call::<q::ViewerData>(org, "MageViewer", q::VIEWER, json!({})).await?;
                TrackerIdentity { user_id: data.viewer.id, app_id: None }
            }
            LinearAuthMode::OAuth => {
                let client_id = self.creds.client_id()?;
                let (data, _) = self.call::<q::ViewerAppData>(org, "MageViewerApp", q::VIEWER_APP, json!({ "clientId": client_id })).await?;
                TrackerIdentity { user_id: data.viewer.id, app_id: data.application_info.map(|a| a.id) }
            }
        };
        self.identities.lock().insert(org, (identity.clone(), now));
        Ok(identity)
    }

    async fn complete_install(&self, code: &str, redirect_uri: &Url, pkce_verifier: &str) -> Result<TrackerGrant, TrackerError> {
        let client_id = self.creds.client_id()?;
        let client_secret = self.creds.client_secret().await?;
        let client_secret = client_secret.expose_str().ok_or_else(|| TrackerError::Invalid("Linear OAuth client secret is not UTF-8".into()))?;
        let form = [
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri.as_str()),
            ("client_id", client_id),
            ("client_secret", client_secret),
            ("code_verifier", pkce_verifier),
        ];
        let grant = self.creds.token_request(&form).await.map_err(|e| match e {
            TokenError::Rejected(detail) => TrackerError::Invalid(format!("Linear refused the OAuth code: {detail}")),
            TokenError::Tracker(err) => err,
        })?;
        let now = self.clock.now();
        let authorization = format!("Bearer {}", grant.access_token);
        let caller = Caller { workspace_id: INSTALL_BUCKET, mode: LinearAuthMode::OAuth, authorization: &authorization };
        let data: q::ViewerOrganizationData = self.gql.execute(&caller, "MageViewerOrganization", q::VIEWER_ORGANIZATION, &json!({})).await?;
        let organization = data.viewer.organization;
        Ok(TrackerGrant {
            workspace_id: organization.id,
            workspace_name: organization.name,
            scopes: scopes(grant.scope.as_deref()),
            access_token: AccessToken::new(grant.access_token),
            refresh_token: grant.refresh_token.map(AccessToken::new),
            expires_at: grant.expires_in.map(|secs| now + Duration::seconds(secs)),
        })
    }
}
