//! The release rules. [`apply`] is the only place a ledger body changes state: mutating
//! [`crate::Ledger`] methods run it on the current state before appending, and every fold (the
//! typed replay behind [`crate::Ledger::state`] and the `deploy` [`aho_log::CustomFold`]) runs it
//! again on replay, so a log edited around the API still fails the same rules.

use std::collections::BTreeSet;

use aho_log::{CustomFold, CustomFolds, Entry, EntryKind};
use aho_types::{Principal, Value};
use cloud_contract::diff::fits_within;
use cloud_contract::ledger::{ActivationReason, LedgerBody, NAMESPACE, Stage};
use cloud_contract::policy::OnCallAction;
use serde::Deserialize;

use crate::LedgerError;
use crate::state::{ApprovalStatus, LedgerState, ReleaseState, StandingGrant};

/// Name of the system principal that appends every entry not signed by a human.
pub const SYSTEM: &str = "aho-cloud";

/// Who must have appended `body`: humans approve, sign grants and change policy; the cloud records
/// everything else.
pub(crate) fn principal_for(body: &LedgerBody) -> Principal {
    match body {
        LedgerBody::Approved { .. } | LedgerBody::StandingOrdersGranted { .. } | LedgerBody::PolicyChanged { .. } => Principal::Operator,
        _ => Principal::system(SYSTEM),
    }
}

/// Decodes one entry as a `deploy` body (namespace, kind name, shape); no rule is checked.
pub(crate) fn decode_body(e: &Entry) -> Result<LedgerBody, LedgerError> {
    let entry = e.id.0;
    let EntryKind::Custom { ns, name, body } = &e.kind else {
        return Err(LedgerError::Malformed { entry, detail: format!("`{}` entry in a release ledger", e.kind.tag()) });
    };
    if ns != NAMESPACE {
        return Err(LedgerError::Malformed { entry, detail: format!("namespace `{ns}` in a release ledger") });
    }
    let parsed = LedgerBody::deserialize(body).map_err(|err| LedgerError::Malformed { entry, detail: format!("{name}: {err}") })?;
    if parsed.name() != name {
        return Err(LedgerError::Malformed { entry, detail: format!("entry named `{name}` carries a `{}` body", parsed.name()) });
    }
    Ok(parsed)
}

/// Checks that `body` was appended by the principal allowed to append it.
fn check_principal(e: &Entry, body: &LedgerBody) -> Result<(), LedgerError> {
    let expected = principal_for(body);
    if e.by == expected {
        return Ok(());
    }
    Err(LedgerError::WrongPrincipal { name: body.name(), expected: Box::new(expected), found: Box::new(e.by.clone()) })
}

/// Replays `entries` from an empty state, calling `visit` after each applies.
pub(crate) fn replay(
    entries: &[Entry],
    mut visit: impl FnMut(&Entry, &LedgerBody, &LedgerState) -> Result<(), LedgerError>,
) -> Result<LedgerState, LedgerError> {
    let mut st = LedgerState::default();
    for e in entries {
        let body = decode_body(e)?;
        check_principal(e, &body)
            .and_then(|()| apply(&mut st, &body, None))
            .map_err(|v| LedgerError::Invariant { entry: e.id.0, violation: Box::new(v) })?;
        visit(e, &body, &st)?;
    }
    Ok(st)
}

/// The `deploy` fold for [`aho_log::fold`]: the same rules over a JSON-encoded [`LedgerState`].
/// It sees only bodies, so the principal check of [`crate::Ledger::state`] is not repeated here.
pub fn fold_deploy(prev: Option<&Value>, name: &str, body: &Value) -> Result<Value, String> {
    let mut st = match prev {
        Some(v) => LedgerState::deserialize(v).map_err(|e| format!("fold state: {e}"))?,
        None => LedgerState::default(),
    };
    let body = LedgerBody::deserialize(body).map_err(|e| format!("{name}: {e}"))?;
    if body.name() != name {
        return Err(format!("entry named `{name}` carries a `{}` body", body.name()));
    }
    apply(&mut st, &body, None).map_err(|e| e.to_string())?;
    serde_json::to_value(&st).map_err(|e| format!("fold state: {e}"))
}

/// `{"deploy": fold_deploy}`, for reading a ledger with [`aho_log::fold`].
pub fn custom_folds() -> CustomFolds {
    CustomFolds::from([(NAMESPACE.to_owned(), fold_deploy as CustomFold)])
}

