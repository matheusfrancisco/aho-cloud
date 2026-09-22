//! Manifests, releases, approvals, promotion, rollback, standing orders, on-call.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use cloud_api::internal::{Maintenance, Scale};
use cloud_api::{Approve, CheckManifest, CheckResult, GrantStandingOrders, OnCall, Promote, ProposeRelease, Reject, Release, Rollback};
use cloud_contract::diff::fits_within;
use cloud_contract::ledger::{Proposer, Provenance, Stage, approval_message};
use cloud_contract::manifest::Source;
use cloud_contract::policy::OnCallAction;

use super::ApiJson;
use crate::approvals::{approver, standing_orders_message};
use crate::auth::{Caller, OnCallCaller, live_project, project_access, require_owner};
use crate::crypto::random_bytes;
use crate::error::AppError;
use crate::ops::{self, ledger_state, users_of};
use crate::state::AppState;
use crate::view;

async fn release_view(st: &AppState, project: &str, release: &str) -> Result<Release, AppError> {
    let state = ledger_state(st, project)?;
    let meta = st.store.release_meta(project, release).await?;
    let users = users_of(st, &state).await?;
    view::release(project, release, &state, meta.as_ref(), &users).ok_or_else(|| AppError::not_found(format!("release {release}")))
}

pub async fn check(Caller(_): Caller, ApiJson(body): ApiJson<CheckManifest>) -> Json<CheckResult> {
    Json(CheckResult::of(&body.manifest, body.edge_toml.as_deref()))
}

pub async fn list(State(st): State<AppState>, Caller(user): Caller, Path(p): Path<String>) -> Result<Json<Vec<Release>>, AppError> {
    let (project, _) = project_access(&st, &user, &p).await?;
    let ledger = st.ledgers.get(&project.slug)?;
    let state = ledger.state()?;
    let users = users_of(&st, &state).await?;
    let mut order: Vec<String> = ledger
        .entries()?
        .into_iter()
        .filter_map(|e| match e.body {
            cloud_contract::ledger::LedgerBody::ReleaseProposed { release, .. } => Some(release),
            _ => None,
        })
        .collect();
    order.reverse();
    let mut out = Vec::with_capacity(order.len());
    for id in order {
        let meta = st.store.release_meta(&project.slug, &id).await?;
        if let Some(r) = view::release(&project.slug, &id, &state, meta.as_ref(), &users) {
            out.push(r);
        }
    }
    Ok(Json(out))
}

pub async fn get(
    State(st): State<AppState>,
    Caller(user): Caller,
    Path((p, r)): Path<(String, String)>,
) -> Result<Json<Release>, AppError> {
    let (project, _) = project_access(&st, &user, &p).await?;
    Ok(Json(release_view(&st, &project.slug, &r).await?))
}

/// Validates, checks provenance and uploads, records the proposal and starts the build.
pub async fn propose(
    State(st): State<AppState>,
    Caller(user): Caller,
    Path(p): Path<String>,
    ApiJson(body): ApiJson<ProposeRelease>,
) -> Result<(StatusCode, Json<Release>), AppError> {
    let (project, _) = project_access(&st, &user, &p).await?;
    let check = CheckResult::of(&body.manifest, body.edge_toml.as_deref());
    if !check.accepted() {
        let mut message = format!("manifest refused: {} refusal(s)", check.refusals.len());
        if !check.edge_errors.is_empty() {
            message.push_str(&format!("; edge.toml: {}", check.edge_errors.join("; ")));
        }
        return Err(AppError::Refused { message, refusals: check.refusals });
    }
    if body.manifest.project != project.slug {
        return Err(AppError::invalid(format!("manifest is for project `{}`, not `{}`", body.manifest.project, project.slug)));
    }
    if let Provenance::Attested { session_id, head_hash } = &body.provenance
        && st.store.session(&project.slug, session_id, head_hash).await?.is_none()
    {
        return Err(AppError::invalid(format!(
            "attested provenance names session {session_id} at {head_hash}, which was not uploaded to this project with a verified chain"
        )));
    }
    if let Source::Archive { hash } = &body.manifest.source
        && st.store.blob(&project.slug, hash).await?.is_none_or(|b| b.kind != crate::store::BlobKind::Archive)
    {
        return Err(AppError::invalid(format!("archive {hash} was not uploaded to this project")));
    }
    let release = format!("r-{}", crate::crypto::hex(&random_bytes(6)));
    let ledger = st.ledgers.get(&project.slug)?;
    ledger.propose(&release, body.manifest.clone(), body.provenance, Proposer::User { user_id: user.id.clone() })?;
    ops::enqueue_build(&st, &project.slug, &release, body.manifest, body.edge_toml).await?;
    Ok((StatusCode::CREATED, Json(release_view(&st, &project.slug, &release).await?)))
}

