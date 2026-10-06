//! `PgTicketStore` (storage.md "`TicketStore`"; triage.md, fix.md, knowledge.md additions) over
//! `tickets`, `ticket_thread_snapshots`, `causes`, `cause_citations`, `fix_briefs`,
//! `fix_pull_requests` and `resolutions`. Every method runs on the tenant pool inside
//! `begin_org`; mutating methods write state, outbox jobs and the org feed entry in one
//! transaction (feed kinds, subjects and order as `MemTicketStore`).
//!
//! Column mapping: `TicketSource::Linear` ↔ `source = 'linear'` + `source_issue_id` /
//! `source_identifier` / `source_url`; `slack_thread` ↔ `slack_team` / `slack_channel` /
//! `slack_ts`; `target` ↔ `target_repo_id` / `target_sha` / `target_origin` (+
//! `target_environment` for `Deployment`); `CauseStatus` ↔ `status` / `status_run` /
//! `status_gate` / `status_reason`; `FixPrState` ↔ `state` / `head_commit` (`CommitCreated`) /
//! `pr_number` (`Opened`) / `merged` (`Closed`). `causes.confidence` is `real` (bound as `f32`).
//! `updated_at` comes from the clock on every store-side change; `save_ingested` writes the
//! ticket's own timestamps.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use mage_core::domain::fix::{BranchName, ChangeSetDigest, FixPrState, FixPullRequest};
use mage_core::domain::{
    ArtifactId, Cause, CauseData, CauseId, CauseStatus, Citation, Confidence, ContentHash, CustomerId, FixBrief, FixBriefId, FixPullRequestId,
    GateId, LineRange, OrgId, RepoId, RepoPath, ReproMethod, Resolution, RunId, SlackThreadRef, TargetOrigin, Ticket, TicketData, TicketId,
    TicketSource, TicketStatus, TicketTarget, UserId,
};
use mage_core::ports::clock::Clock;
use mage_core::ports::common::{Page, PageRequest, Versioned};
use mage_core::ports::ids::IdGen;
use mage_core::ports::jobs::NewJob;
use mage_infra::pg::jobs::enqueue_all_in;
use mage_infra::pg::{DbError, PgPools, TxError, begin_org, retry_tx};
use sqlx::PgConnection;
use sqlx::types::Json;
use time::OffsetDateTime;
use url::Url;
use uuid::Uuid;

use super::codec::{finish_page, fixed, opt_sha, page_limit, parse_enum, sha, to_i64, to_unsigned, uuid_cursor};
use super::feed::append_in;
use crate::domain::intake::ThreadSnapshot;
use crate::ports::feed::FeedKind;
use crate::ports::ticket::{
    SIMILAR_LIMIT_MAX, SimilarMatch, SimilarQuery, SimilarResolved, TicketFilter, TicketStore, TicketStoreError, TicketTransition, TriageWrite,
    check_pull_request_payload, error_overlap, error_tokens, similarity_score,
};

/// Candidate rows `similar_resolved` scores in Rust (newest resolutions first).
const SIMILAR_CANDIDATES: i64 = 500;

macro_rules! ticket_cols {
    () => {
        "id, source, source_issue_id, source_identifier, source_url, title, body, content_hash, triaged_hash, triage_run, \
         slack_team, slack_channel, slack_ts, customer_id, target_repo_id, target_sha, target_origin, target_environment, \
         error_strings, status, rev, created_at, updated_at"
    };
}

/// Written by `save_ingested`, bound by [`bind_ticket`] as `$3..=$23`.
macro_rules! ticket_write_cols {
    () => {
        "source, source_issue_id, source_identifier, source_url, title, body, content_hash, triaged_hash, triage_run, \
         slack_team, slack_channel, slack_ts, customer_id, target_repo_id, target_sha, target_origin, target_environment, \
         error_strings, status, created_at, updated_at"
    };
}

macro_rules! cause_cols {
    () => {
        "id, ticket_id, triage_run, rank, title, explanation, repro_steps, confirm_if, rule_out_if, confidence, method, \
         status, status_run, status_gate, status_reason, superseded_at"
    };
}

macro_rules! pr_cols {
    () => {
        "id, ticket_id, fix_run_id, gate_id, changeset_digest, branch, base_sha, head_commit, pr_number, pr_url, state, merged, \
         created_at, updated_at"
    };
}

macro_rules! brief_cols {
    () => {
        "id, cause_id, version, diagnose_run_id, artifact_id, markdown, created_at"
    };
}

pub struct PgTicketStore {
    pools: PgPools,
    clock: Arc<dyn Clock>,
    ids: Arc<dyn IdGen>,
}

impl PgTicketStore {
    pub fn new(pools: PgPools, clock: Arc<dyn Clock>, ids: Arc<dyn IdGen>) -> Self {
        Self { pools, clock, ids }
    }
}

impl From<DbError> for TicketStoreError {
    fn from(err: DbError) -> Self {
        match err {
            DbError::NotFound => Self::NotFound,
            DbError::Conflict { .. } => Self::Conflict,
            DbError::Invalid(msg) => Self::Invalid(msg),
            DbError::Unavailable(msg) | DbError::Retry(msg) => Self::Unavailable(msg),
            DbError::Timeout => Self::Unavailable("statement timeout".into()),
        }
    }
}

type TxResult<T> = Result<T, TxError<TicketStoreError>>;

fn port<T>(err: TicketStoreError) -> TxResult<T> {
    Err(TxError::Port(err))
}

/// The checks every outbox insert runs, before anything is read or written (as the fake).
fn check_jobs(jobs: &[NewJob]) -> Result<(), TicketStoreError> {
    jobs.iter().try_for_each(NewJob::validate).map_err(|e| TicketStoreError::Invalid(e.to_string()))
}

// ---- enum codecs ----

const TICKET_STATUSES: [TicketStatus; 11] = [
    TicketStatus::New,
    TicketStatus::Triaging,
    TicketStatus::Triaged,
    TicketStatus::TriageFailed,
    TicketStatus::Diagnosing,
    TicketStatus::Diagnosed,
    TicketStatus::Fixing,
    TicketStatus::FixReady,
    TicketStatus::PrOpen,
    TicketStatus::Resolved,
    TicketStatus::Closed,
];

fn parse_status(s: &str) -> Result<TicketStatus, DbError> {
    parse_enum(s, "tickets.status", |s| TICKET_STATUSES.into_iter().find(|t| t.name() == s))
}

const METHODS: [ReproMethod; 4] = [ReproMethod::Ui, ReproMethod::Api, ReproMethod::Job, ReproMethod::Unknown];

fn method_str(m: ReproMethod) -> &'static str {
    match m {
        ReproMethod::Ui => "ui",
        ReproMethod::Api => "api",
        ReproMethod::Job => "job",
        ReproMethod::Unknown => "unknown",
    }
}

fn parse_method(s: &str) -> Result<ReproMethod, DbError> {
    parse_enum(s, "causes.method", |s| METHODS.into_iter().find(|m| method_str(*m) == s))
}

/// `(target_origin, target_environment)`.
fn origin_cols(origin: &TargetOrigin) -> (&'static str, Option<String>) {
    match origin {
        TargetOrigin::Override => ("override", None),
        TargetOrigin::Directive => ("directive", None),
        TargetOrigin::Deployment { environment } => ("deployment", Some(environment.clone())),
        TargetOrigin::DefaultHead => ("default_head", None),
    }
}

fn parse_origin(origin: &str, environment: Option<String>) -> Result<TargetOrigin, DbError> {
    Ok(match (origin, environment) {
        ("override", _) => TargetOrigin::Override,
        ("directive", _) => TargetOrigin::Directive,
        ("deployment", Some(environment)) => TargetOrigin::Deployment { environment },
        ("default_head", _) => TargetOrigin::DefaultHead,
        (other, _) => return Err(DbError::Invalid(format!("tickets.target_origin: {other:?} is unknown or lacks its environment"))),
    })
}

/// `(status, status_run, status_gate, status_reason)`.
fn cause_status_cols(s: &CauseStatus) -> (&'static str, Option<Uuid>, Option<Uuid>, Option<String>) {
    match s {
        CauseStatus::Untested => ("untested", None, None, None),
        CauseStatus::Running(run) => ("running", Some(run.as_uuid()), None, None),
        CauseStatus::Reproduced { run, gate } => ("reproduced", Some(run.as_uuid()), Some(gate.as_uuid()), None),
        CauseStatus::NotReproduced { run } => ("not_reproduced", Some(run.as_uuid()), None, None),
        CauseStatus::Inconclusive { run, reason } => ("inconclusive", Some(run.as_uuid()), None, Some(reason.clone())),
    }
}

