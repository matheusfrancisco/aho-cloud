//! The per-project release ledger: an [`aho_log`] hash chain of `deploy` entries
//! ([`cloud_contract::ledger::LedgerBody`]) whose fold enforces the release rules.
//!
//! Every mutating [`Ledger`] method folds the current state, applies the new body with the same
//! rule code the fold replays ([`fold_deploy`]), and appends only if it holds. Humans' entries
//! (approvals, standing-order grants, policy changes) are appended as [`Principal::Operator`];
//! everything else as `system:aho-cloud`. [`Ledger::state`] re-verifies the chain and replays
//! every rule, so a log edited and re-hashed around the API fails with [`LedgerError::Invariant`];
//! [`Ledger::audit`] additionally re-verifies every approver signature.

#![forbid(unsafe_code)]

mod rules;
mod state;
#[cfg(test)]
mod tests;

use std::path::Path;
use std::sync::{Mutex, PoisonError};

use aho_log::{Entry, EntryKind, JsonlLog, LogError, LogStore, MemoryLog};
use aho_types::Principal;
use cloud_contract::ledger::{
    ActivationReason, Approver, LedgerBody, NAMESPACE, Proposer, Provenance, Stage, TaintFinding, approval_message,
};
use cloud_contract::policy::{OnCallAction, ReleasePolicy, Requirement, StandingOrder};
use cloud_contract::{Manifest, Refusal};

pub use rules::{SYSTEM, custom_folds, fold_deploy};
pub use state::{ApprovalStatus, LedgerState, ReleaseState, StandingGrant};

/// `release` slot of the message an owner signs to grant standing orders.
pub const STANDING_ORDERS_SUBJECT: &str = "standing-orders";
/// `release` slot of the message an owner signs to change the release policy.
pub const POLICY_SUBJECT: &str = "policy";

/// The bytes an owner signs to grant `orders`:
/// `approval_message(project, "standing-orders", canonical::hash(orders), live)`.
pub fn standing_orders_message(project: &str, orders: &[StandingOrder]) -> Vec<u8> {
    approval_message(project, STANDING_ORDERS_SUBJECT, &cloud_contract::canonical::hash(orders), Stage::Live)
}

/// The bytes an owner signs to set `policy`:
/// `approval_message(project, "policy", canonical::hash(policy), live)`.
pub fn policy_message(project: &str, policy: &ReleasePolicy) -> Vec<u8> {
    approval_message(project, POLICY_SUBJECT, &cloud_contract::canonical::hash(policy), Stage::Live)
}

/// One project's release ledger over any [`LogStore`].
pub struct Ledger<S: LogStore> {
    project: String,
    store: S,
    /// Serializes check-then-append so two callers cannot both pass a rule only one may.
    writer: Mutex<()>,
}

impl Ledger<MemoryLog> {
    /// An empty in-memory ledger.
    pub fn memory(project: &str) -> Self {
        Self::with_store(project, MemoryLog::new())
    }
}

impl Ledger<JsonlLog> {
    /// Opens (or creates) the JSONL ledger at `path`; every entry's hash is checked on load.
    pub fn open(project: &str, path: &Path) -> Result<Self, LedgerError> {
        JsonlLog::open(path).map(|store| Self::with_store(project, store)).map_err(log_error)
    }
}

impl<S: LogStore> Ledger<S> {
    fn with_store(project: &str, store: S) -> Self {
        Self { project: project.to_owned(), store, writer: Mutex::new(()) }
    }

    /// Verifies the hash chain and replays every rule from the first entry.
    pub fn state(&self) -> Result<LedgerState, LedgerError> {
        rules::replay(&self.branch()?, |_, _, _| Ok(()))
    }

    /// Every entry on the verified chain, oldest first. Decodes bodies without replaying rules, so
    /// a ledger whose fold fails can still be inspected.
    pub fn entries(&self) -> Result<Vec<cloud_api::LedgerEntry>, LedgerError> {
        self.branch()?
            .iter()
            .map(|e| Ok(cloud_api::LedgerEntry { id: e.id.0, hash: e.hash.to_hex(), at: e.at.0, body: rules::decode_body(e)? }))
            .collect()
    }

