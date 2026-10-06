//! `Executor` / `Env` contract suite (executor.md "Testing"). Runs against `FakeExecutor` and
//! `exec_host` (dev in CI; enforced opt-in). Process-dependent cases run only when the fixture
//! has real processes; database cases only when it has a runner Postgres.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use mage_core::domain::recipe::RecipeSpec;
use mage_core::domain::{CommitSha, OrgId, RepoId, RunId};
use sha2::Digest;

use crate::ports::executor::*;

#[async_trait]
pub trait ExecutorFixture: Send + Sync {
    fn executor(&self) -> Arc<dyn Executor>;
    /// Publishes a SourceBundle containing `files`.
    async fn bundle(&self, files: &[(&str, &[u8])]) -> BundleSource;
    /// Creates a symlink `path → target` inside `env` (like `ln -s target path`).
    async fn symlink(&self, env: &dyn Env, path: &EnvPath, target: &str);
    /// Real processes (a POSIX shell) run in envs.
    fn processes(&self) -> bool;
    /// A runner Postgres backs `[database] provider = "runner"`.
    fn database(&self) -> bool;
}

/// Counts `Started` events per step name across concurrent prepares.
#[derive(Default)]
struct CountingObserver {
    sources: AtomicU32,
}

impl PrepareObserver for CountingObserver {
    fn step(&self, step: &str, status: StepStatus) {
        if step == "source" && status == StepStatus::Started {
            self.sources.fetch_add(1, Ordering::SeqCst);
        }
    }
    fn line(&self, _step: &str, _line: &str) {}
}

const ORG: u128 = 0x0190_0000_0000_7000_8000_0000_0000_00a1;
const REPO: u128 = 0x0190_0000_0000_7000_8000_0000_0000_00b2;

fn org() -> OrgId {
    OrgId::from_uuid(uuid::Uuid::from_u128(ORG))
}

fn repo() -> RepoId {
    RepoId::from_uuid(uuid::Uuid::from_u128(REPO))
}

fn opts(purpose: ForkPurpose) -> ForkOptions {
    ForkOptions { run_id: RunId::from_uuid(uuid::Uuid::now_v7()), purpose, egress: EgressProfile::Registries }
}

fn ws(p: &str) -> EnvPath {
    EnvPath::ws(p).unwrap_or_else(|e| panic!("{p}: {e}"))
}

async fn request(fx: &dyn ExecutorFixture, files: &[(&str, &[u8])], recipe: Option<RecipeSpec>) -> PrepareRequest {
    let source = fx.bundle(files).await;
    let mut sha = [0u8; 20];
    sha.copy_from_slice(&source.sha256[..20]);
    PrepareRequest { org_id: org(), repo_id: repo(), sha: CommitSha(sha), recipe, source_bundle: source }
}

async fn text(env: &dyn Env, path: &str) -> Option<String> {
    match env.read(&ws(path), ReadRange::Whole { max_bytes: 1 << 20 }).await {
        Ok(read) => Some(String::from_utf8_lossy(&read.bytes).into_owned()),
        Err(ExecError::NotFound(_)) => None,
        Err(e) => panic!("read {path}: {e}"),
    }
}

/// Recipe used by the suite: two ports owned by one service healthy via a command probe, two
/// build steps (one with `inputs`).
pub fn contract_recipe() -> RecipeSpec {
    RecipeSpec::parse(
        r#"
version = 1
ports = ["api", "web"]

[[build]]
name = "deps"
run = "echo deps >> build.log"
inputs = ["package.json"]

[[build]]
name = "compile"
run = "echo compile >> build.log"

[services.app]
run = "sleep 600"
ports = ["api", "web"]
[services.app.health]
command = "true"
interval_ms = 200

[test]
run = "true"
"#,
    )
    .unwrap_or_else(|e| panic!("contract recipe: {e}"))
}

const FILES: &[(&str, &[u8])] = &[
    ("package.json", b"{\"name\":\"fixture\"}\n"),
    ("src/a.ts", b"export const a = 1;\nexport const b = 2;\nexport const c = 3;\n"),
    ("src/b.ts", b"export const x = 'x';\n"),
    ("README.md", b"# fixture\n"),
];

