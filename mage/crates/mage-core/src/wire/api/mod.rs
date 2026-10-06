//! UI JSON API (`/api/v1`, session cookie + CSRF) and live updates (`/events`, SSE), per
//! server.md "Endpoint catalog" and "Live updates". Every type derives `ts_rs::TS`;
//! [`crate::wire::ts`] renders them, with the domain types they embed, into
//! `mage/web/src/gen/mage.ts`.
//!
//! Responses reuse domain types where the UI shows them as they are (`Ticket`, `Cause`, `Run`,
//! `Gate`, `RunEvent`, `Repo`, `Recipe`, `Customer`, `Deployment`, `Integration`, `Resolution`,
//! `FixSettings`, …); views exist only where a route adds derived data or must hide internals
//! (blob keys, content hashes). `…` below is `/api/v1/orgs/{org}` with the org's immutable slug.
//! Errors are [`ApiErrorBody`]; "—" means no body (204 for responses).
//!
//! | Route ([`routes`]) | Request | Response |
//! |---|---|---|
//! | `GET SESSION` | — | [`SessionView`] |
//! | `GET RESOLVE` | — | [`ResolvedId`] |
//! | `POST ORGS` | [`CreateOrgRequest`] | `Org` |
//! | `GET TICKETS` | query [`TicketListQuery`] | [`Paged`]`<`[`TicketSummary`]`>` |
//! | `GET TICKET` | — | [`TicketDetail`] |
//! | `POST TICKET_RETRIAGE` | — | [`JobQueued`] |
//! | `PUT TICKET_TARGET` | [`SetTargetRequest`] | `TicketTarget` |
//! | `POST TICKET_RESOLVE` | [`ResolveTicketRequest`] | `Resolution` |
//! | `POST CAUSE_DIAGNOSE`, `POST CAUSE_FIX` | [`StartHarnessRunRequest`] | [`RunStarted`] |
//! | `GET CAUSE_FIX_BRIEF` | query [`FixBriefQuery`] | [`FixBriefView`] |
//! | `POST CAUSE_PULL_REQUEST` | — | [`JobQueued`] |
//! | `GET RUNS` | query [`RunListQuery`] | [`Paged`]`<`[`RunSummary`]`>` |
//! | `GET RUN` | — | [`RunDetail`] |
//! | `GET RUN_EVENTS` | query [`RunEventsQuery`] | [`RunEventPage`] |
//! | `POST RUN_STEER` | [`SteerRequest`] | [`CommandQueued`] |
//! | `POST RUN_CANCEL` | — | [`RunStatusView`] |
//! | `GET RUN_ARTIFACTS` | — | [`ArtifactList`] |
//! | `GET ARTIFACT` | — | [`ArtifactView`] |
//! | `GET ARTIFACT_CONTENT` | `Range` header | bytes |
//! | `POST ARTIFACT_SHARE` | — | [`ShareLink`] |
//! | `GET REPOS` | — | [`RepoList`] |
//! | `GET REPO` | — | [`RepoDetail`] |
//! | `DELETE REPO` | — | — |
//! | `GET GITHUB_REPOSITORIES` | — | [`InstallationRepoList`] |
//! | `POST REPOS` | [`ConnectRepoRequest`] | `Repo` |
//! | `POST REPO_VERSIONS` | [`IndexVersionRequest`] | [`JobQueued`] |
//! | `POST REPO_LEARN` | [`StartLearnRequest`] | [`RunStarted`] |
//! | `GET REPO_RECIPES` | — | [`RecipeHistory`] |
//! | `PUT REPO_RECIPE` | [`SaveRecipeRequest`] | [`SavedRecipe`] |
//! | `POST REPO_RECIPE_VERIFY` | [`VerifyRecipeRequest`] | [`RunStarted`] |
//! | `GET REPO_DEPLOYMENTS` | — | [`DeploymentList`] |
//! | `PUT REPO_DEPLOYMENT` | [`SetDeploymentRequest`] | `Deployment` |
//! | `GET CUSTOMERS` | — | [`CustomerList`] |
//! | `POST CUSTOMERS`, `PUT CUSTOMER` | [`CustomerInput`] | `Customer` |
//! | `GET SETTINGS`, `PUT SETTINGS` | — / [`OrgSettings`] | [`OrgSettings`] |
//! | `GET SETTINGS_MODEL` | — | [`ModelSettingsView`] |
//! | `PUT SETTINGS_MODEL` | [`ModelSettingsUpdate`] | [`ModelSettingsView`] |
//! | `PUT SETTINGS_MODEL_KEY` | [`SetModelKeyRequest`] | [`ModelKeyMeta`] |
//! | `DELETE SETTINGS_MODEL_KEY` | — | — |
//! | `GET INTEGRATIONS` | — | [`IntegrationList`] |
//! | `POST INTEGRATION_CONNECT` | — | [`ConnectIntegrationResponse`] |
//! | `DELETE INTEGRATION` | — | — |
//! | `GET MEMBERS` | — | [`MemberList`] |
//! | `POST MEMBER_INVITES` | [`InviteRequest`] | [`InviteView`] |
//! | `DELETE MEMBER_INVITE` | — | — |
//! | `PUT MEMBER` | [`SetRoleRequest`] | [`MemberView`] |
//! | `DELETE MEMBER` | — | — |
//! | `GET AUDIT` | query [`AuditQuery`] | [`AuditPage`] |
//! | `GET USAGE` | query [`UsageQuery`] | [`UsageReport`] |
//! | SSE `EVENTS_RUN` | `Last-Event-ID` / `?after=` | event [`sse::RUN`]: `RunEvent` |
//! | SSE `EVENTS_FEED` | `Last-Event-ID` | event [`sse::FEED`]: [`FeedEvent`] |

