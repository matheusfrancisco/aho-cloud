//! Job payloads, kinds, lanes and retry policies, shared by every role that enqueues or works jobs
//! (storage.md "`JobQueue`" and "Job kinds"; payload tables in intake.md, knowledge.md, triage.md,
//! runs.md, fix.md and server.md).
//!
//! Serde shape: `JobPayload` is an object tagged by `"kind"` whose value is `JobKind::as_str()`,
//! e.g. `{"kind":"notify.ticket","org_id":"org_…","ticket_id":"tkt_…","notice":{…}}`.

use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use time::OffsetDateTime;
use url::Url;

use crate::domain::budget::duration_ms;
use crate::domain::ids::{ArtifactId, BlobOid, CauseId, CommitSha, ContentHash, OrgId, RepoId, RunId, TicketId, TriageEvalId, UserId};
use crate::domain::repo::VersionReason;

/// Provider endpoint a webhook delivery arrived on (intake.md `WebhookInbox`). The serde names are
/// the `webhook_inbox.source` column values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum WebhookSource {
    /// Read-only GitHub App (`/hooks/github`).
    #[serde(rename = "github")]
    GitHub,
    /// Mage Fix App (`/hooks/github-fix`, fix.md).
    #[serde(rename = "github_fix")]
    GitHubFix,
    #[serde(rename = "linear")]
    Linear,
    #[serde(rename = "slack")]
    Slack,
}

impl WebhookSource {
    /// `webhook_inbox.source` value.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::GitHub => "github",
            Self::GitHubFix => "github_fix",
            Self::Linear => "linear",
            Self::Slack => "slack",
        }
    }

    /// Kind of the `IntakeDelivery` job that processes a delivery from this source.
    pub const fn job_kind(self) -> JobKind {
        match self {
            Self::GitHub => JobKind::IntakeGithub,
            Self::GitHubFix => JobKind::IntakeGithubFix,
            Self::Linear => JobKind::IntakeLinear,
            Self::Slack => JobKind::IntakeSlack,
        }
    }

    const fn from_job_kind(kind: JobKind) -> Option<Self> {
        match kind {
            JobKind::IntakeGithub => Some(Self::GitHub),
            JobKind::IntakeGithubFix => Some(Self::GitHubFix),
            JobKind::IntakeLinear => Some(Self::Linear),
            JobKind::IntakeSlack => Some(Self::Slack),
            _ => None,
        }
    }
}

/// Outbound message request carried by `notify.ticket` (intake.md "Notify"). Triage, runs and fix
/// enqueue it; only intake talks to Linear and Slack.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Notice {
    /// Triage succeeded (triage.md).
    TriageReady { run_id: RunId },
    /// A Diagnose run reached a verdict (runs.md `ApplyOutcome`).
    DiagnosisVerdict { cause_id: CauseId, run_id: RunId },
    /// A fix brief version was written (fix.md F1).
    FixBriefReady { cause_id: CauseId, version: u32 },
    /// The fix gate passed (fix.md F2).
    FixReady { cause_id: CauseId, run_id: RunId },
    /// A Fix run failed (fix.md F2).
    FixFailed { cause_id: CauseId, run_id: RunId },
    /// A fix PR was opened (fix.md F3). A new PR after a closed one is a new notice.
    PrOpened { cause_id: CauseId, url: Url },
}

impl Notice {
    /// Outbound identity of the notice per ticket: the `OutboundLog` key and the suffix of the
    /// `notify.ticket` dedupe key.
    pub fn key(&self) -> String {
        NoticeKey(self).to_string()
    }
}

/// Formats `Notice::key` without an intermediate `String` when embedded in another key.
struct NoticeKey<'a>(&'a Notice);

impl fmt::Display for NoticeKey<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Notice::TriageReady { run_id } => write!(f, "triage:{run_id}"),
            Notice::DiagnosisVerdict { run_id, .. } => write!(f, "diag:{run_id}"),
            Notice::FixBriefReady { cause_id, version } => write!(f, "brief:{cause_id}:v{version}"),
            Notice::FixReady { run_id, .. } => write!(f, "fix:{run_id}"),
            Notice::FixFailed { run_id, .. } => write!(f, "fixfail:{run_id}"),
            Notice::PrOpened { url, .. } => write!(f, "pr:{url}"),
        }
    }
}

/// Why a `triage.ticket` job was enqueued (triage.md "Jobs"). Absent in a payload means `Auto`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TriageTrigger {
    /// Intake (content changed) or runs (ticket back to `Triaged` with a changed hash).
    #[default]
    Auto,
    /// UI "Retriage" (`RequestRetriage`).
    Manual { by: UserId },
    /// The single automatic retry after an `Errored` triage run (`ApplyTriageOutcome`).
    Requeue,
}

/// Priority of one commit's `index.*` DAG, derived from the `VersionReason` that triggered
/// `index.version` and copied into every derived payload as `prio` (knowledge.md "Which shas get
/// indexed").
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IndexPriority {
    /// `Release(tag)`.
    Low,
    /// `DefaultHead`.
    Normal,
    /// `Deployed(env)`.
    High,
    /// `OnDemand(ticket)`.
    Highest,
}

impl IndexPriority {
    /// `jobs.priority` value (higher is leased first). `Normal` equals the default 0 of every job
    /// without a priority.
    pub const fn value(self) -> i16 {
        match self {
            Self::Low => -10,
            Self::Normal => 0,
            Self::High => 10,
            Self::Highest => 20,
        }
    }
}

impl From<&VersionReason> for IndexPriority {
    fn from(reason: &VersionReason) -> Self {
        match reason {
            VersionReason::OnDemand(_) => Self::Highest,
            VersionReason::Deployed(_) => Self::High,
            VersionReason::DefaultHead => Self::Normal,
            VersionReason::Release(_) => Self::Low,
        }
    }
}

/// Worker pool that leases a kind: the indexer leases `index.*`, the server everything else.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobLane {
    Server,
    Indexer,
}

impl JobLane {
    /// `jobs.lane` value.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Server => "server",
            Self::Indexer => "indexer",
        }
    }
}

/// How the backoff delay is randomized (`u` is the caller's jitter fraction in `[0, 1]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Jitter {
    /// `delay · (0.5 + 0.5·u)`: storage.md's default schedule.
    Equal,
    /// `delay · u`: intake and notify jobs (intake.md).
    Full,
}

/// Per-kind retry, lease and defer limits (storage.md "Backoff"; per-kind values in intake.md and
/// knowledge.md).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetryPolicy {
    /// Delay after the first failure, before jitter.
    #[serde(with = "duration_ms")]
    pub base: Duration,
    /// Upper bound of the unjittered delay.
    #[serde(with = "duration_ms")]
    pub cap: Duration,
    /// Leases counted before the job goes `dead`.
    pub max_attempts: u16,
    /// Lease length; the worker heartbeats every `lease / 3`.
    #[serde(with = "duration_ms")]
    pub lease: Duration,
    /// `defer` calls before the job goes `dead` with "defer limit".
    pub max_defers: u16,
    pub jitter: Jitter,
}

