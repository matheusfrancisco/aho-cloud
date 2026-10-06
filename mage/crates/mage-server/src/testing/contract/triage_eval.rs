//! `TriageEvalStore` contract (triage.md "Evaluation harness", "Testing" → port contract tests):
//! evals and their per-(ticket, repeat) cases, case runs and results, frozen labels, tenancy.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use mage_core::domain::{OrgId, RepoPath, RunId, RunStatus, TicketId, TriageEvalId, Usage, UserId};
use mage_core::ports::clock::Clock;
use mage_core::testing::FakeClock;
use uuid::Uuid;

use crate::ports::triage_eval::{
    EvalCase, EvalCaseResult, EvalStoreError, GroundTruth, LabelSource, LabelStrength, NewTriageEval, TriageEval, TriageEvalStore,
};
use crate::testing::seed::{Seed, SeedRun};

/// A fresh, empty eval store.
pub struct EvalFixture {
    pub evals: Arc<dyn TriageEvalStore>,
    pub seed: Arc<dyn Seed>,
}

/// Runs every `TriageEvalStore` scenario, each against a fresh fixture from `make`.
pub async fn contract_triage_eval_store<F, Fut>(make: F)
where
    F: Fn(Arc<FakeClock>) -> Fut,
    Fut: Future<Output = EvalFixture>,
{
    macro_rules! scenario {
        ($name:ident) => {{
            let clock = Arc::new(FakeClock::new());
            let fixture = make(clock.clone()).await;
            $name(&clock, &fixture).await;
        }};
    }
    scenario!(evals_count_their_cases_and_keep_existing_rows);
    scenario!(case_runs_are_recorded_once);
    scenario!(results_mark_cases_done_in_ticket_order);
    scenario!(labels_are_upserted_and_filtered);
    scenario!(evals_are_tenant_scoped);
}

fn org(n: u128) -> OrgId {
    OrgId::from_uuid(Uuid::from_u128(0x0c00 + n))
}

fn ticket(n: u128) -> TicketId {
    TicketId::from_uuid(Uuid::from_u128(0x0c10 + n))
}

fn run(n: u128) -> RunId {
    RunId::from_uuid(Uuid::from_u128(0x0c20 + n))
}

fn user() -> UserId {
    UserId::from_uuid(Uuid::from_u128(0x0c31))
}

fn unknown_eval() -> TriageEvalId {
    TriageEvalId::from_uuid(Uuid::from_u128(0x0c41))
}

fn new_eval(name: &str) -> NewTriageEval {
    NewTriageEval {
        name: name.into(),
        corpus: "resolved-2026q1".into(),
        variant_toml: format!("name = \"{name}\"\nharness = \"omp\"\n"),
        repeat: 3,
        created_by: user(),
    }
}

fn case(ticket: TicketId, input_run: Option<RunId>, repeats: u8) -> EvalCase {
    EvalCase { ticket, input_run, repeats }
}

fn paths(list: &[&str]) -> BTreeSet<RepoPath> {
    list.iter().map(|p| RepoPath::new(*p).expect("repo path")).collect()
}

/// `org` with the user, `tickets` and one finished triage run per `(run, ticket)`.
async fn seed(f: &EvalFixture, org: OrgId, tickets: &[TicketId], runs: &[(RunId, TicketId)]) {
    f.seed.org(org).await;
    f.seed.user(user()).await;
    for t in tickets {
        f.seed.ticket(org, *t).await;
    }
    for (r, t) in runs {
        f.seed.run(org, *r, SeedRun::Triage { ticket: *t }).await;
    }
}

fn full_result(eval: TriageEvalId, ticket: TicketId, repeat: u8, run: RunId) -> EvalCaseResult {
    EvalCaseResult {
        eval,
        ticket,
        repeat,
        run,
        status: RunStatus::Succeeded,
        hit_rank: Some(2),
        primary_hit_rank: Some(3),
        causes: 4,
        usage: Usage { input_tokens: 120_000, output_tokens: 3_400, cost_micros: 450_000, turns: 17, wall: Duration::from_millis(95_250) },
        queue_wait: Duration::from_millis(1_500),
        agent_time: Duration::from_millis(93_750),
        gate_attempts: 2,
        dropped_causes: 1,
        adjusted_citations: 3,
    }
}