pub async fn approve(
    State(st): State<AppState>,
    Caller(user): Caller,
    Path((p, r)): Path<(String, String)>,
    ApiJson(body): ApiJson<Approve>,
) -> Result<Json<Release>, AppError> {
    let (project, member) = project_access(&st, &user, &p).await?;
    let ledger = st.ledgers.get(&project.slug)?;
    let state = ledger.state()?;
    let rs = state.releases.get(&r).ok_or_else(|| AppError::not_found(format!("release {r}")))?;
    let message = approval_message(&project.slug, &r, &rs.manifest_hash, body.stage);
    let a = approver(&st, &user, member.owner, &message, &body.proof).await?;
    ledger.approve(&r, body.stage, a)?;
    Ok(Json(release_view(&st, &project.slug, &r).await?))
}

pub async fn reject(
    State(st): State<AppState>,
    Caller(user): Caller,
    Path((p, r)): Path<(String, String)>,
    ApiJson(body): ApiJson<Reject>,
) -> Result<Json<Release>, AppError> {
    let (project, _) = project_access(&st, &user, &p).await?;
    let reason = body.reason.trim();
    if reason.is_empty() {
        return Err(AppError::invalid("a rejection needs a reason"));
    }
    st.ledgers.get(&project.slug)?.reject(&r, body.stage, &user.id, reason)?;
    Ok(Json(release_view(&st, &project.slug, &r).await?))
}

/// Activates at `stage` when the ledger's rules hold (quorum for staging and live), then deploys.
pub async fn promote(
    State(st): State<AppState>,
    Caller(user): Caller,
    Path((p, r)): Path<(String, String)>,
    ApiJson(body): ApiJson<Promote>,
) -> Result<Json<Release>, AppError> {
    let (project, _) = project_access(&st, &user, &p).await?;
    ops::activate_and_deploy(&st, &project.slug, &r, body.stage).await?;
    Ok(Json(release_view(&st, &project.slug, &r).await?))
}

/// Rolls live back to an earlier live release. Narrowing (fits within live) needs no proof;
/// widening needs signed approvals meeting the policy.
pub async fn rollback(
    State(st): State<AppState>,
    Caller(user): Caller,
    Path(p): Path<String>,
    ApiJson(body): ApiJson<Rollback>,
) -> Result<Json<Release>, AppError> {
    let (project, member) = project_access(&st, &user, &p).await?;
    let ledger = st.ledgers.get(&project.slug)?;
    let before = ledger.state()?;
    let target = before.releases.get(&body.release).ok_or_else(|| AppError::not_found(format!("release {}", body.release)))?;
    let secrets = ops::deployment_secrets(&st, &project.slug, &target.manifest, Stage::Live).await?;
    let a = match &body.proof {
        Some(proof) => {
            let message = approval_message(&project.slug, &body.release, &target.manifest_hash, Stage::Live);
            Some(approver(&st, &user, member.owner, &message, proof).await?)
        }
        None => None,
    };
    ledger.rollback(&body.release, a)?;
    ops::deploy(&st, &project.slug, &body.release, Stage::Live, secrets, &before).await?;
    Ok(Json(release_view(&st, &project.slug, &body.release).await?))
}

