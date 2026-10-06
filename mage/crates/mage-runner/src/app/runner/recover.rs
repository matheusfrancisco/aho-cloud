//! UC8 Recovery on boot (executor.md): runs before the lease loop starts. Harness sessions do not
//! survive a daemon restart, so every fork is orphaned.

use crate::error::AppError;
use crate::ports::executor::{Executor, ReapScope};
use crate::ports::runner_state::{ActiveRun, RunnerState};

/// Reads the recovery snapshot, then `reap(Boot)`: destroys every recorded fork and its resources,
/// kills recorded processes (only when `boot_id` and `start_time` match), deletes Preparing bases,
/// releases non-base ports, and runs the orphan scan.
///
/// Returns the runs that were active at the crash. The caller reports them as held runs when
/// registering (the server applies `LeaseLost`), then `end_run`s each one. Ready bases survive.
pub async fn recover(executor: &dyn Executor, state: &dyn RunnerState) -> Result<Vec<ActiveRun>, AppError> {
    let snap = state.recovery_snapshot().await?;
    tracing::info!(
        runs = snap.runs.len(),
        processes = snap.processes.len(),
        forks = snap.forks.len(),
        preparing_bases = snap.preparing_bases.len(),
        ports = snap.ports.len(),
        "runner recovery snapshot"
    );
    let report = executor.reap(ReapScope::Boot).await?;
    tracing::info!(
        forks_destroyed = report.forks_destroyed,
        processes_killed = report.processes_killed,
        databases_dropped = report.databases_dropped,
        compose_projects_removed = report.compose_projects_removed,
        dirs_removed = report.dirs_removed,
        bases_evicted = report.bases_evicted,
        bytes_freed = report.bytes_freed,
        "boot reap finished"
    );
    Ok(snap.runs)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use mage_core::domain::{OrgId, RunId, RunKind};
    use mage_core::ports::Clock;
    use mage_core::testing::FakeClock;
    use uuid::Uuid;

    use super::*;
    use crate::ports::executor::{EgressProfile, ForkOptions, ForkPurpose};
    use crate::ports::runner_state::RunPhase;
    use crate::testing::executor::FakeExecutor;
    use crate::testing::runner_state::MemRunnerState;

    #[tokio::test]
    async fn returns_held_runs_and_reaps_orphaned_envs() {
        let clock: Arc<dyn Clock> = Arc::new(FakeClock::default());
        let state = MemRunnerState::new(Arc::clone(&clock));
        let run = ActiveRun {
            run_id: RunId::from_uuid(Uuid::from_u128(7)),
            org_id: OrgId::from_uuid(Uuid::from_u128(1)),
            kind: RunKind::Triage,
            attempt: 2,
            phase: RunPhase::Running,
            env: None,
            started_at: clock.now(),
        };
        state.begin_run(run.clone()).await.unwrap();
        let exec = FakeExecutor::new();
        let env = exec
            .empty_env(ForkOptions { run_id: run.run_id, purpose: ForkPurpose::Triage, egress: EgressProfile::ServerOnly })
            .await
            .unwrap();

        let held = recover(exec.as_ref(), state.as_ref()).await.unwrap();

        assert_eq!(held, vec![run]);
        assert!(exec.envs()[0].destroyed());
        assert!(matches!(env.list(&crate::ports::executor::EnvPath::ws(".").unwrap(), Default::default()).await, Err(crate::ports::executor::ExecError::Destroyed)));
    }
}