fn empty_result(eval: TriageEvalId, ticket: TicketId, repeat: u8, run: RunId) -> EvalCaseResult {
    EvalCaseResult {
        eval,
        ticket,
        repeat,
        run,
        status: RunStatus::TimedOut,
        hit_rank: None,
        primary_hit_rank: None,
        causes: 0,
        usage: Usage::default(),
        queue_wait: Duration::ZERO,
        agent_time: Duration::from_secs(600),
        gate_attempts: 0,
        dropped_causes: 0,
        adjusted_citations: 0,
    }
}

async fn evals_count_their_cases_and_keep_existing_rows(clock: &FakeClock, f: &EvalFixture) {
    seed(f, org(1), &[ticket(1), ticket(2)], &[(run(1), ticket(1))]).await;
    let t0 = clock.now();
    let id = f.evals.create_eval(org(1), new_eval("baseline")).await.unwrap();
    let mut expected = TriageEval { id, name: "baseline".into(), variant_toml: new_eval("baseline").variant_toml, created_at: t0, cases: 0, done: 0 };
    assert_eq!(f.evals.eval(org(1), id).await, Ok(expected.clone()));

    clock.advance(Duration::from_secs(1));
    let other = f.evals.create_eval(org(1), new_eval("candidate")).await.unwrap();
    assert_ne!(other, id, "every eval gets its own id");

    f.evals.add_cases(org(1), id, &[case(ticket(2), None, 1), case(ticket(1), Some(run(1)), 2)]).await.unwrap();
    expected.cases = 3;
    assert_eq!(f.evals.eval(org(1), id).await, Ok(expected.clone()), "one case row per (ticket, repeat)");
    assert_eq!(f.evals.cases(org(1), id).await, Ok(vec![case(ticket(1), Some(run(1)), 2), case(ticket(2), None, 1)]), "one entry per ticket, by ticket");
    assert_eq!(f.evals.cases(org(1), other).await, Ok(vec![]), "cases belong to their eval");

    f.evals.record_case_run(org(1), id, ticket(1), 0, run(1)).await.unwrap();
    f.evals.add_cases(org(1), id, &[case(ticket(1), Some(run(1)), 3), case(ticket(2), None, 0)]).await.unwrap();
    expected.cases = 4;
    assert_eq!(f.evals.eval(org(1), id).await, Ok(expected), "existing rows are kept, missing repeats added");
    assert_eq!(f.evals.cases(org(1), id).await, Ok(vec![case(ticket(1), Some(run(1)), 3), case(ticket(2), None, 1)]));
    assert_eq!(f.evals.case_run(org(1), id, ticket(1), 0).await, Ok(Some(run(1))), "re-adding a case keeps its recorded run");
    assert_eq!(f.evals.case_run(org(1), id, ticket(1), 2).await, Ok(None));
    assert_eq!(f.evals.case_run(org(1), id, ticket(1), 3).await, Err(EvalStoreError::NotFound));

    assert_eq!(f.evals.add_cases(org(1), unknown_eval(), &[case(ticket(1), None, 1)]).await, Err(EvalStoreError::NotFound));
    assert_eq!(f.evals.eval(org(1), unknown_eval()).await, Err(EvalStoreError::NotFound));
}

