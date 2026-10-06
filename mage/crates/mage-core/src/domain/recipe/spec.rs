//! Recipe `.mage/env.toml` (executor.md "Recipe"; the `[fix]` keys are owned by fix.md):
//! `RecipeSpec`, `parse`, `validate`, `canonical_toml`, `hash`.
//!
//! The spec keeps what the author wrote, with every schema default filled in by serde. Path
//! normalization and set semantics are applied by the canonical emitter, so the hash does not
//! depend on spelling (`./a/` vs `a`) or on the order of set-like arrays.

use std::collections::BTreeMap;
use std::fmt;

use serde::de::{self, SeqAccess, Unexpected, Visitor};
use serde::{Deserialize, Deserializer, Serialize};

mod canonical;
mod scan;
mod template;
mod validate;

pub use template::{TemplatePart, TemplateParts, UnterminatedTemplate, template_parts};
pub use validate::{IssueCode, RecipeIssue, RecipeReport};

/// The only recipe `version` v1 accepts.
pub const RECIPE_VERSION: u32 = 1;
/// Maximum size of a recipe document in bytes (`TooLarge`; also the `submit_recipe` input cap).
pub const RECIPE_MAX_BYTES: usize = 64 * 1024;

/// Parsed `.mage/env.toml`. Tables are in schema order; `deny_unknown_fields` on every table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecipeSpec {
    /// Must be [`RECIPE_VERSION`] (`IssueCode::Version`).
    pub version: u32,
    /// At most 2 KiB; shown to agents and in the UI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Every named port the app listens on (a set).
    #[serde(default)]
    pub ports: Vec<String>,
    /// Tool name → version, matched against the runner's `[runner.toolchains]`.
    #[serde(default)]
    pub toolchain: BTreeMap<String, String>,
    /// Applied to build, services, exec, and test. Fake values only.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Ordered build steps.
    #[serde(default)]
    pub build: Vec<BuildStep>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database: Option<DatabaseSpec>,
    /// Long-running processes or compose services, by name.
    #[serde(default)]
    pub services: BTreeMap<String, ServiceSpec>,
    /// Project test command; the Fix gate refuses recipes without one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub test: Option<TestSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browser: Option<BrowserSpec>,
    /// Which service fakes which external dependency (documentation for agents and the brief).
    #[serde(default)]
    pub fakes: BTreeMap<String, FakeSpec>,
    /// SCIP indexer commands, in order. Empty = runner defaults per detected language.
    #[serde(default)]
    pub scip: Vec<ScipSpec>,
    /// Per-repo fix policy (fix.md).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fix: Option<FixSection>,
}

/// One `[[build]]` step, run as `/bin/sh -c run` during prepare (no ports, no DB).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuildStep {
    /// `^[a-z][a-z0-9_-]{0,31}$`, unique among steps.
    pub name: String,
    pub run: String,
    /// Workspace-relative.
    #[serde(default = "default_cwd")]
    pub cwd: String,
    #[serde(default = "default_build_timeout_s")]
    pub timeout_s: u32,
    /// Step-only additions to `[env]`.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Globs; `Env::rebuild(Changed)` re-runs the step only if a changed path matches.
    /// Empty = always re-run.
    #[serde(default)]
    pub inputs: Vec<String>,
}

/// `[database]`: the test database.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DatabaseSpec {
    #[serde(default)]
    pub provider: DbProvider,
    /// Only `"postgres"` for `provider = "runner"` in v1; free-form for `provider = "service"`.
    #[serde(default = "default_db_engine")]
    pub engine: String,
    /// Expected major version; the host adapter warns on a mismatch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Logical name; the physical name is per fork.
    #[serde(default = "default_db_name")]
    pub name: String,
    /// Created by the runner admin before `migrate` (a set).
    #[serde(default)]
    pub extensions: Vec<String>,
    /// Must be idempotent: it re-runs after a patch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub migrate: Option<String>,
    /// Runs once in prepare; never re-run on a cloned DB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<String>,
    #[serde(default = "default_cwd")]
    pub cwd: String,
    /// Per command.
    #[serde(default = "default_db_timeout_s")]
    pub timeout_s: u32,
    /// Service hosting the DB (`provider = "service"` only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
    /// Loopback URL template (`provider = "service"` only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DbProvider {
    /// Runner-local Postgres with template fork.
    #[default]
    Runner,
    /// The DB runs as a recipe service; data is cloned with the fork.
    Service,
}

