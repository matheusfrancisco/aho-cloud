//! edge_routes and edge_verifier through a planned composition, with the workspace served by an
//! in-memory filesystem.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration-test helpers may panic on setup failure

use std::collections::BTreeMap;
use std::sync::Arc;

use aho_cap::*;
use aho_config::{ResolveInputs, Schema, resolve};
use aho_fs::{AppliedDiff, FileVersion, Fs, FsError, FsRead, GrepHit, ReadResult, Stat};
use aho_log::CapState;
use aho_types::*;
use cap_edge_contract::{EdgeContract, Framework, SECTION, VerifierParams, render_verifier};
use cloud_contract::ledger::Stage;
use cloud_contract::principal;
use serde_json::json;

/// The workspace: read-only files by path.
struct MemFs(BTreeMap<String, String>);

fn denied<T: Send + 'static>() -> BoxFuture<'static, Result<T, FsError>> {
    Box::pin(async { Err(FsError::Denied("read-only test workspace".into())) })
}

impl Fs for MemFs {
    fn root(&self) -> String {
        "/workspace".into()
    }
    fn read(&self, path: &str, _: Option<(u32, u32)>) -> BoxFuture<'_, Result<ReadResult, FsError>> {
        let r = self.0.get(path).map(|t| ReadResult {
            text: t.clone(),
            version: FileVersion::of(t.as_bytes()),
            total_lines: t.lines().count() as u32,
            partial: false,
        });
        let path = path.to_owned();
        Box::pin(async move { r.ok_or(FsError::NotFound(path)) })
    }
    fn read_bytes(&self, path: &str) -> BoxFuture<'_, Result<Vec<u8>, FsError>> {
        let r = self.0.get(path).map(|t| t.clone().into_bytes()).ok_or_else(|| FsError::NotFound(path.to_owned()));
        Box::pin(async move { r })
    }
    fn stat(&self, _: &str) -> BoxFuture<'_, Result<Stat, FsError>> {
        denied()
    }
    fn write(&self, _: &str, _: &str, _: Option<FileVersion>) -> BoxFuture<'_, Result<AppliedDiff, FsError>> {
        denied()
    }
    fn edit(&self, _: &str, _: &str, _: &str, _: bool, _: FileVersion) -> BoxFuture<'_, Result<AppliedDiff, FsError>> {
        denied()
    }
    fn glob(&self, _: &str, _: Option<&str>) -> BoxFuture<'_, Result<Vec<String>, FsError>> {
        denied()
    }
    fn grep(&self, _: &str, _: Option<&str>, _: Option<&str>) -> BoxFuture<'_, Result<Vec<GrepHit>, FsError>> {
        denied()
    }
    fn list(&self, _: &str) -> BoxFuture<'_, Result<Vec<Stat>, FsError>> {
        denied()
    }
    fn remove(&self, _: &str) -> BoxFuture<'_, Result<(), FsError>> {
        denied()
    }
}

/// Binds the fs seam the way a world does; the session seeds the handle itself.
struct TestWorld;

