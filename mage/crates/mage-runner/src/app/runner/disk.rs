//! UC9 Disk and periodic reaper (executor.md). Every 5 minutes the daemon calls
//! `Executor::reap(Periodic)`: destroy forks whose run is no longer active, kill recorded processes
//! whose env is gone, release ports whose owner is gone, empty `trash/`, and apply the disk policy
//! (LRU eviction, idle / over-count / over-size bases, `disk_pressure`). The executor also runs
//! the disk check itself before every prepare and fork.

use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::ports::executor::{Executor, ReapReport, ReapScope};

/// Interval between periodic reaps.
pub const PERIODIC_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// One periodic pass. Errors are logged and returned; the loop keeps going.
pub async fn tick(executor: &dyn Executor) -> Result<ReapReport, crate::error::AppError> {
    let report = executor.reap(ReapScope::Periodic).await?;
    if report != ReapReport::default() {
        tracing::info!(
            forks_destroyed = report.forks_destroyed,
            processes_killed = report.processes_killed,
            databases_dropped = report.databases_dropped,
            compose_projects_removed = report.compose_projects_removed,
            dirs_removed = report.dirs_removed,
            bases_evicted = report.bases_evicted,
            bytes_freed = report.bytes_freed,
            "periodic reap"
        );
    }
    Ok(report)
}

/// Runs [`tick`] every `interval` until `cancel` fires. The first pass runs after one interval
/// (boot recovery already reaped everything).
pub async fn run_periodic(executor: Arc<dyn Executor>, interval: Duration, cancel: CancellationToken) {
    let mut timer = tokio::time::interval_at(tokio::time::Instant::now() + interval, interval);
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            () = cancel.cancelled() => return,
            _ = timer.tick() => {
                if let Err(err) = tick(executor.as_ref()).await {
                    tracing::warn!(error = %err, "periodic reap failed");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use mage_core::domain::RunId;
    use uuid::Uuid;

    use super::*;
    use crate::ports::executor::{EgressProfile, ForkOptions, ForkPurpose};
    use crate::testing::executor::FakeExecutor;

    #[tokio::test(start_paused = true)]
    async fn periodic_loop_reaps_envs_of_inactive_runs_until_cancelled() {
        let exec = FakeExecutor::new();
        exec.empty_env(ForkOptions {
            run_id: RunId::from_uuid(Uuid::from_u128(9)),
            purpose: ForkPurpose::Triage,
            egress: EgressProfile::ServerOnly,
        })
        .await
        .unwrap();
        let cancel = CancellationToken::new();
        let task = tokio::spawn(run_periodic(exec.clone(), Duration::from_secs(60), cancel.clone()));

        tokio::time::sleep(Duration::from_secs(30)).await;
        assert!(!exec.envs()[0].destroyed(), "nothing happens before the first interval");
        tokio::time::sleep(Duration::from_secs(31)).await;
        assert!(exec.envs()[0].destroyed(), "the fake has no active runs, so its env is reaped");

        cancel.cancel();
        task.await.unwrap();
    }
}
