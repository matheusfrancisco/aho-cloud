//! Compose files: services built from this repository are processes of this project.

use std::path::Path;

use serde_json::{Value, json};

use crate::yaml::{self, Node, Yaml};
use crate::{Fact, Findings, ProcessFacts, health_from_argv, read, shell, slug};

const NAME: &str = "docker-compose";
const FILES: [&str; 4] = ["compose.yaml", "compose.yml", "docker-compose.yaml", "docker-compose.yml"];

pub(crate) fn import(root: &Path) -> Option<Findings> {
    let (file, text) = FILES.iter().find_map(|f| read(root, f).map(|t| (*f, t)))?;
    let text = match text {
        Ok(t) => t,
        Err(e) => return Some(Findings::failed(NAME, e)),
    };
    let doc = match yaml::parse(&text) {
        Ok(d) => d,
        Err(e) => return Some(Findings::failed(NAME, format!("{file}: {e}"))),
    };
    let mut f = Findings::new(NAME);
    let mut built: Vec<(String, &Node, &Node, BuildRef)> = Vec::new();
    for (key, svc) in doc.get("services").map(|s| s.entries().collect::<Vec<_>>()).unwrap_or_default() {
        let Some(raw) = key.str() else { continue };
        let Some(build) = svc.get("build").and_then(build_ref) else { continue };
        if !build.in_repo() {
            continue;
        }
        match slug(raw, 30) {
            Some(name) => built.push((name, key, svc, build)),
            None => f.problems.push(format!("{file}:{}: `{raw}` cannot be a process name", key.line)),
        }
    }
    let primary = ["web", "app"]
        .iter()
        .find_map(|n| built.iter().position(|b| b.0 == *n))
        .or_else(|| built.iter().position(|b| b.2.get("ports").is_some()))
        .or((!built.is_empty()).then_some(0));
    for (i, (name, key, svc, build)) in built.iter().enumerate() {
        let facts = process(file, name, key, svc);
        if Some(i) == primary {
            f.build = Some(Fact::new(build.value(), file, build.line, format!("service `{name}` builds from this repository")));
            f.primary = Some(facts);
        } else {
            f.named.push((name.clone(), facts));
        }
    }
    Some(f)
}

struct BuildRef {
    context: String,
    dockerfile: Option<String>,
    line: usize,
}

impl BuildRef {
    fn in_repo(&self) -> bool {
        !self.context.contains("://") && !self.context.starts_with("..") && !self.context.starts_with('/')
    }

    fn value(&self) -> Value {
        let context = normalize(&self.context);
        let file = self.dockerfile.as_deref().unwrap_or("Dockerfile");
        let path = if context == "." { normalize(file) } else { normalize(&format!("{context}/{file}")) };
        json!({"kind": "dockerfile", "path": path, "context": context})
    }
}

fn normalize(p: &str) -> String {
    let parts: Vec<&str> = p.split('/').filter(|s| !s.is_empty() && *s != ".").collect();
    if parts.is_empty() { ".".into() } else { parts.join("/") }
}

fn build_ref(node: &Node) -> Option<BuildRef> {
    if let Some(ctx) = node.str() {
        return Some(BuildRef { context: ctx.to_owned(), dockerfile: None, line: node.line });
    }
    node.is_map().then(|| BuildRef {
        context: node.get("context").and_then(Node::str).unwrap_or(".").to_owned(),
        dockerfile: node.get("dockerfile").and_then(Node::str).map(str::to_owned),
        line: node.line,
    })
}

fn process(file: &str, name: &str, key: &Node, svc: &Node) -> ProcessFacts {
    let mut p =
        ProcessFacts { name: Some(Fact::new(json!(name), file, key.line, format!("compose service `{name}`"))), ..Default::default() };
    if let Some(cmd) = svc.get("command").or_else(|| svc.get("entrypoint")) {
        let argv = match (&cmd.v, cmd.str()) {
            (_, Some(s)) => Some(shell::argv(s)),
            (Yaml::Seq(items), _) => Some(items.iter().filter_map(Node::str).map(str::to_owned).collect()),
            _ => None,
        };
        if let Some(argv) = argv.filter(|a: &Vec<String>| !a.is_empty()) {
            p.command = Some(Fact::new(json!(argv), file, cmd.line, "compose `command`"));
        }
    }
    let published = svc.get("ports").and_then(|ports| ports.seq().iter().find_map(|n| container_port(n).map(|p| (p, n.line))));
    let exposed =
        svc.get("expose").and_then(|e| e.seq().iter().find_map(|n| n.str()?.split('/').next()?.parse::<u16>().ok().map(|p| (p, n.line))));
    if let Some((port, line)) = published {
        p.port = Some(Fact::new(json!(port), file, line, format!("container port {port} is published")));
        p.expose = Some(Fact::new(json!("public"), file, line, "published port"));
    } else if let Some((port, line)) = exposed {
        p.port = Some(Fact::new(json!(port), file, line, format!("expose {port}")));
    }
    if let Some(hc) = svc.get("healthcheck")
        && hc.get("disable").and_then(Node::bool) != Some(true)
        && let Some(test) = hc.get("test")
        && let Some(argv) = health_argv(test)
    {
        let port = p.port.as_ref().and_then(|f| f.value.as_u64()).and_then(|v| u16::try_from(v).ok());
        p.health = Some(Fact::new(health_from_argv(&argv, port), file, test.line, "compose healthcheck"));
    }
    p
}

/// `"8080:3000"`, `"127.0.0.1:8080:3000/tcp"`, `"3000"` or `{target: 3000}` → 3000.
fn container_port(n: &Node) -> Option<u16> {
    if let Some(s) = n.str() {
        let (spec, proto) = s.split_once('/').unwrap_or((s, "tcp"));
        if proto != "tcp" {
            return None;
        }
        return spec.rsplit(':').next()?.parse().ok();
    }
    n.get("target")?.str()?.parse().ok()
}

/// `["CMD", …]`, `["CMD-SHELL", "…"]`, or a string (shell form); `["NONE"]` disables.
fn health_argv(test: &Node) -> Option<Vec<String>> {
    if let Some(s) = test.str() {
        return Some(vec!["/bin/sh".into(), "-c".into(), s.to_owned()]);
    }
    let items: Vec<&str> = test.seq().iter().filter_map(Node::str).collect();
    match items.as_slice() {
        ["CMD", rest @ ..] if !rest.is_empty() => Some(rest.iter().map(|s| (*s).to_owned()).collect()),
        ["CMD-SHELL", cmd] => Some(vec!["/bin/sh".into(), "-c".into(), (*cmd).to_owned()]),
        _ => None,
    }
}
