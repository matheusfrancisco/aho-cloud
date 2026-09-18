//! Each importer on its fixture directory, and the precedence between them.

use std::path::{Path, PathBuf};

use cloud_contract::draft::{Draft, Strength};
use serde_json::{Value, json};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
}

/// (value, evidence URIs) at `pointer`; panics with the draft when absent.
fn at(d: &Draft, pointer: &str) -> (Value, Vec<String>) {
    let f = d.fields.get(pointer).unwrap_or_else(|| panic!("no {pointer} in {:#?}", d.fields.keys().collect::<Vec<_>>()));
    assert_eq!(f.strength, Strength::Inferred, "{pointer}");
    assert!(!f.confirmed, "{pointer} must wait for a human");
    (f.value.clone(), f.evidence.iter().map(|e| e.uri.clone()).collect())
}

fn value(d: &Draft, pointer: &str) -> Value {
    at(d, pointer).0
}

#[test]
fn dockerfile_final_stage() {
    let d = cloud_importers::dockerfile(&fixture("dockerfile"), "Dockerfile").unwrap().draft();
    assert_eq!(at(&d, "/build"), (json!({"kind": "dockerfile", "path": "Dockerfile", "context": "."}), vec!["file:Dockerfile:8".into()]));
    assert_eq!(at(&d, "/processes/0/port"), (json!(8080), vec!["file:Dockerfile:12".into()]));
    assert_eq!(value(&d, "/processes/0/expose"), json!("public"));
    assert_eq!(at(&d, "/processes/0/command"), (json!(["tini", "--", "node", "dist/server.js"]), vec!["file:Dockerfile:15".into()]));
    // curl against the exposed port is an HTTP check on the process port.
    assert_eq!(at(&d, "/processes/0/health"), (json!({"kind": "http", "path": "/healthz"}), vec!["file:Dockerfile:13".into()]));
    assert_eq!(value(&d, "/processes/0/name"), json!("web"));
}

#[test]
fn procfile_processes() {
    let d = cloud_importers::procfile(&fixture("procfile")).unwrap().draft();
    assert_eq!(at(&d, "/processes/0/name"), (json!("web"), vec!["file:Procfile:2".into()]));
    assert_eq!(value(&d, "/processes/0/command"), json!(["gunicorn", "app:app", "--bind", "0.0.0.0:8000"]));
    assert_eq!(value(&d, "/processes/0/port"), json!(8000));
    assert_eq!(value(&d, "/processes/0/expose"), json!("public"));
    assert_eq!(at(&d, "/processes/1/name"), (json!("worker"), vec!["file:Procfile:3".into()]));
    assert!(!d.fields.contains_key("/processes/1/expose"), "only web is public");
    // `release` is a release-phase task; `clock` needs a shell for `$INTERVAL`.
    assert_eq!(value(&d, "/processes/2/name"), json!("clock"));
    assert_eq!(value(&d, "/processes/2/command"), json!(["/bin/sh", "-c", "python clock.py $INTERVAL"]));
    assert!(!d.fields.contains_key("/processes/3/name"));
}

#[test]
fn compose_services_built_from_the_repo() {
    let d = cloud_importers::compose(&fixture("compose")).unwrap().draft();
    // `api` inherits `build: .` through a merge key; the anchor's line is the evidence.
    assert_eq!(
        at(&d, "/build"),
        (json!({"kind": "dockerfile", "path": "Dockerfile", "context": "."}), vec!["file:docker-compose.yml:3".into()])
    );
    assert_eq!(at(&d, "/processes/0/name"), (json!("api"), vec!["file:docker-compose.yml:6".into()]));
    assert_eq!(value(&d, "/processes/0/command"), json!(["./bin/api", "--listen", ":3000"]));
    assert_eq!(at(&d, "/processes/0/port"), (json!(3000), vec!["file:docker-compose.yml:10".into()]));
    assert_eq!(value(&d, "/processes/0/expose"), json!("public"));
    assert_eq!(at(&d, "/processes/0/health"), (json!({"kind": "http", "path": "/ready"}), vec!["file:docker-compose.yml:12".into()]));
    assert_eq!(value(&d, "/processes/1/name"), json!("jobs"));
    assert_eq!(value(&d, "/processes/1/port"), json!(9100));
    assert!(!d.fields.contains_key("/processes/1/expose"), "expose without ports stays internal");
    // `db` is an image dependency and `vendored` builds outside the repo.
    assert!(!d.fields.contains_key("/processes/2/name"));
}

