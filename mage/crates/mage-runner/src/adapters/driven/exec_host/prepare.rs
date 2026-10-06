//! UC1 Prepare a base (executor.md): single-flight per `BaseKey`, SourceBundle download and
//! safe extraction, `refs/mage/source`, recipe build steps, runner-local template database with
//! migrate + seed, boot verification of every service, `refs/mage/prepared`, then an atomic
//! rename of the staging dir. Any failure kills the prepare processes, drops the base DB, moves
//! staging to trash, and abandons the claim.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use mage_core::domain::recipe::{DbProvider, RecipeSpec};
use tokio::sync::watch;

use super::bundle::{self, BundleLimits};
use super::env::{check_toolchains, lease_recipe_ports, log_tail, make_sandbox_dirs, run_recipe_step, start_proxy};
use super::git::GitBaseline;
use super::logs::LogWriter;
use super::postgres::RunnerPg;
use super::proxy::EgressProxy;
use super::sandbox::Sandbox;
use super::services::{Services, compose_project};
use super::{HostShared, cgroup, reaper, sys};
use crate::ports::executor::{
    BaseKey, BaseKind, BaseRef, EgressProfile, ExecError, ExecutorKind, KillReason, LogSource, PrepareObserver, PrepareRequest, ProcessRole,
    ServiceSelector, ServiceState, StepStatus,
};
use crate::ports::runner_state::{BaseClaim, BaseMeta, PortOwner};

/// Observer wrapper that also appends every line to `<base>/logs/prepare.log`.
struct Tee<'a> {
    inner: &'a dyn PrepareObserver,
    log: LogWriter,
}

impl PrepareObserver for Tee<'_> {
    fn step(&self, step: &str, status: StepStatus) {
        let text = match &status {
            StepStatus::Started => format!("== {step}: started"),
            StepStatus::Ok { secs } => format!("== {step}: ok ({secs}s)"),
            StepStatus::Failed { exit, reason } => format!("== {step}: failed ({}) {reason}", exit.map_or_else(|| "-".to_owned(), |e| e.to_string())),
        };
        self.log.line(&text);
        self.inner.step(step, status);
    }

    fn line(&self, step: &str, line: &str) {
        self.log.line(&format!("[{step}] {line}"));
        self.inner.line(step, line);
    }
}

pub(crate) async fn prepare(shared: &Arc<HostShared>, req: PrepareRequest, obs: &dyn PrepareObserver) -> Result<BaseRef, ExecError> {
    let key = BaseKey {
        org_id: req.org_id,
        repo_id: req.repo_id,
        sha: req.sha,
        recipe_hash: req.recipe.as_ref().map(RecipeSpec::hash),
        arch: shared.arch,
        executor: ExecutorKind::Host,
    };
    let id = key.id();
    loop {
        // In-process single-flight: one owner per base id, others wait for it to finish.
        enum Role {
            Owner(watch::Sender<()>),
            Waiter(watch::Receiver<()>),
        }
        let role = {
            let mut map = shared.preparing.lock();
            match map.get(&id) {
                Some(rx) => Role::Waiter(rx.clone()),
                None => {
                    let (tx, rx) = watch::channel(());
                    map.insert(id.clone(), rx);
                    Role::Owner(tx)
                }
            }
        };
        match role {
            Role::Owner(tx) => {
                let result = prepare_owned(shared, &key, &req, obs).await;
                shared.preparing.lock().remove(&id);
                drop(tx);
                return result;
            }
            // Resolves with an error once the owner drops the sender.
            Role::Waiter(mut rx) => {
                let _ = rx.changed().await;
            }
        }
    }
}

async fn prepare_owned(shared: &Arc<HostShared>, key: &BaseKey, req: &PrepareRequest, obs: &dyn PrepareObserver) -> Result<BaseRef, ExecError> {
    let id = key.id();
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    let staging = shared.layout.bases().join(format!("{}.staging-{}", id.as_str(), &nonce[..12]));
    loop {
        match shared.state.claim_base(key, &staging.display().to_string()).await? {
            BaseClaim::Ready(rec) => {
                shared.state.touch_base(key, shared.clock.now()).await?;
                return Ok(shared.base_ref(&rec));
            }
            BaseClaim::Owned => break,
            // A Preparing row without an in-process owner is left over from a crash.
            BaseClaim::InProgress => reaper::abandon_preparing(shared, key).await?,
        }
    }
    let estimate = reaper::estimate_for_repo(shared, key).await.unwrap_or(shared.cfg.disk.first_base_estimate_gib << 30);
    if let Err(err) = reaper::ensure_disk(shared, estimate).await {
        shared.state.base_abandon(key).await?;
        return Err(err);
    }
    let sb = Arc::new(Sandbox::new(
        Arc::clone(shared),
        format!("base_{}", id.as_str()),
        None,
        None,
        staging.clone(),
        None,
        PortOwner::Base(id.clone()),
        format!("mage-b{}", id.as_str()),
    ));
    let mut run = PrepareRun { shared: Arc::clone(shared), sb, key: *key, db_created: false, proxy: None, services: Services::default(), cgroup: None };
    match run.execute(req, obs).await {
        Ok(base) => Ok(base),
        Err(err) => {
            run.fail().await;
            Err(err)
        }
    }
}

