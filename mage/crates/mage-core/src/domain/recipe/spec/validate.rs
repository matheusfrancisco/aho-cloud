//! Recipe validation (executor.md "Validation rules"). Pure; every rule runs on every input, so
//! one call reports all issues.
//!
//! Field roles (which rules see which strings):
//! - command fields — every `run`, `health.command`, `database.migrate`/`seed`, `[fix].lint` —
//!   get `AbsoluteHostPath` and `HardcodedPort`;
//! - env values (top-level and per build step / service / test) get `AbsoluteHostPath`,
//!   `HardcodedPort`, and `ExternalUrl`;
//! - URL fields (`health.http`/`tcp`, `database.url`, `[browser]`) get `HardcodedPort`;
//!   `[browser]` URLs also get `ExternalUrl` (`health.http` and `database.url` must be loopback,
//!   an error under `ServiceShape` / `Database`);
//! - path fields (`cwd`, `compose.file`, `inputs`, `[fix].generated`, `[fix].regression_dir`) get
//!   `PathNotRelative` and are not templates;
//! - every string value gets `LiveSecret`.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

use super::canonical::canonical_len;
use super::scan::{
    absolute_host_path, external_host, hardcoded_port, is_env_key, is_extension_name, is_loopback_host, is_name, is_port_name,
    is_reserved_env, is_secret_key, live_secret, looks_fake, normalize_path, path_problem, url_host,
};
use super::template::{TemplatePart, template_parts, vars};
use super::{DbProvider, HealthSpec, RECIPE_MAX_BYTES, RECIPE_VERSION, RecipeSpec};

const DESCRIPTION_MAX_BYTES: usize = 2 * 1024;
const MAX_BUILD_STEPS: usize = 32;
const MAX_SERVICES: usize = 16;
const MAX_PORTS: usize = 32;
const MAX_ENV_VARS: usize = 64;
const MAX_SCIP: usize = 8;
const MAX_TIMEOUT_S: u32 = 3600;
const DB_VARS: [&str; 6] = ["url", "host", "port", "name", "user", "password"];

/// Validation rule identifiers (executor.md "Validation rules").
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "snake_case")]
pub enum IssueCode {
    /// `version == 1`.
    Version,
    /// Canonical TOML ≤ 64 KiB; description ≤ 2 KiB; ≤ 32 build steps, 16 services, 32 ports,
    /// 64 env vars per table, 8 scip entries.
    TooLarge,
    /// Port, service, build, and scip names match their regex and are unique; env keys match
    /// `^[A-Z_][A-Z0-9_]{0,127}$`.
    BadName,
    /// A path field is absolute, has `..`, `~`, `\`, NUL, a template, or is under `.git/`.
    PathNotRelative,
    /// A command or env value names a host path outside `/bin/`, `/usr/bin/`, `/dev/null`.
    AbsoluteHostPath,
    /// `127.0.0.1:<digits>` and friends instead of `${port.<name>}`.
    HardcodedPort,
    /// `${port.x}` where `x` is not declared in `ports`.
    UnknownPort,
    /// A declared port without exactly one owning service, or a service owning an undeclared port.
    PortOwnership,
    /// An unknown variable, a variable used where it is not available, or an unterminated `${`.
    /// As a warning: `${ws}` in a build step.
    UnknownVariable,
    /// Not exactly one of `run`/`compose`; missing health or not exactly one probe;
    /// `compose.ports` keys outside the service's `ports`; `cwd` on a compose service;
    /// a non-loopback `health.http`.
    ServiceShape,
    /// `depends_on` (or a fake's `service`) names an unknown service, or the graph has a cycle.
    DependsOn,
    /// An env key the runner owns or that changes process behaviour.
    ReservedEnv,
    /// A value matches a live-credential pattern.
    LiveSecret,
    /// A secret-looking env key whose value is neither a template reference nor marked fake.
    NotFakeSecret,
    /// Warning: a URL value whose host is not loopback or a template reference.
    ExternalUrl,
    /// `[database]` provider rules and extension names.
    Database,
    /// `browser.base_url` does not reference a declared port.
    BrowserBaseUrl,
    /// A `[[scip]].run` without `${scip.out}`.
    ScipOut,
    /// A `timeout_s` outside `1..=3600`, or a zero service/probe duration.
    Timeouts,
    /// Warning: no `[test]`; the Fix gate refuses runs for this recipe.
    NoTest,
    /// A key the schema does not define. Reported by [`RecipeSpec::parse`]
    /// (`RecipeParseError::UnknownKey`), never by `validate`.
    UnknownKey,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
pub struct RecipeIssue {
    /// Dotted field path with `[i]` for array items, e.g. `services.api.env.PORT`,
    /// `build[0].inputs[1]`; empty for the whole document.
    pub path: String,
    pub code: IssueCode,
    pub message: String,
}

/// Any error blocks `submit_recipe`, the Boot gate, and prepare. Warnings are returned to the
/// agent and stored with the recipe.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
pub struct RecipeReport {
    pub errors: Vec<RecipeIssue>,
    pub warnings: Vec<RecipeIssue>,
}

impl RecipeReport {
    pub fn is_valid(&self) -> bool {
        self.errors.is_empty()
    }
}

impl RecipeSpec {
    pub fn validate(&self) -> RecipeReport {
        let mut v = Validator { spec: self, report: RecipeReport::default() };
        v.top_level();
        v.build();
        v.database();
        v.services();
        v.test();
        v.browser();
        v.fakes();
        v.scip();
        v.fix();
        v.report
    }
}

/// Where a template is rendered; decides which variables it may use.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Scope {
    Env,
    Build,
    Service,
    DbUrl,
    DbCommand,
    Test,
    Browser,
    Scip,
    Lint,
}

impl Scope {
    /// `${port.*}` and `${service.*.url}`.
    fn ports(self) -> bool {
        matches!(self, Self::Env | Self::Service | Self::DbUrl | Self::DbCommand | Self::Test | Self::Browser)
    }

    /// `${db.*}`.
    fn db(self) -> bool {
        matches!(self, Self::Env | Self::Service | Self::DbCommand | Self::Test)
    }

    fn label(self) -> &'static str {
        match self {
            Self::Env => "[env] values",
            Self::Build => "build steps",
            Self::Service => "services",
            Self::DbUrl => "database.url",
            Self::DbCommand => "database.migrate / database.seed",
            Self::Test => "[test]",
            Self::Browser => "[browser]",
            Self::Scip => "[[scip]]",
            Self::Lint => "[fix].lint",
        }
    }
}

