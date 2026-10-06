//! `Cause` and `CauseStatus` (domain.md "Cause"; runs.md and triage.md Additions).

use std::ops::Deref;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::domain::citation::{Citation, Confidence, ReproMethod};
use crate::domain::error::{DomainError, ensure_same_org};
use crate::domain::evidence::EvidenceChecked;
use crate::domain::gates::ReproGatePassed;
use crate::domain::ids::{CauseId, GateId, OrgId, RunId, TicketId};
use crate::domain::ticket::Ticket;
use crate::domain::triage::CauseDraft;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum CauseStatus {
    Untested,
    /// A Diagnose run is testing the cause.
    Running(RunId),
    Reproduced {
        run: RunId,
        gate: GateId,
    },
    NotReproduced {
        run: RunId,
    },
    Inconclusive {
        run: RunId,
        reason: String,
    },
}

/// Events of the `CauseStatus` machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CauseEvent {
    StartRun(RunId),
    /// Only with the repro gate's proof.
    Reproduced(ReproGatePassed),
    NotReproduced(RunId),
    Inconclusive {
        run: RunId,
        reason: String,
    },
    /// The Diagnose run errored or was cancelled: back to the previous verdict (never
    /// `Reproduced` or `Running`).
    Restore(CauseStatus),
}

impl CauseEvent {
    pub fn name(&self) -> &'static str {
        match self {
            Self::StartRun(_) => "start_run",
            Self::Reproduced(_) => "reproduced",
            Self::NotReproduced(_) => "not_reproduced",
            Self::Inconclusive { .. } => "inconclusive",
            Self::Restore(_) => "restore",
        }
    }
}

impl CauseStatus {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Untested => "untested",
            Self::Running(_) => "running",
            Self::Reproduced { .. } => "reproduced",
            Self::NotReproduced { .. } => "not_reproduced",
            Self::Inconclusive { .. } => "inconclusive",
        }
    }

    /// The run currently testing the cause.
    pub fn running(&self) -> Option<RunId> {
        match self {
            Self::Running(run) => Some(*run),
            _ => None,
        }
    }

    /// A reproduced cause is final: it is never re-diagnosed. Verdict events must name the run the
    /// cause is `Running`; a different run is stale and refused.
    pub fn apply(self, event: CauseEvent) -> Result<CauseStatus, DomainError> {
        use CauseEvent as E;
        use CauseStatus as S;
        let invalid = |from: &S, event: &E| DomainError::InvalidTransition { from: from.name(), event: event.name() };
        match (&self, &event) {
            (S::Untested | S::NotReproduced { .. } | S::Inconclusive { .. }, E::StartRun(run)) => Ok(S::Running(*run)),
            (S::Running(current), E::Reproduced(proof)) if *current == proof.diagnose_run() => {
                Ok(S::Reproduced { run: *current, gate: proof.gate() })
            }
            (S::Running(current), E::NotReproduced(run)) if current == run => Ok(S::NotReproduced { run: *run }),
            (S::Running(current), E::Inconclusive { run, reason }) if current == run => {
                Ok(S::Inconclusive { run: *run, reason: reason.clone() })
            }
            (S::Running(_), E::Restore(previous)) => match previous {
                S::Untested | S::NotReproduced { .. } | S::Inconclusive { .. } => Ok(previous.clone()),
                S::Running(_) | S::Reproduced { .. } => Err(DomainError::invalid(format!("cannot restore a cause to {}", previous.name()))),
            },
            (S::Running(_), E::Reproduced(_) | E::NotReproduced(_) | E::Inconclusive { .. }) => {
                Err(DomainError::invalid("verdict from a run that is not testing the cause"))
            }
            _ => Err(invalid(&self, &event)),
        }
    }
}

/// Fields of a cause (read through `Cause`'s `Deref`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ts_rs::TS)]
pub struct CauseData {
    pub id: CauseId,
    pub ticket_id: TicketId,
    pub org_id: OrgId,
    /// Triage run that produced the cause; rank is scoped to it.
    pub triage_run: RunId,
    pub rank: u16,
    pub title: String,
    pub explanation: String,
    /// ≥ 1, checked by the evidence gate.
    pub evidence: Vec<Citation>,
    pub repro_steps: Vec<String>,
    pub confirm_if: String,
    pub rule_out_if: String,
    pub confidence: Confidence,
    pub method: ReproMethod,
    pub status: CauseStatus,
    #[serde(with = "time::serde::rfc3339::option", default)]
    #[ts(type = "string | null")]
    pub superseded_at: Option<OffsetDateTime>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ts_rs::TS)]
