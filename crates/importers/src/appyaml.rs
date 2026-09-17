//! Google App Engine `app.yaml`.

use std::path::Path;

use serde_json::json;

use crate::yaml;
use crate::{DEFAULT_BUILDER, Fact, Findings, read, shell, slug};

const NAME: &str = "app.yaml";
/// App Engine sends requests to `$PORT`, which it always sets to 8080.
const PORT: u16 = 8080;

pub(crate) fn import(root: &Path) -> Option<Findings> {
    let text = match read(root, NAME)? {
        Ok(t) => t,
        Err(e) => return Some(Findings::failed(NAME, e)),
    };
    let doc = match yaml::parse(&text) {
        Ok(d) => d,
        Err(e) => return Some(Findings::failed(NAME, format!("{NAME}: {e}"))),
    };
    // Other tools use the name too; App Engine's always has a runtime.
    let runtime = doc.get("runtime")?;
    let rt = runtime.str()?;
    let mut f = Findings::new(NAME);
    f.build = Some(if rt == "custom" {
        Fact::new(
            json!({"kind": "dockerfile", "path": "Dockerfile", "context": "."}),
            NAME,
            runtime.line,
            "runtime: custom builds ./Dockerfile",
        )
    } else {
        Fact::new(json!({"kind": "buildpack", "builder": DEFAULT_BUILDER}), NAME, runtime.line, format!("runtime: {rt}"))
    });
    let p = f.primary_mut();
    if let Some(service) = doc.get("service").and_then(|s| s.str().map(|v| (v, s.line)))
        && service.0 != "default"
        && let Some(name) = slug(service.0, 30)
    {
        p.name = Some(Fact::new(json!(name), NAME, service.1, format!("service: {}", service.0)));
    }
    if let Some(ep) = doc.get("entrypoint").and_then(|e| e.str().map(|v| (v, e.line))) {
        p.command = Some(Fact::new(json!(shell::argv(ep.0)), NAME, ep.1, ep.0.to_owned()));
    }
    p.port = Some(Fact::new(json!(PORT), NAME, runtime.line, format!("App Engine routes requests to $PORT = {PORT}")));
    p.expose = Some(Fact::new(json!("public"), NAME, runtime.line, "App Engine services are public"));
    let check = ["readiness_check", "liveness_check"].iter().find_map(|k| doc.get(k)?.get("path"));
    if let Some(path) = check.and_then(|n| n.str().map(|s| (s, n.line))) {
        p.health = Some(Fact::new(json!({"kind": "http", "path": path.0}), NAME, path.1, format!("health check path {}", path.0)));
    }
    Some(f)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_yaml_without_runtime_is_someone_elses() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(NAME), "services:\n  - name: web\n").unwrap();
        assert!(import(dir.path()).is_none());
    }
}
