//! aho capability for the aho cloud principal contract: the edge authenticates, the app
//! authorizes by `(iss, sub)` from a verified `x-principal-jwt`.
//!
//! - a prompt section ([`PROMPT`]) telling the model never to build login and to declare routes
//!   in `edge.toml`;
//! - `edge_routes` (ReadOnly): parses and compiles the workspace `edge.toml` with
//!   `cloud_contract::edge`, resolves request paths, and reports every error;
//! - `edge_verifier` (ReadOnly): the verification middleware for Express, FastAPI, Go or axum,
//!   rendered from `templates/*` for one project, stage and JWKS URL.

#![forbid(unsafe_code)]

use std::sync::Arc;

use aho_cap::{
    BoundWorld, BuildError, Capability, CapabilityId, Config, ConfigError, Deps, ExecOutcome, ExecRequest, HandleKind, Kind, Manifest,
    MissingDep, Present, Registrar, Requirement, Resolver, ToolDecl, ToolExec, declare_capability,
};
use aho_config::{ConfigKey, Loadout, Ty};
use aho_fs::{FsError, FsRead};
use aho_log::OpKind;
use aho_types::{Card, Consequence, ContentBlock, RootSchema, Value};
use cloud_contract::edge::{EdgeConfig, Mode, Routes};
use cloud_contract::ledger::Stage;
use cloud_contract::principal;
use serde_json::json;

/// The prompt section every session with this capability gets.
pub const PROMPT: &str = include_str!("../prompts/edge-contract.md");
/// Name of the prompt section.
pub const SECTION: &str = "aho-cloud-edge-contract";
const SECTION_ORDER: i32 = 100;
const EDGE_TOML: &str = "edge.toml";

// ───────────────────────────── verifier templates ─────────────────────────────

/// A framework `edge_verifier` writes middleware for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Framework {
    Express,
    Fastapi,
    Go,
    Axum,
}

impl Framework {
    pub const ALL: [Framework; 4] = [Framework::Express, Framework::Fastapi, Framework::Go, Framework::Axum];

    pub fn name(self) -> &'static str {
        match self {
            Framework::Express => "express",
            Framework::Fastapi => "fastapi",
            Framework::Go => "go",
            Framework::Axum => "axum",
        }
    }

    pub fn parse(s: &str) -> Option<Framework> {
        Framework::ALL.into_iter().find(|f| f.name().eq_ignore_ascii_case(s.trim()))
    }

    fn template(self) -> &'static str {
        match self {
            Framework::Express => include_str!("../templates/express.js"),
            Framework::Fastapi => include_str!("../templates/fastapi.py"),
            Framework::Go => include_str!("../templates/go.go"),
            Framework::Axum => include_str!("../templates/axum.rs"),
        }
    }

    /// Where the snippet usually lives, the code-fence language, and what to install.
    fn file(self) -> (&'static str, &'static str, &'static str) {
        match self {
            Framework::Express => ("principal.js", "js", "npm install jose"),
            Framework::Fastapi => ("principal.py", "python", "pip install \"PyJWT[crypto]\""),
            Framework::Go => ("principal/principal.go", "go", "go get github.com/golang-jwt/jwt/v5 github.com/MicahParks/keyfunc/v3"),
            Framework::Axum => (
                "src/principal.rs",
                "rust",
                "cargo add axum@0.8 jsonwebtoken@9 serde reqwest tokio --features serde/derive,reqwest/json,tokio/sync",
            ),
        }
    }
}

/// What a rendered verifier checks tokens against.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifierParams {
    pub project: String,
    pub stage: Stage,
    /// Must pass [`jwks_url_ok`]; it is pasted into source code.
    pub jwks_url: String,
}