impl RetryPolicy {
    /// storage.md default: base 5 s, cap 15 min, 10 attempts, lease 60 s, 200 defers.
    pub const DEFAULT: Self = Self {
        base: Duration::from_secs(5),
        cap: Duration::from_secs(15 * 60),
        max_attempts: 10,
        lease: Duration::from_secs(60),
        max_defers: 200,
        jitter: Jitter::Equal,
    };

    /// intake.md: every intake and notify job backs off exponentially with full jitter, base 30 s,
    /// factor 2, cap 1 h.
    const INTAKE: Self = Self { base: Duration::from_secs(30), cap: Duration::from_secs(60 * 60), jitter: Jitter::Full, ..Self::DEFAULT };

    /// Delay before the next attempt after failure number `attempt` (1-based; 0 counts as 1):
    /// `min(cap, base · 2^(attempt−1))` scaled by the jitter mode. `jitter_fraction` is the caller's
    /// random (or hashed) `u`; values outside `[0, 1]` are clamped and NaN counts as 1, so the result
    /// never exceeds `cap`. `jitter_fraction = 1` gives the unjittered delay.
    pub fn backoff(&self, attempt: u32, jitter_fraction: f64) -> Duration {
        let doublings = attempt.saturating_sub(1);
        let delay = 1u32.checked_shl(doublings).and_then(|factor| self.base.checked_mul(factor)).map_or(self.cap, |d| d.min(self.cap));
        let u = if jitter_fraction.is_nan() { 1.0 } else { jitter_fraction.clamp(0.0, 1.0) };
        let scale = match self.jitter {
            Jitter::Equal => 0.5 + 0.5 * u,
            Jitter::Full => u,
        };
        delay.mul_f64(scale)
    }
}

/// Fieldless mirror of `JobPayload`: the `jobs.kind` string `area.name`. `IntakeDelivery` maps to
/// one of the four `intake.<source>` kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum JobKind {
    IntakeGithub,
    IntakeGithubFix,
    IntakeLinear,
    IntakeSlack,
    IntakeLinearIssue,
    IntakeGithubResync,
    IntakeLinearResync,
    IntakeGithubRedeliver,
    IntakeInboxPurge,
    NotifyTicket,
    TriageTicket,
    TriageEvalCase,
    IndexVersion,
    IndexFetch,
    IndexManifest,
    IndexParse,
    IndexErrorSites,
    IndexAppMap,
    IndexHistory,
    IndexBundleSource,
    IndexIngestScip,
    IndexReconcile,
    IndexGc,
    RunsApplyOutcome,
    RunsSourceReady,
    RunsReapLeases,
    FixBrief,
    FixOpenPr,
    FixPrEvent,
    AuthRotateSigningKey,
    AuthSweepSessions,
    VaultRewrap,
    OpsRetention,
    OpsDeleteOrg,
    OpsPurge,
}

const ALL_KINDS: [JobKind; 35] = [
    JobKind::IntakeGithub,
    JobKind::IntakeGithubFix,
    JobKind::IntakeLinear,
    JobKind::IntakeSlack,
    JobKind::IntakeLinearIssue,
    JobKind::IntakeGithubResync,
    JobKind::IntakeLinearResync,
    JobKind::IntakeGithubRedeliver,
    JobKind::IntakeInboxPurge,
    JobKind::NotifyTicket,
    JobKind::TriageTicket,
    JobKind::TriageEvalCase,
    JobKind::IndexVersion,
    JobKind::IndexFetch,
    JobKind::IndexManifest,
    JobKind::IndexParse,
    JobKind::IndexErrorSites,
    JobKind::IndexAppMap,
    JobKind::IndexHistory,
    JobKind::IndexBundleSource,
    JobKind::IndexIngestScip,
    JobKind::IndexReconcile,
    JobKind::IndexGc,
    JobKind::RunsApplyOutcome,
    JobKind::RunsSourceReady,
    JobKind::RunsReapLeases,
    JobKind::FixBrief,
    JobKind::FixOpenPr,
    JobKind::FixPrEvent,
    JobKind::AuthRotateSigningKey,
    JobKind::AuthSweepSessions,
    JobKind::VaultRewrap,
    JobKind::OpsRetention,
    JobKind::OpsDeleteOrg,
    JobKind::OpsPurge,
];

impl JobKind {
    /// Every kind, in declaration order.
    pub const fn all() -> &'static [JobKind] {
        &ALL_KINDS
    }

    /// `jobs.kind` value, e.g. `"triage.ticket"`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::IntakeGithub => "intake.github",
            Self::IntakeGithubFix => "intake.github_fix",
            Self::IntakeLinear => "intake.linear",
            Self::IntakeSlack => "intake.slack",
            Self::IntakeLinearIssue => "intake.linear_issue",
            Self::IntakeGithubResync => "intake.github_resync",
            Self::IntakeLinearResync => "intake.linear_resync",
            Self::IntakeGithubRedeliver => "intake.github_redeliver",
            Self::IntakeInboxPurge => "intake.inbox_purge",
            Self::NotifyTicket => "notify.ticket",
            Self::TriageTicket => "triage.ticket",
            Self::TriageEvalCase => "triage.eval_case",
            Self::IndexVersion => "index.version",
            Self::IndexFetch => "index.fetch",
            Self::IndexManifest => "index.manifest",
            Self::IndexParse => "index.parse",
            Self::IndexErrorSites => "index.error_sites",
            Self::IndexAppMap => "index.app_map",
            Self::IndexHistory => "index.history",
            Self::IndexBundleSource => "index.bundle_source",
            Self::IndexIngestScip => "index.ingest_scip",
            Self::IndexReconcile => "index.reconcile",
            Self::IndexGc => "index.gc",
            Self::RunsApplyOutcome => "runs.apply_outcome",
            Self::RunsSourceReady => "runs.source_ready",
            Self::RunsReapLeases => "runs.reap_leases",
            Self::FixBrief => "fix.brief",
            Self::FixOpenPr => "fix.open_pr",
            Self::FixPrEvent => "fix.pr_event",
            Self::AuthRotateSigningKey => "auth.rotate_signing_key",
            Self::AuthSweepSessions => "auth.sweep_sessions",
            Self::VaultRewrap => "vault.rewrap",
            Self::OpsRetention => "ops.retention",
            Self::OpsDeleteOrg => "ops.delete_org",
            Self::OpsPurge => "ops.purge",
        }
    }

    /// `index.*` → `Indexer`, everything else → `Server`.
    pub const fn lane(self) -> JobLane {
        match self {
            Self::IndexVersion
            | Self::IndexFetch
            | Self::IndexManifest
            | Self::IndexParse
            | Self::IndexErrorSites
            | Self::IndexAppMap
            | Self::IndexHistory
            | Self::IndexBundleSource
            | Self::IndexIngestScip
            | Self::IndexReconcile
            | Self::IndexGc => JobLane::Indexer,
            Self::IntakeGithub
            | Self::IntakeGithubFix
            | Self::IntakeLinear
            | Self::IntakeSlack
            | Self::IntakeLinearIssue
            | Self::IntakeGithubResync
            | Self::IntakeLinearResync
            | Self::IntakeGithubRedeliver
            | Self::IntakeInboxPurge
            | Self::NotifyTicket
            | Self::TriageTicket
            | Self::TriageEvalCase
            | Self::RunsApplyOutcome
            | Self::RunsSourceReady
            | Self::RunsReapLeases
            | Self::FixBrief
            | Self::FixOpenPr
            | Self::FixPrEvent
            | Self::AuthRotateSigningKey
            | Self::AuthSweepSessions
            | Self::VaultRewrap
            | Self::OpsRetention
            | Self::OpsDeleteOrg
            | Self::OpsPurge => JobLane::Server,
        }
    }

    /// Attempts and backoff per intake.md's job table, leases per knowledge.md's DAG table;
    /// `RetryPolicy::DEFAULT` where no doc overrides it.
    pub const fn retry_policy(self) -> RetryPolicy {
        const fn intake(max_attempts: u16) -> RetryPolicy {
            RetryPolicy { max_attempts, ..RetryPolicy::INTAKE }
        }
        const fn index(max_attempts: u16, lease_minutes: u64) -> RetryPolicy {
            RetryPolicy { max_attempts, lease: Duration::from_secs(lease_minutes * 60), ..RetryPolicy::DEFAULT }
        }
        match self {
            Self::IntakeGithub | Self::IntakeGithubFix | Self::IntakeLinear | Self::IntakeSlack => intake(5),
            Self::IntakeLinearIssue => intake(6),
            Self::IntakeGithubResync | Self::IntakeLinearResync | Self::IntakeGithubRedeliver | Self::IntakeInboxPurge => intake(3),
            Self::NotifyTicket => intake(8),
            Self::IndexVersion => index(5, 1),
            Self::IndexFetch => index(6, 10),
            Self::IndexManifest => index(5, 15),
            Self::IndexParse => index(3, 10),
            Self::IndexErrorSites | Self::IndexAppMap => index(3, 20),
            Self::IndexHistory => index(5, 30),
            Self::IndexBundleSource => index(4, 30),
            Self::IndexIngestScip => index(3, 60),
            Self::IndexReconcile => index(1, 10),
            Self::IndexGc => index(1, 60),
            Self::TriageTicket
            | Self::TriageEvalCase
            | Self::RunsApplyOutcome
            | Self::RunsSourceReady
            | Self::RunsReapLeases
            | Self::FixBrief
            | Self::FixOpenPr
            | Self::FixPrEvent
            | Self::AuthRotateSigningKey
            | Self::AuthSweepSessions
            | Self::VaultRewrap
            | Self::OpsRetention
            | Self::OpsDeleteOrg
            | Self::OpsPurge => RetryPolicy::DEFAULT,
        }
    }
}