pub async fn grant_standing_orders(
    State(st): State<AppState>,
    Caller(user): Caller,
    Path(p): Path<String>,
    ApiJson(body): ApiJson<GrantStandingOrders>,
) -> Result<StatusCode, AppError> {
    let (project, member) = project_access(&st, &user, &p).await?;
    require_owner(&member, "granting standing orders")?;
    if body.orders.is_empty() {
        return Err(AppError::invalid("grant at least one standing order"));
    }
    if body.ttl_secs == 0 || body.ttl_secs > st.cfg.max_standing_ttl_secs {
        return Err(AppError::invalid(format!("ttl_secs must be 1–{}", st.cfg.max_standing_ttl_secs)));
    }
    let a = approver(&st, &user, true, &standing_orders_message(&project.slug, &body.orders), &body.proof).await?;
    st.ledgers.get(&project.slug)?.grant_standing_orders(body.orders, st.now() + body.ttl_secs, a)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Executes one on-call action under the standing grant. The caller is the service token or an
/// org owner; whether a rollback fits within the live ceiling is computed here, never taken
/// from the request.
pub async fn oncall(
    State(st): State<AppState>,
    caller: OnCallCaller,
    Path(p): Path<String>,
    ApiJson(body): ApiJson<OnCall>,
) -> Result<StatusCode, AppError> {
    let project = live_project(&st, &p).await?;
    let actor = match &caller {
        OnCallCaller::Service => "service".to_owned(),
        OnCallCaller::User(user) => {
            let m = st
                .store
                .member(&project.org, &user.id)
                .await?
                .ok_or_else(|| AppError::forbidden(format!("not a member of {}", project.org)))?;
            require_owner(&m, "on-call actions")?;
            format!("user:{}", user.id)
        }
    };
    let ledger = st.ledgers.get(&project.slug)?;
    let before = ledger.state()?;
    let live = before.active.get(&Stage::Live).and_then(|r| before.releases.get(r));
    match body.action {
        OnCallAction::Rollback { .. } => {
            let target_id = body.release.as_deref().ok_or_else(|| AppError::invalid("a rollback names its target release"))?;
            let target = before.releases.get(target_id).ok_or_else(|| AppError::not_found(format!("release {target_id}")))?;
            let live_rs = live.ok_or_else(|| AppError::conflict("nothing is live"))?;
            let action = OnCallAction::Rollback { fits_within_ceiling: fits_within(&target.manifest, &live_rs.manifest) };
            let secrets = ops::deployment_secrets(&st, &project.slug, &target.manifest, Stage::Live).await?;
            ledger.execute_standing_order(action, Some(target_id), &actor, st.now())?;
            ops::deploy(&st, &project.slug, target_id, Stage::Live, secrets, &before).await?;
        }
        action => {
            if body.release.is_some() {
                return Err(AppError::invalid("only a rollback names a release"));
            }
            let live_rs = live.ok_or_else(|| AppError::conflict("nothing is live"))?;
            let processes: Vec<String> = live_rs.manifest.processes.iter().map(|p| p.name.clone()).collect();
            ledger.execute_standing_order(action.clone(), None, &actor, st.now())?;
            let rt = &st.services.runtime;
            match action {
                OnCallAction::Restart => rt.restart(&project.slug, Stage::Live).await?,
                OnCallAction::Scale { replicas } => {
                    for process in processes {
                        rt.scale(&project.slug, Stage::Live, Scale { process, replicas }).await?;
                    }
                }
                OnCallAction::MaintenancePage { on } => rt.maintenance(&project.slug, Stage::Live, Maintenance { on }).await?,
                OnCallAction::Rollback { .. } => {}
            }
        }
    }
    Ok(StatusCode::NO_CONTENT)
}
