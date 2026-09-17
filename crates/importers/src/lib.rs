//! Repo files → a manifest [`Draft`]. For projects not built with aho (or before any session
//! exists), the files a repository already ships say most of what the deploy manifest needs:
//! how to build, what to run, which port serves, how health is checked, which secrets exist.
//! Every value is an [`Strength::Inferred`] offer carrying `file:<path>:<line>` evidence, so the
//! confirm screen shows where it came from and a human confirms it.
//!
//! # Precedence
//!
//! Importers run independently; their findings are combined in a fixed order, and when two
//! disagree on a field the higher one wins (the loser's evidence is dropped, equal values merge
//! their evidence). Highest first:
//!
//! 1. `fly.toml`, 2. `render.yaml`, 3. `app.yaml` — written to describe a deployment;
//! 4. `Procfile` — the platform-neutral statement of what runs;
//! 5. `Dockerfile` — what the image does by default;
//! 6. `docker-compose.yml` (or `compose.yaml`, …) — usually local development;
//! 7. `package.json`, `pyproject.toml` / `requirements.txt` — language conventions only;
//! 8. `.env.example` — secret names (and `PORT`).
//!
//! Processes are matched across importers by name. Each importer names at most one *primary*
//! process (the one serving HTTP: Procfile `web`, fly's HTTP process group, render's first web
//! service, the Dockerfile's default command); the primary takes the name given by the
//! highest-precedence importer that names it, else `web`, and is always `/processes/0`.

#![forbid(unsafe_code)]

mod appyaml;
mod compose;
mod dockerfile;
mod envfile;
mod fly;
mod jsonpos;
mod node;
mod procfile;
mod python;
mod render;
mod shell;
mod yaml;

use std::path::Path;

use cloud_contract::draft::{Draft, DraftField, Evidence, Strength};
use serde_json::{Value, json};

/// Buildpack builder offered when a language manifest is the only build evidence.
pub const DEFAULT_BUILDER: &str = "paketobuildpacks/builder-jammy-base";

/// Importer names, highest precedence first (see the crate docs).
pub const PRECEDENCE: [&str; 9] =
    ["fly.toml", "render.yaml", "app.yaml", "Procfile", "Dockerfile", "docker-compose", "package.json", "python", ".env.example"];

/// One value an importer found, with where it found it.
#[derive(Clone, Debug, PartialEq)]
pub struct Fact {
    pub value: Value,
    pub evidence: Evidence,
}

impl Fact {
    pub(crate) fn new(value: Value, path: &str, line: usize, note: impl Into<String>) -> Self {
        Self { value, evidence: evidence(path, line, note) }
    }
}

/// What an importer knows about one process. Values use the manifest's JSON shapes.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ProcessFacts {
    pub name: Option<Fact>,
    /// argv array.
    pub command: Option<Fact>,
    pub port: Option<Fact>,
    /// `"public"` / `"internal"`.
    pub expose: Option<Fact>,
    /// A `Health` object.
    pub health: Option<Fact>,
    /// A `Volume` object.
    pub volume: Option<Fact>,
}

impl ProcessFacts {
    fn facts(&self) -> [(&'static str, Option<&Fact>); 6] {
        [
            ("name", self.name.as_ref()),
            ("command", self.command.as_ref()),
            ("port", self.port.as_ref()),
            ("expose", self.expose.as_ref()),
            ("health", self.health.as_ref()),
            ("volume", self.volume.as_ref()),
        ]
    }

    fn first_evidence(&self) -> Option<&Evidence> {
        self.facts().into_iter().find_map(|(_, f)| f.map(|f| &f.evidence))
    }
}

/// Everything one importer found in one file (or file family).
#[derive(Clone, Debug, PartialEq)]
pub struct Findings {
    /// One of [`PRECEDENCE`].
    pub importer: &'static str,
    pub project: Option<Fact>,
    /// A `Build` object.
    pub build: Option<Fact>,
    /// The process serving HTTP, if this file says which one that is.
    pub primary: Option<ProcessFacts>,
    /// Other processes, by (slug) name.
    pub named: Vec<(String, ProcessFacts)>,
    /// Secret names.
    pub secrets: Vec<Fact>,
    /// Hostnames.
    pub domains: Vec<Fact>,
    /// Parse errors and skipped entries, `<file>:<line>: <why>`.
    pub problems: Vec<String>,
}

impl Findings {
    pub(crate) fn new(importer: &'static str) -> Self {
        Self {
            importer,
            project: None,
            build: None,
            primary: None,
            named: Vec::new(),
            secrets: Vec::new(),
            domains: Vec::new(),
            problems: Vec::new(),
        }
    }

