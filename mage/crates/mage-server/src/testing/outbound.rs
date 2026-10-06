//! `MemOutboundLog`: in-memory `OutboundLog`.

use std::sync::Arc;

use async_trait::async_trait;
use mage_core::domain::{OrgId, TicketId};
use mage_core::ports::clock::Clock;
use mage_core::ports::ids::IdGen;
use parking_lot::Mutex;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::ports::outbound::{OutboundError, OutboundKey, OutboundLog, OutboundRecord, OutboundState, OutboundTarget};

pub struct MemOutboundLog {
    clock: Arc<dyn Clock>,
    ids: Arc<dyn IdGen>,
    rows: Mutex<Vec<Row>>,
}

struct Row {
    org: OrgId,
    /// Row id (`outbound_messages.id`); orders rows created in the same instant.
    id: Uuid,
    record: OutboundRecord,
}

impl MemOutboundLog {
    pub fn new(clock: Arc<dyn Clock>, ids: Arc<dyn IdGen>) -> Self {
        Self { clock, ids, rows: Mutex::new(Vec::new()) }
    }
}

fn find<'a>(rows: &'a mut [Row], org: OrgId, key: &OutboundKey) -> Result<&'a mut OutboundRecord, OutboundError> {
    rows.iter_mut().find(|r| r.org == org && r.record.key == *key).map(|r| &mut r.record).ok_or(OutboundError::NotFound)
}

/// Pending → `next` (reason in `last_error`); the same final state again is a no-op.
fn finish(record: &mut OutboundRecord, next: OutboundState, reason: &str) -> Result<(), OutboundError> {
    match &record.state {
        OutboundState::Pending { .. } => {
            record.state = next;
            record.last_error = Some(reason.to_owned());
            Ok(())
        }
        current if *current == next => Ok(()),
        _ => Err(OutboundError::Conflict),
    }
}

#[async_trait]
impl OutboundLog for MemOutboundLog {
    async fn begin(&self, org: OrgId, key: &OutboundKey, client_id: Uuid, body_sha256: [u8; 32]) -> Result<OutboundRecord, OutboundError> {
        let mut rows = self.rows.lock();
        if let Ok(existing) = find(&mut rows, org, key) {
            return Ok(existing.clone());
        }
        let record = OutboundRecord {
            key: key.clone(),
            client_id,
            state: OutboundState::Pending { uncertain: false },
            attempts: 0,
            body_sha256,
            external_id: None,
            last_error: None,
            created_at: self.clock.now(),
            sent_at: None,
        };
        rows.push(Row { org, id: self.ids.uuid_v7(), record: record.clone() });
        Ok(record)
    }

    async fn mark_attempt(&self, org: OrgId, key: &OutboundKey, uncertain: bool, error: Option<&str>) -> Result<(), OutboundError> {
        let mut rows = self.rows.lock();
        let record = find(&mut rows, org, key)?;
        if !matches!(record.state, OutboundState::Pending { .. }) {
            return Err(OutboundError::Conflict);
        }
        record.state = OutboundState::Pending { uncertain };
        record.attempts = record.attempts.saturating_add(1);
        record.last_error = error.map(str::to_owned);
        Ok(())
    }

    async fn mark_sent(&self, org: OrgId, key: &OutboundKey, external_id: &str, at: OffsetDateTime) -> Result<(), OutboundError> {
        let mut rows = self.rows.lock();
        let record = find(&mut rows, org, key)?;
        match &record.state {
            OutboundState::Pending { .. } => {
                record.state = OutboundState::Sent;
                record.external_id = Some(external_id.to_owned());
                record.sent_at = Some(at);
                Ok(())
            }
            OutboundState::Sent if record.external_id.as_deref() == Some(external_id) => Ok(()),
            _ => Err(OutboundError::Conflict),
        }
    }

    async fn mark_skipped(&self, org: OrgId, key: &OutboundKey, reason: &str) -> Result<(), OutboundError> {
        let mut rows = self.rows.lock();
        finish(find(&mut rows, org, key)?, OutboundState::Skipped { reason: reason.to_owned() }, reason)
    }

    async fn mark_failed(&self, org: OrgId, key: &OutboundKey, reason: &str) -> Result<(), OutboundError> {
        let mut rows = self.rows.lock();
        finish(find(&mut rows, org, key)?, OutboundState::Failed { reason: reason.to_owned() }, reason)
    }

    async fn list_for_ticket(&self, org: OrgId, ticket: TicketId) -> Result<Vec<OutboundRecord>, OutboundError> {
        let rows = self.rows.lock();
        let mut out: Vec<(OffsetDateTime, Uuid, OutboundRecord)> = rows
            .iter()
            .filter(|r| r.org == org && r.record.key.ticket_id == ticket)
            .map(|r| (r.record.created_at, r.id, r.record.clone()))
            .collect();
        out.sort_by_key(|(at, id, _)| (*at, *id));
        Ok(out.into_iter().map(|(_, _, r)| r).collect())
    }

    async fn find_by_external(&self, org: OrgId, target: OutboundTarget, external_id: &str) -> Result<Option<OutboundRecord>, OutboundError> {
        let rows = self.rows.lock();
        Ok(rows
            .iter()
            .find(|r| r.org == org && r.record.key.target == target && r.record.external_id.as_deref() == Some(external_id))
            .map(|r| r.record.clone()))
    }
}