/// Every case that does not need processes or a database.
pub async fn run_core(fx: &dyn ExecutorFixture) {
    prepare_single_flight(fx).await;
    source_only_vs_prepared(fx).await;
    fork_isolation_and_base_unchanged(fx).await;
    diff_round_trip_and_determinism(fx).await;
    excludes_and_binary(fx).await;
    exact_on_dirty_tree_is_base_mismatch(fx).await;
    confinement(fx).await;
    reserved_env_and_bad_timeouts(fx).await;
    ports_unique_across_concurrent_forks(fx).await;
    rebuild_changed_skips_unmatched_steps(fx).await;
    empty_env(fx).await;
    destroy_is_idempotent(fx).await;
}

/// Cases with real processes.
pub async fn run_processes(fx: &dyn ExecutorFixture) {
    assert!(fx.processes(), "fixture has no processes");
    exec_exit_codes_stdin_and_cwd(fx).await;
    captured_boundaries(fx).await;
    timeout_kills_double_forked_grandchild(fx).await;
    spawn_round_trip_and_terminate(fx).await;
    destroy_kills_running_exec(fx).await;
    check_patch_source_vs_build_rewritten_file(fx).await;
}

/// Cases with the runner Postgres.
pub async fn run_database(fx: &dyn ExecutorFixture) {
    assert!(fx.database(), "fixture has no database");
    db_fork_isolation_and_reset(fx).await;
}

async fn prepare_single_flight(fx: &dyn ExecutorFixture) {
    let exec = fx.executor();
    let req = request(fx, &[("one.txt", b"1\n")], None).await;
    let (o1, o2) = (CountingObserver::default(), CountingObserver::default());
    let (a, b) = tokio::join!(exec.prepare(req.clone(), &o1), exec.prepare(req.clone(), &o2));
    let (a, b) = (a.unwrap(), b.unwrap());
    assert_eq!(a.id, b.id);
    assert_eq!(o1.sources.load(Ordering::SeqCst) + o2.sources.load(Ordering::SeqCst), 1, "two concurrent prepares → one build");
    let again = CountingObserver::default();
    assert_eq!(exec.prepare(req, &again).await.unwrap().id, a.id);
    assert_eq!(again.sources.load(Ordering::SeqCst), 0, "a Ready base is reused");
}

async fn source_only_vs_prepared(fx: &dyn ExecutorFixture) {
    let exec = fx.executor();
    let source = exec.prepare(request(fx, FILES, None).await, &NullObserver).await.unwrap();
    assert_eq!(source.kind, BaseKind::SourceOnly);
    assert_eq!(source.key.recipe_hash, None);
    let recipe = contract_recipe();
    let prepared = exec.prepare(request(fx, FILES, Some(recipe.clone())).await, &NullObserver).await.unwrap();
    assert_eq!(prepared.kind, BaseKind::Prepared);
    assert_eq!(prepared.key.recipe_hash, Some(recipe.hash()));
    assert_ne!(prepared.id, source.id);
    assert_eq!(exec.base(&prepared.key).await.unwrap().map(|b| b.id), Some(prepared.id.clone()));
    let caps = exec.capabilities().await.unwrap();
    assert!(caps.bases.contains(&prepared.key), "capabilities list Prepared bases");
    assert!(!caps.bases.contains(&source.key), "SourceOnly bases are not advertised");
    let env = exec.fork(&source, opts(ForkPurpose::Session)).await.unwrap();
    assert_eq!(env.recipe(), None);
    assert!(env.ports().0.is_empty());
    env.destroy().await.unwrap();
}

async fn fork_isolation_and_base_unchanged(fx: &dyn ExecutorFixture) {
    let exec = fx.executor();
    let base = exec.prepare(request(fx, FILES, None).await, &NullObserver).await.unwrap();
    let a = exec.fork(&base, opts(ForkPurpose::Verify)).await.unwrap();
    let b = exec.fork(&base, opts(ForkPurpose::Verify)).await.unwrap();
    assert_ne!(a.id(), b.id());
    a.write(&ws("src/a.ts"), b"changed in A\n", WriteMode::Overwrite).await.unwrap();
    a.write(&ws("new/file.txt"), b"only A\n", WriteMode::Create).await.unwrap();
    assert_eq!(text(b.as_ref(), "src/a.ts").await.as_deref(), Some("export const a = 1;\nexport const b = 2;\nexport const c = 3;\n"));
    assert_eq!(text(b.as_ref(), "new/file.txt").await, None);
    let c = exec.fork(&base, opts(ForkPurpose::Verify)).await.unwrap();
    assert_eq!(text(c.as_ref(), "src/a.ts").await, text(b.as_ref(), "src/a.ts").await, "base bytes unchanged");
    for env in [a, b, c] {
        env.destroy().await.unwrap();
    }
}