    pub(crate) fn failed(importer: &'static str, problem: String) -> Self {
        let mut f = Self::new(importer);
        f.problems.push(problem);
        f
    }

    pub(crate) fn primary_mut(&mut self) -> &mut ProcessFacts {
        self.primary.get_or_insert_with(ProcessFacts::default)
    }

    /// This importer's findings alone, as a draft.
    pub fn draft(&self) -> Draft {
        combine(std::slice::from_ref(self))
    }
}

/// The combined result of every importer that found its file.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Scan {
    pub draft: Draft,
    /// Importers that found their file, highest precedence first.
    pub found: Vec<&'static str>,
    pub problems: Vec<String>,
}

/// Runs every importer on `root` and combines them by [`PRECEDENCE`].
pub fn import_dir(root: &Path) -> Draft {
    scan(root).draft
}

/// [`import_dir`], plus which importers matched and what they could not read.
pub fn scan(root: &Path) -> Scan {
    let fly = fly(root);
    let render = render(root);
    // A platform config may name a Dockerfile other than `./Dockerfile`; read the one it names.
    let dockerfile_path = [&fly, &render]
        .into_iter()
        .flatten()
        .filter_map(|f| f.build.as_ref())
        .find_map(|b| (b.value["kind"] == "dockerfile").then(|| b.value["path"].as_str().map(str::to_owned)).flatten())
        .unwrap_or_else(|| "Dockerfile".to_owned());
    let all: Vec<Findings> = [
        fly,
        render,
        app_yaml(root),
        procfile(root),
        dockerfile(root, &dockerfile_path),
        compose(root),
        package_json(root),
        python(root),
        env_example(root),
    ]
    .into_iter()
    .flatten()
    .collect();
    Scan {
        draft: combine(&all),
        found: all.iter().map(|f| f.importer).collect(),
        problems: all.iter().flat_map(|f| f.problems.iter().cloned()).collect(),
    }
}

/// `Dockerfile` (or the file at `rel`): `EXPOSE`, `CMD`/`ENTRYPOINT`, `HEALTHCHECK` of the final
/// stage, and `build.kind = dockerfile`.
pub fn dockerfile(root: &Path, rel: &str) -> Option<Findings> {
    dockerfile::import(root, rel)
}

/// `Procfile`: `web` is the primary process; `release` is a release-phase task and is skipped.
pub fn procfile(root: &Path) -> Option<Findings> {
    procfile::import(root)
}

/// `compose.yaml` / `compose.yml` / `docker-compose.yaml` / `docker-compose.yml`: services built
/// from this repository become processes; image-only services (databases, caches) are ignored.
pub fn compose(root: &Path) -> Option<Findings> {
    compose::import(root)
}

/// `fly.toml`: app name, build, process groups, HTTP service port and checks, mounts.
pub fn fly(root: &Path) -> Option<Findings> {
    fly::import(root)
}

/// `render.yaml`: web/worker/private services, build, start command, health path, disks,
/// unsynced env vars as secrets, custom domains.
pub fn render(root: &Path) -> Option<Findings> {
    render::import(root)
}

/// Google App Engine `app.yaml` (recognized by its `runtime` key).
pub fn app_yaml(root: &Path) -> Option<Findings> {
    appyaml::import(root)
}

/// `package.json`: name, `scripts.start`; a buildpack build when there is no `Dockerfile`.
pub fn package_json(root: &Path) -> Option<Findings> {
    node::import(root)
}

/// `pyproject.toml` / `requirements.txt`: name; a buildpack build when there is no `Dockerfile`.
pub fn python(root: &Path) -> Option<Findings> {
    python::import(root)
}