#[derive(Clone, Copy)]
struct Role {
    scope: Option<Scope>,
    command: bool,
    env: bool,
    ports: bool,
    external: bool,
}

impl Role {
    const PLAIN: Self = Self { scope: None, command: false, env: false, ports: false, external: false };

    fn command(scope: Scope) -> Self {
        Self { scope: Some(scope), command: true, ..Self::PLAIN }
    }

    fn env(scope: Scope) -> Self {
        Self { scope: Some(scope), env: true, ..Self::PLAIN }
    }

    fn url(scope: Scope) -> Self {
        Self { scope: Some(scope), ports: true, ..Self::PLAIN }
    }

    fn browser_url() -> Self {
        Self { scope: Some(Scope::Browser), ports: true, external: true, ..Self::PLAIN }
    }
}

#[derive(Clone, Copy)]
enum Seg<'a> {
    Key(&'a str),
    Index(usize),
}

const MAX_DEPTH: usize = 5;

/// Field path rendered only when an issue is reported.
#[derive(Clone, Copy)]
struct FieldPath<'a> {
    segs: [Seg<'a>; MAX_DEPTH],
    len: usize,
}

impl<'a> FieldPath<'a> {
    const ROOT: Self = Self { segs: [Seg::Index(0); MAX_DEPTH], len: 0 };

    fn of(key: &'a str) -> Self {
        Self::ROOT.key(key)
    }

    fn key(self, key: &'a str) -> Self {
        self.push(Seg::Key(key))
    }

    fn index(self, i: usize) -> Self {
        self.push(Seg::Index(i))
    }

    fn push(mut self, seg: Seg<'a>) -> Self {
        if let Some(slot) = self.segs.get_mut(self.len) {
            *slot = seg;
            self.len += 1;
        }
        self
    }

    fn render(&self) -> String {
        let mut out = String::new();
        for seg in &self.segs[..self.len] {
            match seg {
                Seg::Key(key) => {
                    if !out.is_empty() {
                        out.push('.');
                    }
                    out.push_str(key);
                }
                Seg::Index(i) => {
                    let _ = write!(out, "[{i}]");
                }
            }
        }
        out
    }
}

struct Validator<'a> {
    spec: &'a RecipeSpec,
    report: RecipeReport,
}

impl<'a> Validator<'a> {
    fn error(&mut self, path: FieldPath<'_>, code: IssueCode, message: impl Into<String>) {
        self.report.errors.push(RecipeIssue { path: path.render(), code, message: message.into() });
    }

    fn warn(&mut self, path: FieldPath<'_>, code: IssueCode, message: impl Into<String>) {
        self.report.warnings.push(RecipeIssue { path: path.render(), code, message: message.into() });
    }

    fn top_level(&mut self) {
        let spec = self.spec;
        if spec.version != RECIPE_VERSION {
            self.error(FieldPath::of("version"), IssueCode::Version, format!("version must be {RECIPE_VERSION}, got {}", spec.version));
        }
        let size = canonical_len(spec);
        if size > RECIPE_MAX_BYTES {
            self.error(
                FieldPath::ROOT,
                IssueCode::TooLarge,
                format!("recipe is {size} bytes in canonical form; the limit is {RECIPE_MAX_BYTES}"),
            );
        }
        if let Some(description) = &spec.description {
            let p = FieldPath::of("description");
            if description.len() > DESCRIPTION_MAX_BYTES {
                self.error(
                    p,
                    IssueCode::TooLarge,
                    format!("description is {} bytes; the limit is {DESCRIPTION_MAX_BYTES}", description.len()),
                );
            }
            self.value(p, description, Role::PLAIN);
        }
        self.count(FieldPath::of("ports"), spec.ports.len(), MAX_PORTS, "ports");
        self.count(FieldPath::of("build"), spec.build.len(), MAX_BUILD_STEPS, "build steps");
        self.count(FieldPath::of("services"), spec.services.len(), MAX_SERVICES, "services");
        self.count(FieldPath::of("scip"), spec.scip.len(), MAX_SCIP, "scip entries");
        for (i, port) in spec.ports.iter().enumerate() {
            let p = FieldPath::of("ports").index(i);
            if !is_port_name(port) {
                self.error(p, IssueCode::BadName, format!("port name `{port}` must match ^[a-z][a-z0-9_]{{0,31}}$"));
            }
            self.value(p, port, Role::PLAIN);
        }
        self.port_ownership();
        for (name, version) in &spec.toolchain {
            self.value(FieldPath::of("toolchain").key(name), version, Role::PLAIN);
        }
        self.env_table(FieldPath::of("env"), &spec.env, Scope::Env);
        if spec.test.is_none() {
            self.warn(FieldPath::of("test"), IssueCode::NoTest, "no [test]: the Fix gate refuses runs for this recipe");
        }
    }

    fn port_ownership(&mut self) {
        let spec = self.spec;
        let mut seen = BTreeSet::new();
        for (i, port) in spec.ports.iter().enumerate() {
            if !seen.insert(port.as_str()) {
                continue;
            }
            match spec.services.values().filter(|svc| svc.ports.contains(port)).count() {
                1 => {}
                0 => self.error(
                    FieldPath::of("ports").index(i),
                    IssueCode::PortOwnership,
                    format!("port `{port}` is not owned by any service"),
                ),
                _ => {
                    let owners: Vec<&str> =
                        spec.services.iter().filter(|(_, svc)| svc.ports.contains(port)).map(|(n, _)| n.as_str()).collect();
                    self.error(
                        FieldPath::of("ports").index(i),
                        IssueCode::PortOwnership,
                        format!("port `{port}` is owned by several services: {}", owners.join(", ")),
                    );
                }
            }
        }
        for (name, svc) in &spec.services {
            for (j, port) in svc.ports.iter().enumerate() {
                if !spec.ports.contains(port) {
                    self.error(
                        FieldPath::of("services").key(name).key("ports").index(j),
                        IssueCode::PortOwnership,
                        format!("port `{port}` is not declared in the top-level `ports`"),
                    );
                }
            }
        }
    }