async fn diff_round_trip_and_determinism(fx: &dyn ExecutorFixture) {
    let exec = fx.executor();
    let base = exec.prepare(request(fx, FILES, Some(contract_recipe())).await, &NullObserver).await.unwrap();
    let a = exec.fork(&base, opts(ForkPurpose::Session)).await.unwrap();
    a.write(&ws("src/a.ts"), b"export const a = 1;\nexport const b = 20;\nexport const c = 3;\n", WriteMode::Overwrite).await.unwrap();
    a.write(&ws("src/new.ts"), b"export const n = 1;\n", WriteMode::Create).await.unwrap();
    a.write(&ws("README.md"), b"# fixture\nmore\n", WriteMode::Overwrite).await.unwrap();
    let clean = exec.fork(&base, opts(ForkPurpose::Verify)).await.unwrap();
    assert!(clean.diff_against_base(&DiffOptions { against: Baseline::Prepared, exclude: vec![] }).await.unwrap().patch.is_empty());
    clean.destroy().await.unwrap();

    let d = DiffOptions { against: Baseline::Prepared, exclude: vec![] };
    let first = a.diff_against_base(&d).await.unwrap();
    let second = a.diff_against_base(&d).await.unwrap();
    assert_eq!(first.patch, second.patch, "deterministic diff bytes");
    let changes: Vec<(String, FileChange)> = first.files.iter().map(|f| (f.path.as_str().to_owned(), f.change)).collect();
    assert_eq!(
        changes,
        vec![("README.md".into(), FileChange::Modified), ("src/a.ts".into(), FileChange::Modified), ("src/new.ts".into(), FileChange::Added)]
    );
    assert!(mage_core::domain::fix::Patch::parse(&first.patch).is_ok(), "patch is in the strict subset");

    let b = exec.fork(&base, opts(ForkPurpose::Verify)).await.unwrap();
    let outcome = b.apply_patch(&first.patch, ApplyMode::Exact).await.unwrap();
    let ApplyOutcome::Applied { files, patch_sha256 } = outcome else { panic!("rejected: {outcome:?}") };
    assert_eq!(files.len(), 3);
    assert_eq!(patch_sha256, <[u8; 32]>::from(sha2::Sha256::digest(&first.patch)));
    for path in ["src/a.ts", "src/new.ts", "README.md", "src/b.ts"] {
        assert_eq!(text(b.as_ref(), path).await, text(a.as_ref(), path).await, "{path} reproduced");
    }
    for env in [a, b] {
        env.destroy().await.unwrap();
    }
}

async fn excludes_and_binary(fx: &dyn ExecutorFixture) {
    let exec = fx.executor();
    let base = exec.prepare(request(fx, FILES, None).await, &NullObserver).await.unwrap();
    let env = exec.fork(&base, opts(ForkPurpose::Session)).await.unwrap();
    env.write(&ws(".mage/repro/bug.spec.ts"), b"test('x', () => {});\n", WriteMode::Create).await.unwrap();
    env.write(&ws("src/b.ts"), b"export const x = 'y';\n", WriteMode::Overwrite).await.unwrap();
    env.write(&ws("assets/logo.bin"), b"\x89PNG\0\0\x01binary", WriteMode::Create).await.unwrap();
    let out = env
        .diff_against_base(&DiffOptions { against: Baseline::Source, exclude: vec![PathGlob::new(".mage/**").unwrap()] })
        .await
        .unwrap();
    let patch = String::from_utf8_lossy(&out.patch);
    assert_eq!(out.excluded, vec![RelPath::new(".mage/repro/bug.spec.ts").unwrap()]);
    assert!(!patch.contains(".mage/"), "excluded paths stay out of the patch");
    assert_eq!(out.binary, vec![RelPath::new("assets/logo.bin").unwrap()]);
    assert!(!patch.contains("logo.bin"), "binary listed, not encoded");
    assert!(out.files.iter().any(|f| f.path.as_str() == "assets/logo.bin" && f.binary));
    assert!(out.files.iter().all(|f| !f.path.as_str().starts_with(".mage/")));
    assert!(patch.contains("src/b.ts"));
    env.destroy().await.unwrap();
}

