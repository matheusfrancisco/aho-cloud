//! Draft → manifest with a human in the loop: every inferred or observed field is shown with its
//! evidence and accepted, corrected or dropped; required fields no evidence covers are asked for.
//! `--accept-inferred` is the explicit opt-out from the per-field questions.

use std::collections::BTreeMap;
use std::io::{BufRead, Write};

use cloud_contract::Manifest;
use cloud_contract::draft::{Draft, DraftField};
use cloud_contract::manifest::API_VERSION;
use cloud_contract::validate::host_covered;
use cloud_importers::Overruled;
use serde_json::{Value, json};

use crate::error::{Error, Result};

pub struct Prompter<R, W> {
    input: R,
    out: W,
    accept_inferred: bool,
}

impl<R: BufRead, W: Write> Prompter<R, W> {
    pub fn new(input: R, out: W, accept_inferred: bool) -> Self {
        Self { input, out, accept_inferred }
    }

    fn say(&mut self, text: &str) {
        let _ = writeln!(self.out, "{text}");
    }

    /// One line from the input; `None` at end of input.
    fn ask(&mut self, prompt: &str) -> Result<Option<String>> {
        let _ = write!(self.out, "{prompt}");
        let _ = self.out.flush();
        let mut line = String::new();
        let n = self.input.read_line(&mut line).map_err(Error::io("<stdin>"))?;
        if n == 0 {
            let _ = writeln!(self.out);
            return Ok(None);
        }
        Ok(Some(line.trim_end_matches(['\n', '\r']).to_owned()))
    }
}

/// Sets a whole subtree (`/source`, `/api_version`) as declared, replacing whatever was there.
pub fn set(draft: &mut Draft, pointer: &str, value: Value) {
    let inside = format!("{pointer}/");
    draft.fields.retain(|k, _| k != pointer && !k.starts_with(&inside));
    draft.declare(pointer, value);
}

/// Walks the human through `draft` and returns the manifest it becomes. `overruled` are machine
/// values that lost to stronger ones (usually the existing `deploy.json`); they are reported.
pub fn confirm<R: BufRead, W: Write>(draft: &mut Draft, overruled: &[Overruled], p: &mut Prompter<R, W>) -> Result<Manifest> {
    set(draft, "/api_version", json!(API_VERSION));
    for o in overruled {
        let from: Vec<&str> = o.offered.evidence.iter().map(|e| e.uri.as_str()).collect();
        p.say(&format!(
            "kept {} = {}; {} suggests {} (edit deploy.json to change it)",
            o.pointer,
            o.kept,
            from.join(", "),
            o.offered.value
        ));
    }

    while let Some(pointer) = draft.unconfirmed().into_iter().next() {
        let Some(field) = draft.fields.get(&pointer).cloned() else { break };
        p.say(&describe(&pointer, &field));
        if p.accept_inferred {
            draft.confirm(&pointer);
            continue;
        }
        let answer = p.ask("  [Enter] accept · type a correction · `-` drop: ")?.ok_or_else(|| {
            Error::Invalid(format!(
                "input ended while confirming {pointer}; answer each field, or pass --accept-inferred to accept drafted values"
            ))
        })?;
        match answer.trim() {
            "" => {
                draft.confirm(&pointer);
            }
            "-" => drop_field(draft, &pointer),
            typed => correct(draft, &pointer, parse_input(&pointer, typed)),
        }
    }

    loop {
        let value = draft.to_value().map_err(|e| Error::Invalid(e.to_string()))?;
        let Some(need) = missing(&value).into_iter().next() else { break };
        let default = need.default.as_ref().map(|d| format!(" [{}]", show(d))).unwrap_or_default();
        let answer = p
            .ask(&format!("{} ({}){default}: ", need.pointer, need.hint))?
            .ok_or_else(|| Error::Invalid(format!("{} is required ({}), but input ended", need.pointer, need.hint)))?;
        let v = match (answer.trim(), &need.default) {
            ("", Some(d)) => d.clone(),
            ("", None) => continue,
            (typed, _) => parse_input(&need.pointer, typed),
        };
        draft.declare(&need.pointer, v.clone());
        if need.pointer.ends_with("/reach") {
            cover_reach(draft, &need.pointer, &v, p);
        }
    }
    draft.into_manifest().map_err(|e| Error::Invalid(format!("{e}; fix the answers or edit deploy.json")))
}