    fn build(&mut self) {
        let spec = self.spec;
        let mut names = BTreeSet::new();
        for (i, step) in spec.build.iter().enumerate() {
            let p = FieldPath::of("build").index(i);
            self.name(p.key("name"), &step.name, "build step");
            if !names.insert(step.name.as_str()) {
                self.error(p.key("name"), IssueCode::BadName, format!("duplicate build step name `{}`", step.name));
            }
            self.value(p.key("run"), &step.run, Role::command(Scope::Build));
            self.path(p.key("cwd"), &step.cwd);
            self.timeout(p.key("timeout_s"), step.timeout_s);
            self.env_table(p.key("env"), &step.env, Scope::Build);
            for (j, input) in step.inputs.iter().enumerate() {
                self.path(p.key("inputs").index(j), input);
            }
        }
    }

    fn database(&mut self) {
        let spec = self.spec;
        let Some(db) = &spec.database else { return };
        let p = FieldPath::of("database");
        self.value(p.key("engine"), &db.engine, Role::PLAIN);
        if let Some(version) = &db.version {
            self.value(p.key("version"), version, Role::PLAIN);
        }
        self.value(p.key("name"), &db.name, Role::PLAIN);
        for (i, ext) in db.extensions.iter().enumerate() {
            let ep = p.key("extensions").index(i);
            if !is_extension_name(ext) {
                self.error(ep, IssueCode::Database, format!("extension name `{ext}` must match ^[a-z0-9_]+$"));
            }
            self.value(ep, ext, Role::PLAIN);
        }
        if let Some(migrate) = &db.migrate {
            self.value(p.key("migrate"), migrate, Role::command(Scope::DbCommand));
        }
        if let Some(seed) = &db.seed {
            self.value(p.key("seed"), seed, Role::command(Scope::DbCommand));
        }
        self.path(p.key("cwd"), &db.cwd);
        self.timeout(p.key("timeout_s"), db.timeout_s);
        if let Some(service) = &db.service {
            self.value(p.key("service"), service, Role::PLAIN);
        }
        if let Some(url) = &db.url {
            self.value(p.key("url"), url, Role::url(Scope::DbUrl));
        }
        match db.provider {
            DbProvider::Runner => {
                if db.engine != "postgres" {
                    self.error(p.key("engine"), IssueCode::Database, "provider = \"runner\" supports only engine = \"postgres\"");
                }
                if db.service.is_some() {
                    self.error(p.key("service"), IssueCode::Database, "`service` applies only to provider = \"service\"");
                }
                if db.url.is_some() {
                    self.error(p.key("url"), IssueCode::Database, "`url` applies only to provider = \"service\"");
                }
            }
            DbProvider::Service => {
                match &db.service {
                    None => self.error(p.key("service"), IssueCode::Database, "provider = \"service\" requires `service`"),
                    Some(service) if !spec.services.contains_key(service) => {
                        self.error(p.key("service"), IssueCode::Database, format!("unknown service `{service}`"));
                    }
                    Some(_) => {}
                }
                match &db.url {
                    None => self.error(p.key("url"), IssueCode::Database, "provider = \"service\" requires `url`"),
                    Some(url) if !url_host(url).is_some_and(is_loopback_host) => {
                        self.error(p.key("url"), IssueCode::Database, "`url` must be a loopback URL");
                    }
                    Some(_) => {}
                }
            }
        }
    }

    fn services(&mut self) {
        let spec = self.spec;
        for (name, svc) in &spec.services {
            let p = FieldPath::of("services").key(name);
            if !is_name(name) {
                self.error(p, IssueCode::BadName, format!("service name `{name}` must match ^[a-z][a-z0-9_-]{{0,31}}$"));
            }
            match (&svc.run, &svc.compose) {
                (Some(_), Some(_)) => self.error(p, IssueCode::ServiceShape, "set exactly one of `run` or `compose`, not both"),
                (None, None) => self.error(p, IssueCode::ServiceShape, "set one of `run` or `compose`"),
                _ => {}
            }
            if let Some(run) = &svc.run {
                self.value(p.key("run"), run, Role::command(Scope::Service));
            }
            if let Some(compose) = &svc.compose {
                let cp = p.key("compose");
                self.path(cp.key("file"), &compose.file);
                self.value(cp.key("service"), &compose.service, Role::PLAIN);
                for port in compose.ports.keys() {
                    if !svc.ports.contains(port) {
                        self.error(
                            cp.key("ports").key(port),
                            IssueCode::ServiceShape,
                            format!("compose port `{port}` is not in this service's `ports`"),
                        );
                    }
                }
                if normalize_path(&svc.cwd) != "." {
                    self.error(p.key("cwd"), IssueCode::ServiceShape, "`cwd` applies to native (`run`) services only");
                }
            }
            self.path(p.key("cwd"), &svc.cwd);
            self.env_table(p.key("env"), &svc.env, Scope::Service);
            for (j, port) in svc.ports.iter().enumerate() {
                self.value(p.key("ports").index(j), port, Role::PLAIN);
            }
            for (j, dep) in svc.depends_on.iter().enumerate() {
                let dp = p.key("depends_on").index(j);
                if !spec.services.contains_key(dep) {
                    self.error(dp, IssueCode::DependsOn, format!("unknown service `{dep}`"));
                }
                self.value(dp, dep, Role::PLAIN);
            }
            self.positive(p.key("stop_timeout_s"), svc.stop_timeout_s);
            match &svc.health {
                None => self.error(p.key("health"), IssueCode::ServiceShape, "[services.<name>.health] is required"),
                Some(health) => self.health(p.key("health"), health),
            }
        }
        self.dependency_cycles();
    }

    fn health(&mut self, p: FieldPath<'_>, health: &HealthSpec) {
        let probes = [health.http.is_some(), health.tcp.is_some(), health.command.is_some()];
        let count = probes.into_iter().filter(|&set| set).count();
        if count != 1 {
            self.error(p, IssueCode::ServiceShape, format!("set exactly one of `http`, `tcp`, `command` (found {count})"));
        }
        if let Some(http) = &health.http {
            self.value(p.key("http"), http, Role::url(Scope::Service));
            if let Some(host) = url_host(http).filter(|h| !h.contains("${") && !is_loopback_host(h)) {
                self.error(p.key("http"), IssueCode::ServiceShape, format!("health.http must target loopback, not `{host}`"));
            }
        }
        if let Some(tcp) = &health.tcp {
            self.value(p.key("tcp"), tcp, Role::url(Scope::Service));
        }
        if let Some(command) = &health.command {
            self.value(p.key("command"), command, Role::command(Scope::Service));
        }
        self.positive(p.key("interval_ms"), health.interval_ms);
        self.positive(p.key("timeout_ms"), health.timeout_ms);
        self.positive(p.key("start_period_s"), health.start_period_s);
    }