async fn exact_on_dirty_tree_is_base_mismatch(fx: &dyn ExecutorFixture) {
    let exec = fx.executor();
    let base = exec.prepare(request(fx, FILES, None).await, &NullObserver).await.unwrap();
    let a = exec.fork(&base, opts(ForkPurpose::Session)).await.unwrap();
    a.write(&ws("src/b.ts"), b"export const x = 'z';\n", WriteMode::Overwrite).await.unwrap();
    let patch = a.diff_against_base(&DiffOptions { against: Baseline::Prepared, exclude: vec![] }).await.unwrap().patch;
    let b = exec.fork(&base, opts(ForkPurpose::Verify)).await.unwrap();
    b.write(&ws("unrelated.txt"), b"dirty\n", WriteMode::Create).await.unwrap();
    let outcome = b.apply_patch(&patch, ApplyMode::Exact).await.unwrap();
    assert!(matches!(outcome, ApplyOutcome::Rejected { reason: RejectReason::BaseMismatch, .. }), "{outcome:?}");
    assert!(matches!(b.apply_patch(&patch, ApplyMode::AllowOffset).await.unwrap(), ApplyOutcome::Applied { .. }));
    let garbage = b.apply_patch(b"not a patch\n", ApplyMode::AllowOffset).await.unwrap();
    assert!(matches!(garbage, ApplyOutcome::Rejected { reason: RejectReason::Malformed, .. }), "{garbage:?}");
    let conflict = b.apply_patch(&patch, ApplyMode::AllowOffset).await.unwrap();
    assert!(matches!(conflict, ApplyOutcome::Rejected { reason: RejectReason::DoesNotApply, .. }), "applying twice conflicts: {conflict:?}");
    for env in [a, b] {
        env.destroy().await.unwrap();
    }
}

async fn confinement(fx: &dyn ExecutorFixture) {
    let exec = fx.executor();
    let base = exec.prepare(request(fx, FILES, None).await, &NullObserver).await.unwrap();
    let env = exec.fork(&base, opts(ForkPurpose::Session)).await.unwrap();
    assert!(matches!(EnvPath::ws("../x"), Err(ExecError::Confinement { .. })));
    assert!(matches!(EnvPath::ws("/etc/passwd"), Err(ExecError::Confinement { .. })));
    fx.symlink(env.as_ref(), &ws("root"), "/").await;
    fx.symlink(env.as_ref(), &ws("hop1"), "src/hop2").await;
    fx.symlink(env.as_ref(), &ws("src/hop2"), "../..").await;
    fx.symlink(env.as_ref(), &ws("inside"), "src/b.ts").await;
    for path in ["root/etc/passwd", "hop1/etc/passwd", "hop1"] {
        let err = env.read(&ws(path), ReadRange::Whole { max_bytes: 64 }).await.unwrap_err();
        assert!(matches!(err, ExecError::Confinement { area: Area::Workspace, .. }), "{path}: {err:?}");
    }
    let err = env.write(&ws("root/tmp/mage-escape"), b"x", WriteMode::Overwrite).await.unwrap_err();
    assert!(matches!(err, ExecError::Confinement { .. }), "{err:?}");
    assert_eq!(text(env.as_ref(), "inside").await.as_deref(), Some("export const x = 'x';\n"), "symlinks inside the area resolve");
    env.destroy().await.unwrap();
}

async fn reserved_env_and_bad_timeouts(fx: &dyn ExecutorFixture) {
    let exec = fx.executor();
    let env = exec.empty_env(opts(ForkPurpose::Triage)).await.unwrap();
    for name in ["PATH", "HOME", "LD_PRELOAD", "HTTPS_PROXY", "no_proxy", "MAGE_RUN_ID"] {
        let mut spec = ExecSpec::shell("true", ProcessRole::Exec);
        spec.env = vec![(name.to_owned(), "x".to_owned())];
        assert!(matches!(env.exec(spec, None).await, Err(ExecError::Invalid(_))), "{name} must be rejected");
        let spawn = SpawnSpec { argv: vec!["true".into()], cwd: EnvPath::scratch("launch").unwrap(), env: vec![(name.to_owned(), "x".into())], role: ProcessRole::Harness };
        assert!(matches!(env.spawn(spawn).await.err(), Some(ExecError::Invalid(_))), "{name} must be rejected for spawn");
    }
    let mut spec = ExecSpec::shell("true", ProcessRole::Exec);
    spec.timeout = Duration::ZERO;
    assert!(matches!(env.exec(spec.clone(), None).await, Err(ExecError::Invalid(_))));
    spec.timeout = Duration::from_secs(24 * 3600);
    assert!(matches!(env.exec(spec, None).await, Err(ExecError::Invalid(_))));
    env.destroy().await.unwrap();
}

