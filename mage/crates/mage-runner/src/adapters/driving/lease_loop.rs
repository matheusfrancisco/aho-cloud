//! Lease loop (runs.md "Lease loop"): the runner's inbound driver. Starts the daemon (recovery +
//! register), the periodic disk reaper, then long-polls `POST /lease` with the free slot counts
//! and prepared bases, hands assignments to the daemon, and drains on shutdown, revocation, or a
//! protocol/auth failure.

use std::sync::Arc;
use std::time::Duration;

use mage_core::ports::IdGen;
use mage_core::wire::runner::{LONG_POLL_MAX, LeaseRequest};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::app::runner::daemon::Daemon;
use crate::app::runner::disk;
use crate::error::AppError;
use crate::ports::executor::Executor;
use crate::ports::link::{LinkError, ServerLink};

/// Re-check capacity at least this often while every slot is busy (executor slots and disk
/// pressure change without a task ending).
const CAPACITY_RECHECK: Duration = Duration::from_secs(5);
const BACKOFF_BASE: Duration = Duration::from_secs(1);
const BACKOFF_CAP: Duration = Duration::from_secs(30);

/// Why leasing stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stopped {
    /// The shutdown token fired (SIGTERM).
    Shutdown,
    /// 403: runner revoked or draining on the server.
    Revoked,
    /// 401 after one re-registration.
    Unauthorized,
    /// 400 (`unsupported_protocol` or a malformed request): stop leasing, keep current runs
    /// until shutdown.
    Protocol(String),
}

pub struct LeaseLoop {
    daemon: Arc<Daemon>,
    link: Arc<dyn ServerLink>,
    executor: Arc<dyn Executor>,
    ids: Arc<dyn IdGen>,
}

impl LeaseLoop {
    pub fn new(daemon: Arc<Daemon>, link: Arc<dyn ServerLink>, executor: Arc<dyn Executor>, ids: Arc<dyn IdGen>) -> Self {
        LeaseLoop { daemon, link, executor, ids }
    }

    /// Runs the runner role until `shutdown`, then drains (`drain_grace_secs`). Returns an
    /// error when the server revoked or rejected this runner.
    pub async fn run(self, shutdown: CancellationToken) -> Result<(), AppError> {
        self.daemon.start().await?;
        let reaper_stop = shutdown.child_token();
        let reaper = tokio::spawn(disk::run_periodic(self.executor.clone(), disk::PERIODIC_INTERVAL, reaper_stop.clone()));
        let stopped = self.lease(&shutdown).await;
        if let Stopped::Protocol(reason) = &stopped {
            tracing::error!(reason, "leasing stopped: protocol error; current runs continue until shutdown");
            shutdown.cancelled().await;
        }
        let grace = Duration::from_secs(self.daemon.config().drain_grace_secs);
        tracing::info!(?stopped, running = self.daemon.running(), grace_secs = grace.as_secs(), "draining");
        self.daemon.drain(grace).await;
        reaper_stop.cancel();
        let _ = reaper.await;
        match stopped {
            Stopped::Shutdown | Stopped::Protocol(_) => Ok(()),
            Stopped::Revoked => Err(AppError::Unavailable("runner revoked or draining on the server (403)".into())),
            Stopped::Unauthorized => Err(AppError::Unavailable("runner token rejected after re-registration (401)".into())),
        }
    }