struct PrepareRun {
    shared: Arc<HostShared>,
    sb: Arc<Sandbox>,
    key: BaseKey,
    db_created: bool,
    proxy: Option<EgressProxy>,
    services: Services,
    cgroup: Option<PathBuf>,
}

impl PrepareRun {
    async fn execute(&mut self, req: &PrepareRequest, obs: &dyn PrepareObserver) -> Result<BaseRef, ExecError> {
        let shared = Arc::clone(&self.shared);
        let id = self.key.id();
        let staging = self.sb.dir.clone();
        if let Some(root) = &shared.cgroup_root {
            let cg = cgroup::create(root, &format!("prep-{}", id.as_str()), &shared.cfg.cgroup)?;
            self.cgroup = Some(cg.clone());
            // The sandbox was created before the cgroup existed; rebuild it with the cgroup.
            self.sb = Arc::new(Sandbox::new(
                Arc::clone(&shared),
                self.sb.label.clone(),
                None,
                None,
                staging.clone(),
                Some(cg),
                PortOwner::Base(id.clone()),
                self.sb.compose_name.clone(),
            ));
        }
        make_sandbox_dirs(&self.sb)?;
        let log = LogWriter::open(&self.sb.log_path(&LogSource::Prepare))?;
        let tee = Tee { inner: obs, log };
        let obs: &dyn PrepareObserver = &tee;

        // 3. SourceBundle → staging/tree.
        obs.step("source", StepStatus::Started);
        let started = std::time::Instant::now();
        let fetched = async {
            let archive = bundle::fetch(&shared.http, &shared.layout.bundles(), &req.source_bundle).await?;
            let tree = staging.join("tree");
            bundle::extract(archive, tree, BundleLimits::from_gib(shared.cfg.prepare.max_bundle_unpacked_gib), shared.owners.agent).await
        }
        .await;
        match &fetched {
            Ok(stats) => {
                obs.line("source", &format!("{} entries, {} bytes", stats.entries, stats.bytes));
                obs.step("source", StepStatus::Ok { secs: secs(started) });
            }
            Err(err) => obs.step("source", StepStatus::Failed { exit: None, reason: err.to_string() }),
        }
        fetched?;

        // 4. refs/mage/source.
        let git = GitBaseline::new(staging.join("baseline.git"), staging.join("tree"), staging.join("run"));
        git.init_source(&shared.cfg.baseline.excludes).await?;

        let (kind, template_db) = match &req.recipe {
            // 5. SourceOnly.
            None => {
                git.alias_ref("source", "prepared").await?;
                (BaseKind::SourceOnly, None)
            }
            Some(recipe) => self.prepare_recipe(recipe, &git, obs).await?,
        };

        // 10. Measure, write metadata, rename staging to the final dir, base_ready.
        self.cleanup_processes().await;
        for sub in ["scratch", "tmp", "shm", "run"] {
            let _ = sys::remove_tree(&staging.join(sub));
        }
        if let Some(recipe) = &req.recipe {
            std::fs::write(staging.join("recipe.toml"), recipe.canonical_toml())?;
        }
        let bytes = {
            let dir = staging.clone();
            tokio::task::spawn_blocking(move || sys::allocated_bytes(&dir)).await.unwrap_or(0)
        };
        let meta_doc = serde_json::json!({
            "key": self.key,
            "kind": kind.as_str(),
            "recipe_hash": self.key.recipe_hash.map(|h| h.to_hex()),
            "bundle_sha256": hex::encode(req.source_bundle.sha256),
            "bytes": bytes,
            "fork_mode": shared.fork_mode.as_str(),
            "template_db": template_db,
            "created_at": shared.clock.now().format(&time::format_description::well_known::Rfc3339).unwrap_or_default(),
        });
        std::fs::write(staging.join("base.json"), serde_json::to_vec_pretty(&meta_doc).map_err(|e| ExecError::Io(e.to_string()))?)?;
        let final_dir = shared.layout.base(&id);
        if final_dir.exists() {
            if let Some(old) = sys::move_to_trash(&final_dir, &shared.layout.trash())? {
                sys::remove_in_background(old);
            }
        }
        std::fs::rename(&staging, &final_dir).map_err(|e| ExecError::Io(format!("rename base: {e}")))?;
        let meta = BaseMeta { kind, dir: final_dir.display().to_string(), bytes, fork_mode: shared.fork_mode, template_db };
        shared.state.base_ready(&self.key, meta).await?;
        let rec = shared.state.base(&self.key).await?.ok_or_else(|| ExecError::NotFound(format!("base {id}")))?;
        Ok(shared.base_ref(&rec))
    }

