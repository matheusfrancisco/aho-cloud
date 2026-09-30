//! One release ledger per project: JSONL files under the data dir in production, memory in tests.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use parking_lot::Mutex;

use aho_log::{JsonlLog, LogStore, MemoryLog};
use cloud_contract::ledger::{Approver, Proposer, Provenance, Stage, TaintFinding};
use cloud_contract::manifest::Manifest;
use cloud_contract::policy::{OnCallAction, ReleasePolicy, StandingOrder};
use cloud_ledger::{ApprovalStatus, Ledger, LedgerError, LedgerState};

/// The ledger operations the control plane uses, object-safe over any log store.
pub trait ProjectLedger: Send + Sync {
    fn state(&self) -> Result<LedgerState, LedgerError>;
    fn entries(&self) -> Result<Vec<cloud_api::LedgerEntry>, LedgerError>;
    fn propose(&self, release: &str, manifest: Manifest, provenance: Provenance, proposer: Proposer) -> Result<(), LedgerError>;
    fn image_built(&self, release: &str, digest: &str) -> Result<(), LedgerError>;
    fn observe(&self, release: &str, listeners: Vec<u16>, egress: Vec<String>) -> Result<(), LedgerError>;
    fn taint_checked(&self, release: &str, findings: Vec<TaintFinding>) -> Result<(), LedgerError>;
    fn secret_rotated(&self, secret: &str, by: &str) -> Result<(), LedgerError>;
    fn approve(&self, release: &str, stage: Stage, approver: Approver) -> Result<ApprovalStatus, LedgerError>;
    fn reject(&self, release: &str, stage: Stage, user_id: &str, reason: &str) -> Result<(), LedgerError>;
    fn activate(&self, release: &str, stage: Stage) -> Result<(), LedgerError>;
    fn rollback(&self, release: &str, approver: Option<Approver>) -> Result<(), LedgerError>;
    fn grant_standing_orders(&self, orders: Vec<StandingOrder>, expires_at: u64, approver: Approver) -> Result<(), LedgerError>;
    fn execute_standing_order(&self, action: OnCallAction, release: Option<&str>, actor: &str, now: u64) -> Result<(), LedgerError>;
    fn set_policy(&self, policy: ReleasePolicy, approver: Approver) -> Result<(), LedgerError>;
    fn members_changed(&self, count: u16) -> Result<(), LedgerError>;
    fn audit(&self, verify: &dyn Fn(&Approver, &[u8]) -> bool) -> Result<(), LedgerError>;
}

impl<S: LogStore> ProjectLedger for Ledger<S>
where
    Ledger<S>: Send + Sync,
{
    fn state(&self) -> Result<LedgerState, LedgerError> {
        Ledger::state(self)
    }
    fn entries(&self) -> Result<Vec<cloud_api::LedgerEntry>, LedgerError> {
        Ledger::entries(self)
    }
    fn propose(&self, release: &str, manifest: Manifest, provenance: Provenance, proposer: Proposer) -> Result<(), LedgerError> {
        Ledger::propose(self, release, manifest, provenance, proposer)
    }
    fn image_built(&self, release: &str, digest: &str) -> Result<(), LedgerError> {
        Ledger::image_built(self, release, digest)
    }
    fn observe(&self, release: &str, listeners: Vec<u16>, egress: Vec<String>) -> Result<(), LedgerError> {
        Ledger::observe(self, release, listeners, egress)
    }
    fn taint_checked(&self, release: &str, findings: Vec<TaintFinding>) -> Result<(), LedgerError> {
        Ledger::taint_checked(self, release, findings)
    }
    fn secret_rotated(&self, secret: &str, by: &str) -> Result<(), LedgerError> {
        Ledger::secret_rotated(self, secret, by)
    }
    fn approve(&self, release: &str, stage: Stage, approver: Approver) -> Result<ApprovalStatus, LedgerError> {
        Ledger::approve(self, release, stage, approver)
    }
    fn reject(&self, release: &str, stage: Stage, user_id: &str, reason: &str) -> Result<(), LedgerError> {
        Ledger::reject(self, release, stage, user_id, reason)
    }
    fn activate(&self, release: &str, stage: Stage) -> Result<(), LedgerError> {
        Ledger::activate(self, release, stage)
    }
    fn rollback(&self, release: &str, approver: Option<Approver>) -> Result<(), LedgerError> {
        Ledger::rollback(self, release, approver)
    }
    fn grant_standing_orders(&self, orders: Vec<StandingOrder>, expires_at: u64, approver: Approver) -> Result<(), LedgerError> {
        Ledger::grant_standing_orders(self, orders, expires_at, approver)
    }
    fn execute_standing_order(&self, action: OnCallAction, release: Option<&str>, actor: &str, now: u64) -> Result<(), LedgerError> {
        Ledger::execute_standing_order(self, action, release, actor, now)
    }
    fn set_policy(&self, policy: ReleasePolicy, approver: Approver) -> Result<(), LedgerError> {
        Ledger::set_policy(self, policy, approver)
    }
    fn members_changed(&self, count: u16) -> Result<(), LedgerError> {
        Ledger::members_changed(self, count)
    }
    fn audit(&self, verify: &dyn Fn(&Approver, &[u8]) -> bool) -> Result<(), LedgerError> {
        Ledger::audit(self, verify)
    }
}

enum Backing {
    Memory,
    Dir(PathBuf),
}

/// Opens each project's ledger once and keeps it for the life of the process (a JSONL ledger
/// holds its file's writer lock).
pub struct Ledgers {
    backing: Backing,
    open: Mutex<HashMap<String, Arc<dyn ProjectLedger>>>,
}

impl Ledgers {
    /// Ledgers that vanish with the process.
    pub fn memory() -> Self {
        Self { backing: Backing::Memory, open: Mutex::new(HashMap::new()) }
    }

    /// `dir/<project>.jsonl`.
    pub fn on_disk(dir: impl Into<PathBuf>) -> Self {
        Self { backing: Backing::Dir(dir.into()), open: Mutex::new(HashMap::new()) }
    }

    /// The ledger of `project`, opened (and its chain verified) on first use. Callers pass only
    /// slugs the store accepted, so the name is a safe file name.
    pub fn get(&self, project: &str) -> Result<Arc<dyn ProjectLedger>, LedgerError> {
        let mut open = self.open.lock();
        if let Some(l) = open.get(project) {
            return Ok(l.clone());
        }
        let ledger: Arc<dyn ProjectLedger> = match &self.backing {
            Backing::Memory => Arc::new(Ledger::<MemoryLog>::memory(project)),
            Backing::Dir(dir) => Arc::new(Ledger::<JsonlLog>::open(project, &dir.join(format!("{project}.jsonl")))?),
        };
        open.insert(project.to_owned(), ledger.clone());
        Ok(ledger)
    }
}