/// The middleware for `framework`, with the project's issuer, audience and JWKS URL filled in.
pub fn render_verifier(framework: Framework, p: &VerifierParams) -> String {
    framework
        .template()
        .replace("{{PROJECT}}", &p.project)
        .replace("{{STAGE}}", p.stage.as_str())
        .replace("{{ISSUER}}", &principal::issuer(&p.project))
        .replace("{{AUDIENCE}}", &principal::audience(&p.project, p.stage))
        .replace("{{JWKS_URL}}", &p.jwks_url)
        .replace("{{HEADER}}", principal::HEADER)
        .replace("{{TTL}}", &principal::TTL_SECS.to_string())
        .replace("{{LEEWAY}}", &principal::LEEWAY_SECS.to_string())
}

/// JWKS URLs are https (plain http only on loopback, for local runs), and carry no character that
/// could end the string literal they are pasted into.
pub fn jwks_url_ok(url: &str) -> bool {
    let rest = match url.strip_prefix("https://") {
        Some(r) => r,
        None => match url.strip_prefix("http://") {
            Some(r) if ["localhost", "127.0.0.1"].iter().any(|h| r.split(['/', ':']).next() == Some(h)) => r,
            _ => return false,
        },
    };
    !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_alphanumeric() || b"-._~:/?#[]@!$&()*+,;=%".contains(&b)) && !rest.starts_with('/')
}

fn is_slug(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 40
        && b[0].is_ascii_lowercase()
        && !s.ends_with('-')
        && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

// ───────────────────────────── config and deps ─────────────────────────────

/// `cloud.project`, the default project for `edge_verifier`.
#[derive(Clone, Debug, Default)]
pub struct EdgeContractConfig {
    pub project: Option<String>,
}

impl Config for EdgeContractConfig {
    const PREFIX: &'static str = "cloud";

    fn keys() -> Vec<ConfigKey> {
        vec![ConfigKey::new("cloud.project", Ty::String, None, "aho cloud project slug this session works on.")]
    }

    fn from_loadout(l: &Loadout) -> Result<Self, ConfigError> {
        let project = l.str("cloud.project").map(str::trim).filter(|p| !p.is_empty()).map(str::to_owned);
        if project.as_deref().is_some_and(|p| !is_slug(p)) {
            return Err(ConfigError::Type { key: "cloud.project".into(), expected: "a project slug (lowercase letters, digits, `-`)" });
        }
        Ok(EdgeContractConfig { project })
    }
}

/// Reads the workspace's `edge.toml`.
pub struct EdgeDeps {
    pub fs: FsRead,
}

impl Deps for EdgeDeps {
    const REQUIRES: &'static [HandleKind] = &[HandleKind::FsRead];
    fn resolve(r: &Resolver) -> Result<Self, MissingDep> {
        Ok(EdgeDeps { fs: r.handle()? })
    }
}

// ───────────────────────────── tools ─────────────────────────────

fn routes_schema(_: &BoundWorld) -> RootSchema {
    RootSchema(json!({
        "type": "object",
        "properties": {
            "paths": {"type": "array", "items": {"type": "string"}, "description": "Request paths to resolve, e.g. `/api/users` or `/webhooks/stripe`."},
            "file": {"type": "string", "description": "Path of the edge config, relative to the workspace (default `edge.toml`)."},
            "manifest": {"type": "object", "description": "The deploy manifest, to cross-check auth and verifier secrets."}
        },
        "additionalProperties": false
    }))
}

fn verifier_schema(_: &BoundWorld) -> RootSchema {
    RootSchema(json!({
        "type": "object",
        "properties": {
            "framework": {"type": "string", "enum": Framework::ALL.map(Framework::name)},
            "project": {"type": "string", "description": "Project slug (default: cloud.project)."},
            "stage": {"type": "string", "enum": ["preview", "staging", "live"], "default": "live"},
            "hostname": {"type": "string", "description": "A hostname the project is served on; the JWKS is `https://<hostname>/.edge/jwks.json`."},
            "jwks_url": {"type": "string", "description": "The JWKS URL itself, instead of `hostname`."}
        },
        "required": ["framework"],
        "additionalProperties": false
    }))
}

fn routes_card(args: &Value) -> Card {
    Card::Generic { title: "edge.toml".into(), body: args.get("file").and_then(Value::as_str).unwrap_or(EDGE_TOML).to_owned() }
}

fn routes_result(_: &Value, v: &Value) -> Card {
    let errors = v.get("errors").and_then(Value::as_array).map_or(0, Vec::len);
    Card::Generic { title: "edge.toml".into(), body: if errors == 0 { "valid".into() } else { format!("{errors} errors") } }
}

fn verifier_card(args: &Value) -> Card {
    Card::Generic { title: "Principal verifier".into(), body: args.get("framework").and_then(Value::as_str).unwrap_or("?").to_owned() }
}

fn verifier_result(_: &Value, v: &Value) -> Card {
    Card::Generic { title: "Principal verifier".into(), body: v.get("file").and_then(Value::as_str).unwrap_or("").to_owned() }
}

pub static TOOLS: &[ToolDecl] = &[
    ToolDecl::new(
        "edge_routes",
        "Check the workspace's edge.toml the way the aho cloud edge reads it: parse it, compile the route globs, and resolve \
         request paths to their mode (edge, optional, bearer, public, passthrough), matching route and webhook verifier. Pass the \
         deploy manifest to also cross-check its auth and verifier secrets. Reports every error found.",
        Consequence::ReadOnly,
    )
    .safe()
    .schema(routes_schema)
    .op(OpKind::Io, true, false)
    .present(Present { call: routes_card, result: routes_result }),
    ToolDecl::new(
        "edge_verifier",
        "Write the middleware an app uses to verify the x-principal-jwt the aho cloud edge forwards (ES256 against the project's \
         JWKS; issuer, audience and lifetime checked) for express, fastapi, go or axum. The app then authorizes by (iss, sub); it \
         never implements login.",
        Consequence::ReadOnly,
    )
    .safe()
    .schema(verifier_schema)
    .present(Present { call: verifier_card, result: verifier_result }),
];

fn mode_name(m: Mode) -> &'static str {
    match m {
        Mode::Edge => "edge",
        Mode::Optional => "optional",
        Mode::Bearer => "bearer",
        Mode::Public => "public",
        Mode::Passthrough => "passthrough",
    }
}