impl DbProvider {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Runner => "runner",
            Self::Service => "service",
        }
    }
}

/// `[services.<name>]`: exactly one of `run` / `compose` (`IssueCode::ServiceShape`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceSpec {
    /// Native process (shell command).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compose: Option<ComposeSpec>,
    /// Native services only.
    #[serde(default = "default_cwd")]
    pub cwd: String,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Ports this service owns (a set); every declared port has exactly one owner.
    #[serde(default)]
    pub ports: Vec<String>,
    /// Started and healthy first (a set); the graph is acyclic.
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// TERM → wait → KILL.
    #[serde(default = "default_stop_timeout_s")]
    pub stop_timeout_s: u32,
    /// Required (`ServiceShape` when absent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health: Option<HealthSpec>,
}

/// A docker compose service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComposeSpec {
    /// Workspace-relative compose file.
    pub file: String,
    pub service: String,
    /// Declared port name → container port. Keys ⊆ the service's `ports`.
    #[serde(default)]
    pub ports: BTreeMap<String, u16>,
}

/// `[services.<name>.health]`: exactly one of `http`, `tcp`, `command`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HealthSpec {
    /// Template URL; GET; loopback only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http: Option<String>,
    /// Accepted statuses for `http` (a set). TOML accepts an int or an array.
    #[serde(default = "default_expect_status", deserialize_with = "status_set")]
    pub expect_status: Vec<u16>,
    /// Template `host:port`; healthy when a connect succeeds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tcp: Option<String>,
    /// Healthy on exit 0; run as `exec` with a 10 s timeout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default = "default_interval_ms")]
    pub interval_ms: u32,
    /// Per probe.
    #[serde(default = "default_probe_timeout_ms")]
    pub timeout_ms: u32,
    /// Maximum time to the first success.
    #[serde(default = "default_start_period_s")]
    pub start_period_s: u32,
    /// Consecutive failures after healthy → `Unhealthy`.
    #[serde(default = "default_retries")]
    pub retries: u32,
}

/// `[test]`: the project test command (required by the Fix gate).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TestSpec {
    pub run: String,
    #[serde(default = "default_cwd")]
    pub cwd: String,
    #[serde(default = "default_test_timeout_s")]
    pub timeout_s: u32,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

/// `[browser]`: exposed to scripts as `MAGE_BASE_URL` and `MAGE_LOGIN_*`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrowserSpec {
    /// Template; must reference a declared port.
    pub base_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub login: Option<BrowserLogin>,
}

/// `[browser.login]`: a fake test login.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrowserLogin {
    /// Path or template URL.
    pub url: String,
    pub username: String,
    /// Fake.
    pub password: String,
}

/// `[fakes.<name>]`: documents which service fakes which external dependency.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FakeSpec {
    /// An existing service.
    pub service: String,
    /// Hostname of the real dependency (informational).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replaces: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

/// One `[[scip]]` indexer (Learn post-gate). The project root is always the repo root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScipSpec {
    /// `^[a-z][a-z0-9_-]{0,31}$`, unique among entries (names the `scip/<language>.scip` output).
    pub language: String,
    /// Must contain `${scip.out}`.
    pub run: String,
    #[serde(default = "default_cwd")]
    pub cwd: String,
    #[serde(default = "default_scip_timeout_s")]
    pub timeout_s: u32,
}

/// `[fix]` (fix.md): per-repo fix policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FixSection {
    /// Lint command run after the test command in the Fix gate (check 6).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lint: Option<String>,
    #[serde(default)]
    pub lint_mode: LintMode,
    /// Globs of generated paths excluded from fix diffs (a set).
    #[serde(default)]
    pub generated: Vec<String>,
    /// Directory for `RegressionPlacement::RepoTests`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub regression_dir: Option<String>,
}

