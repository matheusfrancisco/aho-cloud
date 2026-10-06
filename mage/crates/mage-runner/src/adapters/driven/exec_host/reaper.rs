//! Reaper and disk policy (executor.md "Reaper", UC8, UC9).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use mage_core::domain::RunId;

use super::env::{kind, teardown_from_state};
use super::launch::kill_group;
use super::postgres::RunnerPg;
use super::{HostShared, cgroup, prepare, sys};
use crate::ports::executor::{BaseId, BaseKey, EnvId, ExecError, ReapReport, ReapScope};
use crate::ports::runner_state::{BaseRecord, BaseStatus, PortOwner, ProcessRecord};

/// Kills a recorded process group if it is still the same process (boot id and start time
/// match); otherwise the pid is gone or was reused and the row is only cleared.
async fn kill_recorded(shared: &HostShared, p: &ProcessRecord) -> bool {
    let same = p.boot_id == shared.boot_id && sys::process_start_time(p.pid) == Some(p.start_time);
    if same {
        kill_group(p.pgid, p.cgroup.as_deref().map(Path::new));
    }
    if let Err(err) = shared.state.clear_process(p.pgid).await {
        tracing::warn!(pgid = p.pgid, error = %err, "clear_process failed");
    }
    same
}

/// Cleans a Preparing base without an in-process owner: base DB, compose project, staging dir,
/// prepare port leases, and the claim.
pub(crate) async fn abandon_preparing(shared: &HostShared, key: &BaseKey) -> Result<(), ExecError> {
    let id = key.id();
    if let Some(rec) = shared.state.base(key).await? {
        if rec.status != BaseStatus::Preparing {
            return Ok(());
        }
        if let Some(pg) = &shared.pg {
            if let Err(err) = pg.drop_base(&id).await {
                tracing::warn!(base = %id, error = %err, "dropping a stale base database failed");
            }
        }
        if let Err(err) = shared.compose.down(&format!("mage-b{}", id.as_str())).await {
            if !matches!(&err, ExecError::Unavailable(m) if m == "docker") {
                tracing::warn!(base = %id, error = %err, "compose down of a stale prepare failed");
            }
        }
        if let Some(root) = &shared.cgroup_root {
            let _ = cgroup::destroy(&root.join(format!("prep-{}", id.as_str())));
        }
        if let Some(moved) = sys::move_to_trash(Path::new(&rec.dir), &shared.layout.trash())? {
            sys::remove_in_background(moved);
        }
    }
    shared.state.release_ports(&PortOwner::Base(id)).await?;
    shared.state.base_abandon(key).await?;
    Ok(())
}

/// Last Ready base size of the same repo (prepare disk estimate).
pub(crate) async fn estimate_for_repo(shared: &HostShared, key: &BaseKey) -> Option<u64> {
    let bases = shared.state.bases().await.ok()?;
    bases
        .into_iter()
        .filter(|b| b.status == BaseStatus::Ready && b.key.org_id == key.org_id && b.key.repo_id == key.repo_id)
        .max_by_key(|b| b.created_at)
        .map(|b| b.bytes)
}

/// Evicts a Ready base: template DB, dir, and its bundle when no other base uses it. Conflict
/// while live forks reference it.
pub(crate) async fn evict_base(shared: &HostShared, key: &BaseKey) -> Result<u64, ExecError> {
    let rec = shared.state.base(key).await?.ok_or_else(|| ExecError::NotFound(format!("base {}", key.id())))?;
    if rec.status != BaseStatus::Ready {
        return Err(ExecError::Conflict(format!("base {} is being prepared", rec.id)));
    }
    if rec.live_forks > 0 {
        return Err(ExecError::Conflict(format!("base {} has {} live forks", rec.id, rec.live_forks)));
    }
    shared.state.remove_base(key).await?;
    if rec.template_db.is_some() {
        shared.pg()?.drop_base(&rec.id).await?;
    }
    let dir = PathBuf::from(&rec.dir);
    let bundle = prepare::bundle_sha_of(&dir);
    if let Some(moved) = sys::move_to_trash(&dir, &shared.layout.trash())? {
        sys::remove_in_background(moved);
    }
    if let Some(sha) = bundle {
        let still_used = shared
            .state
            .bases()
            .await?
            .iter()
            .any(|b| prepare::bundle_sha_of(Path::new(&b.dir)).as_deref() == Some(sha.as_str()));
        if !still_used {
            let _ = std::fs::remove_file(shared.layout.bundles().join(format!("{sha}.tar.zst")));
        }
    }
    Ok(rec.bytes)
}