#[serde(transparent)]
pub struct Cause(CauseData);

impl Deref for Cause {
    type Target = CauseData;

    fn deref(&self) -> &CauseData {
        &self.0
    }
}

impl Cause {
    /// A new `Untested` cause. The citations come from the evidence proof, so a cause can never
    /// carry unchecked citations; the draft's own citations are not used.
    pub fn new(
        id: CauseId,
        org_id: OrgId,
        ticket: &Ticket,
        triage_run: RunId,
        rank: u16,
        draft: CauseDraft,
        proof: EvidenceChecked,
    ) -> Result<Cause, DomainError> {
        ensure_same_org(org_id, ticket.org_id)?;
        if rank == 0 {
            return Err(DomainError::invalid("cause rank starts at 1"));
        }
        let evidence = proof.into_citations();
        if evidence.is_empty() {
            return Err(DomainError::MissingEvidence);
        }
        let CauseDraft { title, explanation, repro_steps, confirm_if, rule_out_if, confidence, method, .. } = draft;
        Ok(Cause(CauseData {
            id,
            ticket_id: ticket.id,
            org_id,
            triage_run,
            rank,
            title,
            explanation,
            evidence,
            repro_steps,
            confirm_if,
            rule_out_if,
            confidence,
            method,
            status: CauseStatus::Untested,
            superseded_at: None,
        }))
    }

    /// Rebuild a stored cause (storage adapters only).
    pub fn rehydrate(data: CauseData) -> Cause {
        Cause(data)
    }

    pub fn into_data(self) -> CauseData {
        self.0
    }

    pub fn apply(&mut self, event: CauseEvent) -> Result<(), DomainError> {
        let next = self.0.status.clone().apply(event)?;
        self.0.status = next;
        Ok(())
    }

    pub fn start_run(&mut self, run: RunId) -> Result<(), DomainError> {
        self.apply(CauseEvent::StartRun(run))
    }

    /// The only path to `Reproduced`.
    pub fn mark_reproduced(&mut self, proof: ReproGatePassed) -> Result<(), DomainError> {
        if proof.cause() != self.0.id {
            return Err(DomainError::invalid("repro proof is for another cause"));
        }
        self.apply(CauseEvent::Reproduced(proof))
    }

    pub fn record_not_reproduced(&mut self, run: RunId) -> Result<(), DomainError> {
        self.apply(CauseEvent::NotReproduced(run))
    }

    pub fn record_inconclusive(&mut self, run: RunId, reason: impl Into<String>) -> Result<(), DomainError> {
        self.apply(CauseEvent::Inconclusive { run, reason: reason.into() })
    }

    pub fn restore(&mut self, previous: CauseStatus) -> Result<(), DomainError> {
        self.apply(CauseEvent::Restore(previous))
    }