    /// Steps 6–9: host checks, build, database, boot verification, prepared baseline, template.
    async fn prepare_recipe(&mut self, recipe: &RecipeSpec, git: &GitBaseline, obs: &dyn PrepareObserver) -> Result<(BaseKind, Option<String>), ExecError> {
        let shared = Arc::clone(&self.shared);
        let report = recipe.validate();
        if !report.is_valid() {
            let issues: Vec<String> = report.errors.iter().map(|i| format!("{}: {}", i.path, i.message)).collect();
            return Err(ExecError::Invalid(format!("recipe: {}", issues.join("; "))));
        }
        let total: u64 = recipe.build.iter().map(|b| u64::from(b.timeout_s)).sum();
        if total > u64::from(shared.cfg.prepare.max_total_s) {
            return Err(ExecError::Invalid(format!("sum of build timeouts {total}s exceeds runner.prepare.max_total_s")));
        }
        check_toolchains(&shared, recipe)?;
        let uses_compose = recipe.services.values().any(|s| s.compose.is_some());
        if uses_compose {
            shared.compose.available().await?;
        }
        let runner_db = recipe.database.as_ref().filter(|d| d.provider == DbProvider::Runner);
        if let Some(db) = runner_db {
            let pg = shared.pg()?;
            if let Some(want) = &db.version {
                let have = pg.server_major().await?;
                if want.split('.').next() != Some(have.to_string().as_str()) {
                    return Err(ExecError::Unavailable(format!("recipe wants Postgres {want}, runner Postgres is {have}")));
                }
            }
        }
        self.sb.vars.lock().recipe = Some(recipe.clone());
        self.proxy = start_proxy(&self.sb, EgressProfile::Registries).await?;

        // 6. Build steps (no ports, no DB values).
        for step in &recipe.build {
            let cmd = self.sb.render_build(&step.run)?;
            let log = self.sb.log_path(&LogSource::Build(step.name.clone()));
            run_recipe_step(&self.sb, &step.name, cmd, &step.cwd, step.timeout_s, Some(&step.env), ProcessRole::Build, obs, &log).await?;
        }

        // 7. Ports for prepare-time services, database, migrate, seed.
        lease_recipe_ports(&self.sb, recipe).await?;
        if uses_compose {
            if let Some(project) = compose_project(&self.sb, recipe, false).await? {
                obs.step("compose", StepStatus::Started);
                shared.compose.pull(&project).await?;
                shared.compose.build(&project).await?;
                obs.step("compose", StepStatus::Ok { secs: 0 });
            }
        }
        let mut template_db = None;
        if let Some(db) = &recipe.database {
            match db.provider {
                DbProvider::Runner => {
                    let id = self.key.id();
                    let pg = shared.pg()?;
                    // We own the claim: a database or role with this name is left over from a
                    // prepare that crashed before its cleanup.
                    pg.drop_base(&id).await?;
                    let creds = pg.create_base(&id, &db.extensions).await?;
                    self.db_created = true;
                    self.sb.vars.lock().db = Some(creds);
                    template_db = Some(RunnerPg::base_db_name(&id));
                }
                DbProvider::Service => {
                    if let Some(url) = &db.url {
                        let rendered = self.sb.render(url)?;
                        self.sb.vars.lock().service_db_url = Some(rendered);
                    }
                    if let Some(service) = &db.service {
                        self.boot(ServiceSelector::Named(vec![service.clone()]), obs).await?;
                    }
                }
            }
            for (step, command) in [("migrate", &db.migrate), ("seed", &db.seed)] {
                if let Some(command) = command {
                    let cmd = self.sb.render(command)?;
                    let log = self.sb.log_path(&LogSource::Build(step.to_owned()));
                    run_recipe_step(&self.sb, step, cmd, &db.cwd, db.timeout_s, None, ProcessRole::Exec, obs, &log).await?;
                }
            }
        }

        // 8. Boot verification.
        self.boot(ServiceSelector::All, obs).await?;

        // 9. Stop services cleanly, prepared baseline, template, release ports.
        self.services.stop(&self.sb, ServiceSelector::All).await?;
        if uses_compose {
            shared.compose.down(&self.sb.compose_name).await?;
        }
        git.commit_baseline("prepared").await?;
        if self.db_created {
            shared.pg()?.freeze_base(&self.key.id()).await?;
        }
        shared.state.release_ports(&PortOwner::Base(self.key.id())).await?;
        Ok((BaseKind::Prepared, template_db))
    }

