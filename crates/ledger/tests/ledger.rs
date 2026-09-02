//! The public ledger API end to end: stage rules, quorum by diff, rollback, standing orders,
//! taint, persistence, tampering and audit.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;

use aho_log::{Entry, EntryId, EntryKind, JsonlLog, LogError, LogStore};
use aho_types::Hash;
use cloud_contract::diff::Category;
use cloud_contract::ledger::{
    ActivationReason, Approver, LedgerBody, ProofMethod, Proposer, Provenance, Stage, TaintFinding, approval_message,
};
use cloud_contract::manifest::{Domain, DomainMode, Source};
use cloud_contract::policy::{OnCallAction, ReleasePolicy, Requirement, StandingOrder};
use cloud_contract::{Manifest, RefusalCode};
use cloud_ledger::{Ledger, LedgerError, LedgerState, custom_folds, policy_message, standing_orders_message};

const PROJECT: &str = "acme-shop";

// ── fixtures ──────────────────────────────────────────────────────────────────────────────────

fn base() -> Manifest {
    Manifest::from_json(include_str!("fixtures/app.json")).unwrap()
}

fn code_only() -> Manifest {
    let mut m = base();
    m.source = Source::Git { repo: "https://github.com/acme/shop".into(), commit: "1".repeat(40) };
    m
}

fn with_egress() -> Manifest {
    let mut m = base();
    m.egress.push("*.sentry.io".into());
    m
}

fn with_domain() -> Manifest {
    let mut m = base();
    m.domains.push(Domain { hostname: "shop.acme.com".into(), verification: DomainMode::Cname });
    m
}

fn digest() -> String {
    format!("sha256:{}", "a".repeat(64))
}

/// Test signature scheme: the verifier recomputes it from the user and the exact message bytes.
fn sign(user: &str, message: &[u8]) -> String {
    format!("{user}:{}", Hash::of(message).to_hex())
}

fn verify(a: &Approver, message: &[u8]) -> bool {
    a.signature == sign(&a.user_id, message)
}

fn signed(user: &str, owner: bool, message: &[u8]) -> Approver {
    Approver {
        user_id: user.into(),
        email: format!("{user}@acme.com"),
        owner,
        method: ProofMethod::DeviceKey,
        signature: sign(user, message),
    }
}

/// An approval of `release` (with manifest `m`) at `stage`, signed over the real message.
fn approval(user: &str, owner: bool, release: &str, m: &Manifest, stage: Stage) -> Approver {
    signed(user, owner, &approval_message(PROJECT, release, &m.hash(), stage))
}

fn ana(release: &str, m: &Manifest, stage: Stage) -> Approver {
    approval("ana", true, release, m, stage)
}

fn bo(release: &str, m: &Manifest, stage: Stage) -> Approver {
    approval("bo", false, release, m, stage)
}

fn cy(release: &str, m: &Manifest, stage: Stage) -> Approver {
    approval("cy", false, release, m, stage)
}

fn owner_grant(orders: &[StandingOrder]) -> Approver {
    signed("ana", true, &standing_orders_message(PROJECT, orders))
}

fn propose<S: LogStore>(ledger: &Ledger<S>, release: &str, m: &Manifest) {
    ledger.propose(release, m.clone(), Provenance::Unattested { commit: None, ci_run: None }, Proposer::Ci { name: "ci".into() }).unwrap();
}

/// Proposed, built and active at Preview.
fn previewed<S: LogStore>(ledger: &Ledger<S>, release: &str, m: &Manifest) {
    propose(ledger, release, m);
    ledger.image_built(release, &digest()).unwrap();
    ledger.activate(release, Stage::Preview).unwrap();
}

/// All the way to Live with ana (owner) and bo approving each stage.
fn ship<S: LogStore>(ledger: &Ledger<S>, release: &str, m: &Manifest) {
    previewed(ledger, release, m);
    for stage in [Stage::Staging, Stage::Live] {
        ledger.approve(release, stage, ana(release, m, stage)).unwrap();
        ledger.approve(release, stage, bo(release, m, stage)).unwrap();
        ledger.activate(release, stage).unwrap();
    }
}

fn live(st: &LedgerState) -> &str {
    &st.active[&Stage::Live]
}

/// Rewrites a JSONL ledger after `edit`, re-deriving ids, parents and the whole hash chain the
/// way an attacker with write access to the file would.
fn rechain(path: &Path, edit: impl FnOnce(&mut Vec<Entry>)) {
    let text = std::fs::read_to_string(path).unwrap();
    let mut entries: Vec<Entry> = text.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    edit(&mut entries);
    let mut prev = Hash::ZERO;
    let mut out = String::new();
    for (i, e) in entries.iter_mut().enumerate() {
        e.id = EntryId(i as u64 + 1);
        e.parent = (i > 0).then_some(EntryId(i as u64));
        e.prev_hash = prev;
        e.hash = Hash::chain(&prev, &serde_json::to_vec(&(e.id, e.parent, e.at, &e.by, &e.kind)).unwrap());
        prev = e.hash;
        out.push_str(&serde_json::to_string(e).unwrap());
        out.push('\n');
    }
    std::fs::write(path, out).unwrap();
}

