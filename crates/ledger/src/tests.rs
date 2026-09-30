//! Rules only reachable by appending around the API: envelope checks and fold-only invariants.

use aho_log::{EntryKind, LogStore};
use aho_types::Principal;
use cloud_contract::Manifest;
use cloud_contract::ledger::{ActivationReason, Approver, LedgerBody, NAMESPACE, ProofMethod, Proposer, Provenance, Stage};
use serde_json::json;

use crate::{Ledger, LedgerError};

fn manifest() -> Manifest {
    Manifest::from_json(include_str!("../tests/fixtures/app.json")).unwrap()
}

fn approver(user: &str, owner: bool) -> Approver {
    Approver { user_id: user.into(), email: format!("{user}@acme.com"), owner, method: ProofMethod::DeviceKey, signature: "sig".into() }
}

fn digest() -> String {
    format!("sha256:{}", "a".repeat(64))
}

fn raw(ledger: &Ledger<aho_log::MemoryLog>, by: Principal, body: &LedgerBody) {
    let kind = EntryKind::Custom { ns: NAMESPACE.into(), name: body.name().into(), body: serde_json::to_value(body).unwrap() };
    ledger.store.append(None, by, kind).unwrap();
}

fn system() -> Principal {
    Principal::system(crate::SYSTEM)
}

fn proposed(ledger: &Ledger<aho_log::MemoryLog>, release: &str) {
    ledger.propose(release, manifest(), Provenance::Unattested { commit: None, ci_run: None }, Proposer::Ci { name: "ci".into() }).unwrap();
}

fn violation(err: LedgerError) -> (u64, LedgerError) {
    match err {
        LedgerError::Invariant { entry, violation } => (entry, *violation),
        other => panic!("expected an invariant violation, got {other:?}"),
    }
}

#[test]
fn an_approval_appended_by_the_system_is_not_a_human_approval() {
    let ledger = Ledger::memory("acme-shop");
    proposed(&ledger, "r1");
    raw(&ledger, system(), &LedgerBody::Approved { release: "r1".into(), stage: Stage::Staging, approver: approver("ana", true) });
    let (entry, v) = violation(ledger.state().unwrap_err());
    assert_eq!(entry, 2);
    assert!(matches!(v, LedgerError::WrongPrincipal { name: "approved", .. }), "{v:?}");
}

#[test]
fn a_build_record_appended_by_an_operator_is_refused() {
    let ledger = Ledger::memory("acme-shop");
    proposed(&ledger, "r1");
    raw(&ledger, Principal::Operator, &LedgerBody::ImageBuilt { release: "r1".into(), image_digest: digest() });
    assert!(matches!(violation(ledger.state().unwrap_err()).1, LedgerError::WrongPrincipal { name: "image_built", .. }));
}

#[test]
fn entries_from_other_namespaces_or_kinds_are_malformed() {
    let ledger = Ledger::memory("acme-shop");
    ledger.store.append(None, system(), EntryKind::Custom { ns: "Todo".into(), name: "added".into(), body: json!({}) }).unwrap();
    assert!(matches!(ledger.state().unwrap_err(), LedgerError::Malformed { entry: 1, .. }));

    let ledger = Ledger::memory("acme-shop");
    ledger.store.append(None, system(), EntryKind::SessionSeen).unwrap();
    assert!(matches!(ledger.state().unwrap_err(), LedgerError::Malformed { entry: 1, .. }));
}

#[test]
fn an_entry_name_must_match_its_body() {
    let ledger = Ledger::memory("acme-shop");
    let body = LedgerBody::SecretRotated { secret: "STRIPE_KEY".into(), by: "ana".into() };
    let kind = EntryKind::Custom { ns: NAMESPACE.into(), name: "activated".into(), body: serde_json::to_value(&body).unwrap() };
    ledger.store.append(None, system(), kind).unwrap();
    assert!(matches!(ledger.state().unwrap_err(), LedgerError::Malformed { entry: 1, .. }));
    assert!(ledger.entries().is_err());
}