mod members;
mod repos;
mod runs;
mod session;
mod settings;
mod tickets;

pub use members::*;
pub use repos::*;
pub use runs::*;
pub use session::*;
pub use settings::*;
pub use tickets::*;

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::domain::{JobId, RunId};

/// Route templates (axum `{name}` placeholders; fill with [`crate::wire::fill`]). Also rendered
/// into the generated TypeScript as `routes`.
pub mod routes {
    pub const SESSION: &str = "/api/v1/session";
    pub const RESOLVE: &str = "/api/v1/resolve/{id}";
    pub const ORGS: &str = "/api/v1/orgs";

    pub const TICKETS: &str = "/api/v1/orgs/{org}/tickets";
    pub const TICKET: &str = "/api/v1/orgs/{org}/tickets/{ticket}";
    pub const TICKET_RETRIAGE: &str = "/api/v1/orgs/{org}/tickets/{ticket}/retriage";
    pub const TICKET_TARGET: &str = "/api/v1/orgs/{org}/tickets/{ticket}/target";
    pub const TICKET_RESOLVE: &str = "/api/v1/orgs/{org}/tickets/{ticket}/resolve";
    pub const CAUSE_DIAGNOSE: &str = "/api/v1/orgs/{org}/tickets/{ticket}/causes/{cause}/diagnose";
    pub const CAUSE_FIX_BRIEF: &str = "/api/v1/orgs/{org}/tickets/{ticket}/causes/{cause}/fix-brief";
    pub const CAUSE_FIX: &str = "/api/v1/orgs/{org}/tickets/{ticket}/causes/{cause}/fix";
    pub const CAUSE_PULL_REQUEST: &str = "/api/v1/orgs/{org}/tickets/{ticket}/causes/{cause}/pull-request";

    pub const RUNS: &str = "/api/v1/orgs/{org}/runs";
    pub const RUN: &str = "/api/v1/orgs/{org}/runs/{run}";
    pub const RUN_EVENTS: &str = "/api/v1/orgs/{org}/runs/{run}/events";
    pub const RUN_STEER: &str = "/api/v1/orgs/{org}/runs/{run}/steer";
    pub const RUN_CANCEL: &str = "/api/v1/orgs/{org}/runs/{run}/cancel";
    pub const RUN_ARTIFACTS: &str = "/api/v1/orgs/{org}/runs/{run}/artifacts";
    pub const ARTIFACT: &str = "/api/v1/orgs/{org}/artifacts/{artifact}";
    pub const ARTIFACT_CONTENT: &str = "/api/v1/orgs/{org}/artifacts/{artifact}/content";
    pub const ARTIFACT_SHARE: &str = "/api/v1/orgs/{org}/artifacts/{artifact}/share";

