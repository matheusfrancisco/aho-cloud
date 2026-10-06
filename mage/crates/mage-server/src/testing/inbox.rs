//! `MemWebhookInbox`: in-memory `WebhookInbox`. `accept` validates the job, then stores the row and
//! enqueues the job into the shared `MemJobQueue` in one lock scope. `set_unavailable(true)` makes
//! every call fail with `Unavailable` (fault injection for handler tests).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use mage_core::domain::OrgId;
use mage_core::ports::clock::Clock;
use mage_core::ports::jobs::{Enqueued, NewJob};
use mage_core::testing::MemJobQueue;
use parking_lot::Mutex;
use sha2::{Digest, Sha256};
use time::OffsetDateTime;

use crate::ports::inbox::{Accepted, Delivery, InboxError, InboxOutcome, NewDelivery, WebhookInbox, WebhookSource, check_delivery_job};

pub struct MemWebhookInbox {
    clock: Arc<dyn Clock>,
    jobs: Arc<MemJobQueue>,
    rows: Mutex<Vec<Delivery>>,
    unavailable: AtomicBool,
}

impl MemWebhookInbox {
    pub fn new(clock: Arc<dyn Clock>, jobs: Arc<MemJobQueue>) -> Self {
        Self { clock, jobs, rows: Mutex::new(Vec::new()), unavailable: AtomicBool::new(false) }
    }

    /// Every stored delivery, oldest first (test inspection).
    pub fn deliveries(&self) -> Vec<Delivery> {
        self.rows.lock().clone()
    }

    /// While set, every call returns `InboxError::Unavailable("injected")`.
    pub fn set_unavailable(&self, on: bool) {
        self.unavailable.store(on, Ordering::SeqCst);
    }

    fn up(&self) -> Result<(), InboxError> {
        if self.unavailable.load(Ordering::SeqCst) { Err(InboxError::Unavailable("injected".into())) } else { Ok(()) }
    }
}

fn find<'a>(rows: &'a mut [Delivery], source: WebhookSource, delivery_id: &str) -> Result<&'a mut Delivery, InboxError> {
    rows.iter_mut().find(|d| d.source == source && d.delivery_id == delivery_id).ok_or(InboxError::NotFound)
}

#[async_trait]
impl WebhookInbox for MemWebhookInbox {
    async fn accept(&self, d: NewDelivery, job: NewJob) -> Result<Accepted, InboxError> {
        self.up()?;
        check_delivery_job(&d, &job)?;
        MemJobQueue::validate_all(std::slice::from_ref(&job)).map_err(|e| InboxError::Invalid(e.to_string()))?;
        let sha: [u8; 32] = Sha256::digest(&d.body).into();
        let mut rows = self.rows.lock();
        let first_seen: Option<OffsetDateTime> = rows
            .iter()
            .filter(|r| r.source == d.source && (r.delivery_id == d.delivery_id || r.body_sha256 == sha))
            .map(|r| r.received_at)
            .min();
        if let Some(first_seen_at) = first_seen {
            return Ok(Accepted::Duplicate { first_seen_at });
        }
        let job_id = match self.jobs.insert_validated(std::slice::from_ref(&job)).as_slice() {
            [Enqueued::New(id)] | [Enqueued::Duplicate(id)] => *id,
            other => return Err(InboxError::Unavailable(format!("outbox returned {} results for one job", other.len()))),
        };
        rows.push(Delivery {
            source: d.source,
            delivery_id: d.delivery_id,
            event: d.event,
            org_id: d.org_id,
            received_at: self.clock.now(),
            body: Some(d.body),
            body_sha256: sha,
            outcome: None,
            processed_at: None,
        });
        Ok(Accepted::New { job_id })
    }

    async fn get(&self, source: WebhookSource, delivery_id: &str) -> Result<Delivery, InboxError> {
        self.up()?;
        find(&mut self.rows.lock(), source, delivery_id).map(|d| d.clone())
    }

    async fn resolve_org(&self, source: WebhookSource, delivery_id: &str, org: OrgId) -> Result<(), InboxError> {
        self.up()?;
        let mut rows = self.rows.lock();
        let row = find(&mut rows, source, delivery_id)?;
        match row.org_id {
            Some(current) if current != org => Err(InboxError::Conflict),
            _ => {
                row.org_id = Some(org);
                Ok(())
            }
        }
    }

    async fn mark_processed(&self, source: WebhookSource, delivery_id: &str, outcome: InboxOutcome) -> Result<(), InboxError> {
        self.up()?;
        let now = self.clock.now();
        let mut rows = self.rows.lock();
        let row = find(&mut rows, source, delivery_id)?;
        row.outcome = Some(outcome);
        row.processed_at = Some(now);
        Ok(())
    }

    async fn purge(&self, bodies_before: OffsetDateTime, rows_before: OffsetDateTime) -> Result<u64, InboxError> {
        self.up()?;
        let mut rows = self.rows.lock();
        let mut purged = 0u64;
        for row in rows.iter_mut() {
            if row.body.is_some() && row.processed_at.is_some_and(|at| at < bodies_before) {
                row.body = None;
                purged += 1;
            }
        }
        let before = rows.len();
        rows.retain(|r| r.received_at >= rows_before);
        Ok(purged + (before - rows.len()) as u64)
    }
}
