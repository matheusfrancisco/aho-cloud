//! render.yaml blueprints: web, private and worker services.

use std::path::Path;

use serde_json::json;

use crate::yaml::{self, Node};
use crate::{DEFAULT_BUILDER, Fact, Findings, ProcessFacts, read, shell, slug};

const NAME: &str = "render.yaml";
/// Render routes web traffic to `$PORT`, 10000 unless the blueprint sets it.
const DEFAULT_PORT: u16 = 10000;

pub(crate) fn import(root: &Path) -> Option<Findings> {
    let text = match read(root, NAME)? {
        Ok(t) => t,
        Err(e) => return Some(Findings::failed(NAME, e)),
    };
    let doc = match yaml::parse(&text) {
        Ok(d) => d,
        Err(e) => return Some(Findings::failed(NAME, format!("{NAME}: {e}"))),
    };
    let mut f = Findings::new(NAME);
    let services: Vec<&Node> = doc
        .get("services")
        .map(|s| s.seq().iter().filter(|s| matches!(s.get("type").and_then(Node::str), Some("web" | "pserv" | "worker"))).collect())
        .unwrap_or_default();
    let primary = services.iter().position(|s| s.get("type").and_then(Node::str) == Some("web"));
    for (i, svc) in services.iter().enumerate() {
        let kind = svc.get("type").and_then(Node::str).unwrap_or_default();
        let Some(raw) = svc.get("name").and_then(Node::str) else {
            f.problems.push(format!("{NAME}:{}: service without a name", svc.line));
            continue;
        };
        let Some(name) = slug(raw, 30) else {
            f.problems.push(format!("{NAME}:{}: `{raw}` cannot be a process name", svc.line));
            continue;
        };
        let name_line = svc.get("name").map_or(svc.line, |n| n.line);
        let mut p =
            ProcessFacts { name: Some(Fact::new(json!(name), NAME, name_line, format!("{kind} service `{raw}`"))), ..Default::default() };
        let runtime = svc.get("runtime").or_else(|| svc.get("env"));
        let docker = runtime.and_then(Node::str) == Some("docker");
        let start = if docker { svc.get("dockerCommand") } else { svc.get("startCommand") };
        if let Some(cmd) = start.and_then(|c| c.str().map(|s| (s, c.line))) {
            p.command = Some(Fact::new(json!(shell::argv(cmd.0)), NAME, cmd.1, cmd.0.to_owned()));
        }
        let env_vars = svc.get("envVars").map(Node::seq).unwrap_or_default();
        let port_var = env_vars.iter().find(|v| v.get("key").and_then(Node::str) == Some("PORT"));
        let port = port_var.and_then(|v| v.get("value")).and_then(|v| v.str()?.parse::<u16>().ok().map(|p| (p, v.line)));
        if let Some((port, line)) = port {
            p.port = Some(Fact::new(json!(port), NAME, line, format!("PORT = {port}")));
        } else if kind != "worker" {
            let line = svc.get("type").map_or(svc.line, |t| t.line);
            p.port = Some(Fact::new(
                json!(DEFAULT_PORT),
                NAME,
                line,
                format!("Render sends {kind} traffic to $PORT, {DEFAULT_PORT} by default"),
            ));
        }
        if kind == "web" {
            let line = svc.get("type").map_or(svc.line, |t| t.line);
            p.expose = Some(Fact::new(json!("public"), NAME, line, "type: web is public"));
        }
        if let Some(path) = svc.get("healthCheckPath").and_then(|n| n.str().map(|s| (s, n.line))) {
            p.health = Some(Fact::new(json!({"kind": "http", "path": path.0}), NAME, path.1, format!("healthCheckPath: {}", path.0)));
        }
        if let Some(disk) = svc.get("disk")
            && let Some(mount) = disk.get("mountPath").and_then(Node::str)
        {
            let gb = disk.get("sizeGB").and_then(|n| n.str()?.parse::<u32>().ok()).unwrap_or(1);
            let v = json!({"mount": mount, "size_mb": gb.saturating_mul(1024)});
            let line = svc.key("disk").map_or(disk.line, |k| k.line);
            p.volume = Some(Fact::new(v, NAME, line, format!("disk at {mount}")));
        }
        for var in env_vars {
            if var.get("sync").and_then(Node::bool) == Some(false)
                && let Some(key) = var.get("key").and_then(Node::str)
            {
                f.secrets.push(Fact::new(json!(key), NAME, var.line, format!("`{key}` is set in the dashboard (sync: false)")));
            }
        }
        if Some(i) == primary || (primary.is_none() && i == 0) {
            if let Some(rt) = runtime.and_then(|r| r.str().map(|s| (s, r.line))) {
                f.build = match rt.0 {
                    "docker" => {
                        let path = svc.get("dockerfilePath").and_then(Node::str).unwrap_or("./Dockerfile");
                        let context = svc.get("dockerContext").and_then(Node::str).unwrap_or(".");
                        let v = json!({"kind": "dockerfile", "path": path.trim_start_matches("./"), "context": trim_dot(context)});
                        Some(Fact::new(v, NAME, rt.1, "runtime: docker"))
                    }
                    "image" => None,
                    other => {
                        Some(Fact::new(json!({"kind": "buildpack", "builder": DEFAULT_BUILDER}), NAME, rt.1, format!("runtime: {other}")))
                    }
                };
            }
            if kind == "web" {
                for d in svc.get("domains").map(Node::seq).unwrap_or_default() {
                    if let Some(host) = d.str() {
                        f.domains.push(Fact::new(json!(host.to_ascii_lowercase()), NAME, d.line, format!("custom domain {host}")));
                    }
                }
            }
        }
        if Some(i) == primary {
            f.primary = Some(p);
        } else {
            f.named.push((name, p));
        }
    }
    Some(f)
}

fn trim_dot(context: &str) -> String {
    let c = context.trim_start_matches("./").trim_end_matches('/');
    if c.is_empty() { ".".into() } else { c.to_owned() }
}
