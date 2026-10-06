//! `mage_core::domain`: entities, value objects, state machines, invariants, proof tokens.
//! Pure: no I/O, no async. See `mage/docs/internals/domain.md` and the part docs' Additions.
//!
//! The common types are re-exported here (`mage_core::domain::RunId`, `…::JobPayload`, …);
//! the larger areas keep their own namespaces (`domain::fix`, `domain::recipe`,
//! `domain::evidence`, `domain::triage`, `domain::knowledge`, `domain::job`, `domain::gates`).

pub mod artifact;
pub mod brief;
pub mod budget;
pub mod cause;
pub mod citation;
pub mod customer;
pub mod error;
pub mod event;
pub mod evidence;
pub mod fix;
pub mod gate;
pub mod gates;
pub mod ids;
pub mod job;
pub mod knowledge;
pub mod manifest;
pub mod org;
pub mod path;
pub mod recipe;
pub mod repo;
pub mod resolution;
pub mod run;
pub mod signature;
pub mod ticket;
pub mod triage;
pub mod verify;

pub use artifact::{Artifact, ArtifactKind};
pub use brief::{FixBrief, FixBriefInit};
pub use budget::{Budget, BudgetLimit, Usage};
pub use cause::{Cause, CauseData, CauseEvent, CauseStatus};
pub use citation::{Citation, Confidence, ReproMethod};
pub use customer::{Customer, Deployment, DeploymentSource, ExternalRef};
pub use error::{DomainError, ensure_same_org};
pub use event::{RunEvent, RunEventKind};
pub use evidence::EvidenceChecked;
pub use gate::{Gate, GateEvent, GateKind, GateStatus};
pub use gates::{BootGatePassed, FixGatePassed, GateFailure, GateFailureCode, ReproGatePassed};
pub use ids::{
    ArtifactId, BlobOid, CauseId, CommitSha, ContentHash, CustomerId, FixBriefId, FixPullRequestId, GateId, InstallationId, JobId, OrgId,
    RecipeHash, RepoId, RepoVersionKey, RunId, RunnerId, TicketId, TreeOid, TriageEvalId, UserId,
};
pub use job::{IndexPriority, JobKind, JobLane, JobPayload, Notice, RetryPolicy, TriageTrigger, WebhookSource};
pub use manifest::{EntryKind, ManifestEntry};
pub use org::{Integration, IntegrationKind, IntegrationStatus, MemberRole, Membership, Org, User};
pub use path::{FileClass, Language, LineRange, RepoPath};
pub use recipe::{Recipe, RecipeData, RecipeEvent, RecipeStatus};
pub use repo::{
    AppMapRev, ErrorsRev, IndexRevs, Layer, LayerEvent, LayerMap, LayerState, ParserRev, Repo, RepoAccess, RepoVersion, SourceBundleRef,
    VersionReason,
};
pub use resolution::Resolution;
pub use run::{
    AbortReason, FailStage, GateFeedback, GateOutcome, HarnessKind, Run, RunCommand, RunData, RunError, RunErrorCode, RunInit, RunKind,
    RunStatus, RunStatusEvent, RunSubject, RunnerLease,
};
pub use signature::{FailureSignature, Matcher, SignatureKind, Verdict};
pub use ticket::{SlackThreadRef, TargetOrigin, Ticket, TicketContent, TicketData, TicketEvent, TicketSource, TicketStatus, TicketTarget};
pub use triage::{CauseDraft, CitationDraft, TriageResult};
pub use verify::{
    AttemptKind, DiagnoseResult, FixChecks, FixResult, FixStats, RecipeProposal, RunResult, ScriptKind, VerifyAttempt, VerifyOutcome,
    VerifyResult, VerifySpec,
};