async fn try_evict(shared: &HostShared, rec: &BaseRecord, report: &mut ReapReport) -> bool {
    match evict_base(shared, &rec.key).await {
        Ok(bytes) => {
            report.bases_evicted += 1;
            report.bytes_freed += bytes;
            true
        }
        Err(err) => {
            tracing::warn!(base = %rec.id, error = %err, "base eviction failed");
            false
        }
    }
}

/// Disk policy (UC9 step 2): idle, over-count, and over-size bases go first, then LRU bases
/// with no live forks until `min_free_gib + estimate` is free. Sets `disk_pressure`.
async fn disk_policy(shared: &HostShared, estimate: u64) -> Result<(ReapReport, bool), ExecError> {
    let cfg = &shared.cfg.disk;
    let mut report = ReapReport::default();
    let now = shared.clock.now();
    let mut ready: Vec<BaseRecord> = shared.state.bases().await?.into_iter().filter(|b| b.status == BaseStatus::Ready).collect();
    ready.sort_by_key(|b| b.last_used_at);
    let idle = time::Duration::days(i64::from(cfg.base_idle_days));
    let mut kept: Vec<BaseRecord> = Vec::new();
    for rec in ready {
        if rec.live_forks == 0 && rec.last_used_at < now - idle && try_evict(shared, &rec, &mut report).await {
            continue;
        }
        kept.push(rec);
    }
    let max_bytes = cfg.bases_max_gib << 30;
    loop {
        let over_count = kept.len() > cfg.bases_max as usize;
        let over_size = kept.iter().map(|b| b.bytes).sum::<u64>() > max_bytes;
        if !over_count && !over_size {
            break;
        }
        let Some(pos) = kept.iter().position(|b| b.live_forks == 0) else { break };
        let rec = kept.remove(pos);
        try_evict(shared, &rec, &mut report).await;
    }
    let need = (cfg.min_free_gib << 30) + estimate;
    loop {
        let (free, _) = sys::disk_space(&shared.layout.root)?;
        if free >= need {
            shared.disk_pressure.store(false, Ordering::SeqCst);
            return Ok((report, true));
        }
        let Some(pos) = kept.iter().position(|b| b.live_forks == 0) else {
            shared.disk_pressure.store(true, Ordering::SeqCst);
            return Ok((report, false));
        };
        let rec = kept.remove(pos);
        try_evict(shared, &rec, &mut report).await;
    }
}

/// Disk check before prepare and fork: evicts as needed, `Unavailable("disk")` when still short.
pub(crate) async fn ensure_disk(shared: &HostShared, estimate: u64) -> Result<(), ExecError> {
    let (_, ok) = disk_policy(shared, estimate).await?;
    if ok { Ok(()) } else { Err(ExecError::Unavailable("disk".into())) }
}