/// Applies one body, refusing it if it breaks a rule. `now` (Unix seconds) is known only when the
/// action happens; replays pass `None` and skip the one time-dependent rule (standing-order expiry).
pub(crate) fn apply(st: &mut LedgerState, body: &LedgerBody, now: Option<u64>) -> Result<(), LedgerError> {
    match body {
        LedgerBody::ReleaseProposed { release, manifest, manifest_hash, image_digest, provenance, proposer } => {
            check_release_id(release)?;
            if st.releases.contains_key(release) {
                return Err(LedgerError::DuplicateRelease(release.clone()));
            }
            cloud_contract::validate(manifest).map_err(LedgerError::InvalidManifest)?;
            let actual = manifest.hash();
            if *manifest_hash != actual {
                return Err(LedgerError::ManifestHashMismatch { release: release.clone(), recorded: manifest_hash.clone(), actual });
            }
            if let Some(d) = image_digest {
                check_digest(d)?;
            }
            let mut r = ReleaseState::new((**manifest).clone(), actual, provenance.clone(), proposer.clone());
            r.image_digest = image_digest.clone();
            st.releases.insert(release.clone(), r);
        }
        LedgerBody::ImageBuilt { release, image_digest } => {
            let r = release_mut(st, release)?;
            if let Some(digest) = &r.image_digest {
                return Err(LedgerError::AlreadyBuilt { release: release.clone(), digest: digest.clone() });
            }
            check_digest(image_digest)?;
            r.image_digest = Some(image_digest.clone());
        }
        LedgerBody::SurfaceObserved { release, listeners, egress } => {
            release_mut(st, release)?.observed = Some((listeners.clone(), egress.clone()));
        }
        LedgerBody::TaintChecked { release, findings } => {
            let r = release_mut(st, release)?;
            for f in findings {
                if !r.taint.contains(f) {
                    r.taint.push(f.clone());
                }
            }
        }
        LedgerBody::SecretRotated { secret, .. } => {
            for r in st.releases.values_mut() {
                r.taint.retain(|f| f.secret != *secret);
            }
        }
        LedgerBody::Approved { release, stage, approver } => {
            let r = release_ref(st, release)?;
            if *stage == Stage::Preview {
                return Err(LedgerError::ApprovalNotNeeded { stage: *stage });
            }
            not_rejected(release, r)?;
            if st.active.get(stage) == Some(release) {
                return Err(LedgerError::AlreadyActive { release: release.clone(), stage: *stage });
            }
            if r.unspent(*stage).any(|a| a.user_id == approver.user_id) {
                return Err(LedgerError::DuplicateApprover { release: release.clone(), stage: *stage, user_id: approver.user_id.clone() });
            }
            release_mut(st, release)?.approvals.push((*stage, approver.clone()));
        }
        LedgerBody::Rejected { release, stage, user_id, reason } => {
            not_rejected(release, release_ref(st, release)?)?;
            release_mut(st, release)?.rejected = Some((*stage, user_id.clone(), reason.clone()));
        }
        LedgerBody::Activated { release, stage, reason } => activate(st, release, *stage, *reason)?,
        LedgerBody::StandingOrdersGranted { orders, expires_at, approver } => {
            if !approver.owner {
                return Err(LedgerError::NotOwner { user_id: approver.user_id.clone(), action: "granting standing orders" });
            }
            st.standing = Some(StandingGrant { orders: orders.clone(), expires_at: *expires_at });
        }
        LedgerBody::StandingOrderExecuted { action, release, .. } => {
            let grant = st.standing.as_ref().ok_or(LedgerError::NoStandingOrders)?;
            if let Some(now) = now
                && now >= grant.expires_at
            {
                return Err(LedgerError::StandingOrdersExpired { expires_at: grant.expires_at, now });
            }
            if !grant.orders.iter().any(|o| o.permits(action)) {
                return Err(LedgerError::NotPermitted { action: action.clone() });
            }
            match (action, release) {
                (OnCallAction::Rollback { .. }, Some(target)) => {
                    standing_rollback_fits(st, target)?;
                    st.standing_rollback = Some(target.clone());
                }
                (OnCallAction::Rollback { .. }, None) => return Err(LedgerError::RollbackTargetMissing),
                (_, Some(target)) => return Err(LedgerError::UnexpectedRelease { release: target.clone() }),
                (_, None) => {}
            }
        }
        LedgerBody::PolicyChanged { policy, approver } => {
            if !approver.owner {
                return Err(LedgerError::NotOwner { user_id: approver.user_id.clone(), action: "changing the release policy" });
            }
            st.policy = policy.clone();
        }
    }
    Ok(())
}