/// `edge_routes` and `edge_verifier`, plus the prompt section.
#[derive(Clone)]
pub struct EdgeContract {
    fs: FsRead,
    cfg: Arc<EdgeContractConfig>,
}

impl EdgeContract {
    async fn routes(&self, args: &Value) -> ExecOutcome {
        let file = args.get("file").and_then(Value::as_str).unwrap_or(EDGE_TOML);
        let paths: Vec<String> = match args.get("paths") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(items)) if items.iter().all(Value::is_string) => {
                items.iter().filter_map(Value::as_str).map(str::to_owned).collect()
            }
            Some(_) => return ExecOutcome::err("BAD_ARGS", "`paths` must be a list of request paths."),
        };
        let manifest: Option<cloud_contract::Manifest> = match args.get("manifest") {
            None | Some(Value::Null) => None,
            Some(v) => match serde_json::from_value(v.clone()) {
                Ok(m) => Some(m),
                Err(e) => return ExecOutcome::err("BAD_MANIFEST", format!("`manifest` is not a deploy.aho.cloud/v1 manifest ({e}).")),
            },
        };
        let (config, implied) = match self.fs.0.read_bytes(file).await {
            Ok(bytes) => {
                let text = String::from_utf8_lossy(&bytes);
                match EdgeConfig::parse(&text) {
                    Ok(c) => (c, false),
                    Err(e) => return report(file, None, false, &[], vec![e.to_string()]),
                }
            }
            Err(FsError::NotFound(_)) => match &manifest {
                Some(m) => (EdgeConfig::implied_by(m), true),
                None => {
                    return ExecOutcome::err(
                        "FS_NOT_FOUND",
                        format!(
                            "there is no `{file}` in the workspace, so the edge applies the default the manifest's auth implies: `edge` \
                             for password, magic_link and oidc, `passthrough` for none and app_owned. Pass `manifest` to resolve paths \
                             against it, or write the file."
                        ),
                    );
                }
            },
            Err(e) => return ExecOutcome::err(e.code(), e.to_string()),
        };
        let mut errors = Vec::new();
        let compiled = match config.compile() {
            Ok(r) => Some(r),
            Err(e) => {
                errors.push(e.to_string());
                None
            }
        };
        if let Some(m) = &manifest
            && let Err(es) = config.check_against(m)
        {
            errors.extend(es.iter().map(ToString::to_string));
        }
        let resolved: Vec<Value> = match &compiled {
            Some(r) => paths.iter().map(|p| resolve_one(r, &config, p)).collect(),
            None => Vec::new(),
        };
        report(file, Some(&config), implied, &resolved, errors)
    }

    fn verifier(&self, args: &Value) -> ExecOutcome {
        let Some(framework) = args.get("framework").and_then(Value::as_str).and_then(Framework::parse) else {
            return ExecOutcome::err("BAD_ARGS", "`framework` must be one of express, fastapi, go, axum.");
        };
        let project = match args.get("project").and_then(Value::as_str).map(str::to_owned).or_else(|| self.cfg.project.clone()) {
            Some(p) if is_slug(&p) => p,
            Some(p) => return ExecOutcome::err("BAD_ARGS", format!("`{p}` is not a project slug.")),
            None => return ExecOutcome::err("BAD_ARGS", "give `project` (no cloud.project is set)."),
        };
        let stage: Stage = match args.get("stage") {
            None | Some(Value::Null) => Stage::Live,
            Some(v) => match serde_json::from_value(v.clone()) {
                Ok(s) => s,
                Err(_) => return ExecOutcome::err("BAD_ARGS", "`stage` must be preview, staging or live."),
            },
        };
        let jwks_url = match (args.get("jwks_url").and_then(Value::as_str), args.get("hostname").and_then(Value::as_str)) {
            (Some(u), _) if jwks_url_ok(u) => u.to_owned(),
            (Some(u), _) => {
                return ExecOutcome::err(
                    "BAD_ARGS",
                    format!("`{u}` is not a JWKS URL the app should trust: use https (http only on localhost)."),
                );
            }
            (None, Some(h)) if cloud_contract::validate::is_hostname(h, false) => format!("https://{h}{}", principal::JWKS_PATH),
            (None, Some(h)) => return ExecOutcome::err("BAD_ARGS", format!("`{h}` is not a hostname.")),
            (None, None) => return ExecOutcome::err("BAD_ARGS", "give `hostname` (a domain the project is served on) or `jwks_url`."),
        };
        let params = VerifierParams { project: project.clone(), stage, jwks_url: jwks_url.clone() };
        let snippet = render_verifier(framework, &params);
        let (file, lang, install) = framework.file();
        let text = format!(
            "Save as `{file}` and install: `{install}`. It accepts only tokens for `{project}` at the {} stage; authorize by (iss, sub) \
             in your handlers and never add a login flow.\n```{lang}\n{snippet}```",
            stage.as_str()
        );
        let value = json!({
            "framework": framework.name(),
            "file": file,
            "install": install,
            "issuer": principal::issuer(&project),
            "audience": principal::audience(&project, stage),
            "jwks_url": jwks_url,
            "snippet": snippet,
        });
        ExecOutcome::Done { value, content: vec![ContentBlock::text(text)], meta: None }
    }
}