pub(crate) async fn reap(shared: &HostShared, scope: ReapScope) -> Result<ReapReport, ExecError> {
    let mut report = ReapReport::default();
    let active: HashSet<RunId> = match scope {
        ReapScope::Boot => HashSet::new(),
        ReapScope::Periodic => shared.state.active_runs().await?.into_iter().map(|r| r.run_id).collect(),
    };
    let processes = shared.state.processes().await?;
    let live: std::collections::HashMap<EnvId, _> = shared.live_envs().into_iter().map(|e| (e.sandbox().env_id.unwrap_or(EnvId::from_uuid(uuid::Uuid::nil())), e)).collect();

    // 1. Forks (all on boot; those of inactive runs periodically).
    for fork in shared.state.forks().await? {
        if scope == ReapScope::Periodic && active.contains(&fork.run_id) {
            continue;
        }
        let result = match live.get(&fork.env) {
            Some(env) => env.destroy_inner().await,
            None => {
                for p in processes.iter().filter(|p| p.env == Some(fork.env)) {
                    if kill_recorded(shared, p).await {
                        report.processes_killed += 1;
                    }
                }
                teardown_from_state(shared, fork.env).await
            }
        };
        match result {
            Ok(()) => {
                report.forks_destroyed += 1;
                report.dirs_removed += 1;
                report.databases_dropped += u32::from(fork.resources.iter().any(|r| r.kind == kind::PG_DB));
                report.compose_projects_removed += u32::from(fork.resources.iter().any(|r| r.kind == kind::COMPOSE));
            }
            Err(err) => tracing::warn!(env = %fork.env, error = %err, "reaping fork failed"),
        }
    }
    let remaining = shared.state.forks().await?;
    let remaining_envs: HashSet<EnvId> = remaining.iter().map(|f| f.env).collect();

    // 2. Recorded processes.
    for p in shared.state.processes().await? {
        let stale = match scope {
            ReapScope::Boot => true,
            // Prepare processes (no env) belong to an in-process prepare.
            ReapScope::Periodic => p.env.is_some_and(|e| !remaining_envs.contains(&e)),
        };
        if stale && kill_recorded(shared, &p).await {
            report.processes_killed += 1;
        }
    }

    // 3. Preparing bases without an in-process owner.
    let in_flight: HashSet<BaseId> = shared.preparing.lock().keys().cloned().collect();
    for rec in shared.state.recovery_snapshot().await?.preparing_bases {
        if scope == ReapScope::Boot || !in_flight.contains(&rec.id) {
            abandon_preparing(shared, &rec.key).await?;
            report.dirs_removed += 1;
        }
    }

    // 4. Port leases whose owner is gone.
    let preparing: HashSet<BaseId> = shared.state.recovery_snapshot().await?.preparing_bases.into_iter().map(|b| b.id).collect();
    let mut released: HashSet<PortOwner> = HashSet::new();
    for lease in shared.state.port_leases().await? {
        let gone = match &lease.owner {
            PortOwner::Env(e) => !remaining_envs.contains(e),
            PortOwner::RunMcp(r) => scope == ReapScope::Boot || !active.contains(r),
            PortOwner::Base(b) => !preparing.contains(b),
        };
        if gone && released.insert(lease.owner.clone()) {
            shared.state.release_ports(&lease.owner).await?;
        }
    }

    // 5. Orphan scan (boot).
    if scope == ReapScope::Boot {
        orphan_scan(shared, &remaining_envs, &mut report).await;
    }

    // Trash cleanup (both scopes).
    if let Ok(rd) = std::fs::read_dir(shared.layout.trash()) {
        for entry in rd.flatten() {
            let path = entry.path();
            match tokio::task::spawn_blocking(move || sys::remove_tree(&path)).await {
                Ok(Ok(())) => report.dirs_removed += 1,
                Ok(Err(err)) => tracing::warn!(error = %err, "trash cleanup failed"),
                Err(err) => tracing::warn!(error = %err, "trash cleanup task failed"),
            }
        }
    }

    // Disk policy (periodic).
    if scope == ReapScope::Periodic {
        match disk_policy(shared, 0).await {
            Ok((disk, _)) => report.add(&disk),
            Err(err) => tracing::warn!(error = %err, "disk policy failed"),
        }
    }
    Ok(report)
}