fn body(e: &Entry) -> LedgerBody {
    let EntryKind::Custom { body, .. } = &e.kind else { panic!("not a custom entry") };
    serde_json::from_value(body.clone()).unwrap()
}

fn custom(like: &Entry, b: &LedgerBody) -> Entry {
    let mut e = like.clone();
    e.kind = EntryKind::Custom { ns: "deploy".into(), name: b.name().into(), body: serde_json::to_value(b).unwrap() };
    e
}

// ── happy path ───────────────────────────────────────────────────────────────────────────────

#[test]
fn a_release_goes_preview_staging_live() {
    let ledger = Ledger::memory(PROJECT);
    let m = base();
    propose(&ledger, "r1", &m);
    ledger.image_built("r1", &digest()).unwrap();
    ledger.observe("r1", vec![8080], vec!["api.stripe.com".into()]).unwrap();
    ledger.taint_checked("r1", vec![]).unwrap();
    ledger.activate("r1", Stage::Preview).unwrap();

    // First release: everything widens from nothing, auth included, so 2 approvals with an owner.
    let st = ledger.state().unwrap();
    assert_eq!(st.requirement("r1", Stage::Preview), Some(Requirement { approvals: 0, owner_required: false }));
    assert_eq!(st.requirement("r1", Stage::Staging), Some(Requirement { approvals: 2, owner_required: true }));

    let first = ledger.approve("r1", Stage::Staging, bo("r1", &m, Stage::Staging)).unwrap();
    assert_eq!((first.approvals, first.owner_present, first.met), (1, false, false));
    let second = ledger.approve("r1", Stage::Staging, ana("r1", &m, Stage::Staging)).unwrap();
    assert_eq!((second.approvals, second.owner_present, second.met), (2, true, true));
    ledger.activate("r1", Stage::Staging).unwrap();

    ledger.approve("r1", Stage::Live, ana("r1", &m, Stage::Live)).unwrap();
    ledger.approve("r1", Stage::Live, bo("r1", &m, Stage::Live)).unwrap();
    ledger.activate("r1", Stage::Live).unwrap();

    let st = ledger.state().unwrap();
    for stage in [Stage::Preview, Stage::Staging, Stage::Live] {
        assert_eq!(st.active[&stage], "r1");
    }
    let r = &st.releases["r1"];
    assert_eq!(r.manifest_hash, m.hash());
    assert_eq!(r.image_digest.as_deref(), Some(digest().as_str()));
    assert_eq!(r.observed, Some((vec![8080], vec!["api.stripe.com".to_string()])));
    assert_eq!(
        r.activations,
        vec![
            (Stage::Preview, ActivationReason::Promotion),
            (Stage::Staging, ActivationReason::Promotion),
            (Stage::Live, ActivationReason::Promotion)
        ]
    );
    assert_eq!(r.approvals.len(), 4);
    // Activation spent the Live approvals; the next Live activation of r1 needs fresh ones.
    assert_eq!(st.approval_status("r1", Stage::Live).unwrap().approvals, 0);

    let entries = ledger.entries().unwrap();
    assert_eq!(entries.len(), 11);
    assert!(entries.windows(2).all(|w| w[0].id < w[1].id && w[0].at <= w[1].at));
    assert!(entries.iter().all(|e| e.hash.len() == 64 && e.hash.bytes().all(|b| b.is_ascii_hexdigit())));
    assert!(matches!(entries[0].body, LedgerBody::ReleaseProposed { .. }));
    assert!(matches!(entries[10].body, LedgerBody::Activated { stage: Stage::Live, reason: ActivationReason::Promotion, .. }));
}

// ── refusals ─────────────────────────────────────────────────────────────────────────────────