impl Capability for TestWorld {
    const ID: CapabilityId = Self::ID_;
    const MANIFEST: &'static Manifest = &Self::MANIFEST_;
    type Deps = ();
    type Config = ();
    fn build(_: (), _: ()) -> Result<Self, BuildError> {
        Ok(TestWorld)
    }
    fn register(&self, _: &mut Registrar<'_>) {}
}

declare_capability!(
    TestWorld,
    "test-world",
    kind = Kind::Provider,
    provides = [aho_fs::SEAM],
    requires = [],
    tools = &[],
    provides_handles = [HandleKind::FsRead]
);

fn start(files: &[(&str, &str)], sets: &[(&str, &str)]) -> Registry {
    let comp = Composition::new("t").provide::<TestWorld>().consume::<EdgeContract>();
    let mut schema = Schema::default();
    for k in comp.config_keys() {
        schema.add(k);
    }
    let mut i = ResolveInputs::default();
    i.sets = sets.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    let l = resolve(&i, &schema, false).unwrap();
    let policy = PolicyInput { preset: "read-only".into(), allowed: ConsequenceSet::of(&[Consequence::ReadOnly]) };
    let p = plan(&comp.manifests(), &l, &Env::empty(Os::current()), &policy, vec![]).unwrap();
    assert!(matches!(p.row("cloud-edge-contract").unwrap().state, CapState::Active));
    let mut seed = Registry::default();
    let fs = MemFs(files.iter().map(|(p, t)| (p.to_string(), t.to_string())).collect());
    seed.handles.insert(HandleKind::FsRead, (TestWorld::ID, Arc::new(FsRead(Arc::new(fs)))));
    let (reg, broken) = comp.start_seeded(&p, &l, seed);
    assert!(broken.is_empty(), "{broken:?}");
    reg
}

async fn call(reg: &Registry, tool: &str, args: Value) -> ExecOutcome {
    let (_, exec) = reg.tool(tool).unwrap();
    let view = ExecView {
        call: CallId("e1".into()),
        tool: tool.into(),
        class: Consequence::ReadOnly,
        args,
        policy: PolicyView {
            epoch: Epoch(1),
            allowed: ConsequenceSet::ALL,
            world: BoundWorld::host(),
            workspace: String::new(),
            home: String::new(),
        },
        by: Principal::Agent { id: AgentId(1), turn: TurnId(1) },
    };
    exec(ExecRequest { view, cancel: CancelToken::new(), progress: None }).await
}

fn done(o: ExecOutcome) -> (Value, String) {
    match o {
        ExecOutcome::Done { value, content, .. } => (value, content.iter().filter_map(|c| c.as_text()).collect()),
        other => panic!("expected Done, got {other:?}"),
    }
}

fn err_code(o: ExecOutcome) -> String {
    match o {
        ExecOutcome::Err { code, .. } => code,
        other => panic!("expected Err, got {other:?}"),
    }
}

const EDGE_TOML: &str = r#"
default = "edge"

[[route]]
path = "/webhooks/stripe"
mode = "public"
verify = { kind = "stripe", secret = "STRIPE_WEBHOOK_SECRET" }

[[route]]
path = "/api/**"
mode = "bearer"
"#;

fn manifest(auth: Value) -> Value {
    json!({
        "api_version": "deploy.aho.cloud/v1", "project": "shop",
        "source": {"kind": "archive", "hash": format!("blake3:{}", "a".repeat(64))}, "build": {"kind": "dockerfile"},
        "processes": [{"name": "web", "port": 8080, "expose": "public", "health": {"kind": "http", "path": "/"}}],
        "auth": auth, "data_class": "none", "owner": {"name": "Ana", "email": "ana@shop.io"},
        "budget": {"ceiling_usd_month": 20, "degrade": "sleep"}
    })
}

#[test]
fn the_prompt_section_names_the_header_and_jwks_path_the_edge_uses() {
    let reg = start(&[], &[]);
    let (_, _, _, _, section) = reg.sections.iter().find(|s| s.2 == SECTION).unwrap();
    let text = section(&aho_log::State::default());
    assert!(text.contains(principal::HEADER) && text.contains(principal::JWKS_PATH), "{text}");
}

#[tokio::test]
async fn edge_routes_resolves_paths_like_the_edge_does() {
    let reg = start(&[("edge.toml", EDGE_TOML)], &[]);
    let (v, text) = done(call(&reg, "edge_routes", json!({"paths": ["/webhooks/stripe", "/api/v1/users?page=2", "/dashboard"]})).await);
    assert_eq!(v["ok"], true);
    let got: Vec<(&str, Value)> =
        v["resolved"].as_array().unwrap().iter().map(|r| (r["mode"].as_str().unwrap(), r["route"].clone())).collect();
    assert_eq!(got, vec![("public", json!(0)), ("bearer", json!(1)), ("edge", Value::Null)]);
    assert_eq!(v["resolved"][0]["verify"], json!({"kind": "stripe", "secret": "STRIPE_WEBHOOK_SECRET"}));
    assert_eq!(v["resolved"][2]["principal"], true);
    assert!(text.contains("/dashboard → edge (the default); the app receives x-principal-jwt"), "{text}");

    // cross-checked against a manifest whose auth has no edge login and no webhook secret
    let (v, _) = done(call(&reg, "edge_routes", json!({"manifest": manifest(json!({"mode": "none"}))})).await);
    assert_eq!(v["ok"], false);
    let errors: Vec<&str> = v["errors"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
    assert!(errors.iter().any(|e| e.contains("needs an edge login")), "{errors:?}");
    assert!(errors.iter().any(|e| e.contains("STRIPE_WEBHOOK_SECRET")), "{errors:?}");
}

#[tokio::test]
async fn edge_routes_reports_every_kind_of_error() {
    let bad_verify = "[[route]]\npath = \"/hooks\"\nmode = \"bearer\"\nverify = { kind = \"github\", secret = \"GH\" }\n";
    let relative = "[[route]]\npath = \"api/**\"\nmode = \"public\"\n";
    let reg = start(&[("edge.toml", bad_verify), ("relative.toml", relative), ("broken.toml", "default = [")], &[]);
    for (file, needle) in
        [("edge.toml", "only valid with mode = \"public\""), ("relative.toml", "start with `/`"), ("broken.toml", "edge.toml:")]
    {
        let (v, text) = done(call(&reg, "edge_routes", json!({"file": file, "paths": ["/hooks"]})).await);
        assert_eq!(v["ok"], false, "{file}");
        assert!(v["errors"][0].as_str().unwrap().contains(needle), "{file}: {}", v["errors"]);
        assert!(v["resolved"].as_array().unwrap().is_empty(), "{file}: nothing resolves against routes that do not compile");
        assert!(text.contains("Errors:"), "{text}");
    }

    let reg = start(&[], &[]);
    assert_eq!(err_code(call(&reg, "edge_routes", json!({})).await), "FS_NOT_FOUND");
    let (v, _) = done(call(&reg, "edge_routes", json!({"paths": ["/x"], "manifest": manifest(json!({"mode": "magic_link"}))})).await);
    assert_eq!(
        (v["implied"].clone(), v["resolved"][0]["mode"].clone()),
        (json!(true), json!("edge")),
        "no edge.toml: the manifest's auth decides"
    );
    assert_eq!(err_code(call(&reg, "edge_routes", json!({"paths": "/x"})).await), "BAD_ARGS");
}

#[tokio::test]
async fn edge_verifier_renders_each_framework_for_one_project_and_stage() {
    let reg = start(&[], &[("cloud.project", "shop")]);
    for f in Framework::ALL {
        let (v, text) =
            done(call(&reg, "edge_verifier", json!({"framework": f.name(), "stage": "staging", "hostname": "shop.example.io"})).await);
        let snippet = v["snippet"].as_str().unwrap();
        for needle in ["edge/p/shop", "shop/staging", "https://shop.example.io/.edge/jwks.json", principal::HEADER, "ES256"] {
            assert!(snippet.contains(needle), "{}: missing {needle}", f.name());
        }
        assert!(!snippet.contains("{{"), "{}: every placeholder is filled", f.name());
        assert!(text.contains(v["file"].as_str().unwrap()) && text.contains(snippet), "{}", f.name());
    }
    let (v, _) = done(
        call(&reg, "edge_verifier", json!({"framework": "axum", "project": "blog", "jwks_url": "http://localhost:3000/.edge/jwks.json"}))
            .await,
    );
    assert_eq!(
        (v["issuer"].as_str(), v["audience"].as_str()),
        (Some("edge/p/blog"), Some("blog/live")),
        "an explicit project beats cloud.project; live by default"
    );

    for args in [
        json!({"framework": "rails", "hostname": "shop.example.io"}),
        json!({"framework": "go"}),
        json!({"framework": "go", "hostname": "not a host"}),
        json!({"framework": "go", "jwks_url": "http://shop.example.io/.edge/jwks.json"}),
        json!({"framework": "go", "hostname": "shop.example.io", "stage": "prod"}),
        json!({"framework": "go", "hostname": "shop.example.io", "project": "Shop!"}),
    ] {
        assert_eq!(err_code(call(&reg, "edge_verifier", args.clone()).await), "BAD_ARGS", "{args}");
    }
    let no_project = start(&[], &[]);
    assert_eq!(err_code(call(&no_project, "edge_verifier", json!({"framework": "go", "hostname": "shop.example.io"})).await), "BAD_ARGS");
}

#[test]
fn rendered_verifiers_enforce_the_contract_lifetime_and_leeway() {
    let p = VerifierParams { project: "shop".into(), stage: Stage::Preview, jwks_url: "https://shop.example.io/.edge/jwks.json".into() };
    let py = render_verifier(Framework::Fastapi, &p);
    for line in [
        "ISSUER = \"edge/p/shop\"".to_string(),
        "AUDIENCE = \"shop/preview\"".to_string(),
        format!("MAX_LIFETIME_SECS = {}", principal::TTL_SECS),
        format!("LEEWAY_SECS = {}", principal::LEEWAY_SECS),
    ] {
        assert!(py.lines().any(|l| l == line), "missing `{line}`");
    }
}