async fn ports_unique_across_concurrent_forks(fx: &dyn ExecutorFixture) {
    let exec = fx.executor();
    let base = exec.prepare(request(fx, FILES, Some(contract_recipe())).await, &NullObserver).await.unwrap();
    let (a, b, c) = tokio::join!(
        exec.fork(&base, opts(ForkPurpose::Verify)),
        exec.fork(&base, opts(ForkPurpose::Verify)),
        exec.fork(&base, opts(ForkPurpose::Verify))
    );
    let envs = [a.unwrap(), b.unwrap(), c.unwrap()];
    let mut seen = std::collections::BTreeSet::new();
    for env in &envs {
        let ports = env.ports();
        assert_eq!(ports.0.keys().map(PortName::as_str).collect::<Vec<_>>(), vec!["api", "web"]);
        for addr in ports.0.values() {
            assert!(addr.ip().is_loopback());
            assert!(seen.insert(addr.port()), "port {} leased twice", addr.port());
        }
        let api = env.render("http://127.0.0.1:${port.api}/x").unwrap();
        assert_eq!(api, format!("http://127.0.0.1:{}/x", ports.get("api").map(|a| a.port()).unwrap_or(0)));
        assert!(matches!(env.render("${port.nope}"), Err(ExecError::Invalid(_))));
    }
    for env in envs {
        env.destroy().await.unwrap();
    }
}

async fn rebuild_changed_skips_unmatched_steps(fx: &dyn ExecutorFixture) {
    let exec = fx.executor();
    let base = exec.prepare(request(fx, FILES, Some(contract_recipe())).await, &NullObserver).await.unwrap();
    let env = exec.fork(&base, opts(ForkPurpose::Verify)).await.unwrap();
    let report = env.rebuild(RebuildScope::Changed(vec![RelPath::new("src/a.ts").unwrap()]), &NullObserver).await.unwrap();
    assert_eq!(report.steps_run, vec!["compile".to_owned()]);
    assert_eq!(report.steps_skipped, vec!["deps".to_owned()]);
    let report = env.rebuild(RebuildScope::Changed(vec![RelPath::new("package.json").unwrap()]), &NullObserver).await.unwrap();
    assert_eq!(report.steps_run, vec!["deps".to_owned(), "compile".to_owned()]);
    assert!(!report.migrated, "no database, nothing to migrate");
    env.destroy().await.unwrap();
}

async fn empty_env(fx: &dyn ExecutorFixture) {
    let exec = fx.executor();
    let env = exec.empty_env(opts(ForkPurpose::Triage)).await.unwrap();
    assert_eq!(env.base().kind, BaseKind::Empty);
    assert!(env.ports().0.is_empty());
    assert_eq!(env.db_url(), None);
    assert!(env.list(&ws("."), ListOpts::default()).await.unwrap().entries.is_empty());
    let scratch = env.list(&EnvPath::scratch(".").unwrap(), ListOpts::default()).await.unwrap();
    assert!(scratch.entries.iter().any(|e| e.path.as_str() == "launch" && e.kind == EntryKind::Dir), "launch dir pre-created");
    let diff = env.diff_against_base(&DiffOptions { against: Baseline::Source, exclude: vec![] }).await;
    assert!(matches!(diff, Err(ExecError::Unsupported(_))), "{diff:?}");
    env.put_file(&EnvPath::scratch("harness/omp/config.yml").unwrap(), b"{}\n", FileAccess::AgentReadOnly).await.unwrap();
    env.make_dir(&EnvPath::scratch("harness/omp/agent").unwrap(), FileAccess::AgentCreateOnly).await.unwrap();
    env.make_dir(&EnvPath::scratch("harness/omp/agent").unwrap(), FileAccess::AgentCreateOnly).await.unwrap();
    let read = env.read(&EnvPath::scratch("harness/omp/config.yml").unwrap(), ReadRange::Whole { max_bytes: 10 }).await.unwrap();
    assert_eq!(read.bytes, b"{}\n");
    assert!(matches!(env.list(&EnvPath::scratch("harness/omp/config.yml").unwrap(), ListOpts::default()).await, Err(ExecError::Invalid(_))));
    assert!(matches!(env.list(&EnvPath::scratch("nope").unwrap(), ListOpts::default()).await, Err(ExecError::NotFound(_))));
    env.destroy().await.unwrap();
}