#[test]
fn proposals_are_refused_for_bad_ids_duplicates_and_invalid_manifests() {
    let ledger = Ledger::memory(PROJECT);
    propose(&ledger, "r1", &base());
    let again =
        ledger.propose("r1", code_only(), Provenance::Unattested { commit: None, ci_run: None }, Proposer::Ci { name: "ci".into() });
    assert!(matches!(again, Err(LedgerError::DuplicateRelease(id)) if id == "r1"));

    let mut bad = base();
    bad.budget.ceiling_usd_month = 0;
    bad.owner.email = "nobody".into();
    let err =
        ledger.propose("r2", bad, Provenance::Unattested { commit: None, ci_run: None }, Proposer::Ci { name: "ci".into() }).unwrap_err();
    let LedgerError::InvalidManifest(refusals) = err else { panic!("{err:?}") };
    let codes: Vec<RefusalCode> = refusals.iter().map(|r| r.code).collect();
    assert!(codes.contains(&RefusalCode::BudgetZero) && codes.contains(&RefusalCode::OwnerEmail), "{codes:?}");

    for id in ["", "a/b", "has space", "x".repeat(129).as_str()] {
        let err = ledger.propose(id, base(), Provenance::Unattested { commit: None, ci_run: None }, Proposer::Ci { name: "ci".into() });
        assert!(matches!(err, Err(LedgerError::InvalidReleaseId(_))), "{id:?}: {err:?}");
    }
    assert_eq!(ledger.entries().unwrap().len(), 1, "refused proposals append nothing");
}

#[test]
fn builds_are_recorded_once_with_a_real_digest() {
    let ledger = Ledger::memory(PROJECT);
    assert!(matches!(ledger.image_built("nope", &digest()), Err(LedgerError::UnknownRelease(id)) if id == "nope"));
    propose(&ledger, "r1", &base());
    for bad in ["latest", "sha256:abc", format!("sha256:{}", "A".repeat(64)).as_str(), format!("sha512:{}", "a".repeat(64)).as_str()] {
        assert!(matches!(ledger.image_built("r1", bad), Err(LedgerError::InvalidDigest(_))), "{bad}");
    }
    ledger.image_built("r1", &digest()).unwrap();
    let other = format!("sha256:{}", "b".repeat(64));
    assert!(matches!(ledger.image_built("r1", &other), Err(LedgerError::AlreadyBuilt { .. })));
}

#[test]
fn every_action_on_an_unknown_release_is_refused() {
    let ledger = Ledger::memory(PROJECT);
    let m = base();
    let unknown = |r: Result<(), LedgerError>| assert!(matches!(r, Err(LedgerError::UnknownRelease(_))), "{r:?}");
    unknown(ledger.observe("x", vec![], vec![]));
    unknown(ledger.taint_checked("x", vec![]));
    unknown(ledger.approve("x", Stage::Staging, ana("x", &m, Stage::Staging)).map(drop));
    unknown(ledger.reject("x", Stage::Staging, "ana", "no"));
    unknown(ledger.activate("x", Stage::Preview));
    unknown(ledger.rollback("x", None));
    assert!(ledger.entries().unwrap().is_empty());
}

#[test]
fn preview_needs_a_built_release_and_no_approval() {
    let ledger = Ledger::memory(PROJECT);
    let m = base();
    propose(&ledger, "r1", &m);
    assert!(matches!(ledger.activate("r1", Stage::Preview), Err(LedgerError::NotBuilt { .. })));
    assert!(matches!(ledger.approve("r1", Stage::Preview, ana("r1", &m, Stage::Preview)), Err(LedgerError::ApprovalNotNeeded { .. })));
    ledger.image_built("r1", &digest()).unwrap();
    ledger.activate("r1", Stage::Preview).unwrap();
    assert!(matches!(ledger.activate("r1", Stage::Preview), Err(LedgerError::AlreadyActive { stage: Stage::Preview, .. })));
}

#[test]
fn stages_are_climbed_in_order() {
    let ledger = Ledger::memory(PROJECT);
    let m = base();
    propose(&ledger, "r1", &m);
    ledger.image_built("r1", &digest()).unwrap();
    for stage in [Stage::Staging, Stage::Live] {
        ledger.approve("r1", stage, ana("r1", &m, stage)).unwrap();
        ledger.approve("r1", stage, bo("r1", &m, stage)).unwrap();
    }
    // Approvals in hand do not skip a stage.
    assert!(matches!(ledger.activate("r1", Stage::Staging), Err(LedgerError::WrongStageOrder { required: Stage::Preview, .. })));
    ledger.activate("r1", Stage::Preview).unwrap();
    assert!(matches!(ledger.activate("r1", Stage::Live), Err(LedgerError::WrongStageOrder { required: Stage::Staging, .. })));
    ledger.activate("r1", Stage::Staging).unwrap();
    ledger.activate("r1", Stage::Live).unwrap();
}