/// Cleans what state does not know about: fork and staging dirs, `mf_`/`mb_` databases and
/// roles, `mage-*` compose projects, `env-*`/`prep-*` cgroups, partial bundle downloads, and
/// (Linux enforced) stray `mage-agent` processes.
async fn orphan_scan(shared: &HostShared, envs: &HashSet<EnvId>, report: &mut ReapReport) {
    let trash = shared.layout.trash();
    let known_dirs: HashSet<String> = envs.iter().map(|e| e.to_string()).collect();
    if let Ok(rd) = std::fs::read_dir(shared.layout.forks()) {
        for entry in rd.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !known_dirs.contains(&name) {
                if let Ok(Some(moved)) = sys::move_to_trash(&entry.path(), &trash) {
                    sys::remove_in_background(moved);
                    report.dirs_removed += 1;
                }
            }
        }
    }
    let preparing_dirs: HashSet<String> = shared
        .state
        .recovery_snapshot()
        .await
        .map(|s| s.preparing_bases.into_iter().map(|b| b.dir).collect())
        .unwrap_or_default();
    if let Ok(rd) = std::fs::read_dir(shared.layout.bases()) {
        for entry in rd.flatten() {
            let path = entry.path();
            let is_staging = entry.file_name().to_string_lossy().contains(".staging-");
            if is_staging && !preparing_dirs.contains(&path.display().to_string()) {
                if let Ok(Some(moved)) = sys::move_to_trash(&path, &trash) {
                    sys::remove_in_background(moved);
                    report.dirs_removed += 1;
                }
            }
        }
    }
    if let Ok(rd) = std::fs::read_dir(shared.layout.bundles()) {
        for entry in rd.flatten() {
            if entry.file_name().to_string_lossy().ends_with(".part") {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }

    let bases = shared.state.bases().await.unwrap_or_default();
    if let Some(pg) = &shared.pg {
        let fork_names: HashSet<String> = envs.iter().map(RunnerPg::fork_db_name).collect();
        let base_names: HashSet<String> = bases.iter().map(|b| RunnerPg::base_db_name(&b.id)).collect();
        let keep = |name: &str| fork_names.contains(name) || base_names.contains(name);
        match pg.mage_databases().await {
            Ok(dbs) => {
                for db in dbs.into_iter().filter(|d| !keep(d)) {
                    match pg.drop_database_named(&db).await {
                        Ok(()) => report.databases_dropped += 1,
                        Err(err) => tracing::warn!(db = %db, error = %err, "dropping orphan database failed"),
                    }
                }
            }
            Err(err) => tracing::warn!(error = %err, "runner postgres unavailable during orphan scan"),
        }
        if let Ok(roles) = pg.mage_roles().await {
            for role in roles.into_iter().filter(|r| !keep(r)) {
                if let Err(err) = pg.drop_role_named(&role).await {
                    tracing::warn!(role = %role, error = %err, "dropping orphan role failed");
                }
            }
        }
    }

    let known_projects: HashSet<String> = envs.iter().map(|e| format!("mage-{}", e.slug())).collect();
    match shared.compose.mage_projects().await {
        Ok(projects) => {
            for project in projects.into_iter().filter(|p| !known_projects.contains(p)) {
                match shared.compose.down(&project).await {
                    Ok(()) => report.compose_projects_removed += 1,
                    Err(err) => tracing::warn!(project = %project, error = %err, "removing orphan compose project failed"),
                }
            }
        }
        Err(ExecError::Unavailable(m)) if m == "docker" => {}
        Err(err) => tracing::warn!(error = %err, "listing compose projects failed"),
    }

    if let Some(root) = &shared.cgroup_root {
        let known: HashSet<String> = envs.iter().map(|e| format!("env-{}", e.slug())).collect();
        for cg in cgroup::mage_children(root) {
            let name = cg.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            if !known.contains(&name) {
                if let Err(err) = cgroup::destroy(&cg) {
                    tracing::warn!(cgroup = %cg.display(), error = %err, "removing orphan cgroup failed");
                }
            }
        }
        #[cfg(target_os = "linux")]
        if let Some((uid, _)) = shared.owners.agent {
            report.processes_killed += kill_stray_agent_processes(uid, root, &known);
        }
    }
}

/// Linux: kills processes owned by `mage-agent` that are not inside a known env cgroup.
#[cfg(target_os = "linux")]
fn kill_stray_agent_processes(agent_uid: u32, root: &Path, known: &HashSet<String>) -> u32 {
    let rel_root = root.strip_prefix("/sys/fs/cgroup").map(|p| format!("/{}", p.display())).unwrap_or_default();
    let mut killed = 0;
    let Ok(rd) = std::fs::read_dir("/proc") else { return 0 };
    for entry in rd.flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(|s| s.parse::<i32>().ok()) else { continue };
        let Ok(status) = std::fs::read_to_string(format!("/proc/{pid}/status")) else { continue };
        let uid = status.lines().find_map(|l| l.strip_prefix("Uid:")).and_then(|v| v.split_whitespace().next()).and_then(|v| v.parse::<u32>().ok());
        if uid != Some(agent_uid) {
            continue;
        }
        let cg = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).unwrap_or_default();
        let path = cg.lines().find_map(|l| l.strip_prefix("0::")).unwrap_or("").trim().to_owned();
        let inside_known = known.iter().any(|k| path.starts_with(&format!("{rel_root}/{k}")));
        if !inside_known && nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), nix::sys::signal::Signal::SIGKILL).is_ok() {
            killed += 1;
        }
    }
    killed
}