    /// Records a proposed release. Refuses invalid manifests with every refusal at once.
    pub fn propose(&self, release: &str, manifest: Manifest, provenance: Provenance, proposer: Proposer) -> Result<(), LedgerError> {
        let manifest_hash = manifest.hash();
        let body = LedgerBody::ReleaseProposed {
            release: release.to_owned(),
            manifest: Box::new(manifest),
            manifest_hash,
            image_digest: None,
            provenance,
            proposer,
        };
        self.commit(vec![body], None).map(drop)
    }

    /// Records the digest the cloud build produced. A release is built once.
    pub fn image_built(&self, release: &str, digest: &str) -> Result<(), LedgerError> {
        self.commit(vec![LedgerBody::ImageBuilt { release: release.to_owned(), image_digest: digest.to_owned() }], None).map(drop)
    }

    /// Records the listeners and egress hosts a staging run was seen using.
    pub fn observe(&self, release: &str, listeners: Vec<u16>, egress: Vec<String>) -> Result<(), LedgerError> {
        self.commit(vec![LedgerBody::SurfaceObserved { release: release.to_owned(), listeners, egress }], None).map(drop)
    }

    /// Records a taint scan. Each finding blocks activation until its secret is rotated.
    pub fn taint_checked(&self, release: &str, findings: Vec<TaintFinding>) -> Result<(), LedgerError> {
        self.commit(vec![LedgerBody::TaintChecked { release: release.to_owned(), findings }], None).map(drop)
    }

    /// Records a rotation, resolving every earlier finding for `secret`.
    pub fn secret_rotated(&self, secret: &str, by: &str) -> Result<(), LedgerError> {
        self.commit(vec![LedgerBody::SecretRotated { secret: secret.to_owned(), by: by.to_owned() }], None).map(drop)
    }

    /// Records a (caller-verified) signed approval and reports where the stage now stands.
    pub fn approve(&self, release: &str, stage: Stage, approver: Approver) -> Result<ApprovalStatus, LedgerError> {
        let st = self.commit(vec![LedgerBody::Approved { release: release.to_owned(), stage, approver }], None)?;
        st.approval_status(release, stage).ok_or_else(|| LedgerError::UnknownRelease(release.to_owned()))
    }

    /// Records a rejection; a rejected release is never approved or activated again.
    pub fn reject(&self, release: &str, stage: Stage, user_id: &str, reason: &str) -> Result<(), LedgerError> {
        let body = LedgerBody::Rejected { release: release.to_owned(), stage, user_id: user_id.to_owned(), reason: reason.to_owned() };
        self.commit(vec![body], None).map(drop)
    }

    /// Promotes `release` to `stage` once the stage rules hold.
    pub fn activate(&self, release: &str, stage: Stage) -> Result<(), LedgerError> {
        let body = LedgerBody::Activated { release: release.to_owned(), stage, reason: ActivationReason::Promotion };
        self.commit(vec![body], None).map(drop)
    }

    /// Re-activates a previously live release at Live. Free when it fits within the live surface;
    /// otherwise `approver` is recorded as a Live approval and the policy requirement must be met.
    pub fn rollback(&self, release: &str, approver: Option<Approver>) -> Result<(), LedgerError> {
        let mut bodies = Vec::with_capacity(2);
        if let Some(approver) = approver {
            bodies.push(LedgerBody::Approved { release: release.to_owned(), stage: Stage::Live, approver });
        }
        bodies.push(LedgerBody::Activated { release: release.to_owned(), stage: Stage::Live, reason: ActivationReason::Rollback });
        self.commit(bodies, None).map(drop)
    }

    /// Replaces the standing grant. Owner-only; `expires_at` is Unix seconds.
    pub fn grant_standing_orders(&self, orders: Vec<StandingOrder>, expires_at: u64, approver: Approver) -> Result<(), LedgerError> {
        self.commit(vec![LedgerBody::StandingOrdersGranted { orders, expires_at, approver }], None).map(drop)
    }