impl fmt::Display for JobKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A string that is not a `JobKind::as_str()` value.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown job kind `{0}`")]
pub struct UnknownJobKind(pub String);

impl FromStr for JobKind {
    type Err = UnknownJobKind;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        ALL_KINDS.iter().copied().find(|kind| kind.as_str() == s).ok_or_else(|| UnknownJobKind(s.to_owned()))
    }
}

impl Serialize for JobKind {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for JobKind {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct KindVisitor;

        impl serde::de::Visitor<'_> for KindVisitor {
            type Value = JobKind;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a job kind such as \"triage.ticket\"")
            }

            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<JobKind, E> {
                v.parse().map_err(E::custom)
            }
        }

        deserializer.deserialize_str(KindVisitor)
    }
}

/// One variant per job kind; fields as documented by the owning doc. Every variant carries
/// `org_id` except platform jobs (and `IntakeDelivery`, whose tenant is resolved by the job).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", expecting = "a job payload object whose `kind` is a known job kind and whose fields match it")]
pub enum JobPayload {
    // ---- intake.md ----
    /// `intake.linear_issue`: (re)ingest one Linear issue, debounced.
    #[serde(rename = "intake.linear_issue")]
    IntakeLinearIssue { org_id: OrgId, issue_id: String },
    /// `intake.github_resync`: periodic chain, 60 min.
    #[serde(rename = "intake.github_resync")]
    IntakeGithubResync { org_id: OrgId },
    /// `intake.linear_resync`: periodic chain; `since` is the checkpoint carried forward.
    #[serde(rename = "intake.linear_resync")]
    IntakeLinearResync {
        org_id: OrgId,
        #[serde(with = "time::serde::rfc3339")]
        since: OffsetDateTime,
    },
    /// `intake.github_redeliver` (platform; both GitHub apps).
    #[serde(rename = "intake.github_redeliver")]
    IntakeGithubRedeliver {
        #[serde(with = "time::serde::rfc3339")]
        since: OffsetDateTime,
    },
    /// `intake.inbox_purge` (platform).
    #[serde(rename = "intake.inbox_purge")]
    IntakeInboxPurge,
    /// `notify.ticket`: deliver one notice to Linear / Slack.
    #[serde(rename = "notify.ticket")]
    NotifyTicket { org_id: OrgId, ticket_id: TicketId, notice: Notice },

    // ---- triage.md ----
    #[serde(rename = "triage.ticket")]
    TriageTicket {
        org_id: OrgId,
        ticket_id: TicketId,
        content_hash: ContentHash,
        #[serde(default)]
        trigger: TriageTrigger,
    },
    #[serde(rename = "triage.eval_case")]
    TriageEvalCase { org_id: OrgId, eval_id: TriageEvalId, ticket_id: TicketId, repeat: u8 },

    // ---- knowledge.md ----
    /// Entry of the per-commit DAG.
    #[serde(rename = "index.version")]
    IndexVersion { org_id: OrgId, repo_id: RepoId, sha: CommitSha, reason: VersionReason },
    #[serde(rename = "index.fetch")]
    IndexFetch { org_id: OrgId, repo_id: RepoId, sha: CommitSha, prio: IndexPriority },
    #[serde(rename = "index.manifest")]
    IndexManifest { org_id: OrgId, repo_id: RepoId, sha: CommitSha, prio: IndexPriority },
    /// One parse batch (`batch` numbers from `begin_layer_batches`).
    #[serde(rename = "index.parse")]
    IndexParse { org_id: OrgId, repo_id: RepoId, sha: CommitSha, prio: IndexPriority, batch: u32, oids: Vec<BlobOid> },
    #[serde(rename = "index.error_sites")]
    IndexErrorSites { org_id: OrgId, repo_id: RepoId, sha: CommitSha, prio: IndexPriority },
    #[serde(rename = "index.app_map")]
    IndexAppMap { org_id: OrgId, repo_id: RepoId, sha: CommitSha, prio: IndexPriority },
    #[serde(rename = "index.history")]
    IndexHistory { org_id: OrgId, repo_id: RepoId, sha: CommitSha, prio: IndexPriority },
    #[serde(rename = "index.bundle_source")]
    IndexBundleSource { org_id: OrgId, repo_id: RepoId, sha: CommitSha, prio: IndexPriority },
    /// Ingest a Learn run's `ScipIndex` artifact; `key` is the artifact's object key
    /// (`Artifact.key`).
    #[serde(rename = "index.ingest_scip")]
    IndexIngestScip { org_id: OrgId, repo_id: RepoId, sha: CommitSha, run_id: RunId, artifact_id: ArtifactId, key: String },
    /// Platform; self-reschedules every 10 min.
    #[serde(rename = "index.reconcile")]
    IndexReconcile,
    /// Platform; daily.
    #[serde(rename = "index.gc")]
    IndexGc,