    /// `DependsOn`: one issue per back edge of a DFS over the `depends_on` graph.
    fn dependency_cycles(&mut self) {
        let services = &self.spec.services;
        let names: Vec<&str> = services.keys().map(String::as_str).collect();
        let edges: Vec<Vec<usize>> = services
            .values()
            .map(|svc| {
                let mut deps: Vec<usize> = svc.depends_on.iter().filter_map(|d| names.binary_search(&d.as_str()).ok()).collect();
                deps.sort_unstable();
                deps.dedup();
                deps
            })
            .collect();
        const WHITE: u8 = 0;
        const GRAY: u8 = 1;
        const BLACK: u8 = 2;
        let mut color = vec![WHITE; names.len()];
        let mut cycles: Vec<(usize, String)> = Vec::new();
        for root in 0..names.len() {
            if color[root] != WHITE {
                continue;
            }
            color[root] = GRAY;
            let mut stack: Vec<(usize, usize)> = vec![(root, 0)];
            while let Some(&(node, at)) = stack.last() {
                let Some(&dep) = edges[node].get(at) else {
                    color[node] = BLACK;
                    stack.pop();
                    continue;
                };
                if let Some(top) = stack.last_mut() {
                    top.1 += 1;
                }
                match color[dep] {
                    WHITE => {
                        color[dep] = GRAY;
                        stack.push((dep, 0));
                    }
                    GRAY => {
                        let start = stack.iter().position(|&(n, _)| n == dep).unwrap_or(0);
                        let mut path: Vec<&str> = stack[start..].iter().map(|&(n, _)| names[n]).collect();
                        path.push(names[dep]);
                        cycles.push((dep, path.join(" -> ")));
                    }
                    _ => {}
                }
            }
        }
        for (node, path) in cycles {
            self.error(
                FieldPath::of("services").key(names[node]).key("depends_on"),
                IssueCode::DependsOn,
                format!("dependency cycle: {path}"),
            );
        }
    }

    fn test(&mut self) {
        let Some(test) = &self.spec.test else { return };
        let p = FieldPath::of("test");
        self.value(p.key("run"), &test.run, Role::command(Scope::Test));
        self.path(p.key("cwd"), &test.cwd);
        self.timeout(p.key("timeout_s"), test.timeout_s);
        self.env_table(p.key("env"), &test.env, Scope::Test);
    }

    fn browser(&mut self) {
        let spec = self.spec;
        let Some(browser) = &spec.browser else { return };
        let p = FieldPath::of("browser");
        self.value(p.key("base_url"), &browser.base_url, Role::browser_url());
        let declared = |port: &str| spec.ports.iter().any(|q| q == port);
        let references_port = vars(&browser.base_url).any(|var| {
            if let Some(port) = var.strip_prefix("port.") {
                declared(port)
            } else if let Some(service) = var.strip_prefix("service.").and_then(|r| r.strip_suffix(".url")) {
                spec.services.get(service).is_some_and(|svc| svc.ports.iter().any(|q| declared(q)))
            } else {
                false
            }
        });
        if !references_port {
            self.error(
                p.key("base_url"),
                IssueCode::BrowserBaseUrl,
                "base_url must reference a declared port (`${port.<name>}` or `${service.<name>.url}`)",
            );
        }
        if let Some(login) = &browser.login {
            let lp = p.key("login");
            self.value(lp.key("url"), &login.url, Role::browser_url());
            self.value(lp.key("username"), &login.username, Role::url(Scope::Browser));
            self.value(lp.key("password"), &login.password, Role::url(Scope::Browser));
        }
    }

    fn fakes(&mut self) {
        let spec = self.spec;
        for (name, fake) in &spec.fakes {
            let p = FieldPath::of("fakes").key(name);
            if !spec.services.contains_key(&fake.service) {
                self.error(p.key("service"), IssueCode::DependsOn, format!("unknown service `{}`", fake.service));
            }
            self.value(p.key("service"), &fake.service, Role::PLAIN);
            if let Some(replaces) = &fake.replaces {
                self.value(p.key("replaces"), replaces, Role::PLAIN);
            }
            if let Some(notes) = &fake.notes {
                self.value(p.key("notes"), notes, Role::PLAIN);
            }
        }
    }

    fn scip(&mut self) {
        let spec = self.spec;
        let mut languages = BTreeSet::new();
        for (i, scip) in spec.scip.iter().enumerate() {
            let p = FieldPath::of("scip").index(i);
            self.name(p.key("language"), &scip.language, "scip language");
            if !languages.insert(scip.language.as_str()) {
                self.error(p.key("language"), IssueCode::BadName, format!("duplicate scip language `{}`", scip.language));
            }
            self.value(p.key("run"), &scip.run, Role::command(Scope::Scip));
            if !vars(&scip.run).any(|var| var == "scip.out") {
                self.error(p.key("run"), IssueCode::ScipOut, "run must contain `${scip.out}`");
            }
            self.path(p.key("cwd"), &scip.cwd);
            self.timeout(p.key("timeout_s"), scip.timeout_s);
        }
    }

    fn fix(&mut self) {
        let Some(fix) = &self.spec.fix else { return };
        let p = FieldPath::of("fix");
        if let Some(lint) = &fix.lint {
            self.value(p.key("lint"), lint, Role::command(Scope::Lint));
        }
        for (i, glob) in fix.generated.iter().enumerate() {
            self.path(p.key("generated").index(i), glob);
        }
        if let Some(dir) = &fix.regression_dir {
            self.path(p.key("regression_dir"), dir);
        }
    }