#[test]
fn one_approver_counts_once_per_stage() {
    let ledger = Ledger::memory(PROJECT);
    let m = base();
    previewed(&ledger, "r1", &m);
    ledger.approve("r1", Stage::Staging, ana("r1", &m, Stage::Staging)).unwrap();
    let err = ledger.approve("r1", Stage::Staging, ana("r1", &m, Stage::Staging)).unwrap_err();
    assert!(matches!(&err, LedgerError::DuplicateApprover { user_id, stage: Stage::Staging, .. } if user_id == "ana"), "{err:?}");
    // The same person may approve a different stage.
    ledger.approve("r1", Stage::Live, ana("r1", &m, Stage::Live)).unwrap();
    let err = ledger.activate("r1", Stage::Staging).unwrap_err();
    assert!(
        matches!(
            err,
            LedgerError::QuorumNotMet {
                requirement: Requirement { approvals: 2, owner_required: true },
                approvals: 1,
                owner_present: true,
                ..
            }
        ),
        "{err:?}"
    );
}

#[test]
fn quorum_and_owner_are_separate_refusals() {
    let ledger = Ledger::memory(PROJECT);
    let m = base();
    previewed(&ledger, "r1", &m);
    ledger.approve("r1", Stage::Staging, bo("r1", &m, Stage::Staging)).unwrap();
    let err = ledger.activate("r1", Stage::Staging).unwrap_err();
    assert!(matches!(err, LedgerError::QuorumNotMet { approvals: 1, owner_present: false, .. }), "{err:?}");
    ledger.approve("r1", Stage::Staging, cy("r1", &m, Stage::Staging)).unwrap();
    let err = ledger.activate("r1", Stage::Staging).unwrap_err();
    assert!(matches!(err, LedgerError::OwnerRequired { stage: Stage::Staging, .. }), "{err:?}");
    ledger.approve("r1", Stage::Staging, ana("r1", &m, Stage::Staging)).unwrap();
    ledger.activate("r1", Stage::Staging).unwrap();
}

#[test]
fn a_rejected_release_is_done() {
    let ledger = Ledger::memory(PROJECT);
    let m = base();
    previewed(&ledger, "r1", &m);
    ledger.approve("r1", Stage::Staging, ana("r1", &m, Stage::Staging)).unwrap();
    ledger.approve("r1", Stage::Staging, bo("r1", &m, Stage::Staging)).unwrap();
    ledger.reject("r1", Stage::Staging, "cy", "leaks a header").unwrap();
    let st = ledger.state().unwrap();
    assert_eq!(st.releases["r1"].rejected, Some((Stage::Staging, "cy".into(), "leaks a header".into())));

    let rejected =
        |r: Result<(), LedgerError>| assert!(matches!(&r, Err(LedgerError::Rejected { user_id, .. }) if user_id == "cy"), "{r:?}");
    rejected(ledger.activate("r1", Stage::Staging));
    rejected(ledger.approve("r1", Stage::Live, ana("r1", &m, Stage::Live)).map(drop));
    rejected(ledger.reject("r1", Stage::Live, "ana", "again"));
}

#[test]
fn grants_and_policy_need_an_owner() {
    let ledger = Ledger::memory(PROJECT);
    let orders = vec![StandingOrder::Restart];
    let bo_grant = signed("bo", false, &standing_orders_message(PROJECT, &orders));
    assert!(matches!(ledger.grant_standing_orders(orders, 100, bo_grant), Err(LedgerError::NotOwner { .. })));
    let policy = ReleasePolicy { non_widening: 2, ..ReleasePolicy::default() };
    let bo_policy = signed("bo", false, &policy_message(PROJECT, &policy));
    assert!(matches!(ledger.set_policy(policy.clone(), bo_policy), Err(LedgerError::NotOwner { .. })));
    assert!(ledger.entries().unwrap().is_empty());

    let ana_policy = signed("ana", true, &policy_message(PROJECT, &policy));
    ledger.set_policy(policy.clone(), ana_policy).unwrap();
    assert_eq!(ledger.state().unwrap().policy, policy);
}

// ── quorum by diff category ──────────────────────────────────────────────────────────────────