    // ---- runs.md ----
    /// Project a run's status onto its subject; enqueued at creation and on every terminal
    /// transition.
    #[serde(rename = "runs.apply_outcome")]
    RunsApplyOutcome { org_id: OrgId, run_id: RunId },
    /// The SourceBundle for `(repo_id, sha)` was recorded, or bundling failed permanently.
    #[serde(rename = "runs.source_ready")]
    RunsSourceReady { org_id: OrgId, repo_id: RepoId, sha: CommitSha },
    /// Platform; self-reschedules every 15 s.
    #[serde(rename = "runs.reap_leases")]
    RunsReapLeases,

    // ---- fix.md ----
    #[serde(rename = "fix.brief")]
    FixBrief { org_id: OrgId, cause_id: CauseId, diagnose_run_id: RunId },
    #[serde(rename = "fix.open_pr")]
    FixOpenPr { org_id: OrgId, ticket_id: TicketId, fix_run_id: RunId },
    /// GitHub `pull_request` event on a `mage/*` branch; `action` is GitHub's action string.
    #[serde(rename = "fix.pr_event")]
    FixPrEvent { org_id: OrgId, repo_id: RepoId, number: u64, action: String, merged: bool },

    // ---- server.md (platform tickers) ----
    #[serde(rename = "auth.rotate_signing_key")]
    AuthRotateSigningKey,
    #[serde(rename = "auth.sweep_sessions")]
    AuthSweepSessions,
    #[serde(rename = "vault.rewrap")]
    VaultRewrap,

    // ---- storage.md ----
    /// Per-org daily chain.
    #[serde(rename = "ops.retention")]
    OpsRetention { org_id: OrgId },
    #[serde(rename = "ops.delete_org")]
    OpsDeleteOrg { org_id: OrgId },
    /// Platform ticker, daily.
    #[serde(rename = "ops.purge")]
    OpsPurge,

    /// `intake.github` / `intake.github_fix` / `intake.linear` / `intake.slack`: process one stored
    /// webhook delivery (tenant unknown until the job resolves it). Serialized as
    /// `{"kind":"intake.<source>","delivery_id":…}`: the kind tag carries the source, so this one
    /// variant covers four kinds. Kept last because serde requires untagged variants at the end.
    #[serde(untagged)]
    IntakeDelivery {
        #[serde(rename = "kind", with = "intake_delivery_kind")]
        source: WebhookSource,
        delivery_id: String,
    },
}

/// `IntakeDelivery.source` ⇄ its `intake.<source>` kind string.
mod intake_delivery_kind {
    use serde::{Deserialize, Deserializer, Serializer};

    use super::{JobKind, WebhookSource};

    pub(super) fn serialize<S: Serializer>(source: &WebhookSource, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(source.job_kind().as_str())
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<WebhookSource, D::Error> {
        let kind = JobKind::deserialize(d)?;
        WebhookSource::from_job_kind(kind).ok_or_else(|| serde::de::Error::custom(format_args!("`{kind}` is not a webhook delivery kind")))
    }
}

impl JobPayload {
    pub fn kind(&self) -> JobKind {
        match self {
            Self::IntakeDelivery { source, .. } => source.job_kind(),
            Self::IntakeLinearIssue { .. } => JobKind::IntakeLinearIssue,
            Self::IntakeGithubResync { .. } => JobKind::IntakeGithubResync,
            Self::IntakeLinearResync { .. } => JobKind::IntakeLinearResync,
            Self::IntakeGithubRedeliver { .. } => JobKind::IntakeGithubRedeliver,
            Self::IntakeInboxPurge => JobKind::IntakeInboxPurge,
            Self::NotifyTicket { .. } => JobKind::NotifyTicket,
            Self::TriageTicket { .. } => JobKind::TriageTicket,
            Self::TriageEvalCase { .. } => JobKind::TriageEvalCase,
            Self::IndexVersion { .. } => JobKind::IndexVersion,
            Self::IndexFetch { .. } => JobKind::IndexFetch,
            Self::IndexManifest { .. } => JobKind::IndexManifest,
            Self::IndexParse { .. } => JobKind::IndexParse,
            Self::IndexErrorSites { .. } => JobKind::IndexErrorSites,
            Self::IndexAppMap { .. } => JobKind::IndexAppMap,
            Self::IndexHistory { .. } => JobKind::IndexHistory,
            Self::IndexBundleSource { .. } => JobKind::IndexBundleSource,
            Self::IndexIngestScip { .. } => JobKind::IndexIngestScip,
            Self::IndexReconcile => JobKind::IndexReconcile,
            Self::IndexGc => JobKind::IndexGc,
            Self::RunsApplyOutcome { .. } => JobKind::RunsApplyOutcome,
            Self::RunsSourceReady { .. } => JobKind::RunsSourceReady,
            Self::RunsReapLeases => JobKind::RunsReapLeases,
            Self::FixBrief { .. } => JobKind::FixBrief,
            Self::FixOpenPr { .. } => JobKind::FixOpenPr,
            Self::FixPrEvent { .. } => JobKind::FixPrEvent,
            Self::AuthRotateSigningKey => JobKind::AuthRotateSigningKey,
            Self::AuthSweepSessions => JobKind::AuthSweepSessions,
            Self::VaultRewrap => JobKind::VaultRewrap,
            Self::OpsRetention { .. } => JobKind::OpsRetention,
            Self::OpsDeleteOrg { .. } => JobKind::OpsDeleteOrg,
            Self::OpsPurge => JobKind::OpsPurge,
        }
    }

    /// Tenant written to `jobs.org_id`. `None` for platform jobs and for `IntakeDelivery`.
    pub fn org_id(&self) -> Option<OrgId> {
        match self {
            Self::IntakeLinearIssue { org_id, .. }
            | Self::IntakeGithubResync { org_id }
            | Self::IntakeLinearResync { org_id, .. }
            | Self::NotifyTicket { org_id, .. }
            | Self::TriageTicket { org_id, .. }
            | Self::TriageEvalCase { org_id, .. }
            | Self::IndexVersion { org_id, .. }
            | Self::IndexFetch { org_id, .. }
            | Self::IndexManifest { org_id, .. }
            | Self::IndexParse { org_id, .. }
            | Self::IndexErrorSites { org_id, .. }
            | Self::IndexAppMap { org_id, .. }
            | Self::IndexHistory { org_id, .. }
            | Self::IndexBundleSource { org_id, .. }
            | Self::IndexIngestScip { org_id, .. }
            | Self::RunsApplyOutcome { org_id, .. }
            | Self::RunsSourceReady { org_id, .. }
            | Self::FixBrief { org_id, .. }
            | Self::FixOpenPr { org_id, .. }
            | Self::FixPrEvent { org_id, .. }
            | Self::OpsRetention { org_id }
            | Self::OpsDeleteOrg { org_id } => Some(*org_id),
            Self::IntakeDelivery { .. }
            | Self::IntakeGithubRedeliver { .. }
            | Self::IntakeInboxPurge
            | Self::IndexReconcile
            | Self::IndexGc
            | Self::RunsReapLeases
            | Self::AuthRotateSigningKey
            | Self::AuthSweepSessions
            | Self::VaultRewrap
            | Self::OpsPurge => None,
        }
    }