fn show(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn describe(pointer: &str, f: &DraftField) -> String {
    let strength = serde_json::to_value(f.strength).ok().and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_default();
    let mut s = format!("{pointer} = {}   ({strength})", show(&f.value));
    for e in &f.evidence {
        s.push_str(&format!("\n    {}  {}", e.uri, e.note));
    }
    s
}

/// A correction. A new tag (`kind` / `mode`) invalidates the old variant's fields.
fn correct(draft: &mut Draft, pointer: &str, value: Value) {
    if let Some(group) = pointer.strip_suffix("/kind").or_else(|| pointer.strip_suffix("/mode"))
        && draft.fields.get(pointer).is_some_and(|f| f.value != value)
    {
        let inside = format!("{group}/");
        draft.fields.retain(|k, _| !k.starts_with(&inside));
    }
    draft.declare(pointer, value);
}

/// Drops a field. Dropping an element's identity (`/secrets/1/name`) or a list item
/// (`/egress/2`) drops the element and renumbers the rest; dropping a tag drops its object.
fn drop_field(draft: &mut Draft, pointer: &str) {
    let segs: Vec<&str> = pointer.trim_start_matches('/').split('/').collect();
    let n = segs.len();
    let is_index = |s: &str| s.parse::<usize>().is_ok();
    let (target, list_at) = if n >= 3 && matches!(segs[n - 1], "name" | "hostname") && is_index(segs[n - 2]) {
        (&segs[..n - 1], Some(n - 2))
    } else if n >= 2 && is_index(segs[n - 1]) {
        (&segs[..], Some(n - 1))
    } else if n >= 2 && matches!(segs[n - 1], "kind" | "mode") {
        (&segs[..n - 1], None)
    } else {
        (&segs[..], None)
    };
    let target = format!("/{}", target.join("/"));
    let inside = format!("{target}/");
    draft.fields.retain(|k, _| *k != target && !k.starts_with(&inside));
    let Some(k) = list_at else { return };
    let list = format!("/{}/", segs[..k].join("/"));
    let Ok(removed) = segs[k].parse::<usize>() else { return };
    let fields = std::mem::take(&mut draft.fields);
    draft.fields = fields
        .into_iter()
        .map(|(key, f)| {
            let renamed = key.strip_prefix(&list).and_then(|rest| {
                let (idx, tail) = rest.split_once('/').map_or((rest, ""), |(i, t)| (i, t));
                let i: usize = idx.parse().ok()?;
                (i > removed).then(|| if tail.is_empty() { format!("{list}{}", i - 1) } else { format!("{list}{}/{tail}", i - 1) })
            });
            (renamed.unwrap_or(key), f)
        })
        .collect::<BTreeMap<_, _>>();
}

/// Once a human names where a secret goes, egress must allow it; add what is not covered.
fn cover_reach<R: BufRead, W: Write>(draft: &mut Draft, reach_pointer: &str, reach: &Value, p: &mut Prompter<R, W>) {
    let egress: Vec<String> = draft
        .to_value()
        .ok()
        .and_then(|v| v.get("egress").and_then(Value::as_array).cloned())
        .unwrap_or_default()
        .iter()
        .filter_map(|v| v.as_str().map(str::to_owned))
        .collect();
    let mut next = egress.len();
    let mut allowed = egress;
    let secret = reach_pointer.trim_end_matches("/reach");
    for host in reach.as_array().into_iter().flatten().filter_map(Value::as_str) {
        if !allowed.iter().any(|e| host_covered(e, host)) {
            draft.declare(&format!("/egress/{next}"), json!(host));
            p.say(&format!("  egress + {host} (so the secret at {secret} can reach it)"));
            allowed.push(host.to_owned());
            next += 1;
        }
    }
}

/// What a typed answer means at `pointer`: a few shorthands, else JSON, else a string.
fn parse_input(pointer: &str, typed: &str) -> Value {
    let s = typed.trim();
    let json = serde_json::from_str::<Value>(s).ok();
    let last = pointer.rsplit('/').next().unwrap_or_default();
    if pointer == "/build" {
        match s {
            "dockerfile" => return json!({"kind": "dockerfile", "path": "Dockerfile", "context": "."}),
            "buildpack" => return json!({"kind": "buildpack", "builder": cloud_importers::DEFAULT_BUILDER}),
            _ => {}
        }
    }
    if last == "health" && json.as_ref().is_none_or(|v| !v.is_object()) {
        return if s.starts_with('/') { json!({"kind": "http", "path": s}) } else { json!({"kind": "command", "argv": argv(s)}) };
    }
    if pointer == "/auth" && json.as_ref().is_none_or(|v| !v.is_object()) {
        return match s.split_once(':') {
            Some(("oidc", provider)) => json!({"mode": "oidc", "provider": {"kind": provider}}),
            _ => json!({"mode": s}),
        };
    }
    if (last == "reach" || pointer == "/egress") && json.as_ref().is_none_or(|v| !v.is_array()) {
        let hosts: Vec<String> = s.split([',', ' ']).map(|h| h.trim().to_ascii_lowercase()).filter(|h| !h.is_empty()).collect();
        return json!(hosts);
    }
    if matches!(last, "command" | "argv") && json.as_ref().is_none_or(|v| !v.is_array()) {
        return json!(argv(s));
    }
    json.unwrap_or_else(|| Value::String(s.to_owned()))
}

/// Words, unless the line needs a shell.
fn argv(s: &str) -> Vec<String> {
    if s.contains(['$', '|', '&', ';', '<', '>', '(', ')', '*', '?', '`', '"', '\'', '\\', '~', '#']) {
        vec!["/bin/sh".into(), "-c".into(), s.to_owned()]
    } else {
        s.split_whitespace().map(str::to_owned).collect()
    }
}

struct Need {
    pointer: String,
    hint: &'static str,
    default: Option<Value>,
}

/// Required fields the draft does not set, in the order they are asked.
fn missing(v: &Value) -> Vec<Need> {
    let mut out = Vec::new();
    let mut need = |pointer: String, hint: &'static str, default: Option<Value>| out.push(Need { pointer, hint, default });
    let has = |p: &str| v.pointer(p).is_some_and(|x| !x.is_null());
    let str_at = |p: &str| v.pointer(p).and_then(Value::as_str);

    if !has("/project") {
        need("/project".into(), "project slug: lowercase letters, digits and -", None);
    }
    if !has("/source") {
        need("/source".into(), "source JSON, e.g. {\"kind\":\"git\",\"repo\":…,\"commit\":…}", None);
    }
    match str_at("/build/kind") {
        None => need("/build".into(), "dockerfile, buildpack, or a build JSON", None),
        Some("buildpack") if !has("/build/builder") => {
            need("/build/builder".into(), "buildpack builder image", Some(json!(cloud_importers::DEFAULT_BUILDER)));
        }
        Some("command") => {
            if !has("/build/command") {
                need("/build/command".into(), "build command", None);
            }
            if !has("/build/base_image") {
                need("/build/base_image".into(), "image the build command runs in", None);
            }
        }
        _ => {}
    }
    match v.get("processes").and_then(Value::as_array).filter(|p| !p.is_empty()) {
        None => need("/processes/0/name".into(), "name of the process to run", Some(json!("web"))),
        Some(procs) => {
            for (i, p) in procs.iter().enumerate() {
                if p.get("name").is_none_or(Value::is_null) {
                    need(format!("/processes/{i}/name"), "process name, e.g. web or worker", None);
                }
                match p.pointer("/health/kind").and_then(Value::as_str) {
                    None => need(format!("/processes/{i}/health"), "`/path` for an HTTP check, or a command that exits 0", None),
                    Some("http") if p.pointer("/health/path").is_none() => {
                        need(format!("/processes/{i}/health/path"), "HTTP check path", Some(json!("/")))
                    }
                    Some("command") if p.pointer("/health/argv").is_none() => {
                        need(format!("/processes/{i}/health/argv"), "check command", None)
                    }
                    _ => {}
                }
                if p.get("expose").and_then(Value::as_str) == Some("public") && p.get("port").is_none_or(Value::is_null) {
                    need(format!("/processes/{i}/port"), "port it listens on", None);
                }
            }
        }
    }
    for (i, s) in v.get("secrets").and_then(Value::as_array).into_iter().flatten().enumerate() {
        if s.get("name").is_none_or(Value::is_null) {
            need(format!("/secrets/{i}/name"), "secret name, e.g. STRIPE_KEY", None);
        }
        if s.get("reach").and_then(Value::as_array).is_none_or(Vec::is_empty) {
            need(format!("/secrets/{i}/reach"), "hosts this secret is sent to, comma-separated", None);
        }
    }
    for (i, d) in v.get("domains").and_then(Value::as_array).into_iter().flatten().enumerate() {
        if d.get("hostname").is_none_or(Value::is_null) {
            need(format!("/domains/{i}/hostname"), "hostname", None);
        }
        if d.get("verification").is_none_or(Value::is_null) {
            need(format!("/domains/{i}/verification"), "cname, apex or ns_delegated", Some(json!("cname")));
        }
    }
    if !has("/auth/mode") {
        need("/auth".into(), "none, password, magic_link, app_owned, oidc:google|github|microsoft", Some(json!({"mode": "none"})));
    }
    if !has("/data_class") {
        need("/data_class".into(), "none or pii", Some(json!("none")));
    }
    if !has("/owner/name") {
        need("/owner/name".into(), "who answers when it breaks", None);
    }
    if !has("/owner/email") {
        need("/owner/email".into(), "their email", None);
    }
    if !has("/budget/ceiling_usd_month") {
        need("/budget/ceiling_usd_month".into(), "monthly ceiling, whole US dollars", None);
    }
    if !has("/budget/degrade") {
        need("/budget/degrade".into(), "what happens at the ceiling", Some(json!("sleep")));
    }
    out
}

#[cfg(test)]
mod tests {
    use cloud_contract::draft::{Evidence, Strength};

    use super::*;

    fn ev() -> Evidence {
        Evidence { uri: "file:Dockerfile:3".into(), note: "EXPOSE 3000".into() }
    }

    fn base() -> Draft {
        let mut d = Draft::default();
        set(&mut d, "/source", json!({"kind": "archive", "hash": format!("blake3:{}", "a".repeat(64))}));
        d.offer("/build", json!({"kind": "dockerfile", "path": "Dockerfile", "context": "."}), Strength::Inferred, ev());
        d.offer("/processes/0/name", json!("web"), Strength::Inferred, ev());
        d.offer("/processes/0/port", json!(3000), Strength::Inferred, ev());
        d.offer("/processes/0/expose", json!("public"), Strength::Inferred, ev());
        d.offer("/secrets/0/name", json!("A_KEY"), Strength::Inferred, ev());
        d.offer("/secrets/1/name", json!("B_KEY"), Strength::Inferred, ev());
        d
    }

    fn run(d: &mut Draft, input: &str, accept: bool) -> (Result<Manifest>, String) {
        let mut out = Vec::new();
        let r = confirm(d, &[], &mut Prompter::new(input.as_bytes(), &mut out, accept));
        (r, String::from_utf8(out).unwrap())
    }

    const REQUIRED: &str = "shop\n/healthz\napi.b.com\n\n\nAna\nana@example.com\n25\n\n";

    #[test]
    fn corrections_drops_and_required_answers_make_a_valid_manifest() {
        let mut d = base();
        // /build, /processes/0/expose, /processes/0/name, /processes/0/port, /secrets/0/name, /secrets/1/name
        let input = format!("\n\n\n8080\n-\n\n{REQUIRED}");
        let (m, out) = run(&mut d, &input, false);
        let m = m.unwrap_or_else(|e| panic!("{e}\n{out}"));
        assert_eq!(m.processes[0].port, Some(8080));
        assert_eq!(d.fields["/processes/0/port"].strength, Strength::Declared);
        assert_eq!(m.secrets.len(), 1, "A_KEY was dropped and B_KEY renumbered");
        assert_eq!((m.secrets[0].name.as_str(), m.secrets[0].reach.as_slice()), ("B_KEY", ["api.b.com".to_owned()].as_slice()));
        assert_eq!(m.egress, ["api.b.com"], "reach is added to egress");
        assert_eq!(m.owner.email, "ana@example.com");
        assert!(out.contains("file:Dockerfile:3  EXPOSE 3000"), "{out}");
        assert!(cloud_contract::validate(&m).is_ok(), "{:?}", cloud_contract::validate(&m));
    }

    #[test]
    fn accept_inferred_skips_questions_but_not_required_fields() {
        let mut d = base();
        d.fields.remove("/secrets/1/name");
        let (m, _) = run(&mut d, REQUIRED, true);
        let m = m.unwrap();
        assert_eq!(m.processes[0].port, Some(3000));
        assert!(d.unconfirmed().is_empty());
    }

    #[test]
    fn end_of_input_never_accepts_silently() {
        let mut d = base();
        let (r, _) = run(&mut d, "", false);
        let msg = r.unwrap_err().to_string();
        assert!(msg.contains("--accept-inferred"), "{msg}");
        let mut d = base();
        let (r, _) = run(&mut d, "", true);
        assert!(r.unwrap_err().to_string().contains("/project is required"));
    }

    #[test]
    fn a_new_kind_drops_the_old_variants_fields() {
        let mut d = Draft::default();
        d.offer("/build/kind", json!("dockerfile"), Strength::Inferred, ev());
        d.offer("/build/path", json!("Dockerfile"), Strength::Inferred, ev());
        correct(&mut d, "/build/kind", json!("buildpack"));
        assert!(!d.fields.contains_key("/build/path"));
        assert_eq!(missing(&d.to_value().unwrap()).first().map(|n| n.pointer.as_str()), Some("/project"));
        assert!(missing(&d.to_value().unwrap()).iter().any(|n| n.pointer == "/build/builder"));
    }

    #[test]
    fn shorthands() {
        assert_eq!(parse_input("/processes/0/health", "/up"), json!({"kind": "http", "path": "/up"}));
        assert_eq!(parse_input("/processes/0/health", "pg_isready -q"), json!({"kind": "command", "argv": ["pg_isready", "-q"]}));
        assert_eq!(parse_input("/auth", "oidc:github"), json!({"mode": "oidc", "provider": {"kind": "github"}}));
        assert_eq!(parse_input("/secrets/0/reach", "api.x.com, API.Y.com"), json!(["api.x.com", "api.y.com"]));
        assert_eq!(parse_input("/processes/0/command", "node a.js"), json!(["node", "a.js"]));
        assert_eq!(parse_input("/processes/0/command", "a && b"), json!(["/bin/sh", "-c", "a && b"]));
        assert_eq!(parse_input("/processes/0/port", "8080"), json!(8080));
        assert_eq!(parse_input("/project", "shop"), json!("shop"));
    }
}