/// `.env.example`: variable names become secret names (values are never read); `PORT` becomes
/// the primary port; well-known non-secret settings are skipped.
pub fn env_example(root: &Path) -> Option<Findings> {
    envfile::import(root)
}

/// Combines findings given highest precedence first.
fn combine(all: &[Findings]) -> Draft {
    let mut draft = Draft::default();
    // Offers go lowest precedence first, so the highest lands last and wins equal-strength ties.
    for f in all.iter().rev() {
        if let Some(p) = &f.project {
            offer(&mut draft, "/project", p);
        }
        if let Some(b) = &f.build {
            offer(&mut draft, "/build", b);
        }
    }

    let primary_name = all.iter().find_map(|f| f.primary.as_ref()?.name.as_ref()?.value.as_str().map(str::to_owned));
    let has_primary = all.iter().any(|f| f.primary.is_some());
    let mut slots: Vec<String> = Vec::new();
    if has_primary {
        slots.push(primary_name.clone().unwrap_or_else(|| "web".to_owned()));
    }
    for f in all {
        for (name, _) in &f.named {
            if !slots.contains(name) {
                slots.push(name.clone());
            }
        }
    }
    for f in all.iter().rev() {
        if let Some(p) = &f.primary {
            emit_process(&mut draft, 0, p);
        }
        for (name, p) in &f.named {
            if let Some(i) = slots.iter().position(|s| s == name) {
                emit_process(&mut draft, i, p);
            }
        }
    }
    if has_primary && primary_name.is_none() {
        let seen = all.iter().filter_map(|f| f.primary.as_ref()).find_map(ProcessFacts::first_evidence);
        if let Some(e) = seen {
            let note = "no file names this process; `web` by convention";
            draft.offer("/processes/0/name", json!("web"), Strength::Inferred, Evidence { uri: e.uri.clone(), note: note.into() });
        }
    }

    keyed_list(&mut draft, all, "/secrets", "name", |f| &f.secrets);
    keyed_list(&mut draft, all, "/domains", "hostname", |f| &f.domains);
    draft
}

fn emit_process(draft: &mut Draft, i: usize, p: &ProcessFacts) {
    for (field, fact) in p.facts() {
        if let Some(fact) = fact {
            offer(draft, &format!("/processes/{i}/{field}"), fact);
        }
    }
}

/// Lists keyed by value (secret names, hostnames): one slot per distinct value, in the order the
/// highest-precedence importer lists them.
fn keyed_list(draft: &mut Draft, all: &[Findings], base: &str, field: &str, pick: fn(&Findings) -> &Vec<Fact>) {
    let mut slots: Vec<&Value> = Vec::new();
    for f in all {
        for fact in pick(f) {
            if !slots.contains(&&fact.value) {
                slots.push(&fact.value);
            }
        }
    }
    for f in all.iter().rev() {
        for fact in pick(f) {
            if let Some(i) = slots.iter().position(|v| *v == &fact.value) {
                offer(draft, &format!("{base}/{i}/{field}"), fact);
            }
        }
    }
}

fn offer(draft: &mut Draft, pointer: &str, fact: &Fact) {
    draft.offer(pointer, fact.value.clone(), Strength::Inferred, fact.evidence.clone());
}

/// A machine value that lost to a stronger, different one during [`merge`].
#[derive(Clone, Debug, PartialEq)]
pub struct Overruled {
    pub pointer: String,
    /// The value that stays.
    pub kept: Value,
    /// The value, strength and evidence that lost.
    pub offered: DraftField,
}