    fn env_table(&mut self, p: FieldPath<'_>, table: &BTreeMap<String, String>, scope: Scope) {
        if table.len() > MAX_ENV_VARS {
            self.error(p, IssueCode::TooLarge, format!("{} variables; the limit is {MAX_ENV_VARS} per table", table.len()));
        }
        for (key, value) in table {
            let kp = p.key(key);
            if !is_env_key(key) {
                self.error(kp, IssueCode::BadName, format!("env key `{key}` must match ^[A-Z_][A-Z0-9_]{{0,127}}$"));
            }
            if is_reserved_env(key) {
                self.error(kp, IssueCode::ReservedEnv, format!("`{key}` is reserved"));
            }
            if is_secret_key(key) && !looks_fake(value) {
                self.error(
                    kp,
                    IssueCode::NotFakeSecret,
                    format!(
                        "`{key}` looks like a secret: use a template reference or a value containing fake, test, mage, dummy, example, changeme, or localhost"
                    ),
                );
            }
            self.value(kp, value, Role::env(scope));
        }
    }

    fn value(&mut self, p: FieldPath<'_>, value: &str, role: Role) {
        if let Some(pattern) = live_secret(value) {
            self.error(
                p,
                IssueCode::LiveSecret,
                format!("value matches the live-credential pattern `{pattern}`; recipes hold fake values only"),
            );
        }
        if let Some(scope) = role.scope {
            self.template(p, value, scope);
        }
        if role.command || role.env {
            if let Some(token) = absolute_host_path(value) {
                self.error(p, IssueCode::AbsoluteHostPath, format!("`{token}` is an absolute host path; use `${{ws}}` or `${{scratch}}`"));
            }
        }
        if role.command || role.env || role.ports {
            if let Some(host) = hardcoded_port(value) {
                self.error(p, IssueCode::HardcodedPort, format!("hard-coded port after `{host}`; use `${{port.<name>}}`"));
            }
        }
        if role.env || role.external {
            if let Some(host) = external_host(value) {
                self.warn(
                    p,
                    IssueCode::ExternalUrl,
                    format!("`{host}` is not loopback: the egress firewall blocks it; point at a fake service"),
                );
            }
        }
    }

    fn template(&mut self, p: FieldPath<'_>, value: &str, scope: Scope) {
        for part in template_parts(value) {
            match part {
                Ok(TemplatePart::Literal(_)) => {}
                Ok(TemplatePart::Var(var)) => self.variable(p, var, scope),
                Err(err) => self.error(p, IssueCode::UnknownVariable, format!("{err}; write `$${{` for a literal `${{`")),
            }
        }
    }

    fn variable(&mut self, p: FieldPath<'_>, var: &str, scope: Scope) {
        let spec = self.spec;
        let unavailable = || format!("`${{{var}}}` is not available in {}", scope.label());
        match var {
            "ws" => {
                if scope == Scope::Build {
                    self.warn(p, IssueCode::UnknownVariable, "`${ws}` in a build step: build output must not embed the workspace path");
                }
            }
            "scratch" => {}
            "scip.out" => {
                if scope != Scope::Scip {
                    self.error(p, IssueCode::UnknownVariable, unavailable());
                }
            }
            _ => {
                if let Some(port) = var.strip_prefix("port.") {
                    if !scope.ports() {
                        self.error(p, IssueCode::UnknownVariable, unavailable());
                    } else if !spec.ports.iter().any(|q| q == port) {
                        self.error(p, IssueCode::UnknownPort, format!("port `{port}` is not declared in `ports`"));
                    }
                } else if let Some(service) = var.strip_prefix("service.").and_then(|r| r.strip_suffix(".url")) {
                    if !scope.ports() {
                        self.error(p, IssueCode::UnknownVariable, unavailable());
                    } else {
                        match spec.services.get(service) {
                            None => self.error(p, IssueCode::UnknownVariable, format!("`${{{var}}}`: unknown service `{service}`")),
                            Some(svc) if svc.ports.is_empty() => {
                                self.error(p, IssueCode::UnknownVariable, format!("`${{{var}}}`: service `{service}` owns no port"))
                            }
                            Some(_) => {}
                        }
                    }
                } else if var.strip_prefix("db.").is_some_and(|field| DB_VARS.contains(&field)) {
                    if !scope.db() {
                        self.error(p, IssueCode::UnknownVariable, unavailable());
                    } else if spec.database.is_none() {
                        self.error(p, IssueCode::UnknownVariable, format!("`${{{var}}}` needs a [database] section"));
                    }
                } else {
                    self.error(p, IssueCode::UnknownVariable, format!("unknown variable `${{{var}}}`"));
                }
            }
        }
    }

    fn name(&mut self, p: FieldPath<'_>, name: &str, what: &str) {
        if !is_name(name) {
            self.error(p, IssueCode::BadName, format!("{what} name `{name}` must match ^[a-z][a-z0-9_-]{{0,31}}$"));
        }
        self.value(p, name, Role::PLAIN);
    }

    fn path(&mut self, p: FieldPath<'_>, raw: &str) {
        if let Some(problem) = path_problem(raw) {
            self.error(p, IssueCode::PathNotRelative, format!("path {problem}"));
        }
        self.value(p, raw, Role::PLAIN);
    }

    fn timeout(&mut self, p: FieldPath<'_>, secs: u32) {
        if !(1..=MAX_TIMEOUT_S).contains(&secs) {
            self.error(p, IssueCode::Timeouts, format!("timeout_s must be in 1..={MAX_TIMEOUT_S}, got {secs}"));
        }
    }

    fn positive(&mut self, p: FieldPath<'_>, n: u32) {
        if n == 0 {
            self.error(p, IssueCode::Timeouts, "must be at least 1");
        }
    }