#[test]
fn the_requirement_follows_the_diff_against_live() {
    let ledger = Ledger::memory(PROJECT);
    ship(&ledger, "r1", &base());
    for (id, m) in [("code", code_only()), ("egress", with_egress()), ("domain", with_domain())] {
        previewed(&ledger, id, &m);
    }
    let st = ledger.state().unwrap();
    assert!(st.diff_against_live("code").unwrap().widened().is_empty());
    assert_eq!(st.diff_against_live("egress").unwrap().widened(), [Category::Egress].into());
    assert_eq!(st.diff_against_live("domain").unwrap().widened(), [Category::Domain].into());
    assert_eq!(st.requirement("code", Stage::Live), Some(Requirement { approvals: 1, owner_required: false }));
    assert_eq!(st.requirement("egress", Stage::Live), Some(Requirement { approvals: 2, owner_required: false }));
    assert_eq!(st.requirement("domain", Stage::Live), Some(Requirement { approvals: 2, owner_required: true }));

    // Code-only: one non-owner approval per stage.
    let m = code_only();
    assert!(ledger.approve("code", Stage::Staging, bo("code", &m, Stage::Staging)).unwrap().met);
    ledger.activate("code", Stage::Staging).unwrap();

    // Egress: one is not enough, two non-owners are.
    let m = with_egress();
    assert!(!ledger.approve("egress", Stage::Staging, bo("egress", &m, Stage::Staging)).unwrap().met);
    assert!(matches!(ledger.activate("egress", Stage::Staging), Err(LedgerError::QuorumNotMet { approvals: 1, .. })));
    assert!(ledger.approve("egress", Stage::Staging, cy("egress", &m, Stage::Staging)).unwrap().met);
    ledger.activate("egress", Stage::Staging).unwrap();

    // Domain: two non-owners still need an owner.
    let m = with_domain();
    ledger.approve("domain", Stage::Staging, bo("domain", &m, Stage::Staging)).unwrap();
    ledger.approve("domain", Stage::Staging, cy("domain", &m, Stage::Staging)).unwrap();
    assert!(matches!(ledger.activate("domain", Stage::Staging), Err(LedgerError::OwnerRequired { .. })));
    assert!(ledger.approve("domain", Stage::Staging, ana("domain", &m, Stage::Staging)).unwrap().met);
    ledger.activate("domain", Stage::Staging).unwrap();
}

#[test]
fn a_policy_change_moves_the_requirement() {
    let ledger = Ledger::memory(PROJECT);
    ship(&ledger, "r1", &base());
    previewed(&ledger, "code", &code_only());
    let policy = ReleasePolicy { non_widening: 3, ..ReleasePolicy::default() };
    ledger.set_policy(policy.clone(), signed("ana", true, &policy_message(PROJECT, &policy))).unwrap();
    assert_eq!(ledger.state().unwrap().requirement("code", Stage::Staging), Some(Requirement { approvals: 3, owner_required: false }));
}

// ── rollback ─────────────────────────────────────────────────────────────────────────────────

#[test]
fn rolling_back_within_the_live_ceiling_needs_no_approver() {
    let ledger = Ledger::memory(PROJECT);
    ship(&ledger, "r1", &base());
    ship(&ledger, "r2", &with_egress());
    assert_eq!(live(&ledger.state().unwrap()), "r2");

    // r1 drops an egress host relative to r2: narrowing is free.
    ledger.rollback("r1", None).unwrap();
    let st = ledger.state().unwrap();
    assert_eq!(live(&st), "r1");
    assert_eq!(st.releases["r1"].activations.last(), Some(&(Stage::Live, ActivationReason::Rollback)));
    assert!(matches!(ledger.rollback("r1", None), Err(LedgerError::AlreadyActive { stage: Stage::Live, .. })));
}

#[test]
fn a_widening_rollback_needs_the_policy_requirement() {
    let ledger = Ledger::memory(PROJECT);
    let wide = with_egress();
    ship(&ledger, "r1", &base());
    ship(&ledger, "r2", &wide);
    ledger.rollback("r1", None).unwrap();

    // Back to r2 re-adds the egress host: 2 approvals under the default policy.
    let err = ledger.rollback("r2", None).unwrap_err();
    assert!(matches!(err, LedgerError::RollbackWidens { requirement: Requirement { approvals: 2, owner_required: false }, .. }), "{err:?}");
    let err = ledger.rollback("r2", Some(bo("r2", &wide, Stage::Live))).unwrap_err();
    assert!(matches!(err, LedgerError::QuorumNotMet { approvals: 1, .. }), "{err:?}");
    assert_eq!(live(&ledger.state().unwrap()), "r1", "a refused rollback changes nothing");

    // bo approved r2 for Live when it first shipped; that approval was spent, so bo may sign again.
    ledger.approve("r2", Stage::Live, bo("r2", &wide, Stage::Live)).unwrap();
    ledger.rollback("r2", Some(cy("r2", &wide, Stage::Live))).unwrap();
    assert_eq!(live(&ledger.state().unwrap()), "r2");
}

#[test]
fn a_rollback_widening_an_owner_category_needs_an_owner() {
    let ledger = Ledger::memory(PROJECT);
    let dom = with_domain();
    ship(&ledger, "r1", &dom);
    ship(&ledger, "r2", &base());
    ledger.approve("r1", Stage::Live, bo("r1", &dom, Stage::Live)).unwrap();
    let err = ledger.rollback("r1", Some(cy("r1", &dom, Stage::Live))).unwrap_err();
    assert!(matches!(err, LedgerError::OwnerRequired { .. }), "{err:?}");
    ledger.rollback("r1", Some(ana("r1", &dom, Stage::Live))).unwrap();
    assert_eq!(live(&ledger.state().unwrap()), "r1");
}