async fn destroy_is_idempotent(fx: &dyn ExecutorFixture) {
    let exec = fx.executor();
    let base = exec.prepare(request(fx, FILES, None).await, &NullObserver).await.unwrap();
    let env = exec.fork(&base, opts(ForkPurpose::Verify)).await.unwrap();
    env.destroy().await.unwrap();
    env.destroy().await.unwrap();
    assert!(matches!(env.read(&ws("README.md"), ReadRange::Whole { max_bytes: 1 }).await, Err(ExecError::Destroyed)));
    assert!(matches!(env.write(&ws("x"), b"x", WriteMode::Overwrite).await, Err(ExecError::Destroyed)));
    assert!(matches!(env.exec(ExecSpec::shell("true", ProcessRole::Exec), None).await, Err(ExecError::Destroyed)));
    assert!(matches!(env.diff_against_base(&DiffOptions { against: Baseline::Source, exclude: vec![] }).await, Err(ExecError::Destroyed)));
    assert!(matches!(env.start_services(ServiceSelector::All).await, Err(ExecError::Destroyed)));
    // The base survives and can be evicted once no fork references it.
    assert!(exec.base(&base.key).await.unwrap().is_some());
    exec.evict(&base.key).await.unwrap();
    assert!(exec.base(&base.key).await.unwrap().is_none());
}

// ---------------------------------------------------------------------------------------------
// Process cases
// ---------------------------------------------------------------------------------------------

async fn exec_exit_codes_stdin_and_cwd(fx: &dyn ExecutorFixture) {
    let exec = fx.executor();
    let base = exec.prepare(request(fx, FILES, None).await, &NullObserver).await.unwrap();
    let env = exec.fork(&base, opts(ForkPurpose::Session)).await.unwrap();
    assert_eq!(env.exec(ExecSpec::shell("exit 3", ProcessRole::Exec), None).await.unwrap().exit, Exit::Code(3));
    assert_eq!(env.exec(ExecSpec::shell("kill -9 $$", ProcessRole::Exec), None).await.unwrap().exit, Exit::Signal(9));
    let mut cat = ExecSpec::argv(vec!["cat".into()], ProcessRole::Exec);
    cat.stdin = Some(b"from stdin".to_vec());
    assert_eq!(env.exec(cat, None).await.unwrap().stdout.head, b"from stdin");
    let mut pwd = ExecSpec::shell("pwd; echo \"$FOO\"; test -n \"$MAGE_ENV_ID\" && echo ok", ProcessRole::Exec);
    pwd.cwd = ws("src");
    pwd.env = vec![("FOO".into(), "bar".into())];
    let out = env.exec(pwd, None).await.unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout.head).into_owned();
    let mut lines = stdout.lines();
    assert!(lines.next().is_some_and(|l| l.ends_with("/src")), "{stdout}");
    assert_eq!(lines.next(), Some("bar"));
    assert_eq!(lines.next(), Some("ok"));
    let LogSource::Exec(seq) = out.log else { panic!("exec log source") };
    let log = env.logs(LogQuery { source: LogSource::Exec(seq), tail: 10, grep: Some("^bar$".into()), since: None }).await.unwrap();
    assert_eq!(log.lines, vec!["bar".to_owned()]);
    env.destroy().await.unwrap();
}

async fn captured_boundaries(fx: &dyn ExecutorFixture) {
    let exec = fx.executor();
    let env = exec.empty_env(opts(ForkPurpose::Triage)).await.unwrap();
    let run = |n: usize| {
        let mut spec = ExecSpec::shell(format!("printf '%s' {}", "x".repeat(n)), ProcessRole::Exec);
        spec.capture = CaptureLimits { head: 8, tail: 4 };
        spec.cwd = EnvPath::scratch(".").unwrap();
        spec
    };
    let exact = env.exec(run(8), None).await.unwrap().stdout;
    assert_eq!((exact.head.len(), exact.tail.len(), exact.total_bytes), (8, 0, 8));
    let plus_one = env.exec(run(9), None).await.unwrap().stdout;
    assert_eq!((plus_one.head.len(), plus_one.tail.len(), plus_one.total_bytes), (8, 1, 9));
    let long = env.exec(run(100), None).await.unwrap().stdout;
    assert_eq!((long.head.len(), long.tail.len(), long.total_bytes), (8, 4, 100));
    env.destroy().await.unwrap();
}

