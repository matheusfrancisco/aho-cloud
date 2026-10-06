//! `MemRunnerState`: in-memory `RunnerState` with the same semantics as the SQLite adapter
//! (checked by `testing::contract::runner_state`).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use async_trait::async_trait;
use mage_core::domain::RunId;
use mage_core::ports::Clock;
use parking_lot::Mutex;
use time::OffsetDateTime;

use crate::ports::executor::{BaseId, BaseKey, BaseKind, EnvId, PortName};
use crate::ports::runner_state::{
    ActiveRun, BaseClaim, BaseMeta, BaseRecord, BaseStatus, EnvResource, ForkRecord, ForkStatus, PortLease, PortOwner,
    PortRange, ProcessRecord, RecoverySnapshot, RunPhase, RunnerState, RunnerStateError,
};

#[derive(Default)]
struct Inner {
    runs: BTreeMap<RunId, ActiveRun>,
    processes: BTreeMap<i32, ProcessRecord>,
    bases: BTreeMap<BaseId, BaseRecord>,
    forks: BTreeMap<EnvId, ForkRecord>,
    ports: BTreeMap<u16, PortLease>,
    quarantine: BTreeMap<u16, OffsetDateTime>,
}

impl Inner {
    fn live_forks(&self, id: &BaseId) -> u32 {
        let n = self.forks.values().filter(|f| f.base.as_ref().is_some_and(|k| &k.id() == id)).count();
        u32::try_from(n).unwrap_or(u32::MAX)
    }

    fn base_with_count(&self, rec: &BaseRecord) -> BaseRecord {
        BaseRecord { live_forks: self.live_forks(&rec.id), ..rec.clone() }
    }

    fn sorted_bases(&self, filter: impl Fn(&BaseRecord) -> bool) -> Vec<BaseRecord> {
        let mut out: Vec<BaseRecord> = self.bases.values().filter(|b| filter(b)).map(|b| self.base_with_count(b)).collect();
        out.sort_by(|a, b| (a.created_at, &a.id).cmp(&(b.created_at, &b.id)));
        out
    }

    fn sorted_forks(&self) -> Vec<ForkRecord> {
        let mut out: Vec<ForkRecord> = self.forks.values().cloned().collect();
        out.sort_by(|a, b| (a.created_at, a.env).cmp(&(b.created_at, b.env)));
        out
    }

    fn sorted_runs(&self) -> Vec<ActiveRun> {
        let mut out: Vec<ActiveRun> = self.runs.values().cloned().collect();
        out.sort_by(|a, b| (a.started_at, a.run_id).cmp(&(b.started_at, b.run_id)));
        out
    }
}

pub struct MemRunnerState {
    inner: Mutex<Inner>,
    clock: Arc<dyn Clock>,
}

impl MemRunnerState {
    pub fn new(clock: Arc<dyn Clock>) -> Arc<Self> {
        Arc::new(Self { inner: Mutex::new(Inner::default()), clock })
    }
}

#[async_trait]
impl RunnerState for MemRunnerState {
    async fn begin_run(&self, run: ActiveRun) -> Result<(), RunnerStateError> {
        let mut inner = self.inner.lock();
        if inner.runs.contains_key(&run.run_id) {
            return Err(RunnerStateError::Conflict(format!("run {} already active", run.run_id)));
        }
        inner.runs.insert(run.run_id, run);
        Ok(())
    }

    async fn set_phase(&self, run: RunId, phase: RunPhase) -> Result<(), RunnerStateError> {
        let mut inner = self.inner.lock();
        inner.runs.get_mut(&run).ok_or(RunnerStateError::NotFound)?.phase = phase;
        Ok(())
    }

    async fn attach_env(&self, run: RunId, env: EnvId) -> Result<(), RunnerStateError> {
        let mut inner = self.inner.lock();
        inner.runs.get_mut(&run).ok_or(RunnerStateError::NotFound)?.env = Some(env);
        Ok(())
    }

    async fn end_run(&self, run: RunId) -> Result<(), RunnerStateError> {
        self.inner.lock().runs.remove(&run);
        Ok(())
    }

    async fn active_runs(&self) -> Result<Vec<ActiveRun>, RunnerStateError> {
        Ok(self.inner.lock().sorted_runs())
    }