#[test]
fn only_a_release_that_was_live_can_be_rolled_back_to() {
    let ledger = Ledger::memory(PROJECT);
    let m = code_only();
    ship(&ledger, "r1", &base());
    previewed(&ledger, "r2", &m);
    let err = ledger.rollback("r2", Some(ana("r2", &m, Stage::Live))).unwrap_err();
    assert!(matches!(err, LedgerError::RollbackTargetNeverLive { .. }), "{err:?}");
}

// ── standing orders ──────────────────────────────────────────────────────────────────────────

#[test]
fn standing_orders_permit_only_what_was_granted_until_expiry() {
    let ledger = Ledger::memory(PROJECT);
    assert!(matches!(ledger.execute_standing_order(OnCallAction::Restart, None, "oncall", 10), Err(LedgerError::NoStandingOrders)));

    let orders = vec![StandingOrder::Restart, StandingOrder::Scale { min: 1, max: 4 }];
    ledger.grant_standing_orders(orders.clone(), 1_000, owner_grant(&orders)).unwrap();
    assert_eq!(ledger.state().unwrap().standing.unwrap().expires_at, 1_000);

    ledger.execute_standing_order(OnCallAction::Restart, None, "oncall", 999).unwrap();
    ledger.execute_standing_order(OnCallAction::Scale { replicas: 4 }, None, "oncall", 999).unwrap();
    let not = |r: Result<(), LedgerError>| assert!(matches!(r, Err(LedgerError::NotPermitted { .. })), "{r:?}");
    not(ledger.execute_standing_order(OnCallAction::Scale { replicas: 5 }, None, "oncall", 999));
    not(ledger.execute_standing_order(OnCallAction::MaintenancePage { on: true }, None, "oncall", 999));
    not(ledger.execute_standing_order(OnCallAction::Rollback { fits_within_ceiling: true }, Some("r1"), "oncall", 999));
    assert!(matches!(
        ledger.execute_standing_order(OnCallAction::Restart, Some("r1"), "oncall", 999),
        Err(LedgerError::UnexpectedRelease { .. })
    ));
    assert!(matches!(
        ledger.execute_standing_order(OnCallAction::Restart, None, "oncall", 1_000),
        Err(LedgerError::StandingOrdersExpired { expires_at: 1_000, now: 1_000 })
    ));

    // A new grant replaces the old one entirely.
    let orders = vec![StandingOrder::MaintenancePage];
    ledger.grant_standing_orders(orders.clone(), 5_000, owner_grant(&orders)).unwrap();
    not(ledger.execute_standing_order(OnCallAction::Restart, None, "oncall", 1_000));
    ledger.execute_standing_order(OnCallAction::MaintenancePage { on: true }, None, "oncall", 1_000).unwrap();
}

#[test]
fn a_standing_rollback_must_fit_within_live() {
    let ledger = Ledger::memory(PROJECT);
    ship(&ledger, "r1", &base());
    ship(&ledger, "r2", &with_egress());
    let orders = vec![StandingOrder::RollbackWithinCeiling];
    ledger.grant_standing_orders(orders.clone(), 1_000, owner_grant(&orders)).unwrap();

    let fits = OnCallAction::Rollback { fits_within_ceiling: true };
    assert!(matches!(ledger.execute_standing_order(fits.clone(), None, "oncall", 10), Err(LedgerError::RollbackTargetMissing)));
    assert!(matches!(
        ledger.execute_standing_order(OnCallAction::Rollback { fits_within_ceiling: false }, Some("r1"), "oncall", 10),
        Err(LedgerError::NotPermitted { .. })
    ));
    ledger.execute_standing_order(fits.clone(), Some("r1"), "oncall", 10).unwrap();
    let st = ledger.state().unwrap();
    assert_eq!(live(&st), "r1");
    assert_eq!(st.releases["r1"].activations.last(), Some(&(Stage::Live, ActivationReason::StandingOrder)));

    // r2 is wider than r1: claiming it fits does not make it so.
    let err = ledger.execute_standing_order(fits, Some("r2"), "oncall", 10).unwrap_err();
    assert!(matches!(err, LedgerError::StandingRollbackWidens { ref live, .. } if live == "r1"), "{err:?}");
    assert_eq!(live(&ledger.state().unwrap()), "r1");
}

// ── taint ────────────────────────────────────────────────────────────────────────────────────