fn resolve_one(routes: &Routes, config: &EdgeConfig, path: &str) -> Value {
    let m = routes.resolve(path);
    json!({
        "path": path,
        "mode": mode_name(m.mode),
        "route": m.route,
        "route_path": m.route.and_then(|i| config.routes.get(i)).map(|r| r.path.clone()),
        "verify": m.verify,
        "principal": matches!(m.mode, Mode::Edge | Mode::Optional),
    })
}

fn report(file: &str, config: Option<&EdgeConfig>, implied: bool, resolved: &[Value], errors: Vec<String>) -> ExecOutcome {
    let mut text = match (config, implied) {
        (Some(c), true) => {
            format!("no {file} in the workspace; the manifest's auth implies default `{}` for every path.\n", mode_name(c.default))
        }
        (Some(c), false) => format!("{file}: default `{}`, {} routes.\n", mode_name(c.default), c.routes.len()),
        (None, _) => format!("{file} does not parse.\n"),
    };
    for r in resolved {
        let path = r["path"].as_str().unwrap_or("");
        let mode = r["mode"].as_str().unwrap_or("");
        let route = match (r["route"].as_u64(), r["route_path"].as_str()) {
            (Some(i), Some(p)) => format!("route {i} `{p}`"),
            _ => "the default".into(),
        };
        let verify = match (r["verify"]["kind"].as_str(), r["verify"]["secret"].as_str()) {
            (Some(k), Some(s)) => format!("; the edge checks a {k} signature with secret {s}"),
            _ => String::new(),
        };
        let principal = if r["principal"] == json!(true) { format!("; the app receives {}", principal::HEADER) } else { String::new() };
        text.push_str(&format!("- {path} → {mode} ({route}){verify}{principal}\n"));
    }
    if errors.is_empty() {
        text.push_str("No errors.\n");
    } else {
        text.push_str("Errors:\n");
        for e in &errors {
            text.push_str(&format!("- {e}\n"));
        }
    }
    let value = json!({
        "file": file,
        "implied": implied,
        "ok": errors.is_empty(),
        "default": config.map(|c| mode_name(c.default)),
        "routes": config.map(|c| &c.routes),
        "resolved": resolved,
        "errors": errors,
    });
    ExecOutcome::Done { value, content: vec![ContentBlock::text(text)], meta: None }
}