    pub const REPOS: &str = "/api/v1/orgs/{org}/repos";
    pub const REPO: &str = "/api/v1/orgs/{org}/repos/{repo}";
    pub const GITHUB_REPOSITORIES: &str = "/api/v1/orgs/{org}/integrations/github/repositories";
    pub const REPO_VERSIONS: &str = "/api/v1/orgs/{org}/repos/{repo}/versions";
    pub const REPO_LEARN: &str = "/api/v1/orgs/{org}/repos/{repo}/learn";
    pub const REPO_RECIPES: &str = "/api/v1/orgs/{org}/repos/{repo}/recipes";
    pub const REPO_RECIPE: &str = "/api/v1/orgs/{org}/repos/{repo}/recipe";
    pub const REPO_RECIPE_VERIFY: &str = "/api/v1/orgs/{org}/repos/{repo}/recipe/verify";
    pub const REPO_DEPLOYMENTS: &str = "/api/v1/orgs/{org}/repos/{repo}/deployments";
    pub const REPO_DEPLOYMENT: &str = "/api/v1/orgs/{org}/repos/{repo}/deployments/{environment}";
    pub const CUSTOMERS: &str = "/api/v1/orgs/{org}/customers";
    pub const CUSTOMER: &str = "/api/v1/orgs/{org}/customers/{customer}";

    pub const SETTINGS: &str = "/api/v1/orgs/{org}/settings";
    pub const SETTINGS_MODEL: &str = "/api/v1/orgs/{org}/settings/model";
    pub const SETTINGS_MODEL_KEY: &str = "/api/v1/orgs/{org}/settings/model/key";
    pub const INTEGRATIONS: &str = "/api/v1/orgs/{org}/integrations";
    /// `{kind}` is an [`super::integration_slug`].
    pub const INTEGRATION_CONNECT: &str = "/api/v1/orgs/{org}/integrations/{kind}/connect";
    pub const INTEGRATION: &str = "/api/v1/orgs/{org}/integrations/{kind}/{external_id}";

    pub const MEMBERS: &str = "/api/v1/orgs/{org}/members";
    pub const MEMBER_INVITES: &str = "/api/v1/orgs/{org}/members/invites";
    pub const MEMBER_INVITE: &str = "/api/v1/orgs/{org}/members/invites/{invite}";
    pub const MEMBER: &str = "/api/v1/orgs/{org}/members/{user}";
    pub const AUDIT: &str = "/api/v1/orgs/{org}/audit";
    pub const USAGE: &str = "/api/v1/orgs/{org}/usage";

    /// SSE: org feed ([`super::FeedEvent`]); `Last-Event-ID` = feed id.
    pub const EVENTS_FEED: &str = "/events/orgs/{org}/feed";
    /// SSE: run events (`RunEvent`); `Last-Event-ID` = `seq`.
    pub const EVENTS_RUN: &str = "/events/orgs/{org}/runs/{run}";

    /// Login entry points and logout (not JSON; linked from the UI).
    pub const AUTH_GITHUB_LOGIN: &str = "/auth/github/login";
    pub const AUTH_GOOGLE_LOGIN: &str = "/auth/google/login";
    pub const AUTH_LOGOUT: &str = "/auth/logout";

    /// Every route with its constant name (rendered into TypeScript in this order).
    pub const ALL: &[(&str, &str)] = &[
        ("SESSION", SESSION),
        ("RESOLVE", RESOLVE),
        ("ORGS", ORGS),
        ("TICKETS", TICKETS),
        ("TICKET", TICKET),
        ("TICKET_RETRIAGE", TICKET_RETRIAGE),
        ("TICKET_TARGET", TICKET_TARGET),
        ("TICKET_RESOLVE", TICKET_RESOLVE),
        ("CAUSE_DIAGNOSE", CAUSE_DIAGNOSE),
        ("CAUSE_FIX_BRIEF", CAUSE_FIX_BRIEF),
        ("CAUSE_FIX", CAUSE_FIX),
        ("CAUSE_PULL_REQUEST", CAUSE_PULL_REQUEST),
        ("RUNS", RUNS),
        ("RUN", RUN),
        ("RUN_EVENTS", RUN_EVENTS),
        ("RUN_STEER", RUN_STEER),
        ("RUN_CANCEL", RUN_CANCEL),
        ("RUN_ARTIFACTS", RUN_ARTIFACTS),
        ("ARTIFACT", ARTIFACT),
        ("ARTIFACT_CONTENT", ARTIFACT_CONTENT),
        ("ARTIFACT_SHARE", ARTIFACT_SHARE),
        ("REPOS", REPOS),
        ("REPO", REPO),
        ("GITHUB_REPOSITORIES", GITHUB_REPOSITORIES),
        ("REPO_VERSIONS", REPO_VERSIONS),
        ("REPO_LEARN", REPO_LEARN),
        ("REPO_RECIPES", REPO_RECIPES),
        ("REPO_RECIPE", REPO_RECIPE),
        ("REPO_RECIPE_VERIFY", REPO_RECIPE_VERIFY),
        ("REPO_DEPLOYMENTS", REPO_DEPLOYMENTS),
        ("REPO_DEPLOYMENT", REPO_DEPLOYMENT),
        ("CUSTOMERS", CUSTOMERS),
        ("CUSTOMER", CUSTOMER),
        ("SETTINGS", SETTINGS),
        ("SETTINGS_MODEL", SETTINGS_MODEL),
        ("SETTINGS_MODEL_KEY", SETTINGS_MODEL_KEY),
        ("INTEGRATIONS", INTEGRATIONS),
        ("INTEGRATION_CONNECT", INTEGRATION_CONNECT),
        ("INTEGRATION", INTEGRATION),
        ("MEMBERS", MEMBERS),
        ("MEMBER_INVITES", MEMBER_INVITES),
        ("MEMBER_INVITE", MEMBER_INVITE),
        ("MEMBER", MEMBER),
        ("AUDIT", AUDIT),
        ("USAGE", USAGE),
        ("EVENTS_FEED", EVENTS_FEED),
        ("EVENTS_RUN", EVENTS_RUN),
        ("AUTH_GITHUB_LOGIN", AUTH_GITHUB_LOGIN),
        ("AUTH_GOOGLE_LOGIN", AUTH_GOOGLE_LOGIN),
        ("AUTH_LOGOUT", AUTH_LOGOUT),
    ];
}