    /// Records an on-call action the unexpired grant permits; a rollback also activates `release`.
    pub fn execute_standing_order(&self, action: OnCallAction, release: Option<&str>, actor: &str, now: u64) -> Result<(), LedgerError> {
        let rollback = matches!(action, OnCallAction::Rollback { .. });
        let mut bodies = vec![LedgerBody::StandingOrderExecuted { action, release: release.map(str::to_owned), actor: actor.to_owned() }];
        if let (true, Some(target)) = (rollback, release) {
            bodies.push(LedgerBody::Activated { release: target.to_owned(), stage: Stage::Live, reason: ActivationReason::StandingOrder });
        }
        self.commit(bodies, Some(now)).map(drop)
    }

    /// Replaces the release policy. Owner-only.
    pub fn set_policy(&self, policy: ReleasePolicy, approver: Approver) -> Result<(), LedgerError> {
        self.commit(vec![LedgerBody::PolicyChanged { policy, approver }], None).map(drop)
    }

    /// [`Self::state`] plus every approver signature re-verified by `verify` over its message:
    /// [`approval_message`] for approvals, [`standing_orders_message`] and [`policy_message`].
    pub fn audit(&self, verify: &dyn Fn(&Approver, &[u8]) -> bool) -> Result<(), LedgerError> {
        rules::replay(&self.branch()?, |e, body, st| {
            let (approver, message) = match body {
                LedgerBody::Approved { release, stage, approver } => {
                    let hash = st.releases.get(release).map(|r| r.manifest_hash.as_str()).unwrap_or_default();
                    (approver, approval_message(&self.project, release, hash, *stage))
                }
                LedgerBody::StandingOrdersGranted { orders, approver, .. } => (approver, standing_orders_message(&self.project, orders)),
                LedgerBody::PolicyChanged { policy, approver } => (approver, policy_message(&self.project, policy)),
                _ => return Ok(()),
            };
            if verify(approver, &message) {
                Ok(())
            } else {
                Err(LedgerError::BadSignature { entry: e.id.0, user_id: approver.user_id.clone() })
            }
        })
        .map(drop)
    }

    /// The verified branch ending at the head, oldest first.
    fn branch(&self) -> Result<Vec<Entry>, LedgerError> {
        match self.store.head() {
            Some(head) => self.store.read_branch(head).map_err(log_error),
            None => Ok(Vec::new()),
        }
    }

    /// Applies `bodies` in order to the current state and appends them only if all hold.
    fn commit(&self, bodies: Vec<LedgerBody>, now: Option<u64>) -> Result<LedgerState, LedgerError> {
        let _writer = self.writer.lock().unwrap_or_else(PoisonError::into_inner);
        let mut st = self.state()?;
        let mut kinds = Vec::with_capacity(bodies.len());
        for body in &bodies {
            rules::apply(&mut st, body, now)?;
            let value = serde_json::to_value(body).map_err(|e| LedgerError::Storage(e.to_string()))?;
            kinds.push((
                rules::principal_for(body),
                EntryKind::Custom { ns: NAMESPACE.to_owned(), name: body.name().to_owned(), body: value },
            ));
        }
        for (by, kind) in kinds {
            self.store.append(None, by, kind).map_err(log_error)?;
        }
        Ok(st)
    }
}

