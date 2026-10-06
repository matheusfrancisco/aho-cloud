//! Use case 1, AcceptDelivery (intake.md): store a verified webhook delivery and its
//! `intake.<source>` job in one transaction. Driving adapter: `adapters::driving::http::hooks`.

use std::sync::Arc;

use mage_core::domain::JobPayload;
use mage_core::ports::NewJob;

use crate::error::AppError;
use crate::ports::hooks::WebhookSource;
use crate::ports::inbox::{Accepted, InboxError, NewDelivery, WebhookInbox};

/// A delivery whose signature (and freshness) the HTTP handler verified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboundDelivery {
    pub source: WebhookSource,
    pub delivery_id: String,
    pub event: String,
    pub body: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcceptOutcome {
    Accepted,
    Duplicate,
}

pub struct AcceptDelivery {
    inbox: Arc<dyn WebhookInbox>,
}

impl AcceptDelivery {
    pub fn new(inbox: Arc<dyn WebhookInbox>) -> Self {
        Self { inbox }
    }

    /// Tenant resolution waits for the job, so the handler does one round trip.
    pub async fn accept(&self, d: InboundDelivery) -> Result<AcceptOutcome, AppError> {
        let InboundDelivery { source, delivery_id, event, body } = d;
        let job = NewJob::new(JobPayload::IntakeDelivery { source, delivery_id: delivery_id.clone() });
        let delivery = NewDelivery { source, delivery_id, event, org_id: None, body };
        match self.inbox.accept(delivery, job).await {
            Ok(Accepted::New { .. }) => Ok(AcceptOutcome::Accepted),
            Ok(Accepted::Duplicate { .. }) => Ok(AcceptOutcome::Duplicate),
            Err(InboxError::Unavailable(msg)) => Err(AppError::Unavailable(msg)),
            Err(InboxError::Invalid(msg)) => Err(AppError::Invalid(msg)),
            Err(InboxError::Conflict) => Err(AppError::Conflict("webhook delivery".into())),
            Err(InboxError::NotFound) => Err(AppError::Internal("webhook inbox returned not found on accept".into())),
        }
    }
}