    /// The loop proper; returns why it stopped.
    pub async fn lease(&self, shutdown: &CancellationToken) -> Stopped {
        let mut request_id: Option<Uuid> = None;
        let mut failures = 0u32;
        loop {
            if shutdown.is_cancelled() {
                return Stopped::Shutdown;
            }
            let (free, prepared) = self.daemon.lease_capacity().await;
            if free.total() == 0 {
                tokio::select! {
                    _ = shutdown.cancelled() => return Stopped::Shutdown,
                    _ = self.daemon.slot_freed() => {}
                    _ = tokio::time::sleep(CAPACITY_RECHECK) => {}
                }
                continue;
            }
            // A new id per logical request; reused verbatim when the previous one failed, so a
            // lost response returns the same assignment.
            let id = *request_id.get_or_insert_with(|| self.ids.uuid_v4());
            let req = LeaseRequest { request_id: id, wait: LONG_POLL_MAX, free, prepared };
            let sent = Instant::now();
            let leased = tokio::select! {
                _ = shutdown.cancelled() => return Stopped::Shutdown,
                r = self.link.lease(&req) => r,
            };
            match leased {
                Ok(Some(assignment)) => {
                    request_id = None;
                    failures = 0;
                    tracing::info!(run = %assignment.run_id, attempt = assignment.attempt, kind = ?assignment.kind, "leased");
                    self.daemon.accept(assignment, sent);
                }
                Ok(None) => {
                    request_id = None;
                    failures = 0;
                }
                Err(LinkError::Forbidden) => return Stopped::Revoked,
                Err(LinkError::Unauthorized) => return Stopped::Unauthorized,
                Err(LinkError::Invalid(reason)) => return Stopped::Protocol(reason),
                Err(e) => {
                    failures += 1;
                    let pause = BACKOFF_BASE.saturating_mul(1 << failures.min(5)).min(BACKOFF_CAP);
                    tracing::warn!(error = %e, pause_ms = pause.as_millis() as u64, "lease failed");
                    tokio::select! {
                        _ = shutdown.cancelled() => return Stopped::Shutdown,
                        _ = tokio::time::sleep(pause) => {}
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use mage_core::domain::RunKind;
    use mage_core::ports::UuidV7Gen;
    use mage_core::wire::runner::{FinishOutcome, SlotCounts};

    use super::*;
    use crate::app::runner::daemon::DaemonConfig;
    use crate::ports::link::LinkError;
    use crate::testing::link::LinkCall;
    use crate::testing::runner::FakeRunner;

    const WAIT: Duration = Duration::from_secs(10);

    fn lease_loop(fake: &FakeRunner) -> (Arc<Daemon>, LeaseLoop) {
        let daemon = Daemon::new(fake.deps.clone(), DaemonConfig::default());
        let lease_loop = LeaseLoop::new(daemon.clone(), fake.link.clone(), fake.executor.clone(), Arc::new(UuidV7Gen));
        (daemon, lease_loop)
    }

    #[tokio::test]
    async fn registers_leases_with_free_slots_runs_the_assignment_and_drains_on_shutdown() {
        let dir = tempfile::tempdir().unwrap();
        let fake = FakeRunner::new(dir.path());
        let (daemon, lease_loop) = lease_loop(&fake);
        let shutdown = CancellationToken::new();
        let running = tokio::spawn(lease_loop.run(shutdown.clone()));
        fake.link.queue_assignment(fake.assignment(RunKind::Verify));
        fake.link.wait_for(WAIT, |calls| calls.iter().any(|c| matches!(c, LinkCall::Finish { .. }))).await;
        shutdown.cancel();
        tokio::time::timeout(WAIT, running).await.unwrap().unwrap().unwrap();

        let calls = fake.link.calls();
        assert!(matches!(calls[0], LinkCall::Register { .. }), "register before leasing");
        let LinkCall::Lease { free, .. } = calls.iter().find(|c| matches!(c, LinkCall::Lease { .. })).unwrap() else { unreachable!() };
        assert_eq!(*free, SlotCounts { light: 2, work: 1, verify: 1 });
        assert_eq!(fake.link.finishes()[0].2.outcome, FinishOutcome::Done);
        assert_eq!(daemon.running(), 0);
    }

    #[tokio::test]
    async fn a_revoked_runner_stops_leasing_and_reports_it() {
        let dir = tempfile::tempdir().unwrap();
        let fake = FakeRunner::new(dir.path());
        fake.link.fail_leases(Some(LinkError::Forbidden));
        let (_daemon, lease_loop) = lease_loop(&fake);
        let result = tokio::time::timeout(WAIT, lease_loop.run(CancellationToken::new())).await.unwrap();
        assert!(matches!(result, Err(AppError::Unavailable(_))), "{result:?}");
        let leases = fake.link.calls().iter().filter(|c| matches!(c, LinkCall::Lease { .. })).count();
        assert_eq!(leases, 1, "no lease retries after 403");
    }
}