#[test]
fn fly_toml() {
    let d = cloud_importers::fly(&fixture("fly")).unwrap().draft();
    assert_eq!(at(&d, "/project"), (json!("acme-shop"), vec!["file:fly.toml:1".into()]));
    assert_eq!(at(&d, "/build"), (json!({"kind": "dockerfile", "path": "Dockerfile.fly", "context": "."}), vec!["file:fly.toml:5".into()]));
    assert_eq!(at(&d, "/processes/0/name"), (json!("app"), vec!["file:fly.toml:8".into()]));
    assert_eq!(value(&d, "/processes/0/command"), json!(["bin/server", "--port", "8080"]));
    assert_eq!(at(&d, "/processes/0/port"), (json!(8080), vec!["file:fly.toml:12".into()]));
    assert_eq!(value(&d, "/processes/0/expose"), json!("public"));
    assert_eq!(at(&d, "/processes/0/health"), (json!({"kind": "http", "path": "/healthz"}), vec!["file:fly.toml:19".into()]));
    assert_eq!(at(&d, "/processes/0/volume"), (json!({"mount": "/data", "size_mb": 3072}), vec!["file:fly.toml:23".into()]));
    assert_eq!(value(&d, "/processes/1/name"), json!("worker"));
    assert_eq!(value(&d, "/processes/1/command"), json!(["bin/worker", "--concurrency", "4"]));
}

#[test]
fn render_yaml() {
    let d = cloud_importers::render(&fixture("render")).unwrap().draft();
    assert_eq!(at(&d, "/processes/0/name"), (json!("storefront"), vec!["file:render.yaml:3".into()]));
    assert_eq!(
        at(&d, "/build"),
        (json!({"kind": "dockerfile", "path": "docker/Dockerfile", "context": "."}), vec!["file:render.yaml:4".into()])
    );
    assert_eq!(at(&d, "/processes/0/port"), (json!(9000), vec!["file:render.yaml:10".into()]));
    assert_eq!(value(&d, "/processes/0/expose"), json!("public"));
    assert_eq!(at(&d, "/processes/0/health"), (json!({"kind": "http", "path": "/health"}), vec!["file:render.yaml:7".into()]));
    assert_eq!(at(&d, "/processes/0/volume"), (json!({"mount": "/var/uploads", "size_mb": 2048}), vec!["file:render.yaml:15".into()]));
    assert_eq!(at(&d, "/secrets/0/name"), (json!("STRIPE_SECRET_KEY"), vec!["file:render.yaml:11".into()]));
    assert!(!d.fields.contains_key("/secrets/1/name"), "values set in the blueprint are not secrets");
    assert_eq!(at(&d, "/domains/0/hostname"), (json!("shop.example.com"), vec!["file:render.yaml:20".into()]));
    assert_eq!(value(&d, "/processes/1/name"), json!("mailer"));
    assert_eq!(value(&d, "/processes/1/command"), json!(["./mailer"]));
    assert!(!d.fields.contains_key("/processes/1/port"), "workers take no traffic");
    assert!(!d.fields.contains_key("/processes/2/name"), "static sites are not processes");
}

#[test]
fn app_engine_yaml() {
    let d = cloud_importers::app_yaml(&fixture("appengine")).unwrap().draft();
    assert_eq!(
        at(&d, "/build"),
        (json!({"kind": "buildpack", "builder": cloud_importers::DEFAULT_BUILDER}), vec!["file:app.yaml:1".into()])
    );
    assert_eq!(at(&d, "/processes/0/name"), (json!("checkout"), vec!["file:app.yaml:2".into()]));
    assert_eq!(value(&d, "/processes/0/command"), json!(["/bin/sh", "-c", "gunicorn -b :$PORT main:app"]));
    assert_eq!(value(&d, "/processes/0/port"), json!(8080));
    assert_eq!(at(&d, "/processes/0/health"), (json!({"kind": "http", "path": "/ready"}), vec!["file:app.yaml:5".into()]));
}

#[test]
fn package_json_without_and_with_a_dockerfile() {
    let d = cloud_importers::package_json(&fixture("node")).unwrap().draft();
    assert_eq!(at(&d, "/project"), (json!("web-app"), vec!["file:package.json:2".into()]));
    assert_eq!(at(&d, "/processes/0/command"), (json!(["npm", "start"]), vec!["file:package.json:6".into()]));
    assert_eq!(value(&d, "/processes/0/port"), json!(3000));
    assert_eq!(value(&d, "/build"), json!({"kind": "buildpack", "builder": cloud_importers::DEFAULT_BUILDER}));

    let only_npm = cloud_importers::package_json(&fixture("node-docker")).unwrap().draft();
    assert!(!only_npm.fields.contains_key("/build"), "a Dockerfile means no buildpack guess");
    let d = cloud_importers::import_dir(&fixture("node-docker"));
    assert_eq!(value(&d, "/build")["kind"], "dockerfile");
    assert_eq!(value(&d, "/processes/0/command"), json!(["node", "server.js"]), "Dockerfile outranks package.json");
}