    /// `jobs.priority` (higher first). The `index.*` DAG of a commit is ordered by its
    /// `VersionReason` (`IndexPriority`); every other job is 0.
    pub fn priority(&self) -> i16 {
        match self {
            Self::IndexVersion { reason, .. } => IndexPriority::from(reason).value(),
            Self::IndexFetch { prio, .. }
            | Self::IndexManifest { prio, .. }
            | Self::IndexParse { prio, .. }
            | Self::IndexErrorSites { prio, .. }
            | Self::IndexAppMap { prio, .. }
            | Self::IndexHistory { prio, .. }
            | Self::IndexBundleSource { prio, .. } => prio.value(),
            Self::IntakeDelivery { .. }
            | Self::IntakeLinearIssue { .. }
            | Self::IntakeGithubResync { .. }
            | Self::IntakeLinearResync { .. }
            | Self::IntakeGithubRedeliver { .. }
            | Self::IntakeInboxPurge
            | Self::NotifyTicket { .. }
            | Self::TriageTicket { .. }
            | Self::TriageEvalCase { .. }
            | Self::IndexIngestScip { .. }
            | Self::IndexReconcile
            | Self::IndexGc
            | Self::RunsApplyOutcome { .. }
            | Self::RunsSourceReady { .. }
            | Self::RunsReapLeases
            | Self::FixBrief { .. }
            | Self::FixOpenPr { .. }
            | Self::FixPrEvent { .. }
            | Self::AuthRotateSigningKey
            | Self::AuthSweepSessions
            | Self::VaultRewrap
            | Self::OpsRetention { .. }
            | Self::OpsDeleteOrg { .. }
            | Self::OpsPurge => 0,
        }
    }