    /// Retriage supersedes `Untested` causes; tested causes keep their history.
    pub fn supersede(&mut self, at: OffsetDateTime) -> Result<(), DomainError> {
        if self.0.status != CauseStatus::Untested {
            return Err(DomainError::invalid(format!("cannot supersede a {} cause", self.0.status.name())));
        }
        if self.0.superseded_at.is_some() {
            return Err(DomainError::invalid("cause already superseded"));
        }
        self.0.superseded_at = Some(at);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::evidence::{ManifestView, check};
    use crate::domain::gates::testing::repro_gate_passed;
    use crate::domain::ids::{BlobOid, CommitSha, ContentHash, RepoId, RepoVersionKey};
    use crate::domain::manifest::{EntryKind, ManifestEntry};
    use crate::domain::path::{FileClass, LineRange, RepoPath};
    use crate::domain::signature::{FailureSignature, Matcher, SignatureKind};
    use crate::domain::ticket::{TicketContent, TicketSource};
    use crate::domain::triage::CitationDraft;
    use time::macros::datetime;
    use uuid::Uuid;

    fn run(n: u128) -> RunId {
        RunId::from_uuid(Uuid::from_u128(n))
    }
    fn gate(n: u128) -> GateId {
        GateId::from_uuid(Uuid::from_u128(n))
    }
    fn cause_id() -> CauseId {
        CauseId::from_uuid(Uuid::from_u128(77))
    }
    fn proof_for(run_id: RunId, cause: CauseId) -> ReproGatePassed {
        repro_gate_passed(
            gate(5),
            run_id,
            cause,
            FailureSignature { kind: SignatureKind::HttpStatus, value: "500".into(), matcher: Matcher::Exact },
        )
    }

    fn statuses() -> Vec<CauseStatus> {
        vec![
            CauseStatus::Untested,
            CauseStatus::Running(run(1)),
            CauseStatus::Reproduced { run: run(1), gate: gate(5) },
            CauseStatus::NotReproduced { run: run(1) },
            CauseStatus::Inconclusive { run: run(1), reason: "r".into() },
        ]
    }

    fn events() -> Vec<CauseEvent> {
        vec![
            CauseEvent::StartRun(run(1)),
            CauseEvent::Reproduced(proof_for(run(1), cause_id())),
            CauseEvent::NotReproduced(run(1)),
            CauseEvent::Inconclusive { run: run(1), reason: "budget: max_wall".into() },
            CauseEvent::Restore(CauseStatus::NotReproduced { run: run(0) }),
        ]
    }

    /// Expected outcome of every (state, event) pair with the event naming the running run.
    fn expected(from: &CauseStatus, event: &CauseEvent) -> Option<CauseStatus> {
        use CauseEvent as E;
        use CauseStatus as S;
        match (from, event) {
            (S::Untested | S::NotReproduced { .. } | S::Inconclusive { .. }, E::StartRun(r)) => Some(S::Running(*r)),
            (S::Running(_), E::Reproduced(_)) => Some(S::Reproduced { run: run(1), gate: gate(5) }),
            (S::Running(_), E::NotReproduced(_)) => Some(S::NotReproduced { run: run(1) }),
            (S::Running(_), E::Inconclusive { .. }) => Some(S::Inconclusive { run: run(1), reason: "budget: max_wall".into() }),
            (S::Running(_), E::Restore(prev)) => Some(prev.clone()),
            _ => None,
        }
    }

    #[test]
    fn cause_status_full_transition_table() {
        for from in statuses() {
            for event in events() {
                let got = from.clone().apply(event.clone());
                match expected(&from, &event) {
                    Some(to) => assert_eq!(got, Ok(to), "{} + {}", from.name(), event.name()),
                    None => assert_eq!(
                        got,
                        Err(DomainError::InvalidTransition { from: from.name(), event: event.name() }),
                        "{} + {} must be invalid",
                        from.name(),
                        event.name()
                    ),
                }
            }
        }
    }

    #[test]
    fn verdicts_from_a_stale_run_are_refused() {
        let running = CauseStatus::Running(run(2));
        assert!(running.clone().apply(CauseEvent::NotReproduced(run(1))).is_err());
        assert!(running.clone().apply(CauseEvent::Inconclusive { run: run(1), reason: "x".into() }).is_err());
        assert!(running.clone().apply(CauseEvent::Reproduced(proof_for(run(1), cause_id()))).is_err());
        assert!(running.apply(CauseEvent::Reproduced(proof_for(run(2), cause_id()))).is_ok());
    }

    #[test]
    fn restore_never_reaches_reproduced_or_running() {
        let running = CauseStatus::Running(run(2));
        assert!(running.clone().apply(CauseEvent::Restore(CauseStatus::Reproduced { run: run(1), gate: gate(1) })).is_err());
        assert!(running.clone().apply(CauseEvent::Restore(CauseStatus::Running(run(1)))).is_err());
        assert_eq!(running.apply(CauseEvent::Restore(CauseStatus::Untested)), Ok(CauseStatus::Untested));
    }

    // ---- Cause::new with a real evidence check ----

    const KEY_SHA: [u8; 20] = [3; 20];

    fn org() -> OrgId {
        OrgId::from_uuid(Uuid::from_u128(1))
    }

    fn ticket(org_id: OrgId) -> Ticket {
        Ticket::new(
            TicketId::from_uuid(Uuid::from_u128(2)),
            org_id,
            TicketSource::Linear {
                issue_id: "i".into(),
                identifier: "ENG-1".into(),
                url: url::Url::parse("https://linear.app/x/issue/ENG-1").unwrap(),
            },
            TicketContent {
                title: "t".into(),
                body: "b".into(),
                content_hash: ContentHash::from_bytes([0; 32]),
                slack_thread: None,
                customer: None,
                target: None,
                error_strings: vec![],
            },
            datetime!(2026-01-01 00:00 UTC),
        )
    }

    fn view() -> ManifestView {
        let blob = BlobOid::from_bytes([9; 20]);
        let path = RepoPath::new("src/billing/export.ts").unwrap();
        let lines: Vec<String> = (1..=30).map(|i| format!("const line{i} = computeInvoiceTotal({i});")).collect();
        ManifestView::new(
            RepoVersionKey::new(RepoId::from_uuid(Uuid::from_u128(4)), CommitSha::from_bytes(KEY_SHA)),
            vec![ManifestEntry {
                path,
                kind: EntryKind::File,
                executable: false,
                blob,
                lang: None,
                class: FileClass::Source,
                size: 1000,
                is_text: true,
                line_count: lines.len() as u32,
            }],
            vec![(blob, lines)],
        )
    }

    fn draft(quote: Option<&str>) -> CauseDraft {
        CauseDraft {
            title: "Null customer".into(),
            explanation: "e".into(),
            evidence: vec![CitationDraft {
                path: RepoPath::new("src/billing/export.ts").unwrap(),
                lines: LineRange::new(10, 12).unwrap(),
                quote: quote.map(str::to_string),
                note: Some("total".into()),
            }],
            repro_steps: vec!["open export".into()],
            confirm_if: "500".into(),
            rule_out_if: "200".into(),
            confidence: Confidence::new(0.7).unwrap(),
            method: ReproMethod::Ui,
        }
    }

    fn new_cause(org_id: OrgId, ticket_org: OrgId) -> Result<Cause, DomainError> {
        let d = draft(Some("const line11 = computeInvoiceTotal(11);"));
        let proof = check(&d.evidence, &view()).into_checked()?;
        Cause::new(cause_id(), org_id, &ticket(ticket_org), run(9), 1, d, proof)
    }

    #[test]
    fn new_cause_takes_checked_citations() {
        let cause = new_cause(org(), org()).unwrap();
        assert_eq!(cause.status, CauseStatus::Untested);
        assert_eq!(cause.evidence.len(), 1);
        assert_eq!(cause.evidence[0].sha, CommitSha::from_bytes(KEY_SHA));
        assert!(cause.evidence[0].snippet_hash.is_some());
    }

    #[test]
    fn new_cause_refuses_other_tenant_and_rank_zero() {
        let other = OrgId::from_uuid(Uuid::from_u128(99));
        assert_eq!(new_cause(org(), other).unwrap_err(), DomainError::CrossTenant);
        let d = draft(Some("const line11 = computeInvoiceTotal(11);"));
        let proof = check(&d.evidence, &view()).into_checked().unwrap();
        assert!(Cause::new(cause_id(), org(), &ticket(org()), run(9), 0, d, proof).is_err());
    }

    #[test]
    fn mark_reproduced_requires_matching_proof() {
        let mut cause = new_cause(org(), org()).unwrap();
        assert!(cause.mark_reproduced(proof_for(run(3), cause_id())).is_err(), "not running");
        cause.start_run(run(3)).unwrap();
        let other_cause = CauseId::from_uuid(Uuid::from_u128(1234));
        assert!(cause.mark_reproduced(proof_for(run(3), other_cause)).is_err());
        cause.mark_reproduced(proof_for(run(3), cause_id())).unwrap();
        assert_eq!(cause.status, CauseStatus::Reproduced { run: run(3), gate: gate(5) });
        // Reproduced is final.
        assert!(cause.start_run(run(4)).is_err());
        assert!(cause.supersede(datetime!(2026-01-02 00:00 UTC)).is_err());
    }

    #[test]
    fn supersede_only_untested_once() {
        let mut cause = new_cause(org(), org()).unwrap();
        let at = datetime!(2026-01-02 00:00 UTC);
        cause.supersede(at).unwrap();
        assert_eq!(cause.superseded_at, Some(at));
        assert!(cause.supersede(at).is_err());
    }
}