/// SSE `event:` names (server.md "Live updates").
pub mod sse {
    /// Run stream: `data` is a `RunEvent`, `id` its `seq`.
    pub const RUN: &str = "run";
    /// Org feed: `data` is a [`super::FeedEvent`], `id` the feed id.
    pub const FEED: &str = "feed";
    /// Server rotation or shutdown: reconnect with `Last-Event-ID`.
    pub const RECONNECT: &str = "reconnect";
    /// Run terminal and every event sent: close.
    pub const END: &str = "end";

    pub const ALL: &[(&str, &str)] = &[("RUN", RUN), ("FEED", FEED), ("RECONNECT", RECONNECT), ("END", END)];
}

// ---------------------------------------------------------------------------------------------
// Errors and common shapes
// ---------------------------------------------------------------------------------------------

/// `error.code` of every `/api`, `/events` and `/auth` error (server.md "Error formats").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ApiErrorCode {
    /// Unknown id, another org's id, or an org the caller is not a member of.
    NotFound,
    Unauthenticated,
    Forbidden,
    Conflict,
    Invalid,
    TooLarge,
    RateLimited,
    Unavailable,
    Internal,
}

impl ApiErrorCode {
    pub const fn http_status(self) -> u16 {
        match self {
            ApiErrorCode::NotFound => 404,
            ApiErrorCode::Unauthenticated => 401,
            ApiErrorCode::Forbidden => 403,
            ApiErrorCode::Conflict => 409,
            ApiErrorCode::Invalid => 422,
            ApiErrorCode::TooLarge => 413,
            ApiErrorCode::RateLimited => 429,
            ApiErrorCode::Unavailable => 503,
            ApiErrorCode::Internal => 500,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ApiError {
    pub code: ApiErrorCode,
    pub message: String,
    /// Shown by the UI so support can find the request.
    pub request_id: String,
}

/// `{"error": {"code": "<snake_case>", "message": "…", "request_id": "…"}}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ApiErrorBody {
    pub error: ApiError,
}

/// One page of a list; pass `next_cursor` back as `cursor` for the next page.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct Paged<T> {
    pub items: Vec<T>,
    pub next_cursor: Option<String>,
}

/// A use case created a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct RunStarted {
    pub run_id: RunId,
}

/// A use case enqueued a job (retriage, on-demand index, open PR); progress arrives through the
/// org feed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct JobQueued {
    pub job_id: JobId,
}

// ---------------------------------------------------------------------------------------------
// Org feed (SSE)
// ---------------------------------------------------------------------------------------------

/// What changed; the UI refetches through `/api/v1`, so RBAC applies to all data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum FeedKind {
    Ticket,
    Cause,
    Run,
    Repo,
    Recipe,
    Integration,
    Settings,
    Member,
}

/// `data` of an org-feed SSE event: ids only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct FeedEvent {
    pub kind: FeedKind,
    /// Prefixed id of the changed subject (`tkt_…`, `run_…`, …).
    pub subject: String,
}

#[cfg(test)]
mod tests;
