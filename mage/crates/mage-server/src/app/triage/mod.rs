//! Triage (triage.md): `triage.ticket` → preconditions → Triage run → synchronous evidence gate →
//! `runs.apply_outcome` → causes stored, ticket `Triaged`, `notify.ticket { TriageReady }`; plus
//! retriage, salvage, and the eval harness.
//!
//! | Module | Use case / piece |
//! |---|---|
//! | [`start`] | UC1 `StartTriage` (job `triage.ticket`) |
//! | [`gate`] | UC3 `EvidenceGate`, `salvage` |
//! | [`apply`] | UC4 `ApplyTriageOutcome` (job `runs.apply_outcome`, kind Triage) |
//! | [`retriage`] | UC5 `RequestRetriage` (UI) |
//! | [`eval`] | `StartTriageEval`, `StartEvalCase` (job `triage.eval_case`), labels, report |
//! | [`prompt`] | versioned prompt rendering (`prompt::v1`) |
//! | [`config`] | `TriageConfig` (`[server.triage]`), settings clamp, harness policy |
//! | [`view`] | `ManifestView` loader shared with fix |

pub mod apply;
pub mod config;
pub mod eval;
pub mod gate;
pub mod prompt;
pub mod retriage;
pub mod start;
pub mod view;

#[cfg(test)]
pub(crate) mod testkit;

pub use apply::{ApplyTriageDeps, ApplyTriageOutcome};
pub use config::TriageConfig;
pub use eval::{
    EvalReport, EvalVariant, LabelTriageEval, StartEvalCase, StartEvalCaseDeps, StartTriageEval, StartTriageEvalRequest, TriageEvalReport,
};
pub use gate::{CauseCheck, EvidenceGate, EvidenceGateDetails, TriageGateDecision, newest_evidence_details, salvage};
pub use retriage::RequestRetriage;
pub use start::{DeferReason, SkipReason, StartOutcome, StartTriage, StartTriageDeps};

use mage_core::ports::jobs::JobError;
use mage_core::ports::knowledge::KnowledgeError;
use mage_core::ports::repos::RepoStoreError;

use crate::error::AppError;

pub(crate) fn knowledge_error(e: KnowledgeError) -> AppError {
    match e {
        KnowledgeError::NotFound => AppError::NotFound("knowledge".into()),
        KnowledgeError::Conflict => AppError::Conflict("knowledge".into()),
        KnowledgeError::Timeout => AppError::Unavailable("knowledge query timed out".into()),
        KnowledgeError::Unavailable(m) => AppError::Unavailable(m),
        KnowledgeError::Invalid(m) => AppError::Invalid(m),
    }
}

pub(crate) fn repo_error(e: RepoStoreError) -> AppError {
    match e {
        RepoStoreError::NotFound => AppError::NotFound("repo".into()),
        RepoStoreError::Conflict => AppError::Conflict("repo".into()),
        RepoStoreError::Invalid(m) => AppError::Invalid(m),
        RepoStoreError::Unavailable(m) => AppError::Unavailable(m),
    }
}

pub(crate) fn job_error(e: JobError) -> AppError {
    match e {
        JobError::NotFound => AppError::NotFound("job".into()),
        JobError::Conflict | JobError::LeaseLost => AppError::Conflict(e.to_string()),
        JobError::Invalid(m) => AppError::Invalid(m),
        JobError::Unavailable(m) => AppError::Unavailable(m),
    }
}