async fn timeout_kills_double_forked_grandchild(fx: &dyn ExecutorFixture) {
    let exec = fx.executor();
    let env = exec.empty_env(opts(ForkPurpose::Triage)).await.unwrap();
    let mut spec = ExecSpec::shell("( sleep 300 & echo $! > gc.pid ) ; sleep 300", ProcessRole::Exec);
    spec.cwd = EnvPath::scratch(".").unwrap();
    spec.timeout = Duration::from_secs(1);
    let out = env.exec(spec, None).await.unwrap();
    assert_eq!(out.exit, Exit::TimedOut);
    assert!(out.duration < Duration::from_secs(30));
    let pid = env.read(&EnvPath::scratch("gc.pid").unwrap(), ReadRange::Whole { max_bytes: 64 }).await.unwrap();
    let pid = String::from_utf8_lossy(&pid.bytes).trim().to_owned();
    let mut probe = ExecSpec::shell(format!("kill -0 {pid} 2>/dev/null && echo alive || echo gone"), ProcessRole::Exec);
    probe.cwd = EnvPath::scratch(".").unwrap();
    let probe = env.exec(probe, None).await.unwrap();
    assert_eq!(String::from_utf8_lossy(&probe.stdout.head).trim(), "gone", "the double-forked grandchild died with the exec");
    env.destroy().await.unwrap();
}

async fn spawn_round_trip_and_terminate(fx: &dyn ExecutorFixture) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let exec = fx.executor();
    let env = exec.empty_env(opts(ForkPurpose::Triage)).await.unwrap();
    let mut child = env
        .spawn(SpawnSpec { argv: vec!["cat".into()], cwd: EnvPath::scratch("launch").unwrap(), env: vec![("PI_X".into(), "1".into())], role: ProcessRole::Harness })
        .await
        .unwrap();
    assert!(child.pid() > 0);
    let mut stdin = child.take_stdin().unwrap();
    let mut stdout = child.take_stdout().unwrap();
    stdin.write_all(b"ping\n").await.unwrap();
    let mut buf = [0u8; 5];
    stdout.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"ping\n");
    child.interrupt().unwrap();
    let exit = tokio::time::timeout(Duration::from_secs(10), child.wait()).await.unwrap().unwrap();
    assert_eq!(exit, Exit::Signal(2), "interrupt delivers SIGINT");
    let mut sleeper = env
        .spawn(SpawnSpec { argv: vec!["/bin/sh".into(), "-c".into(), "trap '' TERM; echo ready; sleep 300".into()], cwd: EnvPath::scratch(".").unwrap(), env: vec![], role: ProcessRole::Harness })
        .await
        .unwrap();
    let mut ready = [0u8; 6];
    sleeper.take_stdout().unwrap().read_exact(&mut ready).await.unwrap();
    assert_eq!(&ready, b"ready\n");
    let exit = sleeper.terminate(Duration::from_millis(300)).await.unwrap();
    assert_eq!(exit, Exit::Signal(9), "TERM ignored → KILL after grace");
    env.destroy().await.unwrap();
}

async fn destroy_kills_running_exec(fx: &dyn ExecutorFixture) {
    let exec = fx.executor();
    let env = exec.empty_env(opts(ForkPurpose::Triage)).await.unwrap();
    let mut spec = ExecSpec::shell("sleep 300", ProcessRole::Exec);
    spec.cwd = EnvPath::scratch(".").unwrap();
    let runner = Arc::clone(&env);
    let running = tokio::spawn(async move { runner.exec(spec, None).await });
    tokio::time::sleep(Duration::from_millis(300)).await;
    env.destroy().await.unwrap();
    let out = tokio::time::timeout(Duration::from_secs(30), running).await.unwrap().unwrap().unwrap();
    assert_eq!(out.exit, Exit::Killed(KillReason::Destroyed));
}