#[test]
fn a_manifest_swapped_under_its_hash_fails_replay() {
    let ledger = Ledger::memory("acme-shop");
    let signed = manifest();
    let mut swapped = manifest();
    swapped.egress.push("evil.example".into());
    raw(
        &ledger,
        system(),
        &LedgerBody::ReleaseProposed {
            release: "r1".into(),
            manifest: Box::new(swapped),
            manifest_hash: signed.hash(),
            image_digest: None,
            provenance: Provenance::Unattested { commit: None, ci_run: None },
            proposer: Proposer::Ci { name: "ci".into() },
        },
    );
    assert!(matches!(violation(ledger.state().unwrap_err()).1, LedgerError::ManifestHashMismatch { .. }));
}

/// Proposes, builds and promotes `release` to Live with approvals from ana (owner) and bo.
fn ship(ledger: &Ledger<aho_log::MemoryLog>, release: &str, m: Manifest) {
    ledger.propose(release, m, Provenance::Unattested { commit: None, ci_run: None }, Proposer::Ci { name: "ci".into() }).unwrap();
    ledger.image_built(release, &digest()).unwrap();
    ledger.activate(release, Stage::Preview).unwrap();
    for stage in [Stage::Staging, Stage::Live] {
        ledger.approve(release, stage, approver("ana", true)).unwrap();
        ledger.approve(release, stage, approver("bo", false)).unwrap();
        ledger.activate(release, stage).unwrap();
    }
}

#[test]
fn a_standing_order_activation_needs_a_recorded_execution() {
    let ledger = Ledger::memory("acme-shop");
    ship(&ledger, "r1", manifest());
    let mut next = manifest();
    next.egress.push("*.sentry.io".into());
    ship(&ledger, "r2", next);
    ledger
        .grant_standing_orders(vec![cloud_contract::policy::StandingOrder::RollbackWithinCeiling], u64::MAX, approver("ana", true))
        .unwrap();
    // r1 fits within r2 and the grant permits it, but no execution was recorded.
    raw(&ledger, system(), &LedgerBody::Activated { release: "r1".into(), stage: Stage::Live, reason: ActivationReason::StandingOrder });
    assert!(matches!(violation(ledger.state().unwrap_err()).1, LedgerError::StandingActivationUnrecorded { .. }));
}

#[test]
fn rollbacks_only_happen_at_live() {
    let ledger = Ledger::memory("acme-shop");
    proposed(&ledger, "r1");
    ledger.image_built("r1", &digest()).unwrap();
    raw(&ledger, system(), &LedgerBody::Activated { release: "r1".into(), stage: Stage::Staging, reason: ActivationReason::Rollback });
    assert!(matches!(
        violation(ledger.state().unwrap_err()).1,
        LedgerError::InvalidActivation { stage: Stage::Staging, reason: ActivationReason::Rollback, .. }
    ));
}

#[test]
fn a_refused_multi_entry_action_appends_nothing() {
    let ledger = Ledger::memory("acme-shop");
    proposed(&ledger, "r1");
    let before = ledger.entries().unwrap().len();
    // The approval alone would be legal; the activation is not (never live), so neither lands.
    let err = ledger.rollback("r1", Some(approver("ana", true))).unwrap_err();
    assert!(matches!(err, LedgerError::RollbackTargetNeverLive { .. }), "{err:?}");
    assert_eq!(ledger.entries().unwrap().len(), before);
    assert!(ledger.state().unwrap().releases["r1"].approvals.is_empty());
}

#[test]
fn a_member_count_is_the_clouds_record_never_an_operators() {
    // A human-path entry lowering the quorum to one would let a single approver ship alone.
    let ledger = Ledger::memory("acme-shop");
    raw(&ledger, Principal::Operator, &LedgerBody::MembersChanged { count: 1 });
    let (entry, v) = violation(ledger.state().unwrap_err());
    assert_eq!(entry, 1);
    assert!(matches!(v, LedgerError::WrongPrincipal { name: "members_changed", .. }), "{v:?}");
}

#[test]
fn a_zero_member_count_appended_around_the_api_fails_replay() {
    let ledger = Ledger::memory("acme-shop");
    ledger.members_changed(3).unwrap();
    raw(&ledger, system(), &LedgerBody::MembersChanged { count: 0 });
    let (entry, v) = violation(ledger.state().unwrap_err());
    assert_eq!(entry, 2);
    assert!(matches!(v, LedgerError::NoMembers), "{v:?}");
}