#[test]
fn taint_blocks_every_stage_until_each_secret_is_rotated() {
    let ledger = Ledger::memory(PROJECT);
    ledger.secret_rotated("STRIPE_KEY", "ana").unwrap();
    propose(&ledger, "r1", &base());
    ledger.image_built("r1", &digest()).unwrap();
    let finding = |secret: &str| TaintFinding { secret: secret.into(), location: format!("aho-log:s1#{secret}") };
    ledger.taint_checked("r1", vec![finding("STRIPE_KEY"), finding("SENTRY_DSN")]).unwrap();

    // A rotation before the finding does not resolve it.
    let err = ledger.activate("r1", Stage::Preview).unwrap_err();
    let LedgerError::Tainted { secrets, .. } = err else { panic!("{err:?}") };
    assert_eq!(secrets, vec!["SENTRY_DSN".to_string(), "STRIPE_KEY".to_string()]);

    ledger.secret_rotated("STRIPE_KEY", "ana").unwrap();
    let err = ledger.activate("r1", Stage::Preview).unwrap_err();
    assert!(matches!(&err, LedgerError::Tainted { secrets, .. } if secrets == &["SENTRY_DSN".to_string()]), "{err:?}");
    ledger.secret_rotated("SENTRY_DSN", "ana").unwrap();
    ledger.activate("r1", Stage::Preview).unwrap();
    assert!(ledger.state().unwrap().releases["r1"].taint.is_empty());

    // A later finding taints it again, blocking the next promotion.
    ledger.taint_checked("r1", vec![finding("STRIPE_KEY")]).unwrap();
    let m = base();
    ledger.approve("r1", Stage::Staging, ana("r1", &m, Stage::Staging)).unwrap();
    ledger.approve("r1", Stage::Staging, bo("r1", &m, Stage::Staging)).unwrap();
    assert!(matches!(ledger.activate("r1", Stage::Staging), Err(LedgerError::Tainted { .. })));
}

// ── persistence and tampering ────────────────────────────────────────────────────────────────

#[test]
fn a_reopened_jsonl_ledger_has_the_same_state_and_keeps_going() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("p/ledger.jsonl");
    let before = {
        let ledger = Ledger::open(PROJECT, &path).unwrap();
        ship(&ledger, "r1", &base());
        previewed(&ledger, "r2", &code_only());
        ledger.state().unwrap()
    };
    let ledger = Ledger::open(PROJECT, &path).unwrap();
    assert_eq!(ledger.state().unwrap(), before);
    let m = code_only();
    ledger.approve("r2", Stage::Staging, bo("r2", &m, Stage::Staging)).unwrap();
    ledger.activate("r2", Stage::Staging).unwrap();
    drop(ledger);
    let st = Ledger::open(PROJECT, &path).unwrap().state().unwrap();
    assert_eq!(st.active[&Stage::Staging], "r2");
    assert_eq!(live(&st), "r1");
}

#[test]
fn editing_a_line_breaks_the_chain() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.jsonl");
    {
        let ledger = Ledger::open(PROJECT, &path).unwrap();
        ship(&ledger, "r1", &base());
    }
    let text = std::fs::read_to_string(&path).unwrap();
    let other = format!("sha256:{}", "b".repeat(64));
    std::fs::write(&path, text.replacen(&digest(), &other, 1)).unwrap();
    assert!(matches!(Ledger::open(PROJECT, &path), Err(LedgerError::ChainBroken { entry: 2 })));

    // Re-hashing only the edited entry still breaks the link from its successor.
    std::fs::write(&path, &text).unwrap();
    let mut entries: Vec<Entry> = text.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    let e = &mut entries[1];
    e.kind = custom(e, &LedgerBody::ImageBuilt { release: "r1".into(), image_digest: other }).kind;
    e.hash = Hash::chain(&e.prev_hash, &serde_json::to_vec(&(e.id, e.parent, e.at, &e.by, &e.kind)).unwrap());
    let lines: Vec<String> = entries.iter().map(|e| serde_json::to_string(e).unwrap()).collect();
    std::fs::write(&path, lines.join("\n") + "\n").unwrap();
    let ledger = Ledger::open(PROJECT, &path).unwrap();
    assert!(matches!(ledger.state(), Err(LedgerError::ChainBroken { entry: 3 })));
    assert!(matches!(ledger.entries(), Err(LedgerError::ChainBroken { .. })));
}

