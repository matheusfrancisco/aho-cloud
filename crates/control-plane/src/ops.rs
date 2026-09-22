//! Release operations that span the ledger, the store and the ports: the build pipeline, taint
//! scan, activation + deployment, and the secrets a deployment carries.

use std::collections::{BTreeMap, BTreeSet};

use cloud_api::internal::{BuildJob, BuildResult, Deployment};
use cloud_contract::ledger::{Stage, TaintFinding};
use cloud_contract::manifest::{Manifest, Source};
use cloud_ledger::LedgerState;

use crate::error::AppError;
use crate::state::AppState;
use crate::store::{BuildState, ReleaseMeta, UserRec};
use crate::taint::{self, SessionLog};

/// Placeholder value a preview or staging deployment gets for a secret.
pub fn placeholder(name: &str) -> String {
    format!("aho-cloud-placeholder:{name}")
}

/// Internal route the builder fetches archives from (service-token auth).
pub const ARCHIVE_BLOB: &str = "/internal/v1/archives/{hash}";

/// Ledger state of `project`.
pub fn ledger_state(state: &AppState, project: &str) -> Result<LedgerState, AppError> {
    Ok(state.ledgers.get(project)?.state()?)
}

/// The decrypted Live value of every secret in `names` that has one.
pub async fn live_values(state: &AppState, project: &str, names: &BTreeSet<&str>) -> Result<BTreeMap<String, String>, AppError> {
    let mut out = BTreeMap::new();
    for s in state.store.secrets(project).await? {
        if s.stage == Stage::Live && names.contains(s.name.as_str()) {
            out.insert(s.name.clone(), state.secrets.open(project, &s.name, Stage::Live, &s.ciphertext)?);
        }
    }
    Ok(out)
}

/// The secrets a deployment of `manifest` at `stage` carries: real Live values at Live (every
/// declared secret must have one), placeholders elsewhere.
pub async fn deployment_secrets(
    state: &AppState,
    project: &str,
    manifest: &Manifest,
    stage: Stage,
) -> Result<BTreeMap<String, String>, AppError> {
    if stage != Stage::Live {
        return Ok(manifest.secrets.iter().map(|s| (s.name.clone(), placeholder(&s.name))).collect());
    }
    let names: BTreeSet<&str> = manifest.secrets.iter().map(|s| s.name.as_str()).collect();
    let values = live_values(state, project, &names).await?;
    if let Some(missing) = names.iter().find(|n| !values.contains_key(**n)) {
        return Err(AppError::conflict(format!("secret {missing} has no live value; set it before going live")));
    }
    Ok(values)
}

/// Scans every session log uploaded to `project` for the Live values of `manifest`'s secrets.
pub async fn taint_scan(state: &AppState, project: &str, manifest: &Manifest) -> Result<Vec<TaintFinding>, AppError> {
    let names: BTreeSet<&str> = manifest.secrets.iter().map(|s| s.name.as_str()).collect();
    let values: Vec<(String, String)> = live_values(state, project, &names).await?.into_iter().collect();
    if values.is_empty() {
        return Ok(Vec::new());
    }
    let mut seen = BTreeSet::new();
    let mut logs: Vec<(String, Vec<u8>)> = Vec::new();
    for s in state.store.sessions(project).await? {
        if !seen.insert(s.blob_hash.clone()) {
            continue;
        }
        let bytes = state
            .blobs
            .get(&s.blob_hash)
            .await?
            .ok_or_else(|| AppError::internal(format!("session log blob {} is missing", s.blob_hash)))?;
        logs.push((s.session_id, bytes));
    }
    let logs: Vec<SessionLog<'_>> = logs.iter().map(|(id, b)| SessionLog { session_id: id, bytes: b }).collect();
    Ok(taint::scan(&values, &logs))
}

/// Activates `release` at `stage` in the ledger, then deploys it. Secrets are resolved first so
/// a Live promotion missing a value changes nothing.
pub async fn activate_and_deploy(state: &AppState, project: &str, release: &str, stage: Stage) -> Result<(), AppError> {
    let ledger = state.ledgers.get(project)?;
    let before = ledger.state()?;
    let rs = before.releases.get(release).ok_or_else(|| AppError::not_found(format!("release {release}")))?;
    let secrets = deployment_secrets(state, project, &rs.manifest, stage).await?;
    ledger.activate(release, stage)?;
    deploy(state, project, release, stage, secrets, &before).await
}