    async fn boot(&self, which: ServiceSelector, obs: &dyn PrepareObserver) -> Result<(), ExecError> {
        obs.step("services", StepStatus::Started);
        let started = std::time::Instant::now();
        let report = self.services.start(&self.sb, which).await?;
        if let Some(bad) = report.services.iter().find(|s| s.state != ServiceState::Healthy) {
            let reason = match &bad.state {
                ServiceState::Unhealthy { reason } => reason.clone(),
                ServiceState::Exited { exit } => format!("exited ({exit})"),
                other => format!("{other:?}"),
            };
            obs.step("services", StepStatus::Failed { exit: None, reason: format!("{}: {reason}", bad.name) });
            return Err(ExecError::ServiceUnhealthy {
                service: bad.name.clone(),
                reason,
                log_tail: log_tail(&self.sb.log_path(&LogSource::Service(bad.name.clone())), 4096),
            });
        }
        obs.step("services", StepStatus::Ok { secs: secs(started) });
        Ok(())
    }

    /// Stops services, kills every process, shuts the proxy, removes the cgroup.
    async fn cleanup_processes(&mut self) {
        let recipe = self.sb.vars.lock().recipe.clone();
        self.services.stop_all(&self.sb, recipe.as_ref()).await;
        self.sb.kill_all(KillReason::Cancelled, self.sb.kill_grace()).await;
        if let Some(proxy) = self.proxy.take() {
            proxy.shutdown().await;
        }
        if let Some(cg) = self.cgroup.take() {
            if let Err(err) = cgroup::destroy(&cg) {
                tracing::warn!(error = %err, "prepare cgroup cleanup failed");
            }
        }
    }

    /// Failure path: kill everything, compose down, drop the base DB, release ports, trash
    /// staging, abandon the claim.
    async fn fail(&mut self) {
        let shared = Arc::clone(&self.shared);
        self.cleanup_processes().await;
        let uses_compose = self.sb.vars.lock().recipe.as_ref().is_some_and(|r| r.services.values().any(|s| s.compose.is_some()));
        if uses_compose {
            if let Err(err) = shared.compose.down(&self.sb.compose_name).await {
                tracing::warn!(error = %err, "prepare compose down failed");
            }
        }
        if self.db_created {
            if let Ok(pg) = shared.pg() {
                if let Err(err) = pg.drop_base(&self.key.id()).await {
                    tracing::warn!(error = %err, "prepare base database cleanup failed");
                }
            }
        }
        let _ = shared.state.release_ports(&PortOwner::Base(self.key.id())).await;
        match sys::move_to_trash(&self.sb.dir, &shared.layout.trash()) {
            Ok(Some(moved)) => sys::remove_in_background(moved),
            Ok(None) => {}
            Err(err) => tracing::warn!(error = %err, "prepare staging cleanup failed"),
        }
        if let Err(err) = shared.state.base_abandon(&self.key).await {
            tracing::warn!(error = %err, "base_abandon failed");
        }
    }
}

fn secs(started: std::time::Instant) -> u32 {
    u32::try_from(started.elapsed().as_secs()).unwrap_or(u32::MAX)
}

/// Bundle sha recorded in a base dir's `base.json`.
pub(crate) fn bundle_sha_of(base_dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(base_dir.join("base.json")).ok()?;
    let doc: serde_json::Value = serde_json::from_str(&text).ok()?;
    doc.get("bundle_sha256")?.as_str().map(str::to_owned)
}
