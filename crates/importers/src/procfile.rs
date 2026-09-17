//! Procfile: `<type>: <command>` per line; `web` receives HTTP.

use std::path::Path;

use serde_json::json;

use crate::{Fact, Findings, ProcessFacts, read, shell, slug};

const NAME: &str = "Procfile";

pub(crate) fn import(root: &Path) -> Option<Findings> {
    let text = match read(root, NAME)? {
        Ok(t) => t,
        Err(e) => return Some(Findings::failed(NAME, e)),
    };
    let mut f = Findings::new(NAME);
    for (i, raw) in text.lines().enumerate() {
        let line = i + 1;
        let t = raw.trim();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        let Some((kind, cmd)) = t.split_once(':') else {
            f.problems.push(format!("{NAME}:{line}: not `<type>: <command>`"));
            continue;
        };
        let (kind, cmd) = (kind.trim(), cmd.trim());
        if kind == "release" {
            // Heroku's release phase runs once per deploy; it is not a long-running process.
            continue;
        }
        let Some(name) = slug(kind, 30) else {
            f.problems.push(format!("{NAME}:{line}: `{kind}` cannot be a process name"));
            continue;
        };
        if cmd.is_empty() {
            f.problems.push(format!("{NAME}:{line}: `{kind}` has no command"));
            continue;
        }
        let mut p = ProcessFacts {
            name: Some(Fact::new(json!(name), NAME, line, format!("Procfile process `{kind}`"))),
            command: Some(Fact::new(json!(shell::argv(cmd)), NAME, line, cmd.to_owned())),
            ..Default::default()
        };
        if let Some(port) = shell::port(cmd) {
            p.port = Some(Fact::new(json!(port), NAME, line, format!("`{cmd}` binds {port}")));
        }
        if kind == "web" {
            p.expose = Some(Fact::new(json!("public"), NAME, line, "`web` is the process that receives HTTP"));
            f.primary = Some(p);
        } else if !f.named.iter().any(|(n, _)| *n == name) {
            f.named.push((name, p));
        }
    }
    Some(f)
}