/// Why the ledger refused an action or could not be read. Each refused rule has its own variant.
#[derive(Debug, thiserror::Error)]
pub enum LedgerError {
    // ── storage and integrity ──
    #[error("ledger hash chain broken at entry {entry}")]
    ChainBroken { entry: u64 },
    #[error("ledger storage: {0}")]
    Storage(String),
    #[error("ledger entry {entry} is not a release-ledger entry: {detail}")]
    Malformed { entry: u64, detail: String },
    /// A recorded entry breaks a rule on replay: the log was written around the API.
    #[error("ledger entry {entry} breaks a release rule: {violation}")]
    Invariant { entry: u64, violation: Box<LedgerError> },
    #[error("`{name}` must be appended by {expected:?}, not {found:?}")]
    WrongPrincipal { name: &'static str, expected: Box<Principal>, found: Box<Principal> },
    #[error("signature of {user_id} at ledger entry {entry} does not verify")]
    BadSignature { entry: u64, user_id: String },

    // ── proposals and builds ──
    #[error("unknown release `{0}`")]
    UnknownRelease(String),
    #[error("release `{0}` already exists")]
    DuplicateRelease(String),
    #[error("`{0}` is not a release id (1–128 visible ASCII characters, no `/`)")]
    InvalidReleaseId(String),
    #[error("manifest refused: {}", refusal_summary(.0))]
    InvalidManifest(Vec<Refusal>),
    #[error("release `{release}` records manifest hash {recorded}, but its manifest hashes to {actual}")]
    ManifestHashMismatch { release: String, recorded: String, actual: String },
    #[error("`{0}` is not an image digest (`sha256:` plus 64 lowercase hex)")]
    InvalidDigest(String),
    #[error("release `{release}` is already built as {digest}")]
    AlreadyBuilt { release: String, digest: String },
    #[error("release `{release}` has not been built")]
    NotBuilt { release: String },
    #[error("release `{release}` is tainted until these secrets are rotated: {}", .secrets.join(", "))]
    Tainted { release: String, secrets: Vec<String> },

    // ── approvals and promotion ──
    #[error("{} needs no approval", .stage.as_str())]
    ApprovalNotNeeded { stage: Stage },
    #[error("{user_id} already approved `{release}` for {}", .stage.as_str())]
    DuplicateApprover { release: String, stage: Stage, user_id: String },
    #[error("release `{release}` was rejected at {} by {user_id}: {reason}", .stage.as_str())]
    Rejected { release: String, stage: Stage, user_id: String, reason: String },
    #[error("release `{release}` is already active at {}", .stage.as_str())]
    AlreadyActive { release: String, stage: Stage },
    #[error("release `{release}` must be active at {} before {}", .required.as_str(), .stage.as_str())]
    WrongStageOrder { release: String, stage: Stage, required: Stage },
    #[error("`{release}` has {approvals} of {} approvals needed for {}{}", .requirement.approvals, .stage.as_str(), owner_note(.requirement, .owner_present))]
    QuorumNotMet { release: String, stage: Stage, requirement: Requirement, approvals: u8, owner_present: bool },
    #[error("an org owner must approve `{release}` for {}", .stage.as_str())]
    OwnerRequired { release: String, stage: Stage, requirement: Requirement },
    #[error("a {reason:?} activation of `{release}` at {} is not allowed: only live is rolled back", .stage.as_str())]
    InvalidActivation { release: String, stage: Stage, reason: ActivationReason },

    // ── rollback and standing orders ──
    #[error("rollback target `{release}` was never live")]
    RollbackTargetNeverLive { release: String },
    #[error("rolling back to `{release}` widens the live surface: it needs {} approval(s){}", .requirement.approvals, owner_note(.requirement, &false))]
    RollbackWidens { release: String, requirement: Requirement },
    #[error("no standing orders have been granted")]
    NoStandingOrders,
    #[error("standing orders expired at {expires_at} (now {now})")]
    StandingOrdersExpired { expires_at: u64, now: u64 },
    #[error("no standing order permits {action:?}")]
    NotPermitted { action: OnCallAction },
    #[error("standing-order rollback target `{release}` does not fit within live release `{live}`")]
    StandingRollbackWidens { release: String, live: String },
    #[error("a standing-order rollback needs a target release")]
    RollbackTargetMissing,
    #[error("only a rollback names a release (got `{release}`)")]
    UnexpectedRelease { release: String },
    #[error("standing-order activation of `{release}` follows no standing-order rollback to it")]
    StandingActivationUnrecorded { release: String },
    #[error("{user_id} is not an org owner; {action} needs one")]
    NotOwner { user_id: String, action: &'static str },
}

fn refusal_summary(refusals: &[Refusal]) -> String {
    refusals.iter().map(|r| format!("{} ({})", r.message, r.field)).collect::<Vec<_>>().join("; ")
}

fn owner_note(requirement: &Requirement, owner_present: &bool) -> &'static str {
    if requirement.owner_required && !owner_present { ", including an org owner" } else { "" }
}

fn log_error(e: LogError) -> LedgerError {
    match e {
        LogError::ChainBroken { at } => LedgerError::ChainBroken { entry: at.0 },
        LogError::UnknownParent(parent) => LedgerError::ChainBroken { entry: parent.0 },
        LogError::Io(detail) => LedgerError::Storage(detail),
        LogError::Invariant { rule, at, detail } => LedgerError::Storage(format!("aho-log invariant `{rule}` at entry {}: {detail}", at.0)),
    }
}
