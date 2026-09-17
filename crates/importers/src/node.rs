//! package.json: the project name, `scripts.start`, and a buildpack build without a Dockerfile.

use std::path::Path;

use serde_json::{Value, json};

use crate::jsonpos::key_line;
use crate::{DEFAULT_BUILDER, Fact, Findings, read, shell, slug};

const NAME: &str = "package.json";

pub(crate) fn import(root: &Path) -> Option<Findings> {
    let text = match read(root, NAME)? {
        Ok(t) => t,
        Err(e) => return Some(Findings::failed(NAME, e)),
    };
    let pkg: Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => return Some(Findings::failed(NAME, format!("{NAME}:{}: {e}", e.line()))),
    };
    let mut f = Findings::new(NAME);
    let line = |path: &[&str]| key_line(&text, path).unwrap_or(1);
    if let Some(name) = pkg["name"].as_str() {
        match slug(name, 40) {
            Some(s) => f.project = Some(Fact::new(json!(s), NAME, line(&["name"]), format!("\"name\": \"{name}\""))),
            None => f.problems.push(format!("{NAME}:{}: name `{name}` cannot be a project slug", line(&["name"]))),
        }
    }
    if let Some(start) = pkg["scripts"]["start"].as_str() {
        let at = line(&["scripts", "start"]);
        let p = f.primary_mut();
        p.command = Some(Fact::new(json!(["npm", "start"]), NAME, at, format!("scripts.start: {start}")));
        if let Some(port) = shell::port(start) {
            p.port = Some(Fact::new(json!(port), NAME, at, format!("`{start}` binds {port}")));
        }
    }
    if !root.join("Dockerfile").is_file() {
        let v = json!({"kind": "buildpack", "builder": DEFAULT_BUILDER});
        f.build = Some(Fact::new(v, NAME, 1, "Node.js project without a Dockerfile"));
    }
    Some(f)
}