    async fn record_process(&self, p: ProcessRecord) -> Result<(), RunnerStateError> {
        self.inner.lock().processes.insert(p.pgid, p);
        Ok(())
    }

    async fn clear_process(&self, pgid: i32) -> Result<(), RunnerStateError> {
        self.inner.lock().processes.remove(&pgid);
        Ok(())
    }

    async fn processes(&self) -> Result<Vec<ProcessRecord>, RunnerStateError> {
        Ok(self.inner.lock().processes.values().cloned().collect())
    }

    async fn claim_base(&self, key: &BaseKey, staging_dir: &str) -> Result<BaseClaim, RunnerStateError> {
        let now = self.clock.now();
        let mut inner = self.inner.lock();
        let id = key.id();
        if let Some(rec) = inner.bases.get(&id) {
            return Ok(match rec.status {
                BaseStatus::Ready => BaseClaim::Ready(inner.base_with_count(rec)),
                BaseStatus::Preparing => BaseClaim::InProgress,
            });
        }
        let kind = if key.recipe_hash.is_some() { BaseKind::Prepared } else { BaseKind::SourceOnly };
        inner.bases.insert(
            id.clone(),
            BaseRecord {
                key: *key,
                id,
                kind,
                status: BaseStatus::Preparing,
                dir: staging_dir.to_owned(),
                bytes: 0,
                created_at: now,
                last_used_at: now,
                live_forks: 0,
                fork_mode: None,
                template_db: None,
            },
        );
        Ok(BaseClaim::Owned)
    }

    async fn base_ready(&self, key: &BaseKey, meta: BaseMeta) -> Result<(), RunnerStateError> {
        let now = self.clock.now();
        let mut inner = self.inner.lock();
        let rec = inner.bases.get_mut(&key.id()).ok_or(RunnerStateError::NotFound)?;
        rec.kind = meta.kind;
        rec.status = BaseStatus::Ready;
        rec.dir = meta.dir;
        rec.bytes = meta.bytes;
        rec.fork_mode = Some(meta.fork_mode);
        rec.template_db = meta.template_db;
        rec.last_used_at = now;
        Ok(())
    }

    async fn base_abandon(&self, key: &BaseKey) -> Result<(), RunnerStateError> {
        let mut inner = self.inner.lock();
        let id = key.id();
        if inner.bases.get(&id).is_some_and(|b| b.status == BaseStatus::Preparing) {
            inner.bases.remove(&id);
        }
        Ok(())
    }

    async fn base(&self, key: &BaseKey) -> Result<Option<BaseRecord>, RunnerStateError> {
        let inner = self.inner.lock();
        Ok(inner.bases.get(&key.id()).map(|b| inner.base_with_count(b)))
    }

    async fn bases(&self) -> Result<Vec<BaseRecord>, RunnerStateError> {
        Ok(self.inner.lock().sorted_bases(|_| true))
    }

    async fn touch_base(&self, key: &BaseKey, at: OffsetDateTime) -> Result<(), RunnerStateError> {
        let mut inner = self.inner.lock();
        inner.bases.get_mut(&key.id()).ok_or(RunnerStateError::NotFound)?.last_used_at = at;
        Ok(())
    }

    async fn remove_base(&self, key: &BaseKey) -> Result<(), RunnerStateError> {
        let mut inner = self.inner.lock();
        let id = key.id();
        let live = inner.live_forks(&id);
        if live > 0 {
            return Err(RunnerStateError::Conflict(format!("base {id} has {live} live forks")));
        }
        inner.bases.remove(&id);
        Ok(())
    }

    async fn begin_fork(&self, f: ForkRecord) -> Result<(), RunnerStateError> {
        let mut inner = self.inner.lock();
        if let Some(key) = &f.base {
            if !inner.bases.contains_key(&key.id()) {
                return Err(RunnerStateError::NotFound);
            }
        }
        if inner.forks.contains_key(&f.env) {
            return Err(RunnerStateError::Conflict(format!("fork {} exists", f.env)));
        }
        let mut f = f;
        let unique: BTreeSet<EnvResource> = f.resources.drain(..).collect();
        f.resources = unique.into_iter().collect();
        inner.forks.insert(f.env, f);
        Ok(())
    }