fn parse_cause_status(status: &str, run: Option<Uuid>, gate: Option<Uuid>, reason: Option<String>) -> Result<CauseStatus, DbError> {
    let run = run.map(RunId::from_uuid);
    Ok(match (status, run, gate, reason) {
        ("untested", None, _, _) => CauseStatus::Untested,
        ("running", Some(run), _, _) => CauseStatus::Running(run),
        ("reproduced", Some(run), Some(gate), _) => CauseStatus::Reproduced { run, gate: GateId::from_uuid(gate) },
        ("not_reproduced", Some(run), _, _) => CauseStatus::NotReproduced { run },
        ("inconclusive", Some(run), _, Some(reason)) => CauseStatus::Inconclusive { run, reason },
        (other, ..) => return Err(DbError::Invalid(format!("causes.status: {other:?} does not match its run / gate / reason columns"))),
    })
}

/// `merged` column of a state (`Closed` only).
fn merged_col(s: &FixPrState) -> Option<bool> {
    match s {
        FixPrState::Closed { merged } => Some(*merged),
        _ => None,
    }
}

fn parse_url(s: &str, column: &str) -> Result<Url, DbError> {
    Url::parse(s).map_err(|e| DbError::Invalid(format!("{column}: {e}")))
}

// ---- tickets ----

#[derive(sqlx::FromRow)]
struct TicketRow {
    id: Uuid,
    source: String,
    source_issue_id: String,
    source_identifier: String,
    source_url: String,
    title: String,
    body: String,
    content_hash: Vec<u8>,
    triaged_hash: Option<Vec<u8>>,
    triage_run: Option<Uuid>,
    slack_team: Option<String>,
    slack_channel: Option<String>,
    slack_ts: Option<String>,
    customer_id: Option<Uuid>,
    target_repo_id: Option<Uuid>,
    target_sha: Option<Vec<u8>>,
    target_origin: Option<String>,
    target_environment: Option<String>,
    error_strings: Vec<String>,
    status: String,
    rev: i64,
    created_at: OffsetDateTime,
    updated_at: OffsetDateTime,
}

fn ticket_from(org: OrgId, r: TicketRow) -> Result<Versioned<Ticket>, DbError> {
    if r.source != "linear" {
        return Err(DbError::Invalid(format!("tickets.source: unknown value {:?}", r.source)));
    }
    let source = TicketSource::Linear {
        issue_id: r.source_issue_id,
        identifier: r.source_identifier,
        url: parse_url(&r.source_url, "tickets.source_url")?,
    };
    let slack_thread = match (r.slack_team, r.slack_channel, r.slack_ts) {
        (Some(team), Some(channel), Some(ts)) => Some(SlackThreadRef { team, channel, ts }),
        (None, None, None) => None,
        _ => return Err(DbError::Invalid("tickets.slack_*: partially set".into())),
    };
    let target = match (r.target_repo_id, r.target_sha, r.target_origin) {
        (Some(repo), Some(sha_bytes), Some(origin)) => Some(TicketTarget {
            repo_id: RepoId::from_uuid(repo),
            sha: sha(sha_bytes, "tickets.target_sha")?,
            origin: parse_origin(&origin, r.target_environment)?,
        }),
        (None, None, None) => None,
        _ => return Err(DbError::Invalid("tickets.target_*: partially set".into())),
    };
    let data = TicketData {
        id: TicketId::from_uuid(r.id),
        org_id: org,
        source,
        title: r.title,
        body: r.body,
        content_hash: ContentHash::from_bytes(fixed(r.content_hash, "tickets.content_hash")?),
        slack_thread,
        customer: r.customer_id.map(CustomerId::from_uuid),
        target,
        error_strings: r.error_strings,
        status: parse_status(&r.status)?,
        triaged_hash: r.triaged_hash.map(|h| fixed(h, "tickets.triaged_hash").map(ContentHash::from_bytes)).transpose()?,
        triage_run: r.triage_run.map(RunId::from_uuid),
        created_at: r.created_at,
        updated_at: r.updated_at,
    };
    Ok(Versioned { value: Ticket::rehydrate(data), rev: to_unsigned(r.rev, "tickets.rev")? })
}

type PgQuery<'q> = sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments>;

/// Binds the `ticket_write_cols!()` values (owned) in order.
fn bind_ticket<'q>(q: PgQuery<'q>, t: &Ticket) -> PgQuery<'q> {
    let TicketSource::Linear { issue_id, identifier, url } = &t.source;
    let slack = t.slack_thread.as_ref();
    let origin = t.target.as_ref().map(|x| origin_cols(&x.origin));
    q.bind("linear")
        .bind(issue_id.clone())
        .bind(identifier.clone())
        .bind(url.to_string())
        .bind(t.title.clone())
        .bind(t.body.clone())
        .bind(t.content_hash.as_bytes().to_vec())
        .bind(t.triaged_hash.map(|h| h.as_bytes().to_vec()))
        .bind(t.triage_run.map(|r| r.as_uuid()))
        .bind(slack.map(|s| s.team.clone()))
        .bind(slack.map(|s| s.channel.clone()))
        .bind(slack.map(|s| s.ts.clone()))
        .bind(t.customer.map(|c| c.as_uuid()))
        .bind(t.target.as_ref().map(|x| x.repo_id.as_uuid()))
        .bind(t.target.as_ref().map(|x| x.sha.as_bytes().to_vec()))
        .bind(origin.as_ref().map(|(o, _)| *o))
        .bind(origin.and_then(|(_, e)| e))
        .bind(t.error_strings.clone())
        .bind(t.status.name())
        .bind(t.created_at)
        .bind(t.updated_at)
}

async fn ticket_exists(conn: &mut PgConnection, org: OrgId, id: TicketId) -> Result<bool, DbError> {
    let found: Option<i32> = sqlx::query_scalar("SELECT 1 FROM tickets WHERE org_id = $1 AND id = $2")
        .bind(org.as_uuid())
        .bind(id.as_uuid())
        .fetch_optional(&mut *conn)
        .await?;
    Ok(found.is_some())
}

/// A CAS miss: `Conflict` when the ticket exists, `NotFound` otherwise.
async fn ticket_miss<T>(conn: &mut PgConnection, org: OrgId, id: TicketId) -> TxResult<T> {
    port(if ticket_exists(conn, org, id).await? { TicketStoreError::Conflict } else { TicketStoreError::NotFound })
}

/// Status CAS `expect → next` (rev and `updated_at` follow); `None` on a miss.
async fn cas_status(
    conn: &mut PgConnection,
    org: OrgId,
    id: TicketId,
    expect: TicketStatus,
    next: TicketStatus,
    now: OffsetDateTime,
) -> Result<Option<Ticket>, DbError> {
    let row: Option<TicketRow> = sqlx::query_as(concat!(
        "UPDATE tickets SET status = $4, updated_at = $5, rev = rev + 1 WHERE org_id = $1 AND id = $2 AND status = $3 RETURNING ",
        ticket_cols!()
    ))
    .bind(org.as_uuid())
    .bind(id.as_uuid())
    .bind(expect.name())
    .bind(next.name())
    .bind(now)
    .fetch_optional(&mut *conn)
    .await?;
    row.map(|r| ticket_from(org, r).map(|v| v.value)).transpose()
}

/// The optional ticket CAS of cause / PR writes, with its feed entry.
async fn apply_transition(conn: &mut PgConnection, org: OrgId, id: TicketId, t: Option<TicketTransition>, now: OffsetDateTime) -> TxResult<()> {
    if let Some(t) = t {
        if cas_status(conn, org, id, t.expect, t.next, now).await?.is_none() {
            return ticket_miss(conn, org, id).await;
        }
        append_in(conn, org, FeedKind::Ticket, &id.to_string(), now).await?;
    }
    Ok(())
}

// ---- causes ----

#[derive(sqlx::FromRow)]
struct CauseRow {
    id: Uuid,
    ticket_id: Uuid,
    triage_run: Uuid,
    rank: i16,
    title: String,
    explanation: String,
    repro_steps: Vec<String>,
    confirm_if: String,
    rule_out_if: String,
    confidence: f32,
    method: String,
    status: String,
    status_run: Option<Uuid>,
    status_gate: Option<Uuid>,
    status_reason: Option<String>,
    superseded_at: Option<OffsetDateTime>,
}

#[derive(sqlx::FromRow)]
struct CitationRow {
    cause_id: Uuid,
    repo_id: Uuid,
    sha: Vec<u8>,
    path: String,
    line_start: i32,
    line_end: i32,
    snippet_hash: Option<Vec<u8>>,
    note: Option<String>,
}