fn activate(st: &mut LedgerState, release: &str, stage: Stage, reason: ActivationReason) -> Result<(), LedgerError> {
    let r = release_ref(st, release)?;
    if reason != ActivationReason::Promotion && stage != Stage::Live {
        return Err(LedgerError::InvalidActivation { release: release.to_owned(), stage, reason });
    }
    if reason != ActivationReason::Promotion && !r.was_active(Stage::Live) {
        return Err(LedgerError::RollbackTargetNeverLive { release: release.to_owned() });
    }
    not_rejected(release, r)?;
    if r.image_digest.is_none() {
        return Err(LedgerError::NotBuilt { release: release.to_owned() });
    }
    if !r.taint.is_empty() {
        let secrets: BTreeSet<&str> = r.taint.iter().map(|f| f.secret.as_str()).collect();
        return Err(LedgerError::Tainted { release: release.to_owned(), secrets: secrets.into_iter().map(str::to_owned).collect() });
    }
    if st.active.get(&stage).map(String::as_str) == Some(release) {
        return Err(LedgerError::AlreadyActive { release: release.to_owned(), stage });
    }
    match reason {
        ActivationReason::Promotion => {
            let previous = match stage {
                Stage::Preview => None,
                Stage::Staging => Some(Stage::Preview),
                Stage::Live => Some(Stage::Staging),
            };
            if let Some(required) = previous {
                if !r.was_active(required) {
                    return Err(LedgerError::WrongStageOrder { release: release.to_owned(), stage, required });
                }
                ensure_met(release, stage, status(st, release, stage)?)?;
            }
        }
        ActivationReason::Rollback => {
            let fits = st.live().is_some_and(|(_, live)| fits_within(&r.manifest, &live.manifest));
            if !fits {
                let s = status(st, release, Stage::Live)?;
                if s.approvals == 0 {
                    return Err(LedgerError::RollbackWidens { release: release.to_owned(), requirement: s.requirement });
                }
                ensure_met(release, Stage::Live, s)?;
            }
        }
        ActivationReason::StandingOrder => {
            if st.standing_rollback.as_deref() != Some(release) {
                return Err(LedgerError::StandingActivationUnrecorded { release: release.to_owned() });
            }
            let grant = st.standing.as_ref().ok_or(LedgerError::NoStandingOrders)?;
            let action = OnCallAction::Rollback { fits_within_ceiling: true };
            if !grant.orders.iter().any(|o| o.permits(&action)) {
                return Err(LedgerError::NotPermitted { action });
            }
            standing_rollback_fits(st, release)?;
            st.standing_rollback = None;
        }
    }
    st.active.insert(stage, release.to_owned());
    release_mut(st, release)?.activated(stage, reason);
    Ok(())
}

/// A standing-order rollback target: once live, not live now, and no wider than live.
fn standing_rollback_fits(st: &LedgerState, target: &str) -> Result<(), LedgerError> {
    let r = release_ref(st, target)?;
    if !r.was_active(Stage::Live) {
        return Err(LedgerError::RollbackTargetNeverLive { release: target.to_owned() });
    }
    let Some((live_id, live)) = st.live() else {
        return Err(LedgerError::RollbackTargetNeverLive { release: target.to_owned() });
    };
    if live_id == target {
        return Err(LedgerError::AlreadyActive { release: target.to_owned(), stage: Stage::Live });
    }
    if !fits_within(&r.manifest, &live.manifest) {
        return Err(LedgerError::StandingRollbackWidens { release: target.to_owned(), live: live_id.to_owned() });
    }
    Ok(())
}

fn status(st: &LedgerState, release: &str, stage: Stage) -> Result<ApprovalStatus, LedgerError> {
    st.approval_status(release, stage).ok_or_else(|| LedgerError::UnknownRelease(release.to_owned()))
}

fn ensure_met(release: &str, stage: Stage, s: ApprovalStatus) -> Result<(), LedgerError> {
    if s.approvals < s.requirement.approvals {
        return Err(LedgerError::QuorumNotMet {
            release: release.to_owned(),
            stage,
            requirement: s.requirement,
            approvals: s.approvals,
            owner_present: s.owner_present,
        });
    }
    if s.requirement.owner_required && !s.owner_present {
        return Err(LedgerError::OwnerRequired { release: release.to_owned(), stage, requirement: s.requirement });
    }
    Ok(())
}

fn not_rejected(release: &str, r: &ReleaseState) -> Result<(), LedgerError> {
    match &r.rejected {
        Some((stage, user_id, reason)) => {
            Err(LedgerError::Rejected { release: release.to_owned(), stage: *stage, user_id: user_id.clone(), reason: reason.clone() })
        }
        None => Ok(()),
    }
}

fn release_ref<'a>(st: &'a LedgerState, release: &str) -> Result<&'a ReleaseState, LedgerError> {
    st.releases.get(release).ok_or_else(|| LedgerError::UnknownRelease(release.to_owned()))
}

fn release_mut<'a>(st: &'a mut LedgerState, release: &str) -> Result<&'a mut ReleaseState, LedgerError> {
    st.releases.get_mut(release).ok_or_else(|| LedgerError::UnknownRelease(release.to_owned()))
}

/// Release ids appear in URLs and signed messages: 1–128 visible ASCII characters, no `/`.
fn check_release_id(id: &str) -> Result<(), LedgerError> {
    let ok = !id.is_empty() && id.len() <= 128 && id.bytes().all(|b| b.is_ascii_graphic() && b != b'/');
    if ok { Ok(()) } else { Err(LedgerError::InvalidReleaseId(id.to_owned())) }
}

/// An OCI content digest: `sha256:` plus 64 lowercase hex.
fn check_digest(digest: &str) -> Result<(), LedgerError> {
    let ok = digest
        .strip_prefix("sha256:")
        .is_some_and(|hex| hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
    if ok { Ok(()) } else { Err(LedgerError::InvalidDigest(digest.to_owned())) }
}