/// Merges `extra` into `target`.
///
/// - Object values are split into leaf fields first (`/build` → `/build/kind`, `/build/path`, …),
///   so drafts written at different granularity (importers write whole objects, `cloud-draft`
///   writes leaves) line up.
/// - List elements align by identity, not index: processes and secrets by `name`, domains by
///   `hostname`, egress and secret reach by value.
/// - Tagged objects (`kind` / `mode`) are atomic: when the tags differ, the stronger side's
///   whole object wins (ties go to `extra`), so no field of the losing variant is left behind.
/// - Everything else follows [`Draft::offer`]: stronger values stay, equal values merge their
///   evidence; declared fields stay declared.
///
/// Returns the values `extra` offered that lost to a stronger, different value.
pub fn merge(target: &mut Draft, extra: &Draft) -> Vec<Overruled> {
    let mut base = explode(target);
    let extra = explode(extra);
    let mut map = IndexMap::default();
    for (list, key) in [("/processes", "name"), ("/secrets", "name"), ("/domains", "hostname")] {
        align_keyed(&base, &extra, list, key, &mut map);
    }
    align_scalars(&base, &extra, "/egress", &mut map);
    let mut overruled = Vec::new();

    let mut losing_groups: Vec<String> = Vec::new();
    for (pointer, field) in &extra.fields {
        let p = map.rewrite(pointer);
        let Some(group) = p.strip_suffix("/kind").or_else(|| p.strip_suffix("/mode")) else { continue };
        let Some(existing) = base.fields.get(&p) else { continue };
        if existing.value == field.value {
            continue;
        }
        if field.strength >= existing.strength {
            let inside = format!("{group}/");
            base.fields.retain(|k, _| !k.starts_with(&inside));
        } else {
            overruled.push(Overruled { pointer: p.clone(), kept: existing.value.clone(), offered: field.clone() });
            losing_groups.push(format!("{group}/"));
        }
    }
    for (pointer, field) in &extra.fields {
        let mut p = map.rewrite(pointer);
        if losing_groups.iter().any(|g| p.starts_with(g.as_str())) {
            continue;
        }
        // Reach lists live inside secrets, so align them once the secret's index is known.
        if let Some(secret) = reach_secret(&p) {
            p = align_reach(&base, &extra, pointer, &secret);
        }
        put(&mut base, &p, field, &mut overruled);
    }
    *target = base;
    overruled
}

/// A confirmed manifest as declared fields, at the granularity [`merge`] aligns: objects and
/// lists split into leaves, argv arrays (`command`, `argv`) kept whole, empty lists omitted.
pub fn from_manifest(manifest: &cloud_contract::Manifest) -> Draft {
    let mut d = Draft::default();
    flatten(&mut d, "", &serde_json::to_value(manifest).unwrap_or(Value::Null));
    d
}

fn flatten(d: &mut Draft, pointer: &str, value: &Value) {
    let whole = pointer.ends_with("/command") || pointer.ends_with("/argv");
    match value {
        Value::Null => {}
        Value::Object(map) => {
            for (k, v) in map {
                flatten(d, &format!("{pointer}/{}", k.replace('~', "~0").replace('/', "~1")), v);
            }
        }
        Value::Array(items) if !whole => {
            for (i, v) in items.iter().enumerate() {
                flatten(d, &format!("{pointer}/{i}"), v);
            }
        }
        Value::Array(items) if items.is_empty() => {}
        other => d.declare(pointer, other.clone()),
    }
}

/// Splits object-valued fields into one field per leaf, keeping strength, evidence and
/// confirmation.
fn explode(d: &Draft) -> Draft {
    let mut out = Draft::default();
    for (pointer, field) in &d.fields {
        explode_into(&mut out, pointer, field);
    }
    out
}

fn explode_into(out: &mut Draft, pointer: &str, field: &DraftField) {
    match &field.value {
        Value::Object(map) if !map.is_empty() => {
            for (k, v) in map {
                let leaf = DraftField { value: v.clone(), ..field.clone() };
                explode_into(out, &format!("{pointer}/{}", k.replace('~', "~0").replace('/', "~1")), &leaf);
            }
        }
        _ => {
            out.fields.insert(pointer.to_owned(), field.clone());
        }
    }
}

fn put(target: &mut Draft, pointer: &str, field: &DraftField, overruled: &mut Vec<Overruled>) {
    if let Some(existing) = target.fields.get(pointer)
        && existing.strength > field.strength
    {
        if existing.value != field.value {
            overruled.push(Overruled { pointer: pointer.to_owned(), kept: existing.value.clone(), offered: field.clone() });
        }
        return;
    }
    if field.strength == Strength::Declared {
        target.declare(pointer, field.value.clone());
        return;
    }
    if field.evidence.is_empty() {
        target.fields.insert(pointer.to_owned(), field.clone());
        return;
    }
    for e in &field.evidence {
        target.offer(pointer, field.value.clone(), field.strength, e.clone());
    }
    if field.confirmed
        && let Some(f) = target.fields.get_mut(pointer)
        && f.value == field.value
    {
        f.confirmed = true;
    }
}