#[test]
fn a_rehashed_activation_without_quorum_fails_the_fold() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.jsonl");
    {
        let ledger = Ledger::open(PROJECT, &path).unwrap();
        previewed(&ledger, "r1", &base());
    }
    // Append a Staging activation with no approvals at all and re-chain the file.
    rechain(&path, |entries| {
        let last = entries.last().unwrap().clone();
        entries.push(custom(
            &last,
            &LedgerBody::Activated { release: "r1".into(), stage: Stage::Staging, reason: ActivationReason::Promotion },
        ));
    });
    let ledger = Ledger::open(PROJECT, &path).unwrap();
    let err = ledger.state().unwrap_err();
    let LedgerError::Invariant { entry, violation } = err else { panic!("{err:?}") };
    assert_eq!(entry, 4);
    assert!(matches!(*violation, LedgerError::QuorumNotMet { stage: Stage::Staging, approvals: 0, .. }), "{violation:?}");
    // History stays readable for forensics; the aho-log `deploy` fold refuses it too.
    assert_eq!(ledger.entries().unwrap().len(), 4);
    let log = JsonlLog::open(&path).unwrap();
    let branch = log.read_branch(log.head().unwrap()).unwrap();
    assert!(matches!(aho_log::fold(&branch, &custom_folds()), Err(LogError::Invariant { rule: "custom.fold", .. })));
}

#[test]
fn deleting_approvals_and_rehashing_fails_the_fold() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.jsonl");
    {
        let ledger = Ledger::open(PROJECT, &path).unwrap();
        ship(&ledger, "r1", &base());
    }
    // Drop bo's Live approval: the Live activation is left with one approval of the two needed.
    rechain(&path, |entries| {
        let bo_live = entries
            .iter()
            .position(|e| matches!(body(e), LedgerBody::Approved { stage: Stage::Live, ref approver, .. } if approver.user_id == "bo"))
            .unwrap();
        entries.remove(bo_live);
    });
    let err = Ledger::open(PROJECT, &path).unwrap().state().unwrap_err();
    let LedgerError::Invariant { violation, .. } = err else { panic!("{err:?}") };
    assert!(matches!(*violation, LedgerError::QuorumNotMet { stage: Stage::Live, approvals: 1, .. }), "{violation:?}");
}

#[test]
fn the_aho_log_fold_agrees_with_state() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.jsonl");
    let expected = {
        let ledger = Ledger::open(PROJECT, &path).unwrap();
        ship(&ledger, "r1", &base());
        ship(&ledger, "r2", &with_egress());
        ledger.rollback("r1", None).unwrap();
        ledger.taint_checked("r2", vec![TaintFinding { secret: "STRIPE_KEY".into(), location: "x".into() }]).unwrap();
        let orders = vec![StandingOrder::RollbackWithinCeiling];
        ledger.grant_standing_orders(orders.clone(), 99, owner_grant(&orders)).unwrap();
        ledger.state().unwrap()
    };
    let log = JsonlLog::open(&path).unwrap();
    let branch = log.read_branch(log.head().unwrap()).unwrap();
    let folded = aho_log::fold(&branch, &custom_folds()).unwrap();
    let st: LedgerState = serde_json::from_value(folded.custom["deploy"].clone()).unwrap();
    assert_eq!(st, expected);
}

// ── audit ────────────────────────────────────────────────────────────────────────────────────

#[test]
fn audit_reverifies_every_signature() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.jsonl");
    {
        let ledger = Ledger::open(PROJECT, &path).unwrap();
        ship(&ledger, "r1", &base());
        let orders = vec![StandingOrder::Restart];
        ledger.grant_standing_orders(orders.clone(), 99, owner_grant(&orders)).unwrap();
        let policy = ReleasePolicy::default();
        ledger.set_policy(policy.clone(), signed("ana", true, &policy_message(PROJECT, &policy))).unwrap();
        ledger.audit(&verify).unwrap();
    }
    // The same entries read as another project: every message binds the project, so the first
    // approval (entry 4) no longer verifies.
    let other = Ledger::open("acme-blog", &path).unwrap();
    other.state().unwrap();
    assert!(matches!(other.audit(&verify), Err(LedgerError::BadSignature { entry: 4, ref user_id }) if user_id == "ana"));
}

#[test]
fn audit_catches_a_signature_for_another_stage() {
    let ledger = Ledger::memory(PROJECT);
    let m = code_only();
    ship(&ledger, "r1", &base());
    previewed(&ledger, "r2", &m);
    // cy signed the Staging message but it was recorded as a Live approval.
    ledger.approve("r2", Stage::Live, cy("r2", &m, Stage::Staging)).unwrap();
    let err = ledger.audit(&verify).unwrap_err();
    let LedgerError::BadSignature { entry, user_id } = err else { panic!("{err:?}") };
    assert_eq!(user_id, "cy");
    assert_eq!(ledger.entries().unwrap().last().unwrap().id, entry);
}

#[test]
fn audit_catches_a_forged_grant() {
    let ledger = Ledger::memory(PROJECT);
    let granted = vec![StandingOrder::Restart];
    let signed_for = vec![StandingOrder::MaintenancePage];
    ledger.grant_standing_orders(granted, 99, owner_grant(&signed_for)).unwrap();
    assert!(matches!(ledger.audit(&verify), Err(LedgerError::BadSignature { entry: 1, .. })));
}