/// How a failing `[fix].lint` affects the Fix gate.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LintMode {
    /// Non-zero exit fails the gate (`LintFailed`).
    Required,
    /// Exit recorded only. Default: there is no lint baseline, so a repo whose lint is already
    /// red at the base sha would otherwise block every fix.
    #[default]
    Advisory,
}

impl LintMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Required => "required",
            Self::Advisory => "advisory",
        }
    }
}

fn default_cwd() -> String {
    ".".to_owned()
}
const fn default_build_timeout_s() -> u32 {
    900
}
fn default_db_engine() -> String {
    "postgres".to_owned()
}
fn default_db_name() -> String {
    "app".to_owned()
}
const fn default_db_timeout_s() -> u32 {
    600
}
const fn default_stop_timeout_s() -> u32 {
    10
}
fn default_expect_status() -> Vec<u16> {
    vec![200]
}
const fn default_interval_ms() -> u32 {
    1000
}
const fn default_probe_timeout_ms() -> u32 {
    2000
}
const fn default_start_period_s() -> u32 {
    120
}
const fn default_retries() -> u32 {
    3
}
const fn default_test_timeout_s() -> u32 {
    1800
}
const fn default_scip_timeout_s() -> u32 {
    1800
}

/// `health.expect_status`: an HTTP status or an array of them.
fn status_set<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u16>, D::Error> {
    struct StatusSet;

    impl<'de> Visitor<'de> for StatusSet {
        type Value = Vec<u16>;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("an HTTP status code or an array of HTTP status codes")
        }

        fn visit_i64<E: de::Error>(self, v: i64) -> Result<Vec<u16>, E> {
            u16::try_from(v).map(|s| vec![s]).map_err(|_| E::invalid_value(Unexpected::Signed(v), &self))
        }

        fn visit_u64<E: de::Error>(self, v: u64) -> Result<Vec<u16>, E> {
            u16::try_from(v).map(|s| vec![s]).map_err(|_| E::invalid_value(Unexpected::Unsigned(v), &self))
        }

        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Vec<u16>, A::Error> {
            let mut out = Vec::with_capacity(seq.size_hint().unwrap_or(0).min(16));
            while let Some(status) = seq.next_element::<u16>()? {
                out.push(status);
            }
            Ok(out)
        }
    }

    deserializer.deserialize_any(StatusSet)
}

impl RecipeSpec {
    /// Deserializes a recipe document. Unknown keys and wrong types are errors here; every other
    /// rule is reported by [`RecipeSpec::validate`].
    pub fn parse(text: &str) -> Result<Self, RecipeParseError> {
        toml::from_str(text).map_err(|err| RecipeParseError::from_toml(&err, text))
    }
}

/// 1-based position in the recipe text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextLocation {
    pub line: u32,
    /// In characters.
    pub column: u32,
}

impl TextLocation {
    fn at(text: &str, offset: usize) -> Option<Self> {
        let before = text.get(..offset)?;
        let line = before.bytes().filter(|&b| b == b'\n').count() + 1;
        let column = before.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;
        Some(Self { line: u32::try_from(line).unwrap_or(u32::MAX), column: u32::try_from(column).unwrap_or(u32::MAX) })
    }
}

/// Why a recipe document did not deserialize.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RecipeParseError {
    /// A table has a key the schema does not define (`IssueCode::UnknownKey`).
    #[error("unknown key{}: {message}", At(.location))]
    UnknownKey { message: String, location: Option<TextLocation> },
    /// Not TOML, a value of the wrong type, or a missing required key.
    #[error("invalid recipe{}: {message}", At(.location))]
    Invalid { message: String, location: Option<TextLocation> },
}

impl RecipeParseError {
    fn from_toml(err: &toml::de::Error, text: &str) -> Self {
        let message = err.message().to_owned();
        let location = err.span().and_then(|span| TextLocation::at(text, span.start));
        // serde's `Error::unknown_field` wording, produced by `deny_unknown_fields`.
        if message.starts_with("unknown field") {
            Self::UnknownKey { message, location }
        } else {
            Self::Invalid { message, location }
        }
    }