/// Prefix rewrites `<list>/<i>` → `<list>/<j>`.
#[derive(Default)]
struct IndexMap(Vec<(String, String)>);

impl IndexMap {
    fn rewrite(&self, pointer: &str) -> String {
        for (from, to) in &self.0 {
            if pointer == from {
                return to.clone();
            }
            if let Some(rest) = pointer.strip_prefix(from.as_str())
                && rest.starts_with('/')
            {
                return format!("{to}{rest}");
            }
        }
        pointer.to_owned()
    }
}

/// Index of each element of `list` in `draft`, keyed by `<list>/<i>/<key>` values.
fn keys_of(draft: &Draft, list: &str, key: &str) -> Vec<(usize, Value)> {
    let mut out = Vec::new();
    for (pointer, field) in &draft.fields {
        let Some(rest) = pointer.strip_prefix(list).and_then(|r| r.strip_prefix('/')) else { continue };
        let Some((idx, tail)) = rest.split_once('/') else { continue };
        if tail == key
            && let Ok(i) = idx.parse::<usize>()
        {
            out.push((i, field.value.clone()));
        }
    }
    out
}

fn indices_of(draft: &Draft, list: &str) -> Vec<usize> {
    let mut out: Vec<usize> =
        draft.fields.keys().filter_map(|p| p.strip_prefix(list)?.strip_prefix('/')?.split('/').next()?.parse().ok()).collect();
    out.sort_unstable();
    out.dedup();
    out
}

fn align_keyed(target: &Draft, extra: &Draft, list: &str, key: &str, map: &mut IndexMap) {
    let theirs = keys_of(target, list, key);
    let mut next = indices_of(target, list).last().map_or(0, |i| i + 1);
    for i in indices_of(extra, list) {
        let own_key = keys_of(extra, list, key).into_iter().find(|(j, _)| *j == i).map(|(_, v)| v);
        let j = match own_key.and_then(|k| theirs.iter().find(|(_, v)| *v == k).map(|(j, _)| *j)) {
            Some(j) => j,
            None => {
                next += 1;
                next - 1
            }
        };
        map.0.push((format!("{list}/{i}"), format!("{list}/{j}")));
    }
}

fn align_scalars(target: &Draft, extra: &Draft, list: &str, map: &mut IndexMap) {
    let theirs: Vec<(usize, &Value)> =
        target.fields.iter().filter_map(|(p, f)| Some((p.strip_prefix(list)?.strip_prefix('/')?.parse().ok()?, &f.value))).collect();
    let mut next = theirs.iter().map(|(i, _)| i + 1).max().unwrap_or(0);
    for (p, f) in &extra.fields {
        let Some(i) = p.strip_prefix(list).and_then(|r| r.strip_prefix('/')).and_then(|r| r.parse::<usize>().ok()) else { continue };
        let j = match theirs.iter().find(|(_, v)| **v == f.value) {
            Some((j, _)) => *j,
            None => {
                next += 1;
                next - 1
            }
        };
        map.0.push((format!("{list}/{i}"), format!("{list}/{j}")));
    }
}

/// `/secrets/<i>/reach/<j>` → `/secrets/<i>`.
fn reach_secret(pointer: &str) -> Option<String> {
    let rest = pointer.strip_prefix("/secrets/")?;
    let (i, tail) = rest.split_once('/')?;
    let j = tail.strip_prefix("reach/")?;
    (!j.contains('/')).then(|| format!("/secrets/{i}"))
}