    fn count(&mut self, p: FieldPath<'_>, n: usize, max: usize, what: &str) {
        if n > max {
            self.error(p, IssueCode::TooLarge, format!("{n} {what}; the limit is {max}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{BuildStep, EXAMPLE_TOML, RecipeParseError, ScipSpec};
    use super::IssueCode::*;
    use super::*;

    const BASE: &str = r#"version = 1
ports = ["web"]

[services.web]
run = "npm start"
ports = ["web"]
[services.web.health]
http = "http://127.0.0.1:${port.web}/"

[test]
run = "npm test"
"#;

    fn base() -> RecipeSpec {
        RecipeSpec::parse(BASE).unwrap()
    }

    /// `BASE` plus new tables.
    fn with(extra: &str) -> RecipeSpec {
        RecipeSpec::parse(&format!("{BASE}{extra}")).unwrap()
    }

    fn errors(spec: &RecipeSpec, code: IssueCode) -> Vec<String> {
        spec.validate().errors.into_iter().filter(|i| i.code == code).map(|i| i.path).collect()
    }

    fn warnings(spec: &RecipeSpec, code: IssueCode) -> Vec<String> {
        spec.validate().warnings.into_iter().filter(|i| i.code == code).map(|i| i.path).collect()
    }

    fn step(name: &str) -> BuildStep {
        BuildStep {
            name: name.to_owned(),
            run: "make".to_owned(),
            cwd: ".".to_owned(),
            timeout_s: 900,
            env: BTreeMap::new(),
            inputs: Vec::new(),
        }
    }

    #[test]
    fn base_and_example_are_clean() {
        for spec in [base(), RecipeSpec::parse(EXAMPLE_TOML).unwrap()] {
            let report = spec.validate();
            assert!(report.is_valid() && report.warnings.is_empty(), "{report:#?}");
        }
    }

    #[test]
    fn version() {
        let mut spec = base();
        spec.version = 2;
        assert_eq!(errors(&spec, Version), ["version"]);
        spec.version = 1;
        assert!(errors(&spec, Version).is_empty());
    }

    #[test]
    fn too_large_counts_and_sizes() {
        let mut spec = base();
        spec.build = (0..32).map(|i| step(&format!("s{i}"))).collect();
        spec.description = Some("d".repeat(2048));
        spec.ports.extend((1..32).map(|i| format!("p{i}")));
        let web = spec.services["web"].clone();
        spec.services.extend((1..16).map(|i| (format!("svc{i}"), web.clone())));
        spec.env.extend((0..64).map(|i| (format!("V{i}"), "v".to_owned())));
        spec.scip = (0..8)
            .map(|i| ScipSpec { language: format!("l{i}"), run: "x ${scip.out}".to_owned(), cwd: ".".to_owned(), timeout_s: 60 })
            .collect();
        assert!(errors(&spec, TooLarge).is_empty());

        spec.build.push(step("s32"));
        spec.description = Some("d".repeat(2049));
        spec.ports.push("p32".to_owned());
        spec.services.insert("svc16".to_owned(), web);
        spec.env.insert("V64".to_owned(), "v".to_owned());
        spec.scip.push(spec.scip[0].clone());
        assert_eq!(errors(&spec, TooLarge), ["description", "ports", "build", "services", "scip", "env"]);
    }

    #[test]
    fn too_large_canonical_document() {
        let mut spec = base();
        spec.env.extend((0..60).map(|i| (format!("V{i}"), "x".repeat(1000))));
        assert!(errors(&spec, TooLarge).is_empty());
        spec.env.extend((60..64).map(|i| (format!("V{i}"), "x".repeat(2000))));
        assert_eq!(errors(&spec, TooLarge), [""]);
    }

    #[test]
    fn bad_names() {
        let spec = with(
            r#"[env]
node_env = "x"
[[build]]
name = "deps"
run = "x"
[[build]]
name = "deps"
run = "y"
[[build]]
name = "Compile"
run = "z"
[[scip]]
language = "go"
run = "x ${scip.out}"
[[scip]]
language = "go"
run = "y ${scip.out}"
"#,
        );
        assert_eq!(errors(&spec, BadName), ["env.node_env", "build[1].name", "build[2].name", "scip[1].language"]);

        let mut spec = base();
        spec.ports.push("stripe-mock".to_owned());
        let web = spec.services.remove("web").unwrap();
        spec.services.insert("Web".to_owned(), web);
        assert_eq!(errors(&spec, BadName), ["ports[1]", "services.Web"]);

        let ok = with("[env]\nNODE_ENV = \"x\"\n[[build]]\nname = \"deps-1_a\"\nrun = \"x\"\n");
        assert!(errors(&ok, BadName).is_empty());
    }

    #[test]
    fn paths_must_be_relative() {
        let mut spec = with(
            "[[build]]\nname = \"a\"\nrun = \"x\"\ncwd = \"../up\"\ninputs = [\"ok/**\", \"/abs/**\"]\n[fix]\ngenerated = [\"./.git/x\"]\nregression_dir = \"~/tests\"\n",
        );
        spec.services.get_mut("web").unwrap().cwd = "a\\b".to_owned();
        assert_eq!(
            errors(&spec, PathNotRelative),
            ["build[0].cwd", "build[0].inputs[1]", "services.web.cwd", "fix.generated[0]", "fix.regression_dir"]
        );
        let ok = with(
            "[[build]]\nname = \"a\"\nrun = \"x\"\ncwd = \"./backend/\"\ninputs = [\"src//**\"]\n[fix]\nregression_dir = \"tests/e2e/\"\n",
        );
        assert!(errors(&ok, PathNotRelative).is_empty());
    }

    #[test]
    fn absolute_host_paths() {
        let mut spec = with(
            "[env]\nCONFIG = \"/etc/app.conf\"\nSEARCH = \"x:/opt/y\"\n[[build]]\nname = \"a\"\nrun = \"cp /home/me/x .\"\n[fix]\nlint = \"/opt/lint\"\n",
        );
        spec.services.get_mut("web").unwrap().health.as_mut().unwrap().command = Some("test -f /var/run/x".to_owned());
        assert_eq!(
            errors(&spec, AbsoluteHostPath),
            ["env.CONFIG", "env.SEARCH", "build[0].run", "services.web.health.command", "fix.lint"]
        );
        let ok = with(
            "[env]\nSH = \"/bin/sh\"\nSINK = \"/dev/null\"\nURL = \"http://127.0.0.1:${port.web}/a/b\"\n[[build]]\nname = \"a\"\nrun = \"/usr/bin/env node ${ws}/x > /dev/null\"\n",
        );
        assert!(errors(&ok, AbsoluteHostPath).is_empty());
    }

    #[test]
    fn hardcoded_ports() {
        let mut spec = with(
            r#"[env]
API = "http://localhost:3000"
[[build]]
name = "a"
run = "wait-for 127.0.0.1:5432"
[database]
provider = "service"
service = "web"
url = "postgres://app@127.0.0.1:5432/app"
[browser]
base_url = "http://127.0.0.1:${port.web}"
[browser.login]
url = "http://[::1]:8080/login"
username = "u@example.test"
password = "fake"
"#,
        );
        spec.services.get_mut("web").unwrap().health.as_mut().unwrap().http = Some("http://0.0.0.0:3000/".to_owned());
        assert_eq!(
            errors(&spec, HardcodedPort),
            ["env.API", "build[0].run", "database.url", "services.web.health.http", "browser.login.url"]
        );
        // Compose container ports are exempt.
        assert!(errors(&RecipeSpec::parse(EXAMPLE_TOML).unwrap(), HardcodedPort).is_empty());
    }

    #[test]
    fn unknown_ports() {
        let spec = with("[env]\nAPI = \"http://127.0.0.1:${port.api}\"\n");
        assert_eq!(errors(&spec, UnknownPort), ["env.API"]);
        let ok = with("[env]\nAPI = \"http://127.0.0.1:${port.web}\"\n");
        assert!(errors(&ok, UnknownPort).is_empty());
    }

    #[test]
    fn port_ownership() {
        let mut spec = base();
        spec.ports.push("admin".to_owned());
        assert_eq!(errors(&spec, PortOwnership), ["ports[1]"]);

        let mut spec = base();
        let web = spec.services["web"].clone();
        spec.services.insert("web2".to_owned(), web);
        assert_eq!(errors(&spec, PortOwnership), ["ports[0]"]);

        let mut spec = base();
        spec.services.get_mut("web").unwrap().ports.push("admin".to_owned());
        assert_eq!(errors(&spec, PortOwnership), ["services.web.ports[1]"]);

        assert!(errors(&base(), PortOwnership).is_empty());
    }

    #[test]
    fn unknown_variables() {
        let spec = with(
            r#"[env]
A = "${foo}"
B = "${db.user}"
C = "${scip.out}"
D = "${port.web"
E = "${service.nope.url}"
F = "${service.worker.url}"
G = "${db.schema}"
[[build]]
name = "a"
run = "serve --port ${port.web} ${service.web.url} ${db.url}"
[database]
url = "postgres://127.0.0.1:${port.web}/${db.name}"
[services.worker]
run = "w"
[services.worker.health]
command = "true"
[browser]
base_url = "http://127.0.0.1:${port.web}/?db=${db.name}"
[fix]
lint = "lint --port ${port.web}"
"#,
        );
        assert_eq!(
            errors(&spec, UnknownVariable),
            [
                "env.A",
                "env.C",
                "env.D",
                "env.E",
                "env.F",
                "env.G",
                "build[0].run",
                "build[0].run",
                "build[0].run",
                "database.url",
                "browser.base_url",
                "fix.lint",
            ]
        );
        assert!(warnings(&spec, UnknownVariable).is_empty());
        // `${db.*}` where allowed, but without a [database] section.
        assert_eq!(errors(&with("[env]\nB = \"${db.url}\"\n"), UnknownVariable), ["env.B"]);

        let ws_in_build = with("[[build]]\nname = \"a\"\nrun = \"make -C ${ws}\"\n");
        assert!(errors(&ws_in_build, UnknownVariable).is_empty());
        assert_eq!(warnings(&ws_in_build, UnknownVariable), ["build[0].run"]);

        let ok = with(
            r#"[env]
HOME_DIR = "${ws}/home"
LIT = "$${HOME} and $PATH"
DB = "${db.url}"
WEB = "${service.web.url}"
[[build]]
name = "a"
run = "make OUT=${scratch}/out"
[database]
migrate = "migrate --url ${db.url} --host ${db.host} --tmp ${scratch} --web ${port.web}"
[[scip]]
language = "go"
run = "scip-go --output ${scip.out}"
"#,
        );
        let report = ok.validate();
        assert!(report.errors.iter().chain(&report.warnings).all(|i| i.code != UnknownVariable), "{report:#?}");
    }

    #[test]
    fn service_shapes() {
        let spec = with(
            r#"[services.both]
run = "x"
compose = { file = "c.yml", service = "both" }
[services.both.health]
command = "true"
[services.neither]
[services.neither.health]
command = "true"
[services.nohealth]
run = "x"
[services.twoprobes]
run = "x"
[services.twoprobes.health]
http = "http://127.0.0.1:${port.web}/"
tcp = "127.0.0.1:${port.web}"
[services.composeports]
compose = { file = "c.yml", service = "c", ports = { web = 80 } }
[services.composeports.health]
command = "true"
[services.composecwd]
compose = { file = "c.yml", service = "c" }
cwd = "sub"
[services.composecwd.health]
command = "true"
[services.external]
run = "x"
[services.external.health]
http = "http://example.com/health"
"#,
        );
        assert_eq!(
            errors(&spec, ServiceShape),
            [
                "services.both",
                "services.composecwd.cwd",
                "services.composeports.compose.ports.web",
                "services.external.health.http",
                "services.neither",
                "services.nohealth.health",
                "services.twoprobes.health",
            ]
        );
        assert!(errors(&RecipeSpec::parse(EXAMPLE_TOML).unwrap(), ServiceShape).is_empty());
    }

    #[test]
    fn depends_on_and_cycles() {
        let spec = with(
            r#"[services.a]
run = "x"
depends_on = ["b", "ghost"]
[services.a.health]
command = "true"
[services.b]
run = "x"
depends_on = ["a"]
[services.b.health]
command = "true"
[services.c]
run = "x"
depends_on = ["c"]
[services.c.health]
command = "true"
[fakes.mail]
service = "nobody"
"#,
        );
        let report = spec.validate();
        let issues: Vec<(&str, &str)> =
            report.errors.iter().filter(|i| i.code == DependsOn).map(|i| (i.path.as_str(), i.message.as_str())).collect();
        assert_eq!(
            issues,
            [
                ("services.a.depends_on[1]", "unknown service `ghost`"),
                ("services.a.depends_on", "dependency cycle: a -> b -> a"),
                ("services.c.depends_on", "dependency cycle: c -> c"),
                ("fakes.mail.service", "unknown service `nobody`"),
            ]
        );
        assert!(errors(&RecipeSpec::parse(EXAMPLE_TOML).unwrap(), DependsOn).is_empty());
    }

    #[test]
    fn reserved_env() {
        let mut spec = with("[env]\nPATH = \"x\"\nLD_PRELOAD = \"x\"\nhttps_proxy = \"x\"\nMAGE_RUN = \"x\"\nAWS_REGION = \"x\"\n");
        spec.services.get_mut("web").unwrap().env.insert("HOME".to_owned(), "x".to_owned());
        assert_eq!(
            errors(&spec, ReservedEnv),
            ["env.AWS_REGION", "env.LD_PRELOAD", "env.MAGE_RUN", "env.PATH", "env.https_proxy", "services.web.env.HOME"]
        );
        let ok = with("[env]\nMY_PATH = \"x\"\nNODE_ENV = \"x\"\nPROXY_MODE = \"x\"\n");
        assert!(errors(&ok, ReservedEnv).is_empty());
    }

    #[test]
    fn live_secrets_anywhere_without_echoing_them() {
        let mut spec = with("[env]\nSTRIPE_KEY = \"sk_live_123\"\n[fakes.x]\nservice = \"web\"\nnotes = \"token ghp_abcdef\"\n");
        spec.description = Some("-----BEGIN RSA PRIVATE KEY-----".to_owned());
        let report = spec.validate();
        let hits: Vec<&RecipeIssue> = report.errors.iter().filter(|i| i.code == LiveSecret).collect();
        assert_eq!(hits.iter().map(|i| i.path.as_str()).collect::<Vec<_>>(), ["description", "env.STRIPE_KEY", "fakes.x.notes"]);
        assert!(hits.iter().all(|i| !i.message.contains("sk_live_123") && !i.message.contains("abcdef")));
        assert!(errors(&RecipeSpec::parse(EXAMPLE_TOML).unwrap(), LiveSecret).is_empty());
    }

    #[test]
    fn not_fake_secrets() {
        let spec = with(
            r#"[env]
API_KEY = "abc123"
DB_PASSWORD = "$${db.password}"
SESSION_TOKEN = "Test-Token"
SENTRY_DSN = "${db.url}"
MONKEY = "banana"
[database]
"#,
        );
        assert_eq!(errors(&spec, NotFakeSecret), ["env.API_KEY", "env.DB_PASSWORD"]);
        assert!(errors(&RecipeSpec::parse(EXAMPLE_TOML).unwrap(), NotFakeSecret).is_empty());
    }

    #[test]
    fn external_urls_are_warnings() {
        let spec = with(
            r#"[env]
STRIPE = "https://api.stripe.com"
LOCAL = "http://localhost:${port.web}"
TPL = "http://${db.host}/x"
[database]
[browser]
base_url = "https://staging.example.com/${port.web}"
"#,
        );
        let report = spec.validate();
        assert!(report.is_valid(), "{report:#?}");
        assert_eq!(warnings(&spec, ExternalUrl), ["env.STRIPE", "browser.base_url"]);
    }

    #[test]
    fn database_rules() {
        let runner = with(
            "[database]\nengine = \"mysql\"\nservice = \"web\"\nurl = \"postgres://127.0.0.1:${port.web}/x\"\nextensions = [\"pg-crypto\", \"citext\"]\n",
        );
        assert_eq!(errors(&runner, Database), ["database.extensions[0]", "database.engine", "database.service", "database.url"]);

        let missing = with("[database]\nprovider = \"service\"\n");
        assert_eq!(errors(&missing, Database), ["database.service", "database.url"]);

        let wrong = with("[database]\nprovider = \"service\"\nservice = \"pg\"\nurl = \"postgres://db.example.com/app\"\n");
        assert_eq!(errors(&wrong, Database), ["database.service", "database.url"]);

        let service = with(
            "[database]\nprovider = \"service\"\nengine = \"mysql\"\nservice = \"web\"\nurl = \"postgres://app:app@127.0.0.1:${port.web}/app\"\n",
        );
        assert!(errors(&service, Database).is_empty());
        assert!(errors(&with("[database]\nextensions = [\"pg_trgm\"]\n"), Database).is_empty());
    }

    #[test]
    fn browser_base_url_references_a_declared_port() {
        let no_port = with("[browser]\nbase_url = \"http://127.0.0.1/\"\n");
        assert_eq!(errors(&no_port, BrowserBaseUrl), ["browser.base_url"]);
        let undeclared = with("[browser]\nbase_url = \"http://127.0.0.1:${port.api}\"\n");
        assert_eq!(errors(&undeclared, BrowserBaseUrl), ["browser.base_url"]);
        assert_eq!(errors(&undeclared, UnknownPort), ["browser.base_url"]);
        for ok in ["http://127.0.0.1:${port.web}", "${service.web.url}/app"] {
            assert!(errors(&with(&format!("[browser]\nbase_url = \"{ok}\"\n")), BrowserBaseUrl).is_empty(), "{ok}");
        }
    }

    #[test]
    fn scip_run_must_write_scip_out() {
        let spec =
            with("[[scip]]\nlanguage = \"go\"\nrun = \"scip-go\"\n[[scip]]\nlanguage = \"rust\"\nrun = \"x --output $${scip.out}\"\n");
        assert_eq!(errors(&spec, ScipOut), ["scip[0].run", "scip[1].run"]);
        let ok = with("[[scip]]\nlanguage = \"go\"\nrun = \"scip-go --output ${scip.out}\"\n");
        assert!(errors(&ok, ScipOut).is_empty());
    }

    #[test]
    fn timeouts() {
        let mut spec =
            with("[[build]]\nname = \"a\"\nrun = \"x\"\ntimeout_s = 0\n[[build]]\nname = \"b\"\nrun = \"x\"\ntimeout_s = 3601\n");
        spec.services.get_mut("web").unwrap().health.as_mut().unwrap().interval_ms = 0;
        spec.test.as_mut().unwrap().timeout_s = 3601;
        assert_eq!(
            errors(&spec, Timeouts),
            ["build[0].timeout_s", "build[1].timeout_s", "services.web.health.interval_ms", "test.timeout_s"]
        );
        let ok = with("[[build]]\nname = \"a\"\nrun = \"x\"\ntimeout_s = 1\n[[build]]\nname = \"b\"\nrun = \"x\"\ntimeout_s = 3600\n");
        assert!(errors(&ok, Timeouts).is_empty());
    }

    #[test]
    fn no_test_is_a_warning() {
        let mut spec = base();
        spec.test = None;
        let report = spec.validate();
        assert!(report.is_valid());
        assert_eq!(warnings(&spec, NoTest), ["test"]);
        assert!(warnings(&base(), NoTest).is_empty());
    }

    #[test]
    fn unknown_key_is_reported_by_parse() {
        let err = RecipeSpec::parse(&format!("{BASE}[services.web.helth]\nhttp = \"x\"\n")).unwrap_err();
        assert!(matches!(err, RecipeParseError::UnknownKey { .. }));
        assert_eq!(err.code(), Some(UnknownKey));
        assert!(RecipeSpec::parse(BASE).is_ok());
    }
}