async fn check_patch_source_vs_build_rewritten_file(fx: &dyn ExecutorFixture) {
    let exec = fx.executor();
    let recipe = RecipeSpec::parse(
        "version = 1\n[[build]]\nname = \"lock\"\nrun = \"printf 'v2\\\\n' > lock.txt\"\n[test]\nrun = \"true\"\n",
    )
    .unwrap();
    let files: &[(&str, &[u8])] = &[("lock.txt", b"v1\n"), ("app.txt", b"one\n")];
    let base = exec.prepare(request(fx, files, Some(recipe)).await, &NullObserver).await.unwrap();
    let env = exec.fork(&base, opts(ForkPurpose::Session)).await.unwrap();
    assert_eq!(text(env.as_ref(), "lock.txt").await.as_deref(), Some("v2\n"), "the build rewrote the file");
    assert!(env.diff_against_base(&DiffOptions { against: Baseline::Prepared, exclude: vec![] }).await.unwrap().patch.is_empty(), "prepared baseline absorbs build rewrites");
    env.write(&ws("lock.txt"), b"v3\n", WriteMode::Overwrite).await.unwrap();
    let lock_patch = env.diff_against_base(&DiffOptions { against: Baseline::Prepared, exclude: vec![] }).await.unwrap().patch;
    let on_source = env.check_patch(&lock_patch, Baseline::Source).await.unwrap();
    assert!(matches!(on_source, ApplyOutcome::Rejected { reason: RejectReason::DoesNotApply, .. }), "{on_source:?}");
    assert!(matches!(env.check_patch(&lock_patch, Baseline::Prepared).await.unwrap(), ApplyOutcome::Applied { .. }));
    let other = exec.fork(&base, opts(ForkPurpose::Verify)).await.unwrap();
    other.write(&ws("app.txt"), b"two\n", WriteMode::Overwrite).await.unwrap();
    let app_patch = other.diff_against_base(&DiffOptions { against: Baseline::Prepared, exclude: vec![] }).await.unwrap().patch;
    assert!(matches!(env.check_patch(&app_patch, Baseline::Source).await.unwrap(), ApplyOutcome::Applied { .. }));
    assert_eq!(text(env.as_ref(), "app.txt").await.as_deref(), Some("one\n"), "check_patch never touches the work tree");
    for e in [env, other] {
        e.destroy().await.unwrap();
    }
}

// ---------------------------------------------------------------------------------------------
// Database cases
// ---------------------------------------------------------------------------------------------

async fn db_fork_isolation_and_reset(fx: &dyn ExecutorFixture) {
    let exec = fx.executor();
    let recipe = RecipeSpec::parse(
        "version = 1\n[database]\nextensions = [\"pgcrypto\"]\n[test]\nrun = \"true\"\n",
    )
    .unwrap();
    let base = exec.prepare(request(fx, &[("db.txt", b"db\n")], Some(recipe)).await, &NullObserver).await.unwrap();
    let a = exec.fork(&base, opts(ForkPurpose::Verify)).await.unwrap();
    let b = exec.fork(&base, opts(ForkPurpose::Verify)).await.unwrap();
    assert!(a.db_url().is_some_and(|u| u.starts_with("postgres://mf_")));
    assert_ne!(a.db_url(), b.db_url());
    let q = |sql: &str, write: bool| DbQuery { sql: sql.to_owned(), params: vec![], write, max_rows: 10, timeout: Duration::from_secs(5) };
    a.db_query(q("CREATE TABLE t (id uuid DEFAULT gen_random_uuid(), v int)", true)).await.unwrap();
    a.db_query(q("INSERT INTO t (v) VALUES (1)", true)).await.unwrap();
    let rows = a.db_query(q("SELECT v FROM t", false)).await.unwrap();
    assert_eq!(rows.rows, vec![vec![DbValue::Int(1)]]);
    let in_b = b.db_query(q("SELECT to_regclass('t')::text AS t", false)).await.unwrap();
    assert_eq!(in_b.rows, vec![vec![DbValue::Null]], "row inserted in A absent in B");
    assert!(a.db_query(q("INSERT INTO t (v) VALUES (2)", false)).await.is_err(), "read-only unless write");
    a.reset_db().await.unwrap();
    let after = a.db_query(q("SELECT to_regclass('t')::text AS t", false)).await.unwrap();
    assert_eq!(after.rows, vec![vec![DbValue::Null]], "reset_db restores the template state");
    for env in [a, b] {
        env.destroy().await.unwrap();
    }
}