    /// Documented dedupe key (scoped per `(org_id, kind)` by the queue's unique index).
    ///
    /// `None` for `IntakeDelivery` (the inbox already deduplicates) and for kinds whose documented
    /// key needs data the payload does not carry; their enqueuers set the key themselves:
    /// `runs.apply_outcome:{run_id}:{status}`, `fix.open_pr:{ticket_id}:{changeset_digest_hex}`,
    /// `fix.pr_event:{repo_id}:{number}:{delivery_id}`, `{kind}:{bucket}` for the `auth.*` /
    /// `vault.*` tickers and `ops.purge:{day}`.
    pub fn dedupe_key(&self) -> Option<String> {
        let kind = self.kind();
        match self {
            Self::IntakeLinearIssue { issue_id, .. } => Some(format!("{kind}:{issue_id}")),
            Self::IntakeGithubResync { .. }
            | Self::IntakeLinearResync { .. }
            | Self::IntakeGithubRedeliver { .. }
            | Self::IntakeInboxPurge
            | Self::IndexReconcile
            | Self::IndexGc
            | Self::RunsReapLeases => Some(kind.as_str().to_owned()),
            Self::NotifyTicket { ticket_id, notice, .. } => Some(format!("notify:{ticket_id}:{}", NoticeKey(notice))),
            Self::TriageTicket { ticket_id, content_hash, .. } => Some(format!("{kind}:{ticket_id}:{content_hash}")),
            Self::TriageEvalCase { eval_id, ticket_id, repeat, .. } => Some(format!("{kind}:{eval_id}:{ticket_id}:{repeat}")),
            Self::IndexVersion { repo_id, sha, .. }
            | Self::IndexFetch { repo_id, sha, .. }
            | Self::IndexManifest { repo_id, sha, .. }
            | Self::IndexErrorSites { repo_id, sha, .. }
            | Self::IndexAppMap { repo_id, sha, .. }
            | Self::IndexHistory { repo_id, sha, .. }
            | Self::IndexBundleSource { repo_id, sha, .. }
            | Self::RunsSourceReady { repo_id, sha, .. } => Some(format!("{kind}:{repo_id}:{sha}")),
            Self::IndexParse { repo_id, sha, batch, .. } => Some(format!("{kind}:{repo_id}:{sha}:{batch}")),
            Self::IndexIngestScip { artifact_id, .. } => Some(format!("{kind}:{artifact_id}")),
            Self::FixBrief { cause_id, diagnose_run_id, .. } => Some(format!("{kind}:{cause_id}:{diagnose_run_id}")),
            Self::OpsRetention { org_id } | Self::OpsDeleteOrg { org_id } => Some(format!("{kind}:{org_id}")),
            Self::IntakeDelivery { .. }
            | Self::RunsApplyOutcome { .. }
            | Self::FixOpenPr { .. }
            | Self::FixPrEvent { .. }
            | Self::AuthRotateSigningKey
            | Self::AuthSweepSessions
            | Self::VaultRewrap
            | Self::OpsPurge => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use serde_json::json;
    use time::macros::datetime;
    use uuid::Uuid;

    use super::*;

    fn org() -> OrgId {
        OrgId::from_uuid(Uuid::from_u128(1))
    }
    fn ticket() -> TicketId {
        TicketId::from_uuid(Uuid::from_u128(2))
    }
    fn repo() -> RepoId {
        RepoId::from_uuid(Uuid::from_u128(3))
    }
    fn run() -> RunId {
        RunId::from_uuid(Uuid::from_u128(4))
    }
    fn cause() -> CauseId {
        CauseId::from_uuid(Uuid::from_u128(5))
    }
    fn sha() -> CommitSha {
        CommitSha::from_bytes([0xab; 20])
    }

    const PLATFORM_KINDS: [JobKind; 13] = [
        JobKind::IntakeGithub,
        JobKind::IntakeGithubFix,
        JobKind::IntakeLinear,
        JobKind::IntakeSlack,
        JobKind::IntakeGithubRedeliver,
        JobKind::IntakeInboxPurge,
        JobKind::IndexReconcile,
        JobKind::IndexGc,
        JobKind::RunsReapLeases,
        JobKind::AuthRotateSigningKey,
        JobKind::AuthSweepSessions,
        JobKind::VaultRewrap,
        JobKind::OpsPurge,
    ];

    /// One payload per kind.
    fn samples() -> Vec<JobPayload> {
        let (org_id, ticket_id, repo_id, run_id, cause_id, sha) = (org(), ticket(), repo(), run(), cause(), sha());
        let prio = IndexPriority::High;
        let since = datetime!(2026-10-05 09:12:30.5 UTC);
        let mut out: Vec<JobPayload> = [WebhookSource::GitHub, WebhookSource::GitHubFix, WebhookSource::Linear, WebhookSource::Slack]
            .into_iter()
            .map(|source| JobPayload::IntakeDelivery { source, delivery_id: "d-1".into() })
            .collect();
        out.extend([
            JobPayload::IntakeLinearIssue { org_id, issue_id: "iss-1".into() },
            JobPayload::IntakeGithubResync { org_id },
            JobPayload::IntakeLinearResync { org_id, since },
            JobPayload::IntakeGithubRedeliver { since },
            JobPayload::IntakeInboxPurge,
            JobPayload::NotifyTicket { org_id, ticket_id, notice: Notice::FixBriefReady { cause_id, version: 3 } },
            JobPayload::TriageTicket {
                org_id,
                ticket_id,
                content_hash: ContentHash::from_bytes([7; 32]),
                trigger: TriageTrigger::Manual { by: UserId::from_uuid(Uuid::from_u128(6)) },
            },
            JobPayload::TriageEvalCase { org_id, eval_id: TriageEvalId::from_uuid(Uuid::from_u128(8)), ticket_id, repeat: 2 },
            JobPayload::IndexVersion { org_id, repo_id, sha, reason: VersionReason::Deployed("production".into()) },
            JobPayload::IndexFetch { org_id, repo_id, sha, prio },
            JobPayload::IndexManifest { org_id, repo_id, sha, prio },
            JobPayload::IndexParse {
                org_id,
                repo_id,
                sha,
                prio,
                batch: 4,
                oids: vec![BlobOid::from_bytes([1; 20]), BlobOid::from_bytes([2; 20])],
            },
            JobPayload::IndexErrorSites { org_id, repo_id, sha, prio },
            JobPayload::IndexAppMap { org_id, repo_id, sha, prio },
            JobPayload::IndexHistory { org_id, repo_id, sha, prio },
            JobPayload::IndexBundleSource { org_id, repo_id, sha, prio },
            JobPayload::IndexIngestScip {
                org_id,
                repo_id,
                sha,
                run_id,
                artifact_id: ArtifactId::from_uuid(Uuid::from_u128(9)),
                key: "org/x/runs/y/scip_index/scip-go.scip".into(),
            },
            JobPayload::IndexReconcile,
            JobPayload::IndexGc,
            JobPayload::RunsApplyOutcome { org_id, run_id },
            JobPayload::RunsSourceReady { org_id, repo_id, sha },
            JobPayload::RunsReapLeases,
            JobPayload::FixBrief { org_id, cause_id, diagnose_run_id: run_id },
            JobPayload::FixOpenPr { org_id, ticket_id, fix_run_id: run_id },
            JobPayload::FixPrEvent { org_id, repo_id, number: 42, action: "closed".into(), merged: false },
            JobPayload::AuthRotateSigningKey,
            JobPayload::AuthSweepSessions,
            JobPayload::VaultRewrap,
            JobPayload::OpsRetention { org_id },
            JobPayload::OpsDeleteOrg { org_id },
            JobPayload::OpsPurge,
        ]);
        out
    }

    #[test]
    fn notice_keys_match_documented_formats() {
        let (cause_id, run_id) = (cause(), run());
        let url = Url::parse("https://github.com/hoophq/hoop/pull/412").unwrap();
        let cases = [
            (Notice::TriageReady { run_id }, format!("triage:{run_id}")),
            (Notice::DiagnosisVerdict { cause_id, run_id }, format!("diag:{run_id}")),
            (Notice::FixBriefReady { cause_id, version: 3 }, format!("brief:{cause_id}:v3")),
            (Notice::FixReady { cause_id, run_id }, format!("fix:{run_id}")),
            (Notice::FixFailed { cause_id, run_id }, format!("fixfail:{run_id}")),
            (Notice::PrOpened { cause_id, url }, "pr:https://github.com/hoophq/hoop/pull/412".to_owned()),
        ];
        for (notice, key) in cases {
            assert_eq!(notice.key(), key, "{notice:?}");
        }
        assert!(Notice::TriageReady { run_id }.key().starts_with("triage:run_"));
    }

    #[test]
    fn job_kind_strings_round_trip_and_are_unique() {
        let mut seen = BTreeSet::new();
        for &kind in JobKind::all() {
            let s = kind.as_str();
            let (area, name) = s.split_once('.').unwrap();
            assert!(!area.is_empty() && !name.is_empty() && !name.contains('.'), "{s}");
            assert!(s.bytes().all(|b| b.is_ascii_lowercase() || b == b'.' || b == b'_'), "{s}");
            assert!(seen.insert(s), "duplicate kind string {s}");
            assert_eq!(s.parse::<JobKind>().unwrap(), kind);
            let json = serde_json::to_string(&kind).unwrap();
            assert_eq!(json, format!("\"{s}\""));
            assert_eq!(serde_json::from_str::<JobKind>(&json).unwrap(), kind);
        }
        assert_eq!("index.bogus".parse::<JobKind>(), Err(UnknownJobKind("index.bogus".into())));
        assert!("Triage.Ticket".parse::<JobKind>().is_err());
        assert!(serde_json::from_str::<JobKind>("\"intake\"").is_err());
    }

    #[test]
    fn lane_is_indexer_exactly_for_index_kinds() {
        for &kind in JobKind::all() {
            let expected = if kind.as_str().starts_with("index.") { JobLane::Indexer } else { JobLane::Server };
            assert_eq!(kind.lane(), expected, "{kind}");
        }
        assert_eq!(JobLane::Indexer.as_str(), "indexer");
        assert_eq!(serde_json::to_value(JobLane::Server).unwrap(), json!("server"));
    }

    #[test]
    fn every_kind_has_a_payload_whose_json_tag_is_its_kind_and_round_trips() {
        let samples = samples();
        let kinds: BTreeSet<JobKind> = samples.iter().map(JobPayload::kind).collect();
        assert_eq!(kinds.len(), samples.len(), "one sample per kind");
        assert_eq!(kinds, JobKind::all().iter().copied().collect::<BTreeSet<_>>());
        for payload in samples {
            let value = serde_json::to_value(&payload).unwrap();
            assert_eq!(value["kind"], json!(payload.kind().as_str()), "{value}");
            let back: JobPayload = serde_json::from_value(value).unwrap();
            assert_eq!(back, payload);
            let text = serde_json::to_string(&payload).unwrap();
            assert_eq!(serde_json::from_str::<JobPayload>(&text).unwrap(), payload);
        }
    }

    #[test]
    fn payload_json_shapes() {
        let (org_id, ticket_id, cause_id, run_id) = (org(), ticket(), cause(), run());
        let notify = JobPayload::NotifyTicket { org_id, ticket_id, notice: Notice::DiagnosisVerdict { cause_id, run_id } };
        assert_eq!(
            serde_json::to_value(&notify).unwrap(),
            json!({
                "kind": "notify.ticket",
                "org_id": org_id.to_string(),
                "ticket_id": ticket_id.to_string(),
                "notice": {
                    "type": "diagnosis_verdict",
                    "cause_id": cause_id.to_string(),
                    "run_id": run_id.to_string(),
                },
            })
        );

        let delivery = JobPayload::IntakeDelivery { source: WebhookSource::GitHubFix, delivery_id: "d-9".into() };
        assert_eq!(serde_json::to_value(&delivery).unwrap(), json!({ "kind": "intake.github_fix", "delivery_id": "d-9" }));

        let fetch = JobPayload::IndexFetch { org_id, repo_id: repo(), sha: sha(), prio: IndexPriority::Highest };
        assert_eq!(
            serde_json::to_value(&fetch).unwrap(),
            json!({
                "kind": "index.fetch",
                "org_id": org_id.to_string(),
                "repo_id": repo().to_string(),
                "sha": "ab".repeat(20),
                "prio": "highest",
            })
        );

        assert_eq!(serde_json::to_value(&JobPayload::IndexReconcile).unwrap(), json!({ "kind": "index.reconcile" }));
        assert_eq!(
            serde_json::to_value(&JobPayload::IntakeGithubRedeliver { since: datetime!(2026-10-05 09:12 UTC) }).unwrap(),
            json!({ "kind": "intake.github_redeliver", "since": "2026-10-05T09:12:00Z" })
        );
    }

    #[test]
    fn deserialization_rejects_unknown_or_mismatched_kinds() {
        for bad in [
            json!({ "kind": "intake.bogus", "delivery_id": "d" }),
            json!({ "kind": "triage.ticket", "delivery_id": "d" }),
            json!({ "kind": "intake.github" }),
            json!({ "delivery_id": "d" }),
            json!({ "kind": "ops.retention" }),
        ] {
            assert!(serde_json::from_value::<JobPayload>(bad.clone()).is_err(), "{bad}");
        }
        let slack: JobPayload = serde_json::from_value(json!({ "kind": "intake.slack", "delivery_id": "Ev1" })).unwrap();
        assert_eq!(slack, JobPayload::IntakeDelivery { source: WebhookSource::Slack, delivery_id: "Ev1".into() });
        assert_eq!(slack.kind(), JobKind::IntakeSlack);
    }

    #[test]
    fn triage_ticket_trigger_defaults_to_auto() {
        let hash = ContentHash::from_bytes([0x5c; 32]);
        let base = json!({
            "kind": "triage.ticket",
            "org_id": org().to_string(),
            "ticket_id": ticket().to_string(),
            "content_hash": hash.to_string(),
        });
        let parsed: JobPayload = serde_json::from_value(base.clone()).unwrap();
        assert_eq!(
            parsed,
            JobPayload::TriageTicket { org_id: org(), ticket_id: ticket(), content_hash: hash, trigger: TriageTrigger::Auto }
        );

        let by = UserId::from_uuid(Uuid::from_u128(77));
        let mut manual = base;
        manual["trigger"] = json!({ "type": "manual", "by": by.to_string() });
        let parsed: JobPayload = serde_json::from_value(manual).unwrap();
        assert!(matches!(
            parsed,
            JobPayload::TriageTicket { trigger: TriageTrigger::Manual { by: b }, .. } if b == by
        ));
    }

    #[test]
    fn dedupe_keys_match_documented_formats() {
        let (org_id, ticket_id, repo_id, run_id, cause_id, sha) = (org(), ticket(), repo(), run(), cause(), sha());
        let sha_hex = "ab".repeat(20);
        let prio = IndexPriority::Normal;
        let hash = ContentHash::from_bytes([0x0f; 32]);
        let eval_id = TriageEvalId::from_uuid(Uuid::from_u128(8));
        let artifact_id = ArtifactId::from_uuid(Uuid::from_u128(9));
        let since = datetime!(2026-10-05 00:00 UTC);
        let cases = [
            (JobPayload::IntakeLinearIssue { org_id, issue_id: "abc-123".into() }, "intake.linear_issue:abc-123".to_owned()),
            (JobPayload::IntakeGithubResync { org_id }, "intake.github_resync".to_owned()),
            (JobPayload::IntakeLinearResync { org_id, since }, "intake.linear_resync".to_owned()),
            (JobPayload::IntakeGithubRedeliver { since }, "intake.github_redeliver".to_owned()),
            (JobPayload::IntakeInboxPurge, "intake.inbox_purge".to_owned()),
            (
                JobPayload::NotifyTicket { org_id, ticket_id, notice: Notice::TriageReady { run_id } },
                format!("notify:{ticket_id}:triage:{run_id}"),
            ),
            (
                JobPayload::NotifyTicket { org_id, ticket_id, notice: Notice::DiagnosisVerdict { cause_id, run_id } },
                format!("notify:{ticket_id}:diag:{run_id}"),
            ),
            (
                JobPayload::TriageTicket { org_id, ticket_id, content_hash: hash, trigger: TriageTrigger::Requeue },
                format!("triage.ticket:{ticket_id}:{}", "0f".repeat(32)),
            ),
            (JobPayload::TriageEvalCase { org_id, eval_id, ticket_id, repeat: 3 }, format!("triage.eval_case:{eval_id}:{ticket_id}:3")),
            (
                JobPayload::IndexVersion { org_id, repo_id, sha, reason: VersionReason::DefaultHead },
                format!("index.version:{repo_id}:{sha_hex}"),
            ),
            (JobPayload::IndexFetch { org_id, repo_id, sha, prio }, format!("index.fetch:{repo_id}:{sha_hex}")),
            (JobPayload::IndexManifest { org_id, repo_id, sha, prio }, format!("index.manifest:{repo_id}:{sha_hex}")),
            (JobPayload::IndexParse { org_id, repo_id, sha, prio, batch: 7, oids: vec![] }, format!("index.parse:{repo_id}:{sha_hex}:7")),
            (JobPayload::IndexErrorSites { org_id, repo_id, sha, prio }, format!("index.error_sites:{repo_id}:{sha_hex}")),
            (JobPayload::IndexAppMap { org_id, repo_id, sha, prio }, format!("index.app_map:{repo_id}:{sha_hex}")),
            (JobPayload::IndexHistory { org_id, repo_id, sha, prio }, format!("index.history:{repo_id}:{sha_hex}")),
            (JobPayload::IndexBundleSource { org_id, repo_id, sha, prio }, format!("index.bundle_source:{repo_id}:{sha_hex}")),
            (
                JobPayload::IndexIngestScip { org_id, repo_id, sha, run_id, artifact_id, key: "k".into() },
                format!("index.ingest_scip:{artifact_id}"),
            ),
            (JobPayload::IndexReconcile, "index.reconcile".to_owned()),
            (JobPayload::IndexGc, "index.gc".to_owned()),
            (JobPayload::RunsSourceReady { org_id, repo_id, sha }, format!("runs.source_ready:{repo_id}:{sha_hex}")),
            (JobPayload::RunsReapLeases, "runs.reap_leases".to_owned()),
            (JobPayload::FixBrief { org_id, cause_id, diagnose_run_id: run_id }, format!("fix.brief:{cause_id}:{run_id}")),
            (JobPayload::OpsRetention { org_id }, format!("ops.retention:{org_id}")),
            (JobPayload::OpsDeleteOrg { org_id }, format!("ops.delete_org:{org_id}")),
        ];
        for (payload, key) in cases {
            assert_eq!(payload.dedupe_key().as_deref(), Some(key.as_str()), "{payload:?}");
        }
        let delivery = JobPayload::IntakeDelivery { source: WebhookSource::GitHub, delivery_id: "d".into() };
        assert_eq!(delivery.dedupe_key(), None, "the inbox already deduplicates deliveries");
    }

    #[test]
    fn priority_follows_version_reason() {
        let (org_id, repo_id, sha) = (org(), repo(), sha());
        let reasons = [
            VersionReason::OnDemand(ticket()),
            VersionReason::Deployed("production".into()),
            VersionReason::DefaultHead,
            VersionReason::Release("v1.4.0".into()),
        ];
        let entry: Vec<i16> =
            reasons.iter().map(|reason| JobPayload::IndexVersion { org_id, repo_id, sha, reason: reason.clone() }.priority()).collect();
        assert!(entry.windows(2).all(|w| w[0] > w[1]), "OnDemand > Deployed > DefaultHead > Release: {entry:?}");

        // Derived jobs carry `prio` copied from the reason and sort exactly like the entry job.
        for (reason, expected) in reasons.iter().zip(&entry) {
            let prio = IndexPriority::from(reason);
            let derived = [
                JobPayload::IndexFetch { org_id, repo_id, sha, prio },
                JobPayload::IndexParse { org_id, repo_id, sha, prio, batch: 0, oids: vec![] },
                JobPayload::IndexBundleSource { org_id, repo_id, sha, prio },
            ];
            for payload in derived {
                assert_eq!(payload.priority(), *expected, "{payload:?}");
            }
        }

        // Default-branch indexing is "normal": the same 0 as every job without a priority.
        assert_eq!(IndexPriority::from(&VersionReason::DefaultHead).value(), 0);
        for payload in samples() {
            let has_index_prio = matches!(
                payload.kind(),
                JobKind::IndexVersion
                    | JobKind::IndexFetch
                    | JobKind::IndexManifest
                    | JobKind::IndexParse
                    | JobKind::IndexErrorSites
                    | JobKind::IndexAppMap
                    | JobKind::IndexHistory
                    | JobKind::IndexBundleSource
            );
            if !has_index_prio {
                assert_eq!(payload.priority(), 0, "{payload:?}");
            }
        }
    }

    #[test]
    fn org_id_is_none_only_for_platform_jobs() {
        for payload in samples() {
            let platform = PLATFORM_KINDS.contains(&payload.kind());
            let expected = if platform { None } else { Some(org()) };
            assert_eq!(payload.org_id(), expected, "{:?}", payload.kind());
        }
    }

    #[test]
    fn retry_policies_follow_doc_tables() {
        // (kind, max attempts, lease)
        let table = [
            (JobKind::IntakeGithub, 5, 60),
            (JobKind::IntakeSlack, 5, 60),
            (JobKind::IntakeLinearIssue, 6, 60),
            (JobKind::IntakeLinearResync, 3, 60),
            (JobKind::IntakeInboxPurge, 3, 60),
            (JobKind::NotifyTicket, 8, 60),
            (JobKind::IndexVersion, 5, 60),
            (JobKind::IndexFetch, 6, 600),
            (JobKind::IndexManifest, 5, 900),
            (JobKind::IndexParse, 3, 600),
            (JobKind::IndexErrorSites, 3, 1200),
            (JobKind::IndexAppMap, 3, 1200),
            (JobKind::IndexHistory, 5, 1800),
            (JobKind::IndexBundleSource, 4, 1800),
            (JobKind::IndexIngestScip, 3, 3600),
            (JobKind::IndexReconcile, 1, 600),
            (JobKind::IndexGc, 1, 3600),
        ];
        for (kind, attempts, lease_secs) in table {
            let policy = kind.retry_policy();
            assert_eq!(policy.max_attempts, attempts, "{kind}");
            assert_eq!(policy.lease, Duration::from_secs(lease_secs), "{kind}");
            assert_eq!(policy.max_defers, 200, "{kind}");
        }
        for &kind in JobKind::all() {
            let policy = kind.retry_policy();
            let intake_or_notify = kind.as_str().starts_with("intake.") || kind == JobKind::NotifyTicket;
            if intake_or_notify {
                assert_eq!(
                    (policy.base, policy.cap, policy.jitter),
                    (Duration::from_secs(30), Duration::from_secs(3600), Jitter::Full),
                    "{kind}"
                );
            } else {
                assert_eq!(
                    (policy.base, policy.cap, policy.jitter),
                    (Duration::from_secs(5), Duration::from_secs(900), Jitter::Equal),
                    "{kind}"
                );
            }
        }
        assert_eq!(JobKind::TriageTicket.retry_policy(), RetryPolicy::DEFAULT);
    }

    #[test]
    fn default_backoff_matches_storage_schedule() {
        // storage.md "Backoff": failure n → [lo, hi] seconds.
        let schedule = [
            (1, 2.5, 5.0),
            (2, 5.0, 10.0),
            (3, 10.0, 20.0),
            (4, 20.0, 40.0),
            (5, 40.0, 80.0),
            (6, 80.0, 160.0),
            (7, 160.0, 320.0),
            (8, 320.0, 640.0),
            (9, 450.0, 900.0),
        ];
        let policy = RetryPolicy::DEFAULT;
        for (attempt, lo, hi) in schedule {
            assert_eq!(policy.backoff(attempt, 0.0), Duration::from_secs_f64(lo), "#{attempt}");
            assert_eq!(policy.backoff(attempt, 1.0), Duration::from_secs_f64(hi), "#{attempt}");
        }
        let mid = policy.backoff(3, 0.5);
        assert!(mid > Duration::from_secs(10) && mid < Duration::from_secs(20), "{mid:?}");
    }

    #[test]
    fn backoff_boundaries() {
        let intake = JobKind::IntakeGithub.retry_policy();
        // attempt 1 = base (unjittered); full jitter spans down to zero.
        assert_eq!(intake.backoff(1, 1.0), Duration::from_secs(30));
        assert_eq!(intake.backoff(1, 0.0), Duration::ZERO);
        assert_eq!(intake.backoff(7, 1.0), Duration::from_secs(1920));
        // 30 s · 2^7 = 3840 s > cap.
        assert_eq!(intake.backoff(8, 1.0), Duration::from_secs(3600));
        assert_eq!(intake.backoff(8, 0.5), Duration::from_secs(1800));

        let default = RetryPolicy::DEFAULT;
        // Attempt 0 is treated as the first failure; huge attempts saturate at the cap.
        assert_eq!(default.backoff(0, 1.0), default.backoff(1, 1.0));
        for attempt in [32, 33, 64, u32::MAX] {
            assert_eq!(default.backoff(attempt, 1.0), default.cap, "#{attempt}");
            assert_eq!(intake.backoff(attempt, 1.0), intake.cap, "#{attempt}");
        }
        // Out-of-range jitter is clamped; NaN counts as 1 (never a zero or oversized delay).
        assert_eq!(default.backoff(2, 7.0), Duration::from_secs(10));
        assert_eq!(default.backoff(2, -3.0), Duration::from_secs(5));
        assert_eq!(default.backoff(2, f64::NAN), Duration::from_secs(10));
        assert_eq!(intake.backoff(2, f64::NAN), Duration::from_secs(60));
    }
}