    /// The validation code this error corresponds to, if the schema names one.
    pub fn code(&self) -> Option<IssueCode> {
        match self {
            Self::UnknownKey { .. } => Some(IssueCode::UnknownKey),
            Self::Invalid { .. } => None,
        }
    }
}

struct At<'a>(&'a Option<TextLocation>);

impl fmt::Display for At<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Some(loc) => write!(f, " at line {}, column {}", loc.line, loc.column),
            None => Ok(()),
        }
    }
}

/// The executor.md example recipe, shared by the validation and canonical-form tests.
#[cfg(test)]
const EXAMPLE_TOML: &str = r#"version = 1
description = "Rails-free example: Node API + React web + Postgres + Redis"
ports = ["api", "web", "redis", "stripe"]

[toolchain]
node = "20"

[env]
NODE_ENV = "development"
DATABASE_URL = "${db.url}"
REDIS_URL = "redis://127.0.0.1:${port.redis}"
STRIPE_API_BASE = "${service.stripe-mock.url}"
STRIPE_SECRET_KEY = "sk_test_mage_fake_000000"
SESSION_SECRET = "mage-fake-session-secret"

[[build]]
name = "deps"
run = "npm ci"
inputs = ["package.json", "package-lock.json"]
timeout_s = 1200

[[build]]
name = "compile"
run = "npm run build"

[database]
provider = "runner"
version = "16"
extensions = ["pgcrypto"]
migrate = "npm run db:migrate"
seed = "npm run db:seed"

[services.redis]
compose = { file = "docker-compose.yml", service = "redis", ports = { redis = 6379 } }
ports = ["redis"]
[services.redis.health]
tcp = "127.0.0.1:${port.redis}"

[services.stripe-mock]
compose = { file = "docker-compose.yml", service = "stripe-mock", ports = { stripe = 12111 } }
ports = ["stripe"]
[services.stripe-mock.health]
tcp = "127.0.0.1:${port.stripe}"

[services.api]
run = "npm run start:api"
ports = ["api"]
depends_on = ["redis", "stripe-mock"]
env = { PORT = "${port.api}" }
[services.api.health]
http = "http://127.0.0.1:${port.api}/healthz"

[services.web]
run = "npm run start:web"
ports = ["web"]
depends_on = ["api"]
env = { PORT = "${port.web}", API_URL = "${service.api.url}" }
[services.web.health]
http = "http://127.0.0.1:${port.web}/"
start_period_s = 180

[test]
run = "npm test -- --runInBand"
timeout_s = 1800

[browser]
base_url = "http://127.0.0.1:${port.web}"
[browser.login]
url = "/login"
username = "support@example.test"
password = "mage-fake-password"

[fakes.stripe]
service = "stripe-mock"
replaces = "api.stripe.com"

[[scip]]
language = "typescript"
run = "scip-typescript index --output ${scip.out}"

