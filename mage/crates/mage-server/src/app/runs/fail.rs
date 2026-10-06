//! FailRun (runs.md): `InfraError` with a given error on a non-terminal run; the model gateway
//! calls it when the tenant model key is rejected (`RunErrorCode::ModelAuth`).

use mage_core::domain::{OrgId, RunError, RunId, RunStatus, RunStatusEvent};

use crate::app::runs::machine::Initiator;
use crate::app::runs::{MAX_COMMIT_RETRIES, RunManager, contention, is_stale};
use crate::error::AppError;
use crate::ports::runs::RunWrite;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailRun {
    pub org: OrgId,
    pub run: RunId,
    pub error: RunError,
}

impl RunManager {
    /// No-op on a terminal run; returns the final status. `machine::step` sends `Abort`.
    pub async fn fail_run(&self, req: FailRun) -> Result<RunStatus, AppError> {
        for _ in 0..MAX_COMMIT_RETRIES {
            let mut rec = self.d.runs.get(req.org, req.run).await?;
            if rec.run.status.is_terminal() {
                return Ok(rec.run.status);
            }
            let now = self.now();
            let mut w = RunWrite::default();
            rec.run.set_error(req.error.clone());
            let rec = self.step_and_queue(rec, RunStatusEvent::InfraError, Initiator::Server, now, &mut w).await?;
            match self.d.runs.commit(req.org, w).await {
                Ok(_) => return Ok(rec.run.status),
                Err(e) if is_stale(&e) => continue,
                Err(e) => return Err(e.into()),
            }
        }
        Err(contention())
    }
}