    async fn set_fork_status(&self, env: EnvId, s: ForkStatus) -> Result<(), RunnerStateError> {
        let mut inner = self.inner.lock();
        inner.forks.get_mut(&env).ok_or(RunnerStateError::NotFound)?.status = s;
        Ok(())
    }

    async fn add_resource(&self, env: EnvId, r: EnvResource) -> Result<(), RunnerStateError> {
        let mut inner = self.inner.lock();
        let fork = inner.forks.get_mut(&env).ok_or(RunnerStateError::NotFound)?;
        if !fork.resources.contains(&r) {
            fork.resources.push(r);
            fork.resources.sort();
        }
        Ok(())
    }

    async fn remove_resource(&self, env: EnvId, r: &EnvResource) -> Result<(), RunnerStateError> {
        let mut inner = self.inner.lock();
        if let Some(fork) = inner.forks.get_mut(&env) {
            fork.resources.retain(|x| x != r);
        }
        Ok(())
    }

    async fn forks(&self) -> Result<Vec<ForkRecord>, RunnerStateError> {
        Ok(self.inner.lock().sorted_forks())
    }

    async fn remove_fork(&self, env: EnvId) -> Result<(), RunnerStateError> {
        self.inner.lock().forks.remove(&env);
        Ok(())
    }

    async fn lease_ports(&self, owner: &PortOwner, names: &[Option<PortName>], pool: PortRange) -> Result<Vec<PortLease>, RunnerStateError> {
        if pool.start > pool.end {
            return Err(RunnerStateError::Invalid(format!("empty port range {}..={}", pool.start, pool.end)));
        }
        let now = self.clock.now();
        let mut inner = self.inner.lock();
        inner.quarantine.retain(|_, until| *until > now);
        let free: Vec<u16> = (pool.start..=pool.end)
            .filter(|p| !inner.ports.contains_key(p) && !inner.quarantine.contains_key(p))
            .take(names.len())
            .collect();
        if free.len() < names.len() {
            return Err(RunnerStateError::Unavailable(format!("port pool {}..={} exhausted", pool.start, pool.end)));
        }
        let leases: Vec<PortLease> =
            free.into_iter().zip(names).map(|(port, name)| PortLease { port, owner: owner.clone(), name: name.clone(), at: now }).collect();
        for lease in &leases {
            inner.ports.insert(lease.port, lease.clone());
        }
        Ok(leases)
    }

    async fn quarantine_port(&self, port: u16, until: OffsetDateTime) -> Result<(), RunnerStateError> {
        let mut inner = self.inner.lock();
        inner.ports.remove(&port);
        inner.quarantine.insert(port, until);
        Ok(())
    }

    async fn release_ports(&self, owner: &PortOwner) -> Result<(), RunnerStateError> {
        self.inner.lock().ports.retain(|_, lease| &lease.owner != owner);
        Ok(())
    }

    async fn port_leases(&self) -> Result<Vec<PortLease>, RunnerStateError> {
        Ok(self.inner.lock().ports.values().cloned().collect())
    }

    async fn recovery_snapshot(&self) -> Result<RecoverySnapshot, RunnerStateError> {
        let inner = self.inner.lock();
        Ok(RecoverySnapshot {
            runs: inner.sorted_runs(),
            processes: inner.processes.values().cloned().collect(),
            forks: inner.sorted_forks(),
            preparing_bases: inner.sorted_bases(|b| b.status == BaseStatus::Preparing),
            ports: inner.ports.values().cloned().collect(),
        })
    }
}

#[cfg(test)]
mod tests {
    use mage_core::ports::SystemClock;

    use super::*;
    use crate::testing::contract::runner_state as contract;

    #[tokio::test]
    async fn mem_runner_state_contract() {
        contract::run(|| async { MemRunnerState::new(Arc::new(SystemClock)) as Arc<dyn RunnerState> }).await;
    }

    #[tokio::test]
    async fn mem_runner_state_snapshot() {
        let state = MemRunnerState::new(Arc::new(SystemClock));
        contract::recovery_after_restart(|| {
            let state = Arc::clone(&state);
            async move { state as Arc<dyn RunnerState> }
        })
        .await;
    }
}
