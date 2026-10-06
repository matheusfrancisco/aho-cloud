//! `Retriever::triage_candidates`: stage signals, merging, ranking, layer gating, history boost.

use mage_core::domain::Layer;

use super::graph::app_fixture;
use super::{Fx, sha};
use crate::app::knowledge_query::{CandidateSet, RetrievalLimits, RetrievalWeights, Retriever, RetrieverDeps, Stage, TicketContext};

fn retriever(fx: &Fx) -> Retriever {
    Retriever::new(RetrieverDeps {
        knowledge: fx.knowledge.clone(),
        repos: fx.repos.clone(),
        tickets: fx.tickets.clone(),
        indexer: fx.indexer.clone(),
        weights: RetrievalWeights::default(),
    })
}

fn ticket() -> TicketContext {
    TicketContext {
        title: "Connection page broken".into(),
        body: "Opening /api/connections/42 shows an error.\nError: connection pg-prod not found\n    at /app/api/conn.go:5\n".into(),
        error_strings: vec!["connection pg-prod not found".into()],
        last_good: None,
    }
}

fn stage(set: &CandidateSet, stage: Stage) -> &crate::app::knowledge_query::StageReport {
    set.stages.iter().find(|s| s.stage == stage).unwrap_or_else(|| panic!("{stage:?} reported"))
}

#[tokio::test]
async fn signals_from_independent_stages_merge_on_one_symbol() {
    let fx = Fx::new().await;
    app_fixture(&fx).await;
    fx.file("api/unrelated.go", "package api\n").await;
    let set = retriever(&fx).triage_candidates(&fx.scope(), &ticket(), &RetrievalLimits::default()).await.unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(set.version, fx.key());
    let top = &set.candidates[0];
    assert_eq!(top.citation.path.as_str(), "api/conn.go");
    assert_eq!((top.citation.lines.start(), top.citation.lines.end()), (3, 8), "the unit is the enclosing symbol");
    assert_eq!(top.symbol.as_ref().map(|s| s.name.as_str()), Some("GetConnection"));
    let stages: Vec<Stage> = top.signals.iter().map(|s| s.stage).collect();
    for want in [Stage::StackFrame, Stage::ErrorMatch, Stage::Route] {
        assert!(stages.contains(&want), "{want:?} in {stages:?}");
    }
    let total: f32 = top.signals.iter().map(|s| s.contribution).sum();
    assert!((top.score - total).abs() < 1e-5, "score is the sum of contributions");
    assert!(set.candidates.windows(2).all(|w| w[0].score >= w[1].score), "ranked by score");

    assert!(stage(&set, Stage::StackFrame).ran && stage(&set, Stage::StackFrame).hits >= 1);
    assert!(stage(&set, Stage::ErrorMatch).ran);
    let semantic = stage(&set, Stage::Semantic);
    assert!(!semantic.ran && semantic.reason.as_deref().is_some_and(|r| r.contains("Semantic")), "{semantic:?}");
    assert_eq!(stage(&set, Stage::History).reason.as_deref(), Some("no last good version"));
    assert!(stage(&set, Stage::SimilarTicket).ran);
}

#[tokio::test]
async fn disabled_and_unready_layers_skip_their_stages() {
    let fx = Fx::new().await;
    app_fixture(&fx).await;
    let mut scope = fx.scope();
    scope.disabled_layers = vec![Layer::ErrorSites];
    let set = retriever(&fx).triage_candidates(&scope, &ticket(), &RetrievalLimits::default()).await.unwrap_or_else(|e| panic!("{e}"));
    let errors = stage(&set, Stage::ErrorMatch);
    assert!(!errors.ran);
    assert_eq!(errors.reason.as_deref(), Some("layer_disabled"));
    assert!(set.candidates.iter().all(|c| c.signals.iter().all(|s| s.stage != Stage::ErrorMatch)));
    assert_eq!(set.candidates[0].symbol.as_ref().map(|s| s.name.as_str()), Some("GetConnection"), "other stages still find it");

    fx.reset(Layer::AppMap).await;
    let set = retriever(&fx).triage_candidates(&fx.scope(), &ticket(), &RetrievalLimits::default()).await.unwrap_or_else(|e| panic!("{e}"));
    let route = stage(&set, Stage::Route);
    assert!(!route.ran && route.reason.as_deref().is_some_and(|r| r.contains("AppMap")), "{route:?}");
}

#[tokio::test]
async fn files_changed_since_last_good_are_boosted() {
    let fx = Fx::new().await;
    app_fixture(&fx).await;
    let last_good = sha(0x10);
    fx.other_version(last_good, &[("api/conn.go", "package api\n// old\n"), ("api/routes.go", "package api\n")]).await;
    let plain = retriever(&fx).triage_candidates(&fx.scope(), &ticket(), &RetrievalLimits::default()).await.unwrap_or_else(|e| panic!("{e}"));
    let mut ctx = ticket();
    ctx.last_good = Some(last_good);
    let boosted = retriever(&fx).triage_candidates(&fx.scope(), &ctx, &RetrievalLimits::default()).await.unwrap_or_else(|e| panic!("{e}"));
    let before = plain.candidates[0].score;
    let after = &boosted.candidates[0];
    assert!(after.signals.iter().any(|s| s.stage == Stage::History));
    assert!((after.score - before * RetrievalWeights::default().history_boost).abs() < 1e-4, "{} vs {before}", after.score);
    assert!(stage(&boosted, Stage::History).ran);
}

#[tokio::test]
async fn max_candidates_caps_the_output() {
    let fx = Fx::new().await;
    app_fixture(&fx).await;
    let limits = RetrievalLimits { max_candidates: 1, ..RetrievalLimits::default() };
    let set = retriever(&fx).triage_candidates(&fx.scope(), &ticket(), &limits).await.unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(set.candidates.len(), 1);
}