fn citation_from(r: CitationRow) -> Result<Citation, DbError> {
    let invalid = |what: &str, e: mage_core::domain::DomainError| DbError::Invalid(format!("cause_citations.{what}: {e}"));
    Ok(Citation {
        repo_id: RepoId::from_uuid(r.repo_id),
        sha: sha(r.sha, "cause_citations.sha")?,
        path: RepoPath::new(r.path).map_err(|e| invalid("path", e))?,
        lines: LineRange::new(to_unsigned(i64::from(r.line_start), "cause_citations.line_start")?, to_unsigned(i64::from(r.line_end), "cause_citations.line_end")?)
            .map_err(|e| invalid("lines", e))?,
        snippet_hash: r.snippet_hash.map(|h| fixed(h, "cause_citations.snippet_hash")).transpose()?,
        note: r.note,
    })
}

fn cause_from(org: OrgId, r: CauseRow, evidence: Vec<Citation>) -> Result<Cause, DbError> {
    Ok(Cause::rehydrate(CauseData {
        id: CauseId::from_uuid(r.id),
        ticket_id: TicketId::from_uuid(r.ticket_id),
        org_id: org,
        triage_run: RunId::from_uuid(r.triage_run),
        rank: to_unsigned(i64::from(r.rank), "causes.rank")?,
        title: r.title,
        explanation: r.explanation,
        evidence,
        repro_steps: r.repro_steps,
        confirm_if: r.confirm_if,
        rule_out_if: r.rule_out_if,
        confidence: Confidence::new(f64::from(r.confidence)).map_err(|e| DbError::Invalid(format!("causes.confidence: {e}")))?,
        method: parse_method(&r.method)?,
        status: parse_cause_status(&r.status, r.status_run, r.status_gate, r.status_reason)?,
        superseded_at: r.superseded_at,
    }))
}

/// Citations of `causes`, each list in `ord` order.
async fn load_citations(conn: &mut PgConnection, org: OrgId, causes: &[Uuid]) -> Result<HashMap<Uuid, Vec<Citation>>, DbError> {
    let rows: Vec<CitationRow> = sqlx::query_as(
        "SELECT cause_id, repo_id, sha, path, line_start, line_end, snippet_hash, note FROM cause_citations \
         WHERE org_id = $1 AND cause_id = ANY($2) ORDER BY cause_id, ord",
    )
    .bind(org.as_uuid())
    .bind(causes)
    .fetch_all(&mut *conn)
    .await?;
    let mut out: HashMap<Uuid, Vec<Citation>> = HashMap::new();
    for row in rows {
        let cause = row.cause_id;
        out.entry(cause).or_default().push(citation_from(row)?);
    }
    Ok(out)
}

async fn causes_from(conn: &mut PgConnection, org: OrgId, rows: Vec<CauseRow>) -> Result<Vec<Cause>, DbError> {
    let ids: Vec<Uuid> = rows.iter().map(|r| r.id).collect();
    let mut citations = load_citations(conn, org, &ids).await?;
    rows.into_iter().map(|r| {
        let evidence = citations.remove(&r.id).unwrap_or_default();
        cause_from(org, r, evidence)
    })
    .collect()
}