#[test]
fn python_project() {
    let d = cloud_importers::python(&fixture("python")).unwrap().draft();
    assert_eq!(at(&d, "/project"), (json!("billing-api"), vec!["file:pyproject.toml:5".into()]));
    assert_eq!(
        at(&d, "/build"),
        (json!({"kind": "buildpack", "builder": cloud_importers::DEFAULT_BUILDER}), vec!["file:pyproject.toml:1".into()])
    );
}

#[test]
fn env_example_names_secrets_but_never_reads_values() {
    let f = cloud_importers::env_example(&fixture("envexample")).unwrap();
    let d = f.draft();
    assert_eq!(at(&d, "/secrets/0/name"), (json!("DATABASE_URL"), vec!["file:.env.example:2".into()]));
    assert_eq!(at(&d, "/secrets/1/name"), (json!("STRIPE_KEY"), vec!["file:.env.example:3".into()]));
    assert!(!d.fields.contains_key("/secrets/2/name"), "NODE_ENV is configuration and STRIPE_KEY is listed once");
    assert_eq!(at(&d, "/processes/0/port"), (json!(4000), vec!["file:.env.example:5".into()]));
    assert_eq!(f.problems.len(), 1, "{:?}", f.problems);
    assert!(f.problems[0].starts_with(".env.example:6:"), "{:?}", f.problems);
    let all = serde_json::to_string(&d).unwrap();
    assert!(!all.contains("sk_test") && !all.contains("postgres://"), "values leaked into the draft: {all}");
}

#[test]
fn precedence_between_importers() {
    let scan = cloud_importers::scan(&fixture("precedence"));
    assert_eq!(scan.found, ["fly.toml", "Procfile", "Dockerfile", "docker-compose", "package.json", ".env.example"]);
    let d = &scan.draft;
    // fly.toml > package.json
    assert_eq!(at(d, "/project"), (json!("prec"), vec!["file:fly.toml:1".into()]));
    // fly names the HTTP process group `app`; every importer's primary process merges into it.
    assert_eq!(value(d, "/processes/0/name"), json!("app"));
    // fly.toml > Procfile > Dockerfile > compose > .env.example
    assert_eq!(at(d, "/processes/0/port"), (json!(9090), vec!["file:fly.toml:4".into()]));
    // Procfile > Dockerfile > package.json
    assert_eq!(at(d, "/processes/0/command"), (json!(["./server", "--port", "7070"]), vec!["file:Procfile:1".into()]));
    // Only the Dockerfile has a health check.
    assert_eq!(value(d, "/processes/0/health"), json!({"kind": "command", "argv": ["/bin/check"]}));
    // Dockerfile and compose agree on the build: one value, both pieces of evidence.
    assert_eq!(
        at(d, "/build"),
        (
            json!({"kind": "dockerfile", "path": "Dockerfile", "context": "."}),
            vec!["file:docker-compose.yml:3".into(), "file:Dockerfile:1".into()]
        )
    );
    // Procfile's worker > compose's worker.
    assert_eq!(value(d, "/processes/1/name"), json!("worker"));
    assert_eq!(at(d, "/processes/1/command"), (json!(["./worker"]), vec!["file:Procfile:2".into()]));
    assert!(!d.fields.contains_key("/processes/2/name"));
    assert_eq!(value(d, "/secrets/0/name"), json!("API_TOKEN"));
    // Deterministic: same input, same draft.
    assert_eq!(&cloud_importers::import_dir(&fixture("precedence")), d);
}

#[test]
fn unreadable_files_are_reported_not_guessed() {
    let scan = cloud_importers::scan(&fixture("broken"));
    assert!(scan.draft.fields.is_empty(), "{:?}", scan.draft.fields);
    assert_eq!(scan.problems.len(), 2, "{:?}", scan.problems);
    assert!(scan.problems.iter().any(|p| p.starts_with("fly.toml:")));
    assert!(scan.problems.iter().any(|p| p.starts_with("package.json:")));
}

#[test]
fn nothing_to_import() {
    let dir = tempfile::tempdir().unwrap();
    let scan = cloud_importers::scan(dir.path());
    assert!(scan.found.is_empty() && scan.draft.fields.is_empty());
}