/// Where `extra`'s reach entry at `original` lands under `secret`: the index of an equal host,
/// else the next free one.
fn align_reach(target: &Draft, extra: &Draft, original: &str, secret: &str) -> String {
    let base = format!("{secret}/reach");
    let value = extra.fields.get(original).map(|f| &f.value);
    let existing: Vec<(usize, &Value)> =
        target.fields.iter().filter_map(|(p, f)| Some((p.strip_prefix(&base)?.strip_prefix('/')?.parse().ok()?, &f.value))).collect();
    if let Some((j, _)) = existing.iter().find(|(_, v)| Some(*v) == value) {
        return format!("{base}/{j}");
    }
    let next = existing.iter().map(|(i, _)| i + 1).max().unwrap_or(0);
    format!("{base}/{next}")
}

pub(crate) fn evidence(path: &str, line: usize, note: impl Into<String>) -> Evidence {
    Evidence { uri: format!("file:{path}:{line}"), note: note.into() }
}

pub(crate) fn read(root: &Path, rel: &str) -> Option<Result<String, String>> {
    let path = root.join(rel);
    if !path.is_file() {
        return None;
    }
    Some(std::fs::read_to_string(&path).map_err(|e| format!("{rel}: {e}")))
}

/// Lowercase slug (`[a-z][a-z0-9-]*`, at most `max` chars) from a free-form name; scoped npm
/// names keep their last segment.
pub(crate) fn slug(raw: &str, max: usize) -> Option<String> {
    let base = raw.rsplit('/').next().unwrap_or(raw);
    let mut s = String::new();
    for c in base.chars() {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            s.push(c);
        } else if !s.is_empty() && !s.ends_with('-') {
            s.push('-');
        }
    }
    let trimmed = s.trim_start_matches(|c: char| !c.is_ascii_lowercase());
    let cut: String = trimmed.chars().take(max).collect();
    let out = cut.trim_end_matches('-');
    (!out.is_empty()).then(|| out.to_owned())
}

/// 1-based line of byte `offset` in `text`.
pub(crate) fn line_of(text: &str, offset: usize) -> usize {
    text.as_bytes()[..offset.min(text.len())].iter().filter(|b| **b == b'\n').count() + 1
}

/// `Health` from a check command: `curl`/`wget` against a local URL is an HTTP check on that
/// path; anything else runs as a command. `port` is the process port, when known.
pub(crate) fn health_from_argv(argv: &[String], port: Option<u16>) -> Value {
    let words: Vec<&str> = argv.iter().flat_map(|a| a.split_whitespace()).collect();
    let fetches = words.iter().any(|w| {
        let base = w.rsplit('/').next().unwrap_or(w);
        base == "curl" || base == "wget"
    });
    if fetches && let Some((url_port, path)) = words.iter().find_map(|w| local_http_url(w)) {
        return match port {
            Some(p) if p == url_port => json!({"kind": "http", "path": path}),
            _ => json!({"kind": "http", "path": path, "port": url_port}),
        };
    }
    json!({"kind": "command", "argv": argv})
}

/// `http://localhost:8080/healthz` → (8080, `/healthz`), for loopback hosts only.
fn local_http_url(word: &str) -> Option<(u16, String)> {
    let w = word.trim_matches(|c| c == '"' || c == '\'');
    let rest = w.strip_prefix("http://")?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], rest[i..].to_owned()),
        None => (rest, "/".to_owned()),
    };
    let (host, port) = match authority.strip_prefix('[') {
        Some(v6) => {
            let (h, after) = v6.split_once(']')?;
            (h, if after.is_empty() { 80 } else { after.strip_prefix(':')?.parse().ok()? })
        }
        None => match authority.rsplit_once(':') {
            Some((h, p)) => (h, p.parse().ok()?),
            None => (authority, 80),
        },
    };
    matches!(host, "localhost" | "127.0.0.1" | "0.0.0.0" | "::1").then_some((port, path))
}

