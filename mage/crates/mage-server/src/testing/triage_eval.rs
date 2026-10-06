//! `MemTriageEvalStore`: in-memory `TriageEvalStore`.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use mage_core::domain::{OrgId, RunId, TicketId, TriageEvalId};
use mage_core::ports::clock::Clock;
use mage_core::ports::ids::IdGen;
use parking_lot::Mutex;
use time::OffsetDateTime;

use crate::ports::triage_eval::{EvalCase, EvalCaseResult, EvalStoreError, GroundTruth, NewTriageEval, TriageEval, TriageEvalStore};

pub struct MemTriageEvalStore {
    clock: Arc<dyn Clock>,
    ids: Arc<dyn IdGen>,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    evals: BTreeMap<(OrgId, TriageEvalId), (NewTriageEval, OffsetDateTime)>,
    cases: BTreeMap<(OrgId, TriageEvalId, TicketId, u8), CaseRow>,
    labels: BTreeMap<(OrgId, TicketId), GroundTruth>,
}

struct CaseRow {
    input_run: Option<RunId>,
    run: Option<RunId>,
    result: Option<EvalCaseResult>,
}

impl MemTriageEvalStore {
    pub fn new(clock: Arc<dyn Clock>, ids: Arc<dyn IdGen>) -> Self {
        Self { clock, ids, state: Mutex::new(State::default()) }
    }
}

impl State {
    fn eval_cases(&self, org: OrgId, eval: TriageEvalId) -> impl Iterator<Item = (&(OrgId, TriageEvalId, TicketId, u8), &CaseRow)> {
        self.cases.iter().filter(move |((o, e, _, _), _)| *o == org && *e == eval)
    }
}

#[async_trait]
impl TriageEvalStore for MemTriageEvalStore {
    async fn create_eval(&self, org: OrgId, eval: NewTriageEval) -> Result<TriageEvalId, EvalStoreError> {
        let id = TriageEvalId::from_uuid(self.ids.uuid_v7());
        self.state.lock().evals.insert((org, id), (eval, self.clock.now()));
        Ok(id)
    }

    async fn add_cases(&self, org: OrgId, eval: TriageEvalId, cases: &[EvalCase]) -> Result<(), EvalStoreError> {
        let mut state = self.state.lock();
        if !state.evals.contains_key(&(org, eval)) {
            return Err(EvalStoreError::NotFound);
        }
        for case in cases {
            for repeat in 0..case.repeats {
                state.cases.entry((org, eval, case.ticket, repeat)).or_insert(CaseRow { input_run: case.input_run, run: None, result: None });
            }
        }
        Ok(())
    }

    async fn record_case_run(&self, org: OrgId, eval: TriageEvalId, ticket: TicketId, repeat: u8, run: RunId) -> Result<(), EvalStoreError> {
        let mut state = self.state.lock();
        let row = state.cases.get_mut(&(org, eval, ticket, repeat)).ok_or(EvalStoreError::NotFound)?;
        match row.run {
            Some(current) if current != run => Err(EvalStoreError::Conflict),
            _ => {
                row.run = Some(run);
                Ok(())
            }
        }
    }

    async fn record_case_result(&self, org: OrgId, result: &EvalCaseResult) -> Result<(), EvalStoreError> {
        let mut state = self.state.lock();
        let row = state.cases.get_mut(&(org, result.eval, result.ticket, result.repeat)).ok_or(EvalStoreError::NotFound)?;
        row.run = Some(result.run);
        row.result = Some(result.clone());
        Ok(())
    }

    async fn eval(&self, org: OrgId, eval: TriageEvalId) -> Result<TriageEval, EvalStoreError> {
        let state = self.state.lock();
        let (new, created_at) = state.evals.get(&(org, eval)).ok_or(EvalStoreError::NotFound)?;
        let cases = state.eval_cases(org, eval).count() as u32;
        let done = state.eval_cases(org, eval).filter(|(_, row)| row.result.is_some()).count() as u32;
        Ok(TriageEval { id: eval, name: new.name.clone(), variant_toml: new.variant_toml.clone(), created_at: *created_at, cases, done })
    }

    async fn case_results(&self, org: OrgId, eval: TriageEvalId) -> Result<Vec<EvalCaseResult>, EvalStoreError> {
        let state = self.state.lock();
        Ok(state.eval_cases(org, eval).filter_map(|(_, row)| row.result.clone()).collect())
    }

    async fn put_label(&self, org: OrgId, label: &GroundTruth) -> Result<(), EvalStoreError> {
        if label.paths.is_empty() {
            return Err(EvalStoreError::Invalid("a label needs at least one path".into()));
        }
        self.state.lock().labels.insert((org, label.ticket), label.clone());
        Ok(())
    }

    async fn labels(&self, org: OrgId, tickets: &[TicketId]) -> Result<Vec<GroundTruth>, EvalStoreError> {
        let state = self.state.lock();
        Ok(state.labels.iter().filter(|((o, t), _)| *o == org && tickets.contains(t)).map(|(_, l)| l.clone()).collect())
    }

    async fn cases(&self, org: OrgId, eval: TriageEvalId) -> Result<Vec<EvalCase>, EvalStoreError> {
        let state = self.state.lock();
        let mut out: Vec<EvalCase> = Vec::new();
        for ((_, _, ticket, _), row) in state.eval_cases(org, eval) {
            match out.last_mut() {
                Some(last) if last.ticket == *ticket => last.repeats += 1,
                _ => out.push(EvalCase { ticket: *ticket, input_run: row.input_run, repeats: 1 }),
            }
        }
        Ok(out)
    }

    async fn case_run(&self, org: OrgId, eval: TriageEvalId, ticket: TicketId, repeat: u8) -> Result<Option<RunId>, EvalStoreError> {
        self.state.lock().cases.get(&(org, eval, ticket, repeat)).map(|row| row.run).ok_or(EvalStoreError::NotFound)
    }
}