[fix]
lint = "npm run lint"
lint_mode = "advisory"
generated = ["src/generated/**"]
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_parses_with_defaults_filled() {
        let spec = RecipeSpec::parse(EXAMPLE_TOML).unwrap();
        assert_eq!(spec.build[0].timeout_s, 1200);
        assert_eq!(spec.build[1].timeout_s, 900);
        assert_eq!(spec.build[1].cwd, ".");
        let db = spec.database.as_ref().unwrap();
        assert_eq!(
            (db.provider, db.engine.as_str(), db.name.as_str(), db.timeout_s),
            (DbProvider::Runner, "postgres", "app", 600)
        );
        let web = &spec.services["web"];
        let health = web.health.as_ref().unwrap();
        assert_eq!(health.expect_status, vec![200]);
        assert_eq!(
            (health.interval_ms, health.timeout_ms, health.start_period_s, health.retries),
            (1000, 2000, 180, 3)
        );
        assert_eq!(web.stop_timeout_s, 10);
        assert_eq!(spec.services["redis"].compose.as_ref().unwrap().ports["redis"], 6379);
        assert_eq!(spec.scip[0].timeout_s, 1800);
        assert_eq!(spec.fix.as_ref().unwrap().lint_mode, LintMode::Advisory);
    }

    #[test]
    fn minimal_recipe_has_empty_sections() {
        let spec = RecipeSpec::parse("version = 1\n").unwrap();
        assert!(spec.ports.is_empty() && spec.build.is_empty() && spec.services.is_empty() && spec.scip.is_empty());
        assert!(spec.database.is_none() && spec.test.is_none() && spec.browser.is_none() && spec.fix.is_none());
    }

    #[test]
    fn expect_status_accepts_int_or_array() {
        let doc = |status: &str| {
            format!(
                "version = 1\n[services.a]\nrun = \"x\"\n[services.a.health]\nhttp = \"http://127.0.0.1/\"\nexpect_status = {status}\n"
            )
        };
        let one = RecipeSpec::parse(&doc("204")).unwrap();
        assert_eq!(one.services["a"].health.as_ref().unwrap().expect_status, vec![204]);
        let many = RecipeSpec::parse(&doc("[200, 302]")).unwrap();
        assert_eq!(many.services["a"].health.as_ref().unwrap().expect_status, vec![200, 302]);
        assert!(matches!(RecipeSpec::parse(&doc("70000")), Err(RecipeParseError::Invalid { .. })));
        assert!(matches!(RecipeSpec::parse(&doc("\"200\"")), Err(RecipeParseError::Invalid { .. })));
    }

    #[test]
    fn unknown_keys_are_rejected_in_every_table() {
        let cases = [
            ("version = 1\nname = \"x\"\n", 2),
            ("version = 1\n[[build]]\nname = \"a\"\nrun = \"x\"\nretries = 2\n", 5),
            ("version = 1\n[services.a]\nrun = \"x\"\n[services.a.helth]\nhttp = \"x\"\n", 4),
            ("version = 1\n[services.a]\ncompose = { file = \"c.yml\", service = \"a\", image = \"x\" }\n", 3),
            (
                "version = 1\n[browser]\nbase_url = \"x\"\n[browser.login]\nurl = \"/\"\nusername = \"u\"\npassword = \"p\"\notp = \"1\"\n",
                8,
            ),
            ("version = 1\n[fix]\nlint_level = \"x\"\n", 3),
        ];
        for (doc, line) in cases {
            let err = RecipeSpec::parse(doc).unwrap_err();
            assert_eq!(err.code(), Some(IssueCode::UnknownKey), "{doc}: {err}");
            let RecipeParseError::UnknownKey { location, .. } = &err else { unreachable!() };
            assert_eq!(location.map(|l| l.line), Some(line), "{doc}: {err}");
        }
    }

    #[test]
    fn wrong_types_and_missing_required_keys_are_invalid() {
        let cases = [
            "version = \"1\"\n",
            "description = \"no version\"\n",
            "version = 1\nports = \"web\"\n",
            "version = 1\n[[build]]\nname = \"a\"\nrun = \"x\"\ntimeout_s = \"10\"\n",
            "version = 1\n[[build]]\nname = \"a\"\ntimeout_s = 10\n",
            "version = 1\n[[build]]\nname = \"a\"\nrun = \"x\"\ntimeout_s = -1\n",
            "version = 1\n[database]\nprovider = \"sqlite\"\n",
            "version = 1\n[fix]\nlint_mode = \"strict\"\n",
            "version = 1\n[env]\nA = 1\n",
            "version = 1\n[services.a]\ncompose = { file = \"c.yml\", service = \"a\", ports = { web = 70000 } }\n",
            "version = 1\n[[scip]]\nlanguage = \"go\"\n",
            "version = 1\n[fakes.x]\nreplaces = \"api.example.com\"\n",
            "version = 1\n[test]\ncwd = \".\"\n",
            "version = 1\nversion = 1\n",
            "not toml at all",
        ];
        for doc in cases {
            let err = RecipeSpec::parse(doc).unwrap_err();
            assert!(matches!(err, RecipeParseError::Invalid { .. }), "{doc}: {err}");
            assert_eq!(err.code(), None);
        }
    }

    #[test]
    fn spec_round_trips_through_json() {
        let spec = RecipeSpec::parse(EXAMPLE_TOML).unwrap();
        let json = serde_json::to_string(&spec).unwrap();
        assert_eq!(serde_json::from_str::<RecipeSpec>(&json).unwrap(), spec);
    }
}
