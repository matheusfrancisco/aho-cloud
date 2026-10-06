//! `GET …/causes/{cause}/fix-brief?version=` (fix.md F1 "Storage and exposure"): the stored
//! markdown (UI Copy is the raw markdown), the version picker range, and a signed download URL of
//! the immutable `brief-v{n}.md` artifact.

use std::sync::Arc;
use std::time::Duration;

use mage_core::domain::{CauseId, OrgId, TicketId};
use mage_core::ports::blob::{BlobKey, DownloadPolicy};
use mage_core::wire::api::FixBriefView;

use crate::app::fix::{FixDeps, blob_err};
use crate::error::AppError;

#[derive(Clone)]
pub struct GetFixBrief {
    d: Arc<FixDeps>,
}

impl GetFixBrief {
    pub fn new(d: Arc<FixDeps>) -> Self {
        Self { d }
    }

    pub async fn run(&self, org: OrgId, ticket: TicketId, cause: CauseId, version: Option<u32>) -> Result<FixBriefView, AppError> {
        let d = &self.d;
        let c = d.tickets.get_cause(org, cause).await?;
        if c.ticket_id != ticket {
            return Err(AppError::NotFound("cause".into()));
        }
        let briefs = d.tickets.fix_briefs(org, cause).await?;
        let latest_version = briefs.iter().map(|b| b.version).max().ok_or_else(|| AppError::NotFound("fix brief".into()))?;
        let wanted = version.unwrap_or(latest_version);
        let brief = briefs.into_iter().find(|b| b.version == wanted).ok_or_else(|| AppError::NotFound(format!("fix brief version {wanted}")))?;
        let rec = d.runs.artifact(org, brief.artifact).await?;
        let key = BlobKey::parse(&rec.artifact.key).map_err(blob_err)?;
        let url = d.artifacts.signed_download(&key, &DownloadPolicy { ttl: Duration::from_secs(d.config.brief_url_ttl_secs) }).await.map_err(blob_err)?;
        Ok(FixBriefView {
            markdown: brief.markdown,
            version: brief.version,
            latest_version,
            diagnose_run: brief.diagnose_run,
            created_at: brief.created_at,
            artifact_url: url.url,
        })
    }
}