async fn insert_cause(conn: &mut PgConnection, org: OrgId, c: &Cause, now: OffsetDateTime) -> Result<(), DbError> {
    let (status, run, gate, reason) = cause_status_cols(&c.status);
    let rank = i16::try_from(c.rank).map_err(|_| DbError::Invalid(format!("cause {}: rank {} out of range", c.id, c.rank)))?;
    sqlx::query(
        "INSERT INTO causes (org_id, id, ticket_id, triage_run, rank, title, explanation, repro_steps, confirm_if, rule_out_if, confidence, \
                             method, status, status_run, status_gate, status_reason, superseded_at, created_at, updated_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $18)",
    )
    .bind(org.as_uuid())
    .bind(c.id.as_uuid())
    .bind(c.ticket_id.as_uuid())
    .bind(c.triage_run.as_uuid())
    .bind(rank)
    .bind(&c.title)
    .bind(&c.explanation)
    .bind(&c.repro_steps)
    .bind(&c.confirm_if)
    .bind(&c.rule_out_if)
    .bind(c.confidence.get() as f32)
    .bind(method_str(c.method))
    .bind(status)
    .bind(run)
    .bind(gate)
    .bind(reason)
    .bind(c.superseded_at)
    .bind(now)
    .execute(&mut *conn)
    .await?;
    for (ord, cit) in c.evidence.iter().enumerate() {
        let ord = i16::try_from(ord).map_err(|_| DbError::Invalid(format!("cause {}: too many citations", c.id)))?;
        let line = |n: u32, what: &str| i32::try_from(n).map_err(|_| DbError::Invalid(format!("cause {}: {what} {n} out of range", c.id)));
        sqlx::query(
            "INSERT INTO cause_citations (org_id, cause_id, ord, repo_id, sha, path, line_start, line_end, snippet_hash, note) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
        )
        .bind(org.as_uuid())
        .bind(c.id.as_uuid())
        .bind(ord)
        .bind(cit.repo_id.as_uuid())
        .bind(cit.sha.as_bytes().to_vec())
        .bind(cit.path.as_str())
        .bind(line(cit.lines.start(), "line_start")?)
        .bind(line(cit.lines.end(), "line_end")?)
        .bind(cit.snippet_hash.map(|h| h.to_vec()))
        .bind(cit.note.as_deref())
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

// ---- briefs, pull requests, resolutions ----

#[derive(sqlx::FromRow)]
struct BriefRow {
    id: Uuid,
    cause_id: Uuid,
    version: i32,
    diagnose_run_id: Uuid,
    artifact_id: Uuid,
    markdown: String,
    created_at: OffsetDateTime,
}

fn brief_from(org: OrgId, r: BriefRow) -> Result<FixBrief, DbError> {
    Ok(FixBrief {
        id: FixBriefId::from_uuid(r.id),
        org_id: org,
        cause_id: CauseId::from_uuid(r.cause_id),
        version: to_unsigned(i64::from(r.version), "fix_briefs.version")?,
        diagnose_run: RunId::from_uuid(r.diagnose_run_id),
        artifact: ArtifactId::from_uuid(r.artifact_id),
        markdown: r.markdown,
        created_at: r.created_at,
    })
}

#[derive(sqlx::FromRow)]
struct PrRow {
    id: Uuid,
    ticket_id: Uuid,
    fix_run_id: Uuid,
    gate_id: Uuid,
    changeset_digest: Vec<u8>,
    branch: String,
    base_sha: Vec<u8>,
    head_commit: Option<Vec<u8>>,
    pr_number: Option<i64>,
    pr_url: Option<String>,
    state: String,
    merged: Option<bool>,
    created_at: OffsetDateTime,
    updated_at: OffsetDateTime,
}

fn pr_from(org: OrgId, r: PrRow) -> Result<FixPullRequest, DbError> {
    let head_commit = opt_sha(r.head_commit, "fix_pull_requests.head_commit")?;
    let number: Option<u64> = r.pr_number.map(|n| to_unsigned(n, "fix_pull_requests.pr_number")).transpose()?;
    let state = match (r.state.as_str(), head_commit, number, r.merged) {
        ("planned", ..) => FixPrState::Planned,
        ("commit_created", Some(commit), ..) => FixPrState::CommitCreated { commit },
        ("branch_created", ..) => FixPrState::BranchCreated,
        ("opened", _, Some(number), _) => FixPrState::Opened { number },
        ("closed", _, _, Some(merged)) => FixPrState::Closed { merged },
        (other, ..) => return Err(DbError::Invalid(format!("fix_pull_requests.state: {other:?} without its payload column"))),
    };
    Ok(FixPullRequest {
        id: FixPullRequestId::from_uuid(r.id),
        org_id: org,
        ticket_id: TicketId::from_uuid(r.ticket_id),
        fix_run: RunId::from_uuid(r.fix_run_id),
        gate: GateId::from_uuid(r.gate_id),
        change_set: ChangeSetDigest::from_bytes(fixed(r.changeset_digest, "fix_pull_requests.changeset_digest")?),
        branch: BranchName::new(r.branch).map_err(|e| DbError::Invalid(format!("fix_pull_requests.branch: {e}")))?,
        base_sha: sha(r.base_sha, "fix_pull_requests.base_sha")?,
        head_commit,
        number,
        url: r.pr_url.map(|u| parse_url(&u, "fix_pull_requests.pr_url")).transpose()?,
        state,
        created_at: r.created_at,
        updated_at: r.updated_at,
    })
}

#[derive(sqlx::FromRow)]
struct ResolutionRow {
    ticket_id: Uuid,
    confirmed_cause: Option<Uuid>,
    root_cause: String,
    fix_pr_url: Option<String>,
    repro_script: Option<Uuid>,
    resolved_by: Uuid,
    at: OffsetDateTime,
}

fn resolution_from(org: OrgId, r: ResolutionRow) -> Result<Resolution, DbError> {
    Ok(Resolution {
        ticket_id: TicketId::from_uuid(r.ticket_id),
        org_id: org,
        confirmed_cause: r.confirmed_cause.map(CauseId::from_uuid),
        root_cause: r.root_cause,
        fix_pr_url: r.fix_pr_url.map(|u| parse_url(&u, "resolutions.fix_pr_url")).transpose()?,
        repro_script: r.repro_script.map(ArtifactId::from_uuid),
        resolved_by: UserId::from_uuid(r.resolved_by),
        at: r.at,
    })
}

#[derive(sqlx::FromRow)]
struct SimilarRow {
    id: Uuid,
    source_identifier: String,
    title: String,
    source_url: String,
    error_strings: Vec<String>,
    at: OffsetDateTime,
    root_cause: String,
    fix_pr_url: Option<String>,
    confirmed_cause: Option<Uuid>,
    text_match: bool,
    rank: f32,
}

#[async_trait]
impl TicketStore for PgTicketStore {
    async fn get_ticket(&self, org: OrgId, id: TicketId) -> Result<Versioned<Ticket>, TicketStoreError> {
        retry_tx(|| async {
            let mut tx = begin_org(&self.pools.tenant, org).await?;
            let row: Option<TicketRow> = sqlx::query_as(concat!("SELECT ", ticket_cols!(), " FROM tickets WHERE org_id = $1 AND id = $2"))
                .bind(org.as_uuid())
                .bind(id.as_uuid())
                .fetch_optional(&mut *tx)
                .await?;
            tx.commit().await?;
            match row {
                Some(r) => Ok(ticket_from(org, r)?),
                None => port(TicketStoreError::NotFound),
            }
        })
        .await
    }

    async fn get_by_linear_issue(&self, org: OrgId, issue_id: &str) -> Result<Option<Versioned<Ticket>>, TicketStoreError> {
        retry_tx(|| async {
            let mut tx = begin_org(&self.pools.tenant, org).await?;
            let row: Option<TicketRow> = sqlx::query_as(concat!(
                "SELECT ",
                ticket_cols!(),
                " FROM tickets WHERE org_id = $1 AND source = 'linear' AND source_issue_id = $2"
            ))
            .bind(org.as_uuid())
            .bind(issue_id)
            .fetch_optional(&mut *tx)
            .await?;
            tx.commit().await?;
            Ok::<_, TxError<TicketStoreError>>(row.map(|r| ticket_from(org, r)).transpose()?)
        })
        .await
    }

    async fn list_tickets(&self, org: OrgId, filter: &TicketFilter, page: PageRequest) -> Result<Page<Ticket>, TicketStoreError> {
        let after = uuid_cursor(page.after.as_ref())?;
        let statuses: Option<Vec<&'static str>> = filter.status.as_ref().map(|s| s.iter().map(|t| t.name()).collect());
        retry_tx(|| async {
            let mut tx = begin_org(&self.pools.tenant, org).await?;
            let rows: Vec<TicketRow> = sqlx::query_as(concat!(
                "SELECT ",
                ticket_cols!(),
                " FROM tickets WHERE org_id = $1 \
                   AND ($2::text[] IS NULL OR status = ANY($2)) \
                   AND ($3::uuid IS NULL OR customer_id = $3) \
                   AND ($4::uuid IS NULL OR target_repo_id = $4) \
                   AND ($5::text IS NULL OR search @@ websearch_to_tsquery('simple', $5)) \
                   AND ($6::uuid IS NULL OR id < $6) \
                 ORDER BY id DESC LIMIT $7"
            ))
            .bind(org.as_uuid())
            .bind(statuses.as_deref())
            .bind(filter.customer.map(|c| c.as_uuid()))
            .bind(filter.repo.map(|r| r.as_uuid()))
            .bind(filter.text.as_deref())
            .bind(after)
            .bind(page_limit(&page))
            .fetch_all(&mut *tx)
            .await?;
            tx.commit().await?;
            let items = rows.into_iter().map(|r| ticket_from(org, r).map(|v| v.value)).collect::<Result<Vec<_>, _>>()?;
            Ok::<_, TxError<TicketStoreError>>(finish_page(items, &page, |t| t.id.as_uuid().to_string()))
        })
        .await
    }

    async fn list_open_assumed(&self, org: OrgId, repo: RepoId) -> Result<Vec<TicketId>, TicketStoreError> {
        retry_tx(|| async {
            let mut tx = begin_org(&self.pools.tenant, org).await?;
            let ids: Vec<Uuid> = sqlx::query_scalar(
                "SELECT id FROM tickets WHERE org_id = $1 AND target_repo_id = $2 AND target_origin = 'default_head' \
                   AND status NOT IN ('resolved', 'closed') ORDER BY id",
            )
            .bind(org.as_uuid())
            .bind(repo.as_uuid())
            .fetch_all(&mut *tx)
            .await?;
            tx.commit().await?;
            Ok::<_, TxError<TicketStoreError>>(ids.into_iter().map(TicketId::from_uuid).collect())
        })
        .await
    }

    async fn save_ingested(&self, org: OrgId, ticket: &Ticket, expected_rev: Option<u64>, jobs: &[NewJob]) -> Result<u64, TicketStoreError> {
        if ticket.org_id != org {
            return Err(TicketStoreError::Invalid("ticket belongs to another org".into()));
        }
        check_jobs(jobs)?;
        let expected = expected_rev.map(|r| to_i64(r, "tickets.rev")).transpose()?;
        retry_tx(|| async {
            let now = self.clock.now();
            let mut tx = begin_org(&self.pools.tenant, org).await?;
            let rev = match expected {
                None => {
                    let q = sqlx::query(concat!(
                        "INSERT INTO tickets (org_id, id, ",
                        ticket_write_cols!(),
                        ", rev) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20, $21, \
                         $22, $23, 1)"
                    ))
                    .bind(org.as_uuid())
                    .bind(ticket.id.as_uuid());
                    bind_ticket(q, ticket).execute(&mut *tx).await?;
                    1
                }
                Some(expected) => {
                    let q = sqlx::query(
                        "UPDATE tickets SET source = $3, source_issue_id = $4, source_identifier = $5, source_url = $6, title = $7, body = $8, \
                           content_hash = $9, triaged_hash = $10, triage_run = $11, slack_team = $12, slack_channel = $13, slack_ts = $14, \
                           customer_id = $15, target_repo_id = $16, target_sha = $17, target_origin = $18, target_environment = $19, \
                           error_strings = $20, status = $21, created_at = $22, updated_at = $23, rev = rev + 1 \
                         WHERE org_id = $1 AND id = $2 AND rev = $24",
                    )
                    .bind(org.as_uuid())
                    .bind(ticket.id.as_uuid());
                    let done = bind_ticket(q, ticket).bind(expected).execute(&mut *tx).await?;
                    if done.rows_affected() == 0 {
                        return ticket_miss(&mut tx, org, ticket.id).await;
                    }
                    expected + 1
                }
            };
            enqueue_all_in(&mut tx, jobs, now, &*self.ids).await?;
            append_in(&mut tx, org, FeedKind::Ticket, &ticket.id.to_string(), now).await?;
            tx.commit().await?;
            Ok::<_, TxError<TicketStoreError>>(to_unsigned::<u64>(rev, "tickets.rev")?)
        })
        .await
    }

    async fn transition(&self, org: OrgId, ticket: TicketId, expect: TicketStatus, next: TicketStatus, jobs: &[NewJob])
    -> Result<Ticket, TicketStoreError> {
        check_jobs(jobs)?;
        retry_tx(|| async {
            let now = self.clock.now();
            let mut tx = begin_org(&self.pools.tenant, org).await?;
            let Some(out) = cas_status(&mut tx, org, ticket, expect, next, now).await? else {
                return ticket_miss(&mut tx, org, ticket).await;
            };
            enqueue_all_in(&mut tx, jobs, now, &*self.ids).await?;
            append_in(&mut tx, org, FeedKind::Ticket, &ticket.to_string(), now).await?;
            tx.commit().await?;
            Ok::<_, TxError<TicketStoreError>>(out)
        })
        .await
    }

    async fn set_target(&self, org: OrgId, ticket: TicketId, target: Option<TicketTarget>, expected_rev: u64, jobs: &[NewJob])
    -> Result<u64, TicketStoreError> {
        check_jobs(jobs)?;
        let expected = to_i64(expected_rev, "tickets.rev")?;
        let origin = target.as_ref().map(|t| origin_cols(&t.origin));
        let target = &target;
        let origin = &origin;
        retry_tx(|| async {
            let now = self.clock.now();
            let mut tx = begin_org(&self.pools.tenant, org).await?;
            let rev: Option<i64> = sqlx::query_scalar(
                "UPDATE tickets SET target_repo_id = $3, target_sha = $4, target_origin = $5, target_environment = $6, updated_at = $7, \
                   rev = rev + 1 \
                 WHERE org_id = $1 AND id = $2 AND rev = $8 RETURNING rev",
            )
            .bind(org.as_uuid())
            .bind(ticket.as_uuid())
            .bind(target.as_ref().map(|t| t.repo_id.as_uuid()))
            .bind(target.as_ref().map(|t| t.sha.as_bytes().to_vec()))
            .bind(origin.as_ref().map(|(o, _)| *o))
            .bind(origin.as_ref().and_then(|(_, e)| e.clone()))
            .bind(now)
            .bind(expected)
            .fetch_optional(&mut *tx)
            .await?;
            let Some(rev) = rev else {
                return ticket_miss(&mut tx, org, ticket).await;
            };
            enqueue_all_in(&mut tx, jobs, now, &*self.ids).await?;
            append_in(&mut tx, org, FeedKind::Ticket, &ticket.to_string(), now).await?;
            tx.commit().await?;
            Ok::<_, TxError<TicketStoreError>>(to_unsigned::<u64>(rev, "tickets.rev")?)
        })
        .await
    }

    async fn get_thread_snapshot(&self, org: OrgId, ticket: TicketId) -> Result<Option<ThreadSnapshot>, TicketStoreError> {
        retry_tx(|| async {
            let mut tx = begin_org(&self.pools.tenant, org).await?;
            let snapshot: Option<Json<ThreadSnapshot>> =
                sqlx::query_scalar("SELECT snapshot FROM ticket_thread_snapshots WHERE org_id = $1 AND ticket_id = $2")
                    .bind(org.as_uuid())
                    .bind(ticket.as_uuid())
                    .fetch_optional(&mut *tx)
                    .await?;
            tx.commit().await?;
            Ok::<_, TxError<TicketStoreError>>(snapshot.map(|s| s.0))
        })
        .await
    }

    async fn put_thread_snapshot(&self, org: OrgId, ticket: TicketId, s: &ThreadSnapshot) -> Result<(), TicketStoreError> {
        retry_tx(|| async {
            let now = self.clock.now();
            let mut tx = begin_org(&self.pools.tenant, org).await?;
            if !ticket_exists(&mut tx, org, ticket).await? {
                return port(TicketStoreError::NotFound);
            }
            sqlx::query(
                "INSERT INTO ticket_thread_snapshots (org_id, ticket_id, snapshot, fetched_at) VALUES ($1, $2, $3, $4) \
                 ON CONFLICT (org_id, ticket_id) DO UPDATE SET snapshot = EXCLUDED.snapshot, fetched_at = EXCLUDED.fetched_at",
            )
            .bind(org.as_uuid())
            .bind(ticket.as_uuid())
            .bind(Json(s))
            .bind(s.fetched_at)
            .execute(&mut *tx)
            .await?;
            append_in(&mut tx, org, FeedKind::Ticket, &ticket.to_string(), now).await?;
            tx.commit().await?;
            Ok::<_, TxError<TicketStoreError>>(())
        })
        .await
    }

    async fn begin_triage(
        &self,
        org: OrgId,
        ticket: TicketId,
        expect: &[TicketStatus],
        content_hash: ContentHash,
        triage_run: RunId,
    ) -> Result<Ticket, TicketStoreError> {
        let expect: Vec<&'static str> = expect.iter().map(|s| s.name()).collect();
        let expect = &expect;
        retry_tx(|| async {
            let now = self.clock.now();
            let mut tx = begin_org(&self.pools.tenant, org).await?;
            let row: Option<TicketRow> = sqlx::query_as(concat!(
                "UPDATE tickets SET status = 'triaging', triaged_hash = $3, triage_run = $4, updated_at = $5, rev = rev + 1 \
                 WHERE org_id = $1 AND id = $2 AND status = ANY($6) AND content_hash = $3 RETURNING ",
                ticket_cols!()
            ))
            .bind(org.as_uuid())
            .bind(ticket.as_uuid())
            .bind(content_hash.as_bytes().to_vec())
            .bind(triage_run.as_uuid())
            .bind(now)
            .bind(expect.as_slice())
            .fetch_optional(&mut *tx)
            .await?;
            let Some(row) = row else {
                return ticket_miss(&mut tx, org, ticket).await;
            };
            let out = ticket_from(org, row)?.value;
            append_in(&mut tx, org, FeedKind::Ticket, &ticket.to_string(), now).await?;
            tx.commit().await?;
            Ok::<_, TxError<TicketStoreError>>(out)
        })
        .await
    }

    async fn apply_triage(&self, org: OrgId, w: TriageWrite, jobs: &[NewJob]) -> Result<Ticket, TicketStoreError> {
        check_jobs(jobs)?;
        if let Some(bad) = w.causes.iter().find(|c| c.org_id != org || c.ticket_id != w.ticket) {
            return Err(TicketStoreError::Invalid(format!("cause {} belongs to another ticket or org", bad.id)));
        }
        let w = &w;
        retry_tx(|| async {
            let now = self.clock.now();
            let mut tx = begin_org(&self.pools.tenant, org).await?;
            let row: Option<TicketRow> =
                sqlx::query_as(concat!("SELECT ", ticket_cols!(), " FROM tickets WHERE org_id = $1 AND id = $2 FOR UPDATE"))
                    .bind(org.as_uuid())
                    .bind(w.ticket.as_uuid())
                    .fetch_optional(&mut *tx)
                    .await?;
            let Some(row) = row else {
                return port(TicketStoreError::NotFound);
            };
            let current = ticket_from(org, row)?.value;
            if current.status != w.expect || current.triage_run != Some(w.triage_run) || current.content_hash != w.content_hash {
                return port(TicketStoreError::Conflict);
            }
            sqlx::query(
                "UPDATE causes SET superseded_at = $3, updated_at = $3 \
                 WHERE org_id = $1 AND ticket_id = $2 AND status = 'untested' AND superseded_at IS NULL",
            )
            .bind(org.as_uuid())
            .bind(w.ticket.as_uuid())
            .bind(now)
            .execute(&mut *tx)
            .await?;
            for cause in &w.causes {
                insert_cause(&mut tx, org, cause, now).await?;
            }
            let Some(out) = cas_status(&mut tx, org, w.ticket, w.expect, w.next, now).await? else {
                return port(TicketStoreError::Conflict);
            };
            enqueue_all_in(&mut tx, jobs, now, &*self.ids).await?;
            append_in(&mut tx, org, FeedKind::Ticket, &w.ticket.to_string(), now).await?;
            tx.commit().await?;
            Ok::<_, TxError<TicketStoreError>>(out)
        })
        .await
    }

    async fn fail_triage(&self, org: OrgId, ticket: TicketId, triage_run: RunId, jobs: &[NewJob]) -> Result<Ticket, TicketStoreError> {
        check_jobs(jobs)?;
        retry_tx(|| async {
            let now = self.clock.now();
            let mut tx = begin_org(&self.pools.tenant, org).await?;
            let row: Option<TicketRow> = sqlx::query_as(concat!(
                "UPDATE tickets SET status = 'triage_failed', updated_at = $4, rev = rev + 1 \
                 WHERE org_id = $1 AND id = $2 AND status = 'triaging' AND triage_run = $3 RETURNING ",
                ticket_cols!()
            ))
            .bind(org.as_uuid())
            .bind(ticket.as_uuid())
            .bind(triage_run.as_uuid())
            .bind(now)
            .fetch_optional(&mut *tx)
            .await?;
            let Some(row) = row else {
                return ticket_miss(&mut tx, org, ticket).await;
            };
            let out = ticket_from(org, row)?.value;
            enqueue_all_in(&mut tx, jobs, now, &*self.ids).await?;
            append_in(&mut tx, org, FeedKind::Ticket, &ticket.to_string(), now).await?;
            tx.commit().await?;
            Ok::<_, TxError<TicketStoreError>>(out)
        })
        .await
    }

    async fn causes(&self, org: OrgId, ticket: TicketId, include_superseded: bool) -> Result<Vec<Cause>, TicketStoreError> {
        retry_tx(|| async {
            let mut tx = begin_org(&self.pools.tenant, org).await?;
            let rows: Vec<CauseRow> = sqlx::query_as(concat!(
                "SELECT ",
                cause_cols!(),
                " FROM causes WHERE org_id = $1 AND ticket_id = $2 AND ($3 OR superseded_at IS NULL) \
                 ORDER BY superseded_at IS NOT NULL, superseded_at DESC, rank, id"
            ))
            .bind(org.as_uuid())
            .bind(ticket.as_uuid())
            .bind(include_superseded)
            .fetch_all(&mut *tx)
            .await?;
            let out = causes_from(&mut tx, org, rows).await?;
            tx.commit().await?;
            Ok::<_, TxError<TicketStoreError>>(out)
        })
        .await
    }

    async fn get_cause(&self, org: OrgId, id: CauseId) -> Result<Cause, TicketStoreError> {
        retry_tx(|| async {
            let mut tx = begin_org(&self.pools.tenant, org).await?;
            let rows: Vec<CauseRow> = sqlx::query_as(concat!("SELECT ", cause_cols!(), " FROM causes WHERE org_id = $1 AND id = $2"))
                .bind(org.as_uuid())
                .bind(id.as_uuid())
                .fetch_all(&mut *tx)
                .await?;
            let out = causes_from(&mut tx, org, rows).await?;
            tx.commit().await?;
            match out.into_iter().next() {
                Some(cause) => Ok(cause),
                None => port(TicketStoreError::NotFound),
            }
        })
        .await
    }

    async fn set_cause_status(
        &self,
        org: OrgId,
        id: CauseId,
        expect: &CauseStatus,
        next: CauseStatus,
        ticket: Option<TicketTransition>,
        jobs: &[NewJob],
    ) -> Result<Cause, TicketStoreError> {
        check_jobs(jobs)?;
        let (status, run, gate, reason) = cause_status_cols(&next);
        let reason = &reason;
        retry_tx(|| async {
            let now = self.clock.now();
            let mut tx = begin_org(&self.pools.tenant, org).await?;
            // Lock order ticket → cause, as `apply_triage`.
            let ticket_id: Option<Uuid> = sqlx::query_scalar("SELECT ticket_id FROM causes WHERE org_id = $1 AND id = $2")
                .bind(org.as_uuid())
                .bind(id.as_uuid())
                .fetch_optional(&mut *tx)
                .await?;
            let Some(ticket_id) = ticket_id else {
                return port(TicketStoreError::NotFound);
            };
            sqlx::query("SELECT 1 FROM tickets WHERE org_id = $1 AND id = $2 FOR UPDATE")
                .bind(org.as_uuid())
                .bind(ticket_id)
                .execute(&mut *tx)
                .await?;
            let stored: String = sqlx::query_scalar("SELECT status FROM causes WHERE org_id = $1 AND id = $2 FOR UPDATE")
                .bind(org.as_uuid())
                .bind(id.as_uuid())
                .fetch_one(&mut *tx)
                .await?;
            if stored != expect.name() {
                return port(TicketStoreError::Conflict);
            }
            apply_transition(&mut tx, org, TicketId::from_uuid(ticket_id), ticket, now).await?;
            let row: CauseRow = sqlx::query_as(concat!(
                "UPDATE causes SET status = $3, status_run = $4, status_gate = $5, status_reason = $6, updated_at = $7 \
                 WHERE org_id = $1 AND id = $2 RETURNING ",
                cause_cols!()
            ))
            .bind(org.as_uuid())
            .bind(id.as_uuid())
            .bind(status)
            .bind(run)
            .bind(gate)
            .bind(reason.as_deref())
            .bind(now)
            .fetch_one(&mut *tx)
            .await?;
            let out = causes_from(&mut tx, org, vec![row]).await?.remove(0);
            enqueue_all_in(&mut tx, jobs, now, &*self.ids).await?;
            append_in(&mut tx, org, FeedKind::Cause, &id.to_string(), now).await?;
            tx.commit().await?;
            Ok::<_, TxError<TicketStoreError>>(out)
        })
        .await
    }

    async fn put_fix_brief(&self, org: OrgId, brief: FixBrief, jobs: &[NewJob]) -> Result<FixBrief, TicketStoreError> {
        if brief.org_id != org {
            return Err(TicketStoreError::Invalid("fix brief belongs to another org".into()));
        }
        check_jobs(jobs)?;
        let brief = &brief;
        retry_tx(|| async {
            let now = self.clock.now();
            let mut tx = begin_org(&self.pools.tenant, org).await?;
            let locked: Option<i32> = sqlx::query_scalar("SELECT 1 FROM causes WHERE org_id = $1 AND id = $2 FOR UPDATE")
                .bind(org.as_uuid())
                .bind(brief.cause_id.as_uuid())
                .fetch_optional(&mut *tx)
                .await?;
            if locked.is_none() {
                return port(TicketStoreError::NotFound);
            }
            let row: BriefRow = sqlx::query_as(concat!(
                "INSERT INTO fix_briefs (org_id, id, cause_id, version, diagnose_run_id, artifact_id, markdown, created_at) \
                 SELECT $1, $2, $3, COALESCE(max(version), 0) + 1, $4, $5, $6, $7 FROM fix_briefs WHERE org_id = $1 AND cause_id = $3 \
                 RETURNING ",
                brief_cols!()
            ))
            .bind(org.as_uuid())
            .bind(brief.id.as_uuid())
            .bind(brief.cause_id.as_uuid())
            .bind(brief.diagnose_run.as_uuid())
            .bind(brief.artifact.as_uuid())
            .bind(&brief.markdown)
            .bind(brief.created_at)
            .fetch_one(&mut *tx)
            .await?;
            let stored = brief_from(org, row)?;
            enqueue_all_in(&mut tx, jobs, now, &*self.ids).await?;
            append_in(&mut tx, org, FeedKind::Cause, &brief.cause_id.to_string(), now).await?;
            tx.commit().await?;
            Ok::<_, TxError<TicketStoreError>>(stored)
        })
        .await
    }

    async fn latest_fix_brief(&self, org: OrgId, cause: CauseId) -> Result<Option<FixBrief>, TicketStoreError> {
        retry_tx(|| async {
            let mut tx = begin_org(&self.pools.tenant, org).await?;
            let row: Option<BriefRow> = sqlx::query_as(concat!(
                "SELECT ",
                brief_cols!(),
                " FROM fix_briefs WHERE org_id = $1 AND cause_id = $2 ORDER BY version DESC LIMIT 1"
            ))
            .bind(org.as_uuid())
            .bind(cause.as_uuid())
            .fetch_optional(&mut *tx)
            .await?;
            tx.commit().await?;
            Ok::<_, TxError<TicketStoreError>>(row.map(|r| brief_from(org, r)).transpose()?)
        })
        .await
    }

    async fn fix_briefs(&self, org: OrgId, cause: CauseId) -> Result<Vec<FixBrief>, TicketStoreError> {
        retry_tx(|| async {
            let mut tx = begin_org(&self.pools.tenant, org).await?;
            let rows: Vec<BriefRow> =
                sqlx::query_as(concat!("SELECT ", brief_cols!(), " FROM fix_briefs WHERE org_id = $1 AND cause_id = $2 ORDER BY version"))
                    .bind(org.as_uuid())
                    .bind(cause.as_uuid())
                    .fetch_all(&mut *tx)
                    .await?;
            tx.commit().await?;
            Ok::<_, TxError<TicketStoreError>>(rows.into_iter().map(|r| brief_from(org, r)).collect::<Result<Vec<_>, _>>()?)
        })
        .await
    }

    async fn upsert_pull_request(&self, org: OrgId, pr: FixPullRequest, jobs: &[NewJob]) -> Result<(FixPullRequest, bool), TicketStoreError> {
        if pr.org_id != org {
            return Err(TicketStoreError::Invalid("pull request belongs to another org".into()));
        }
        check_pull_request_payload(&pr)?;
        check_jobs(jobs)?;
        let number = pr.number.map(|n| to_i64(n, "fix_pull_requests.pr_number")).transpose()?;
        let pr = &pr;
        retry_tx(|| async {
            let now = self.clock.now();
            let mut tx = begin_org(&self.pools.tenant, org).await?;
            if !ticket_exists(&mut tx, org, pr.ticket_id).await? {
                return port(TicketStoreError::NotFound);
            }
            let inserted: Option<Uuid> = sqlx::query_scalar(
                "INSERT INTO fix_pull_requests (org_id, id, ticket_id, fix_run_id, gate_id, changeset_digest, branch, base_sha, head_commit, \
                                                pr_number, pr_url, state, merged, created_at, updated_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15) \
                 ON CONFLICT (org_id, ticket_id, changeset_digest) DO NOTHING RETURNING id",
            )
            .bind(org.as_uuid())
            .bind(pr.id.as_uuid())
            .bind(pr.ticket_id.as_uuid())
            .bind(pr.fix_run.as_uuid())
            .bind(pr.gate.as_uuid())
            .bind(pr.change_set.as_bytes().to_vec())
            .bind(pr.branch.as_str())
            .bind(pr.base_sha.as_bytes().to_vec())
            .bind(pr.head_commit.map(|c| c.as_bytes().to_vec()))
            .bind(number)
            .bind(pr.url.as_ref().map(Url::as_str))
            .bind(pr.state.name())
            .bind(merged_col(&pr.state))
            .bind(pr.created_at)
            .bind(pr.updated_at)
            .fetch_optional(&mut *tx)
            .await?;
            if inserted.is_none() {
                let row: PrRow = sqlx::query_as(concat!(
                    "SELECT ",
                    pr_cols!(),
                    " FROM fix_pull_requests WHERE org_id = $1 AND ticket_id = $2 AND changeset_digest = $3"
                ))
                .bind(org.as_uuid())
                .bind(pr.ticket_id.as_uuid())
                .bind(pr.change_set.as_bytes().to_vec())
                .fetch_one(&mut *tx)
                .await?;
                tx.commit().await?;
                return Ok((pr_from(org, row)?, false));
            }
            enqueue_all_in(&mut tx, jobs, now, &*self.ids).await?;
            append_in(&mut tx, org, FeedKind::Ticket, &pr.ticket_id.to_string(), now).await?;
            tx.commit().await?;
            Ok::<_, TxError<TicketStoreError>>((pr.clone(), true))
        })
        .await
    }

    async fn set_pull_request_state(
        &self,
        org: OrgId,
        id: FixPullRequestId,
        expect: &FixPrState,
        next: FixPrState,
        jobs: &[NewJob],
    ) -> Result<FixPullRequest, TicketStoreError> {
        check_jobs(jobs)?;
        let commit = match &next {
            FixPrState::CommitCreated { commit } => Some(commit.as_bytes().to_vec()),
            _ => None,
        };
        let number = match &next {
            FixPrState::Opened { number } => Some(to_i64(*number, "fix_pull_requests.pr_number")?),
            _ => None,
        };
        let (commit, next) = (&commit, &next);
        retry_tx(|| async {
            let now = self.clock.now();
            let mut tx = begin_org(&self.pools.tenant, org).await?;
            let row: Option<PrRow> = sqlx::query_as(concat!(
                "UPDATE fix_pull_requests SET state = $4, head_commit = COALESCE($5, head_commit), pr_number = COALESCE($6, pr_number), \
                   merged = $7, updated_at = $8 \
                 WHERE org_id = $1 AND id = $2 AND state = $3 RETURNING ",
                pr_cols!()
            ))
            .bind(org.as_uuid())
            .bind(id.as_uuid())
            .bind(expect.name())
            .bind(next.name())
            .bind(commit.as_deref())
            .bind(number)
            .bind(merged_col(next))
            .bind(now)
            .fetch_optional(&mut *tx)
            .await?;
            let Some(row) = row else {
                let exists: Option<i32> = sqlx::query_scalar("SELECT 1 FROM fix_pull_requests WHERE org_id = $1 AND id = $2")
                    .bind(org.as_uuid())
                    .bind(id.as_uuid())
                    .fetch_optional(&mut *tx)
                    .await?;
                return port(if exists.is_some() { TicketStoreError::Conflict } else { TicketStoreError::NotFound });
            };
            let out = pr_from(org, row)?;
            enqueue_all_in(&mut tx, jobs, now, &*self.ids).await?;
            append_in(&mut tx, org, FeedKind::Ticket, &out.ticket_id.to_string(), now).await?;
            tx.commit().await?;
            Ok::<_, TxError<TicketStoreError>>(out)
        })
        .await
    }

    async fn update_pull_request(
        &self,
        org: OrgId,
        pr: &FixPullRequest,
        expect: &FixPrState,
        ticket: Option<TicketTransition>,
        jobs: &[NewJob],
    ) -> Result<FixPullRequest, TicketStoreError> {
        check_pull_request_payload(pr)?;
        check_jobs(jobs)?;
        let number = pr.number.map(|n| to_i64(n, "fix_pull_requests.pr_number")).transpose()?;
        retry_tx(|| async {
            let now = self.clock.now();
            let mut tx = begin_org(&self.pools.tenant, org).await?;
            let current: Option<(String, Uuid)> =
                sqlx::query_as("SELECT state, ticket_id FROM fix_pull_requests WHERE org_id = $1 AND id = $2 FOR UPDATE")
                    .bind(org.as_uuid())
                    .bind(pr.id.as_uuid())
                    .fetch_optional(&mut *tx)
                    .await?;
            let Some((state, ticket_id)) = current else {
                return port(TicketStoreError::NotFound);
            };
            if state != expect.name() {
                return port(TicketStoreError::Conflict);
            }
            let ticket_id = TicketId::from_uuid(ticket_id);
            apply_transition(&mut tx, org, ticket_id, ticket, now).await?;
            let row: PrRow = sqlx::query_as(concat!(
                "UPDATE fix_pull_requests SET branch = $3, head_commit = $4, pr_number = $5, pr_url = $6, state = $7, merged = $8, \
                   updated_at = $9 \
                 WHERE org_id = $1 AND id = $2 RETURNING ",
                pr_cols!()
            ))
            .bind(org.as_uuid())
            .bind(pr.id.as_uuid())
            .bind(pr.branch.as_str())
            .bind(pr.head_commit.map(|c| c.as_bytes().to_vec()))
            .bind(number)
            .bind(pr.url.as_ref().map(Url::as_str))
            .bind(pr.state.name())
            .bind(merged_col(&pr.state))
            .bind(now)
            .fetch_one(&mut *tx)
            .await?;
            let out = pr_from(org, row)?;
            enqueue_all_in(&mut tx, jobs, now, &*self.ids).await?;
            append_in(&mut tx, org, FeedKind::Ticket, &ticket_id.to_string(), now).await?;
            tx.commit().await?;
            Ok::<_, TxError<TicketStoreError>>(out)
        })
        .await
    }

    async fn pull_request_by_number(&self, org: OrgId, repo: RepoId, number: u64) -> Result<Option<FixPullRequest>, TicketStoreError> {
        let number = to_i64(number, "fix_pull_requests.pr_number")?;
        retry_tx(|| async {
            let mut tx = begin_org(&self.pools.tenant, org).await?;
            let row: Option<PrRow> = sqlx::query_as(
                "SELECT p.id, p.ticket_id, p.fix_run_id, p.gate_id, p.changeset_digest, p.branch, p.base_sha, p.head_commit, p.pr_number, \
                        p.pr_url, p.state, p.merged, p.created_at, p.updated_at \
                 FROM fix_pull_requests p JOIN runs r ON r.org_id = p.org_id AND r.id = p.fix_run_id \
                 WHERE p.org_id = $1 AND p.pr_number = $2 AND r.repo_id = $3 ORDER BY p.id LIMIT 1",
            )
            .bind(org.as_uuid())
            .bind(number)
            .bind(repo.as_uuid())
            .fetch_optional(&mut *tx)
            .await?;
            tx.commit().await?;
            Ok::<_, TxError<TicketStoreError>>(row.map(|r| pr_from(org, r)).transpose()?)
        })
        .await
    }

    async fn pull_requests(&self, org: OrgId, ticket: TicketId) -> Result<Vec<FixPullRequest>, TicketStoreError> {
        retry_tx(|| async {
            let mut tx = begin_org(&self.pools.tenant, org).await?;
            let rows: Vec<PrRow> = sqlx::query_as(concat!(
                "SELECT ",
                pr_cols!(),
                " FROM fix_pull_requests WHERE org_id = $1 AND ticket_id = $2 ORDER BY created_at, id"
            ))
            .bind(org.as_uuid())
            .bind(ticket.as_uuid())
            .fetch_all(&mut *tx)
            .await?;
            tx.commit().await?;
            Ok::<_, TxError<TicketStoreError>>(rows.into_iter().map(|r| pr_from(org, r)).collect::<Result<Vec<_>, _>>()?)
        })
        .await
    }

    async fn resolve(&self, org: OrgId, resolution: Resolution, expect: TicketStatus, jobs: &[NewJob]) -> Result<Ticket, TicketStoreError> {
        if resolution.org_id != org {
            return Err(TicketStoreError::Invalid("resolution belongs to another org".into()));
        }
        check_jobs(jobs)?;
        let r = &resolution;
        retry_tx(|| async {
            let now = self.clock.now();
            let ticket = r.ticket_id;
            let mut tx = begin_org(&self.pools.tenant, org).await?;
            let status: Option<String> = sqlx::query_scalar("SELECT status FROM tickets WHERE org_id = $1 AND id = $2 FOR UPDATE")
                .bind(org.as_uuid())
                .bind(ticket.as_uuid())
                .fetch_optional(&mut *tx)
                .await?;
            let Some(status) = status else {
                return port(TicketStoreError::NotFound);
            };
            let resolved: Option<i32> = sqlx::query_scalar("SELECT 1 FROM resolutions WHERE org_id = $1 AND ticket_id = $2")
                .bind(org.as_uuid())
                .bind(ticket.as_uuid())
                .fetch_optional(&mut *tx)
                .await?;
            if resolved.is_some() || status != expect.name() {
                return port(TicketStoreError::Conflict);
            }
            if let Some(cause) = r.confirmed_cause {
                let of_ticket: Option<i32> = sqlx::query_scalar("SELECT 1 FROM causes WHERE org_id = $1 AND id = $2 AND ticket_id = $3")
                    .bind(org.as_uuid())
                    .bind(cause.as_uuid())
                    .bind(ticket.as_uuid())
                    .fetch_optional(&mut *tx)
                    .await?;
                if of_ticket.is_none() {
                    return port(TicketStoreError::Invalid(format!("confirmed cause {cause} is not a cause of {ticket}")));
                }
            }
            sqlx::query(
                "INSERT INTO resolutions (org_id, ticket_id, confirmed_cause, root_cause, fix_pr_url, repro_script, resolved_by, at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
            )
            .bind(org.as_uuid())
            .bind(ticket.as_uuid())
            .bind(r.confirmed_cause.map(|c| c.as_uuid()))
            .bind(&r.root_cause)
            .bind(r.fix_pr_url.as_ref().map(Url::as_str))
            .bind(r.repro_script.map(|a| a.as_uuid()))
            .bind(r.resolved_by.as_uuid())
            .bind(r.at)
            .execute(&mut *tx)
            .await?;
            let Some(out) = cas_status(&mut tx, org, ticket, expect, TicketStatus::Resolved, now).await? else {
                return port(TicketStoreError::Conflict);
            };
            enqueue_all_in(&mut tx, jobs, now, &*self.ids).await?;
            append_in(&mut tx, org, FeedKind::Ticket, &ticket.to_string(), now).await?;
            tx.commit().await?;
            Ok::<_, TxError<TicketStoreError>>(out)
        })
        .await
    }

    async fn resolution(&self, org: OrgId, ticket: TicketId) -> Result<Option<Resolution>, TicketStoreError> {
        retry_tx(|| async {
            let mut tx = begin_org(&self.pools.tenant, org).await?;
            let row: Option<ResolutionRow> = sqlx::query_as(
                "SELECT ticket_id, confirmed_cause, root_cause, fix_pr_url, repro_script, resolved_by, at FROM resolutions \
                 WHERE org_id = $1 AND ticket_id = $2",
            )
            .bind(org.as_uuid())
            .bind(ticket.as_uuid())
            .fetch_optional(&mut *tx)
            .await?;
            tx.commit().await?;
            Ok::<_, TxError<TicketStoreError>>(row.map(|r| resolution_from(org, r)).transpose()?)
        })
        .await
    }

    async fn similar_resolved(&self, org: OrgId, q: &SimilarQuery, limit: u16) -> Result<Vec<SimilarResolved>, TicketStoreError> {
        let query_errors = error_tokens(q.error_strings.iter().map(String::as_str));
        let repos: Vec<Uuid> = q.repos.iter().map(|r| r.as_uuid()).collect();
        let exclude: Vec<Uuid> = q.exclude.iter().map(|t| t.as_uuid()).collect();
        let (repos, exclude, query_errors) = (&repos, &exclude, &query_errors);
        retry_tx(|| async {
            let mut tx = begin_org(&self.pools.tenant, org).await?;
            let rows: Vec<SimilarRow> = sqlx::query_as(
                "SELECT t.id, t.source_identifier, t.title, t.source_url, t.error_strings, r.at, r.root_cause, r.fix_pr_url, \
                        r.confirmed_cause, (t.search @@ q.q OR r.search @@ q.q) AS text_match, \
                        (ts_rank(t.search, q.q, 32) + ts_rank(r.search, q.q, 32))::real AS rank \
                 FROM resolutions r \
                 JOIN tickets t ON t.org_id = r.org_id AND t.id = r.ticket_id \
                 CROSS JOIN (SELECT websearch_to_tsquery('simple', $3) AS q) q \
                 WHERE r.org_id = $1 AND t.status = 'resolved' AND t.target_repo_id = ANY($2) AND NOT (t.id = ANY($4)) \
                   AND ($5::timestamptz IS NULL OR r.at <= $5) \
                   AND ((t.search @@ q.q OR r.search @@ q.q) OR ($6 AND cardinality(t.error_strings) > 0)) \
                 ORDER BY r.at DESC LIMIT $7",
            )
            .bind(org.as_uuid())
            .bind(repos.as_slice())
            .bind(&q.text)
            .bind(exclude.as_slice())
            .bind(q.as_of)
            .bind(!query_errors.is_empty())
            .bind(SIMILAR_CANDIDATES)
            .fetch_all(&mut *tx)
            .await?;
            let mut hits = Vec::new();
            for row in rows {
                let errors = error_overlap(query_errors, &error_tokens(row.error_strings.iter().map(String::as_str)));
                let mut matched_on = Vec::new();
                if row.text_match {
                    matched_on.push(SimilarMatch::Text);
                }
                if errors > 0.0 {
                    matched_on.push(SimilarMatch::ErrorString);
                }
                if matched_on.is_empty() {
                    continue;
                }
                let text = if row.text_match { 0.5 + 0.5 * row.rank.min(1.0) } else { 0.0 };
                hits.push((similarity_score(text, errors), matched_on, row));
            }
            hits.sort_by(|a, b| b.0.total_cmp(&a.0).then(b.2.at.cmp(&a.2.at)).then(a.2.id.cmp(&b.2.id)));
            hits.truncate(usize::from(limit.clamp(1, SIMILAR_LIMIT_MAX)));
            let confirmed: Vec<Uuid> = hits.iter().filter_map(|h| h.2.confirmed_cause).collect();
            let mut citations = load_citations(&mut tx, org, &confirmed).await?;
            tx.commit().await?;
            let out = hits
                .into_iter()
                .map(|(score, matched_on, row)| {
                    let cause_citations = row.confirmed_cause.and_then(|c| citations.remove(&c)).unwrap_or_default();
                    SimilarResolved {
                        ticket: TicketId::from_uuid(row.id),
                        identifier: row.source_identifier,
                        title: row.title,
                        url: row.source_url,
                        resolved_at: row.at,
                        root_cause: row.root_cause,
                        cause_citations,
                        fix_pr_url: row.fix_pr_url,
                        score,
                        matched_on,
                    }
                })
                .collect();
            Ok::<_, TxError<TicketStoreError>>(out)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use mage_core::testing::SeqIdGen;
    use mage_infra::pg::PgJobQueue;

    use super::PgTicketStore;
    use crate::adapters::driven::pg::feed::PgChangeFeed;
    use crate::testing::contract::{TicketFixture, contract_ticket_store};
    use crate::testing::pg::with_pg;

    #[tokio::test]
    async fn pg_ticket_store_passes_the_contract() {
        with_pg("pg_ticket_store_contract", |env| async move {
            contract_ticket_store(|clock| {
                let env = env.clone();
                async move {
                    let db = env.fresh().await;
                    let ids = Arc::new(SeqIdGen::new());
                    TicketFixture {
                        tickets: Arc::new(PgTicketStore::new(db.pools.clone(), clock.clone(), ids.clone())),
                        jobs: Arc::new(PgJobQueue::new(db.pools.clone(), clock.clone(), ids)),
                        feed: Arc::new(PgChangeFeed::new(db.pools.clone(), clock.clone())),
                        seed: db.seed(clock),
                    }
                }
            })
            .await;
        })
        .await;
    }
}