/// Megabytes from fly/render size strings: `1gb`, `500mb`, `10GB`, `3` (GB).
pub(crate) fn size_mb(raw: &str) -> Option<u32> {
    let s = raw.trim().to_ascii_lowercase();
    let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
    let n: u32 = digits.parse().ok()?;
    match s[digits.len()..].trim() {
        "" | "g" | "gb" | "gib" => n.checked_mul(1024),
        "m" | "mb" | "mib" => Some(n),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fact(v: Value, line: usize) -> Fact {
        Fact::new(v, "f", line, "n")
    }

    #[test]
    fn slug_normalizes_names() {
        assert_eq!(slug("@acme/My_App", 40).as_deref(), Some("my-app"));
        assert_eq!(slug("9lives--api-", 40).as_deref(), Some("lives-api"));
        assert_eq!(slug("___", 40), None);
        assert_eq!(slug("abcdefghij", 4).as_deref(), Some("abcd"));
    }

    #[test]
    fn health_recognizes_local_http_checks_only() {
        let argv = |s: &str| s.split(' ').map(str::to_owned).collect::<Vec<_>>();
        assert_eq!(
            health_from_argv(&argv("curl -fsS http://localhost:8080/healthz"), Some(8080)),
            json!({"kind": "http", "path": "/healthz"})
        );
        assert_eq!(
            health_from_argv(&argv("wget -qO- http://127.0.0.1:9000/"), Some(8080)),
            json!({"kind": "http", "path": "/", "port": 9000})
        );
        assert_eq!(health_from_argv(&argv("curl https://example.com/"), None)["kind"], "command");
        assert_eq!(health_from_argv(&argv("pg_isready"), None), json!({"kind": "command", "argv": ["pg_isready"]}));
    }

    #[test]
    fn sizes() {
        assert_eq!(size_mb("1gb"), Some(1024));
        assert_eq!(size_mb("500MB"), Some(500));
        assert_eq!(size_mb("3"), Some(3072));
        assert_eq!(size_mb("lots"), None);
    }

    #[test]
    fn higher_precedence_wins_and_equal_values_merge_evidence() {
        let mut high = Findings::new("fly.toml");
        high.primary_mut().port = Some(fact(json!(8080), 1));
        let mut low = Findings::new("Dockerfile");
        low.primary_mut().port = Some(fact(json!(3000), 2));
        low.primary_mut().command = Some(fact(json!(["node", "a.js"]), 3));
        let d = combine(&[high.clone(), low]);
        assert_eq!(d.fields["/processes/0/port"].value, json!(8080));
        assert_eq!(d.fields["/processes/0/port"].evidence.len(), 1);
        assert_eq!(d.fields["/processes/0/command"].value, json!(["node", "a.js"]));
        assert_eq!(d.fields["/processes/0/name"].value, json!("web"));

        let mut same = Findings::new("Dockerfile");
        same.primary_mut().port = Some(fact(json!(8080), 7));
        let d = combine(&[high, same]);
        assert_eq!(d.fields["/processes/0/port"].evidence.len(), 2);
    }

    #[test]
    fn named_processes_matching_the_primary_name_merge_into_it() {
        let mut a = Findings::new("Procfile");
        a.primary_mut().name = Some(fact(json!("web"), 1));
        a.named.push(("worker".into(), ProcessFacts { name: Some(fact(json!("worker"), 2)), ..Default::default() }));
        let mut b = Findings::new("docker-compose");
        b.named.push((
            "web".into(),
            ProcessFacts { name: Some(fact(json!("web"), 5)), port: Some(fact(json!(3000), 6)), ..Default::default() },
        ));
        let d = combine(&[a, b]);
        assert_eq!(d.fields["/processes/0/name"].value, json!("web"));
        assert_eq!(d.fields["/processes/0/port"].value, json!(3000));
        assert_eq!(d.fields["/processes/1/name"].value, json!("worker"));
        assert!(!d.fields.contains_key("/processes/2/name"));
    }

    #[test]
    fn merge_aligns_lists_by_identity() {
        let ev = || evidence("x", 1, "n");
        let mut target = Draft::default();
        target.offer("/processes/0/name", json!("web"), Strength::Inferred, ev());
        target.offer("/processes/0/port", json!(3000), Strength::Inferred, ev());
        target.offer("/secrets/0/name", json!("A"), Strength::Inferred, ev());
        target.offer("/egress/0", json!("api.x.com"), Strength::Inferred, ev());
        let mut extra = Draft::default();
        extra.offer("/processes/0/name", json!("worker"), Strength::Chained, ev());
        extra.offer("/processes/1/name", json!("web"), Strength::Chained, ev());
        extra.offer("/processes/1/port", json!(8080), Strength::Chained, ev());
        extra.offer("/secrets/0/name", json!("B"), Strength::Chained, ev());
        extra.offer("/secrets/1/name", json!("A"), Strength::Chained, ev());
        extra.offer("/secrets/1/reach/0", json!("api.x.com"), Strength::Chained, ev());
        extra.offer("/egress/0", json!("api.y.com"), Strength::Observed, ev());
        extra.offer("/egress/1", json!("api.x.com"), Strength::Observed, ev());
        merge(&mut target, &extra);
        let v = |p: &str| target.fields[p].value.clone();
        assert_eq!((v("/processes/0/name"), v("/processes/0/port")), (json!("web"), json!(8080)));
        assert_eq!(v("/processes/1/name"), json!("worker"));
        assert_eq!((v("/secrets/0/name"), v("/secrets/0/reach/0")), (json!("A"), json!("api.x.com")));
        assert_eq!(v("/secrets/1/name"), json!("B"));
        assert_eq!((v("/egress/0"), v("/egress/1")), (json!("api.x.com"), json!("api.y.com")));
        assert_eq!(target.fields["/egress/0"].strength, Strength::Observed);
        assert!(target.to_value().is_ok());
    }

    #[test]
    fn tagged_objects_merge_atomically() {
        let ev = || evidence("x", 1, "n");
        let builder = json!({"kind": "buildpack", "builder": DEFAULT_BUILDER});
        // A chained dockerfile build replaces an inferred buildpack guess, builder and all.
        let mut target = Draft::default();
        target.offer("/build", builder.clone(), Strength::Inferred, ev());
        let mut session = Draft::default();
        session.offer("/build/kind", json!("dockerfile"), Strength::Chained, ev());
        session.offer("/build/path", json!("Dockerfile"), Strength::Chained, ev());
        assert!(merge(&mut target, &session).is_empty());
        assert_eq!(target.to_value().unwrap()["build"], json!({"kind": "dockerfile", "path": "Dockerfile"}));

        // A weaker different variant loses whole and is reported.
        let mut target = Draft::default();
        target.declare("/build", builder.clone());
        let lost = merge(&mut target, &session);
        assert_eq!(target.to_value().unwrap()["build"], builder);
        assert_eq!(lost.len(), 1);
        assert_eq!(
            (lost[0].pointer.as_str(), &lost[0].kept, &lost[0].offered.value),
            ("/build/kind", &json!("buildpack"), &json!("dockerfile"))
        );
    }

    #[test]
    fn declared_values_survive_and_report_what_they_overrule() {
        let m: cloud_contract::Manifest = serde_json::from_value(json!({
            "api_version": "deploy.aho.cloud/v1", "project": "demo",
            "source": {"kind": "archive", "hash": format!("blake3:{}", "0".repeat(64))},
            "build": {"kind": "dockerfile", "path": "Dockerfile", "context": "."},
            "processes": [{"name": "web", "command": ["node", "a.js"], "port": 3000, "expose": "public", "health": {"kind": "http", "path": "/"}}],
            "egress": ["api.x.com"],
            "auth": {"mode": "none"}, "data_class": "none",
            "owner": {"name": "A", "email": "a@x.io"}, "budget": {"ceiling_usd_month": 5, "degrade": "sleep"}
        }))
        .unwrap();
        let mut d = from_manifest(&m);
        assert!(d.unconfirmed().is_empty());
        assert_eq!(d.fields["/processes/0/command"].value, json!(["node", "a.js"]));
        assert_eq!(d.into_manifest().unwrap(), m);

        let ev = || evidence("x", 1, "n");
        let mut session = Draft::default();
        session.offer("/processes/0/name", json!("web"), Strength::Inferred, ev());
        session.offer("/processes/0/port", json!(8080), Strength::Chained, ev());
        session.offer("/egress/0", json!("api.y.com"), Strength::Observed, ev());
        let lost = merge(&mut d, &session);
        assert_eq!(lost.iter().map(|o| o.pointer.as_str()).collect::<Vec<_>>(), ["/processes/0/port"]);
        assert_eq!(d.fields["/processes/0/port"].value, json!(3000));
        assert_eq!(d.unconfirmed(), ["/egress/1"]);
    }
}