impl Capability for EdgeContract {
    const ID: CapabilityId = Self::ID_;
    const MANIFEST: &'static Manifest = &Self::MANIFEST_;
    type Deps = EdgeDeps;
    type Config = EdgeContractConfig;

    fn build(d: EdgeDeps, cfg: EdgeContractConfig) -> Result<Self, BuildError> {
        Ok(EdgeContract { fs: d.fs, cfg: Arc::new(cfg) })
    }

    fn register(&self, r: &mut Registrar<'_>) {
        r.prompt_section(SECTION_ORDER, SECTION, false, Arc::new(|_: &aho_log::State| PROMPT.to_owned()));
        let me = self.clone();
        let routes: ToolExec = Arc::new(move |req: ExecRequest| {
            let me = me.clone();
            Box::pin(async move { me.routes(&req.view.args).await })
        });
        r.tool(&TOOLS[0], routes);
        let me = self.clone();
        let verifier: ToolExec = Arc::new(move |req: ExecRequest| {
            let out = me.verifier(&req.view.args);
            Box::pin(async move { out })
        });
        r.tool(&TOOLS[1], verifier);
    }
}

declare_capability!(
    EdgeContract,
    "cloud-edge-contract",
    kind = Kind::Tool,
    provides = [],
    requires = [Requirement::Seam(aho_fs::SEAM)],
    tools = TOOLS,
    doc = "aho cloud principal contract: prompt section, edge_routes (check edge.toml) and edge_verifier (x-principal-jwt middleware)."
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jwks_urls_must_be_https_and_safe_to_paste_into_code() {
        for ok in ["https://shop.example.io/.edge/jwks.json", "http://localhost:8080/.edge/jwks.json", "http://127.0.0.1/.edge/jwks.json"] {
            assert!(jwks_url_ok(ok), "{ok}");
        }
        for bad in [
            "http://shop.example.io/.edge/jwks.json",
            "http://localhost.evil.io/jwks",
            "https://x.io/\"; process.exit(1); \"",
            "https://x.io/a b",
            "https:///path",
            "ftp://x.io/jwks",
        ] {
            assert!(!jwks_url_ok(bad), "{bad}");
        }
    }
}
