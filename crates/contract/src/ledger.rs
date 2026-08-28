//! Bodies of the per-project release ledger. The ledger itself (hash chain, storage, folds) is
//! `cloud-ledger`; these are the facts it records, in the `deploy` namespace.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::manifest::Manifest;
use crate::policy::{OnCallAction, StandingOrder};

/// Namespace of every ledger entry kind.
pub const NAMESPACE: &str = "deploy";

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    /// `<project>.preview.<cloud>` / `dev.<domain>`: invited reviewers only, placeholder secrets.
    Preview,
    /// Real domain wired, not public; placeholder secrets; surface observed.
    Staging,
    /// Public, real secrets.
    Live,
}

impl Stage {
    pub fn as_str(self) -> &'static str {
        match self {
            Stage::Preview => "preview",
            Stage::Staging => "staging",
            Stage::Live => "live",
        }
    }

    /// The stage a release is promoted to from this one.
    pub fn next(self) -> Option<Stage> {
        match self {
            Stage::Preview => Some(Stage::Staging),
            Stage::Staging => Some(Stage::Live),
            Stage::Live => None,
        }
    }
}

/// Where the release's bits came from. The cloud rebuilds from source either way.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Provenance {
    /// An aho session log whose chain verified server-side up to `head_hash`.
    Attested { session_id: String, head_hash: String },
    /// No transcript: a git push, a CI run, or a hand-written manifest.
    Unattested {
        #[serde(default)]
        commit: Option<String>,
        #[serde(default)]
        ci_run: Option<String>,
    },
}

/// Who proposed. Proposing never grants anything.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Proposer {
    /// An aho agent, through the `deploy` capability.
    Agent {
        session_id: String,
    },
    Ci {
        name: String,
    },
    User {
        user_id: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum ProofMethod {
    /// WebAuthn assertion from the web console.
    Passkey,
    /// Ed25519 signature from the key `aho login` minted on the operator's device.
    DeviceKey,
}

/// A human's signed approval of one ledger action.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
pub struct Approver {
    pub user_id: String,
    pub email: String,
    pub owner: bool,
    pub method: ProofMethod,
    /// Base64 signature over [`approval_message`].
    pub signature: String,
}

/// The bytes an approver signs: binds the project, release, manifest and target stage.
pub fn approval_message(project: &str, release: &str, manifest_hash: &str, stage: Stage) -> Vec<u8> {
    format!("aho-cloud/approve/v1\n{project}\n{release}\n{manifest_hash}\n{}", stage.as_str()).into_bytes()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum ActivationReason {
    Promotion,
    Rollback,
    StandingOrder,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
pub struct TaintFinding {
    pub secret: String,
    /// Where a fingerprint of the value was found, e.g. `aho-log:<session>#<entry-hash>`.
    pub location: String,
}

/// Every fact the ledger records. `kind` is the entry name inside [`NAMESPACE`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum LedgerBody {
    ReleaseProposed {
        release: String,
        manifest: Box<Manifest>,
        manifest_hash: String,
        /// Set once the cloud build finished.
        #[serde(default)]
        image_digest: Option<String>,
        provenance: Provenance,
        proposer: Proposer,
    },
    ImageBuilt {
        release: String,
        image_digest: String,
    },
    SurfaceObserved {
        release: String,
        listeners: Vec<u16>,
        egress: Vec<String>,
    },
    TaintChecked {
        release: String,
        findings: Vec<TaintFinding>,
    },
    SecretRotated {
        secret: String,
        by: String,
    },
    Approved {
        release: String,
        stage: Stage,
        approver: Approver,
    },
    Rejected {
        release: String,
        stage: Stage,
        user_id: String,
        reason: String,
    },
    Activated {
        release: String,
        stage: Stage,
        reason: ActivationReason,
    },
    StandingOrdersGranted {
        orders: Vec<StandingOrder>,
        /// Unix seconds.
        expires_at: u64,
        approver: Approver,
    },
    StandingOrderExecuted {
        action: OnCallAction,
        /// Rollback target, when the action is a rollback.
        #[serde(default)]
        release: Option<String>,
        actor: String,
    },
    PolicyChanged {
        policy: crate::policy::ReleasePolicy,
        approver: Approver,
    },
}

impl LedgerBody {
    /// Entry name inside [`NAMESPACE`].
    pub fn name(&self) -> &'static str {
        match self {
            LedgerBody::ReleaseProposed { .. } => "release_proposed",
            LedgerBody::ImageBuilt { .. } => "image_built",
            LedgerBody::SurfaceObserved { .. } => "surface_observed",
            LedgerBody::TaintChecked { .. } => "taint_checked",
            LedgerBody::SecretRotated { .. } => "secret_rotated",
            LedgerBody::Approved { .. } => "approved",
            LedgerBody::Rejected { .. } => "rejected",
            LedgerBody::Activated { .. } => "activated",
            LedgerBody::StandingOrdersGranted { .. } => "standing_orders_granted",
            LedgerBody::StandingOrderExecuted { .. } => "standing_order_executed",
            LedgerBody::PolicyChanged { .. } => "policy_changed",
        }
    }
}