/// Deploys the release now active at `stage` (after a ledger activation by promotion, rollback
/// or standing order). `before` is the ledger state prior to the activation, for detaching
/// domains the new Live manifest dropped.
pub async fn deploy(
    state: &AppState,
    project: &str,
    release: &str,
    stage: Stage,
    secrets: BTreeMap<String, String>,
    before: &LedgerState,
) -> Result<(), AppError> {
    let rs = before.releases.get(release).ok_or_else(|| AppError::not_found(format!("release {release}")))?;
    let image_digest = rs.image_digest.clone().ok_or_else(|| AppError::conflict(format!("release {release} has not been built")))?;
    let manifest = rs.manifest.clone();
    if stage == Stage::Live {
        detach_dropped_domains(state, project, before, &manifest).await?;
    }
    state
        .services
        .runtime
        .deploy(Deployment { project: project.to_owned(), stage, release: release.to_owned(), image_digest, manifest, secrets })
        .await?;
    Ok(())
}

/// Hostnames the previous Live manifest declared and the new one does not stop serving.
async fn detach_dropped_domains(state: &AppState, project: &str, before: &LedgerState, next: &Manifest) -> Result<(), AppError> {
    let Some(prev) = before.active.get(&Stage::Live).and_then(|r| before.releases.get(r)) else {
        return Ok(());
    };
    let keep: BTreeSet<String> = next.domains.iter().map(|d| cloud_domains::normalize_name(&d.hostname)).collect();
    let dropped: Vec<String> =
        prev.manifest.domains.iter().map(|d| cloud_domains::normalize_name(&d.hostname)).filter(|h| !keep.contains(h)).collect();
    if dropped.is_empty() {
        return Ok(());
    }
    let mut registry = state.registry.lock().await;
    let now = state.now();
    let mut changed = false;
    for host in dropped {
        if registry.for_project(project).iter().any(|b| b.hostname == host && b.is_live()) {
            registry.detach(project, &host, now)?;
            changed = true;
        }
    }
    if changed {
        state.save_registry(&registry).await?;
    }
    Ok(())
}

/// Records the proposal's side data and starts its build in the background.
pub async fn enqueue_build(
    state: &AppState,
    project: &str,
    release: &str,
    manifest: Manifest,
    edge_toml: Option<String>,
) -> Result<(), AppError> {
    let meta = ReleaseMeta {
        project: project.to_owned(),
        release: release.to_owned(),
        edge_toml,
        build: BuildState::Pending,
        build_log: String::new(),
        created_at: state.now(),
    };
    state.store.put_release_meta(&meta).await?;
    let st = state.clone();
    let (project, release) = (project.to_owned(), release.to_owned());
    state.jobs.spawn(async move {
        if let Err(error) = run_build(&st, meta, &manifest).await {
            tracing::warn!(%project, %release, %error, "release pipeline stopped");
        }
    });
    Ok(())
}

/// Build → `image_built` → taint scan → `taint_checked` → Preview activation (when clean).
async fn run_build(state: &AppState, mut meta: ReleaseMeta, manifest: &Manifest) -> Result<(), AppError> {
    let (project, release) = (meta.project.clone(), meta.release.clone());
    let archive_url = match &manifest.source {
        Source::Archive { hash } => {
            Some(format!("{}{}", state.cfg.public_url.trim_end_matches('/'), cloud_api::routes::fill(ARCHIVE_BLOB, &[("hash", hash)])))
        }
        _ => None,
    };
    let job = BuildJob {
        project: project.clone(),
        release: release.clone(),
        source: manifest.source.clone(),
        build: manifest.build.clone(),
        archive_url,
    };
    let (digest, log) = match state.services.builder.build(job).await {
        Ok(BuildResult::Built { image_digest, log }) => (image_digest, log),
        Ok(BuildResult::Failed { reason, log }) => return fail(state, &mut meta, reason, log).await,
        Err(e) => return fail(state, &mut meta, e.to_string(), String::new()).await,
    };
    let ledger = state.ledgers.get(&project)?;
    if let Err(e) = ledger.image_built(&release, &digest) {
        return fail(state, &mut meta, format!("builder returned an unusable digest: {e}"), log).await;
    }
    meta.build = BuildState::Built;
    meta.build_log = log;
    state.store.put_release_meta(&meta).await?;

    let findings = taint_scan(state, &project, manifest).await?;
    let clean = findings.is_empty();
    ledger.taint_checked(&release, findings)?;
    if clean {
        activate_and_deploy(state, &project, &release, Stage::Preview).await?;
    }
    Ok(())
}

async fn fail(state: &AppState, meta: &mut ReleaseMeta, reason: String, log: String) -> Result<(), AppError> {
    meta.build = BuildState::Failed { reason };
    meta.build_log = log;
    state.store.put_release_meta(meta).await?;
    Ok(())
}

/// Users referenced by approvals, for views.
pub async fn users_of(state: &AppState, ledger: &LedgerState) -> Result<BTreeMap<String, UserRec>, AppError> {
    let ids: BTreeSet<&str> = ledger.releases.values().flat_map(|r| r.approvals.iter().map(|(_, a)| a.user_id.as_str())).collect();
    let mut out = BTreeMap::new();
    for id in ids {
        if let Some(u) = state.store.user(id).await? {
            out.insert(id.to_owned(), u);
        }
    }
    Ok(out)
}
