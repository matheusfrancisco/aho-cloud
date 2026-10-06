//! `mage_core::domain::fix` (fix.md): patches, patch policy, change sets and their digest, the
//! gated `CommitPlan`, fix PR progress, and per-org `FixSettings`. Pure, no I/O.

mod change_set;
mod commit;
mod glob;
mod patch;
mod policy;
mod pr;
mod settings;

pub use change_set::{ChangeSet, ChangeSetDigest, RegressionContent, RegressionFile, RegressionPlacement};
pub use commit::{BaseFile, CommitChange, CommitFile, CommitPlan};
pub use glob::{GLOB_MAX_BYTES, Glob};
pub use patch::{ApplyError, ApplyMode, FileMode, FilePatch, Hunk, Patch, PatchChange, PatchError};
pub use policy::{ExcludeReason, Exclusion, LockfilePolicy, PatchLimits, PatchPolicy, PolicyViolation, PolicyViolations};
pub use pr::{BRANCH_NAME_MAX, BranchName, FixPrEvent, FixPrState, FixPullRequest};
pub use settings::{BranchFrom, BriefInLinear, FixSettings, OpenPrMode, PrReviewers, PrSettings, RegressionMode};
