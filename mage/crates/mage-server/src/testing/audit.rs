//! `MemAuditLog`: vector-backed `AuditLog` (server.md "Testing" fakes).

use std::sync::Arc;

use async_trait::async_trait;
use mage_core::domain::OrgId;
use mage_core::ports::clock::Clock;
use parking_lot::Mutex;

use crate::ports::audit::{AUDIT_LIST_MAX, AuditError, AuditEvent, AuditFilter, AuditLog, AuditRecord};

pub struct MemAuditLog {
    clock: Arc<dyn Clock>,
    records: Mutex<Vec<AuditRecord>>,
}

impl MemAuditLog {
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self { clock, records: Mutex::new(Vec::new()) }
    }

    /// Every record, oldest first, platform rows included (test inspection).
    pub fn records(&self) -> Vec<AuditRecord> {
        self.records.lock().clone()
    }
}

#[async_trait]
impl AuditLog for MemAuditLog {
    async fn record(&self, event: AuditEvent) -> Result<i64, AuditError> {
        let mut records = self.records.lock();
        let id = records.last().map_or(1, |r| r.id + 1);
        records.push(AuditRecord { id, at: self.clock.now(), event });
        Ok(id)
    }

    async fn list(&self, org: OrgId, filter: AuditFilter, limit: u32) -> Result<Vec<AuditRecord>, AuditError> {
        let limit = limit.clamp(1, AUDIT_LIST_MAX) as usize;
        let records = self.records.lock();
        Ok(records
            .iter()
            .rev()
            .filter(|r| r.event.org == Some(org))
            .filter(|r| filter.before.is_none_or(|before| r.id < before))
            .filter(|r| filter.action.is_none_or(|a| r.event.action == a))
            .filter(|r| filter.actor.as_ref().is_none_or(|a| &r.event.actor == a))
            .take(limit)
            .cloned()
            .collect())
    }
}