async fn case_runs_are_recorded_once(_clock: &FakeClock, f: &EvalFixture) {
    seed(f, org(1), &[ticket(1)], &[(run(1), ticket(1)), (run(2), ticket(1))]).await;
    let id = f.evals.create_eval(org(1), new_eval("baseline")).await.unwrap();
    f.evals.add_cases(org(1), id, &[case(ticket(1), None, 2)]).await.unwrap();

    assert_eq!(f.evals.record_case_run(org(1), id, ticket(1), 1, run(1)).await, Ok(()));
    assert_eq!(f.evals.record_case_run(org(1), id, ticket(1), 1, run(1)).await, Ok(()), "the same run again is a no-op");
    assert_eq!(f.evals.record_case_run(org(1), id, ticket(1), 1, run(2)).await, Err(EvalStoreError::Conflict));
    assert_eq!(f.evals.case_run(org(1), id, ticket(1), 1).await, Ok(Some(run(1))));
    assert_eq!(f.evals.case_run(org(1), id, ticket(1), 0).await, Ok(None), "runs are per repeat");
    assert_eq!(f.evals.record_case_run(org(1), id, ticket(1), 2, run(2)).await, Err(EvalStoreError::NotFound), "no such repeat");
    assert_eq!(f.evals.record_case_run(org(1), unknown_eval(), ticket(1), 0, run(2)).await, Err(EvalStoreError::NotFound));
    assert_eq!(f.evals.eval(org(1), id).await.map(|e| e.done), Ok(0), "a run alone does not finish a case");
}

async fn results_mark_cases_done_in_ticket_order(clock: &FakeClock, f: &EvalFixture) {
    seed(f, org(1), &[ticket(1), ticket(2)], &[(run(1), ticket(1)), (run(2), ticket(2)), (run(3), ticket(2))]).await;
    let id = f.evals.create_eval(org(1), new_eval("baseline")).await.unwrap();
    f.evals.add_cases(org(1), id, &[case(ticket(2), None, 2), case(ticket(1), None, 1)]).await.unwrap();
    assert_eq!(f.evals.case_results(org(1), id).await, Ok(vec![]), "no finished case yet");

    clock.advance(Duration::from_secs(60));
    let late = full_result(id, ticket(2), 1, run(3));
    assert_eq!(f.evals.record_case_result(org(1), &late).await, Ok(()));
    let early = empty_result(id, ticket(1), 0, run(1));
    assert_eq!(f.evals.record_case_result(org(1), &early).await, Ok(()));

    assert_eq!(f.evals.case_results(org(1), id).await, Ok(vec![early, late]), "finished cases only, by (ticket, repeat), every field kept");
    let eval = f.evals.eval(org(1), id).await.unwrap();
    assert_eq!((eval.cases, eval.done), (3, 2));
    assert_eq!(f.evals.case_run(org(1), id, ticket(2), 1).await, Ok(Some(run(3))), "the result records its run");
    assert_eq!(f.evals.case_run(org(1), id, ticket(2), 0).await, Ok(None));

    let missing = full_result(id, ticket(1), 1, run(2));
    assert_eq!(f.evals.record_case_result(org(1), &missing).await, Err(EvalStoreError::NotFound));
    let elsewhere = full_result(unknown_eval(), ticket(1), 0, run(1));
    assert_eq!(f.evals.record_case_result(org(1), &elsewhere).await, Err(EvalStoreError::NotFound));
    assert_eq!(f.evals.case_results(org(1), unknown_eval()).await, Ok(vec![]));
}

