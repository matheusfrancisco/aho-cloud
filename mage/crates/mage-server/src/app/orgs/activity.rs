//! Audit log and usage reads (`GET …/audit`, `GET …/usage`; Admin).

use std::collections::BTreeMap;
use std::sync::Arc;

use mage_core::domain::{OrgId, RunKind};
use mage_core::wire::api::{AuditActor, AuditDetail, AuditEntry, AuditPage, AuditQuery, UsageGroupBy, UsageQuery, UsageReport, UsageRow};

use crate::error::AppError;
use crate::ports::audit::{AuditAction, AuditFilter, AuditLog, AuditRecord};
use crate::ports::runs::{RunStore, RunUsage};

const AUDIT_PAGE_DEFAULT: u32 = 50;
const AUDIT_PAGE_MAX: u32 = 200;

pub struct Activity {
    audit: Arc<dyn AuditLog>,
    runs: Arc<dyn RunStore>,
}

impl Activity {
    pub fn new(audit: Arc<dyn AuditLog>, runs: Arc<dyn RunStore>) -> Activity {
        Activity { audit, runs }
    }

    /// Newest first; `next_before` pages further back when the page is full.
    pub async fn audit_page(&self, org: OrgId, query: &AuditQuery) -> Result<AuditPage, AppError> {
        let action = match query.action.as_deref().filter(|a| !a.is_empty()) {
            Some(name) => Some(
                AuditAction::ALL.iter().copied().find(|a| a.as_str() == name).ok_or_else(|| AppError::Invalid(format!("unknown audit action {name:?}")))?,
            ),
            None => None,
        };
        let limit = query.limit.unwrap_or(AUDIT_PAGE_DEFAULT).clamp(1, AUDIT_PAGE_MAX);
        let records = self.audit.list(org, AuditFilter { action, actor: None, before: query.before }, limit).await?;
        let full = u32::try_from(records.len()).unwrap_or(u32::MAX) >= limit;
        let next_before = if full { records.last().map(|r| r.id) } else { None };
        Ok(AuditPage { entries: records.into_iter().map(entry).collect(), next_before })
    }

    /// Metered cost of runs created in `[from, to)`, grouped and sorted by cost (highest first).
    pub async fn usage(&self, org: OrgId, query: &UsageQuery) -> Result<UsageReport, AppError> {
        if query.from >= query.to {
            return Err(AppError::Invalid("`from` must be before `to`".into()));
        }
        let runs = self.runs.usage(org, query.from, query.to).await?;
        Ok(report(query, &runs))
    }
}

fn entry(r: AuditRecord) -> AuditEntry {
    let e = r.event;
    AuditEntry {
        id: r.id,
        at: r.at,
        actor: AuditActor::from(&e.actor),
        action: e.action.as_str().to_owned(),
        target_kind: e.target.kind.into_owned(),
        target_id: e.target.id,
        outcome: e.outcome,
        ip: e.ip.map(|ip| ip.to_string()),
        request_id: e.request_id,
        details: e.details.into_iter().map(|(k, v)| AuditDetail { key: k.into_owned(), value: v }).collect(),
    }
}

fn kind_label(kind: RunKind) -> &'static str {
    match kind {
        RunKind::Learn => "learn",
        RunKind::Triage => "triage",
        RunKind::Diagnose => "diagnose",
        RunKind::Verify => "verify",
        RunKind::Fix => "fix",
    }
}

/// Groups run usage per `group_by`: runs without a ticket share the key `none` when grouped
/// by ticket; runs without a model (Verify) share `none` when grouped by model.
pub fn report(query: &UsageQuery, runs: &[RunUsage]) -> UsageReport {
    let mut rows: BTreeMap<String, UsageRow> = BTreeMap::new();
    for run in runs {
        let (key, label) = match query.group_by {
            UsageGroupBy::Run => (run.run_id.to_string(), Some(kind_label(run.kind).to_owned())),
            UsageGroupBy::Ticket => (run.ticket_id.map_or_else(|| "none".to_owned(), |t| t.to_string()), None),
            UsageGroupBy::Model => (if run.model.is_empty() { "none".to_owned() } else { run.model.clone() }, None),
        };
        let row = rows.entry(key.clone()).or_insert_with(|| UsageRow { key, label, runs: 0, input_tokens: 0, output_tokens: 0, cost_micros: 0 });
        row.runs += 1;
        row.input_tokens = row.input_tokens.saturating_add(run.usage.input_tokens);
        row.output_tokens = row.output_tokens.saturating_add(run.usage.output_tokens);
        row.cost_micros = row.cost_micros.saturating_add(run.usage.cost_micros);
    }
    let mut rows: Vec<UsageRow> = rows.into_values().collect();
    rows.sort_by(|a, b| b.cost_micros.cmp(&a.cost_micros).then_with(|| a.key.cmp(&b.key)));
    let total_cost_micros = rows.iter().fold(0u64, |acc, r| acc.saturating_add(r.cost_micros));
    UsageReport { from: query.from, to: query.to, group_by: query.group_by, rows, total_cost_micros }
}

#[cfg(test)]
mod tests {
    use mage_core::domain::{RunId, TicketId, Usage};
    use time::OffsetDateTime;

    use super::*;

    fn run(n: u128, kind: RunKind, ticket: Option<u128>, model: &str, cost: u64) -> RunUsage {
        RunUsage {
            run_id: RunId::from(uuid::Uuid::from_u128(n)),
            kind,
            ticket_id: ticket.map(|t| TicketId::from(uuid::Uuid::from_u128(t))),
            model: model.into(),
            usage: Usage { input_tokens: 10, output_tokens: 5, cost_micros: cost, ..Usage::default() },
            created_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    fn query(group_by: UsageGroupBy) -> UsageQuery {
        UsageQuery { from: OffsetDateTime::UNIX_EPOCH, to: OffsetDateTime::UNIX_EPOCH + time::Duration::days(1), group_by }
    }

    #[test]
    fn usage_groups_by_model_ticket_and_run_sorted_by_cost() {
        let runs = [
            run(1, RunKind::Triage, Some(100), "claude-a", 300),
            run(2, RunKind::Diagnose, Some(100), "claude-b", 500),
            run(3, RunKind::Learn, None, "claude-a", 50),
            run(4, RunKind::Verify, None, "", 0),
        ];
        let by_model = report(&query(UsageGroupBy::Model), &runs);
        let keys: Vec<_> = by_model.rows.iter().map(|r| (r.key.as_str(), r.runs, r.cost_micros)).collect();
        assert_eq!(keys, [("claude-b", 1, 500), ("claude-a", 2, 350), ("none", 1, 0)]);
        assert_eq!(by_model.total_cost_micros, 850);

        let by_ticket = report(&query(UsageGroupBy::Ticket), &runs);
        assert_eq!(by_ticket.rows.len(), 2);
        assert_eq!((by_ticket.rows[0].runs, by_ticket.rows[0].cost_micros, by_ticket.rows[0].input_tokens), (2, 800, 20));
        assert_eq!(by_ticket.rows[1].key, "none");

        let by_run = report(&query(UsageGroupBy::Run), &runs);
        assert_eq!(by_run.rows.len(), 4);
        assert_eq!(by_run.rows[0].label.as_deref(), Some("diagnose"));
    }
}