async fn labels_are_upserted_and_filtered(_clock: &FakeClock, f: &EvalFixture) {
    seed(f, org(1), &[ticket(1), ticket(2), ticket(3)], &[]).await;
    let fix_pr = GroundTruth { ticket: ticket(2), paths: paths(&["src/z.rs", "src/a.rs", "Src/b.rs"]), strength: LabelStrength::Strong, source: LabelSource::FixPr };
    let confirmed = GroundTruth { ticket: ticket(1), paths: paths(&["web/app.ts"]), strength: LabelStrength::Medium, source: LabelSource::ConfirmedCause };
    assert_eq!(f.evals.put_label(org(1), &fix_pr).await, Ok(()));
    assert_eq!(f.evals.put_label(org(1), &confirmed).await, Ok(()));
    assert_eq!(f.evals.labels(org(1), &[ticket(2), ticket(3), ticket(1)]).await, Ok(vec![confirmed.clone(), fix_pr.clone()]), "labelled tickets only, by ticket");
    assert_eq!(f.evals.labels(org(1), &[ticket(2)]).await, Ok(vec![fix_pr.clone()]));
    assert_eq!(f.evals.labels(org(1), &[]).await, Ok(vec![]));

    let manual = GroundTruth { ticket: ticket(1), paths: paths(&["web/app.ts", "web/api.ts"]), strength: LabelStrength::Strong, source: LabelSource::Manual };
    assert_eq!(f.evals.put_label(org(1), &manual).await, Ok(()), "a manual label replaces the automatic one");
    let empty = GroundTruth { paths: BTreeSet::new(), ..fix_pr.clone() };
    assert!(matches!(f.evals.put_label(org(1), &empty).await, Err(EvalStoreError::Invalid(_))), "a label needs a path");
    assert_eq!(f.evals.labels(org(1), &[ticket(1), ticket(2)]).await, Ok(vec![manual, fix_pr]), "the invalid label changed nothing");
}

async fn evals_are_tenant_scoped(_clock: &FakeClock, f: &EvalFixture) {
    seed(f, org(1), &[ticket(1)], &[(run(1), ticket(1))]).await;
    seed(f, org(2), &[ticket(1)], &[]).await;
    let id = f.evals.create_eval(org(1), new_eval("baseline")).await.unwrap();
    f.evals.add_cases(org(1), id, &[case(ticket(1), Some(run(1)), 1)]).await.unwrap();
    f.evals.record_case_result(org(1), &full_result(id, ticket(1), 0, run(1))).await.unwrap();
    let label = GroundTruth { ticket: ticket(1), paths: paths(&["src/lib.rs"]), strength: LabelStrength::Strong, source: LabelSource::FixPr };
    f.evals.put_label(org(1), &label).await.unwrap();

    assert_eq!(f.evals.eval(org(2), id).await, Err(EvalStoreError::NotFound));
    assert_eq!(f.evals.add_cases(org(2), id, &[case(ticket(1), None, 1)]).await, Err(EvalStoreError::NotFound));
    assert_eq!(f.evals.cases(org(2), id).await, Ok(vec![]));
    assert_eq!(f.evals.case_results(org(2), id).await, Ok(vec![]));
    assert_eq!(f.evals.case_run(org(2), id, ticket(1), 0).await, Err(EvalStoreError::NotFound));
    assert_eq!(f.evals.record_case_run(org(2), id, ticket(1), 0, run(1)).await, Err(EvalStoreError::NotFound));
    assert_eq!(f.evals.record_case_result(org(2), &empty_result(id, ticket(1), 0, run(1))).await, Err(EvalStoreError::NotFound));
    assert_eq!(f.evals.labels(org(2), &[ticket(1)]).await, Ok(vec![]));

    let theirs = GroundTruth { paths: paths(&["other/main.go"]), source: LabelSource::Manual, ..label.clone() };
    assert_eq!(f.evals.put_label(org(2), &theirs).await, Ok(()));
    assert_eq!(f.evals.labels(org(1), &[ticket(1)]).await, Ok(vec![label]), "another org's label of the same ticket id is separate");
    assert_eq!(f.evals.case_results(org(1), id).await.map(|r| r.len()), Ok(1), "org 1 data is untouched");
}

#[cfg(test)]
mod tests {
    use mage_core::testing::SeqIdGen;

    use super::*;
    use crate::testing::{MemTriageEvalStore, NoSeed};

    #[tokio::test]
    async fn mem_triage_eval_store_passes_the_contract() {
        contract_triage_eval_store(|clock| async move {
            EvalFixture { evals: Arc::new(MemTriageEvalStore::new(clock, Arc::new(SeqIdGen::new()))), seed: Arc::new(NoSeed) }
        })
        .await;
    }
}
