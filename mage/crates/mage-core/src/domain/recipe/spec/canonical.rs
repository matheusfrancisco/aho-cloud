//! Canonical form and `RecipeHash` (executor.md "Canonical form and `RecipeHash`").
//!
//! Hand-written so the hash never depends on the `toml` crate's formatter:
//! - every default is emitted explicitly (serde already filled them in);
//! - path fields are normalized (`scan::normalize_path`);
//! - set-like arrays are sorted bytewise (numerically for `expect_status`) and deduplicated;
//!   `[[build]]` and `[[scip]]` keep their order;
//! - tables and keys follow schema order; map keys are sorted bytewise;
//! - strings are TOML basic strings escaping only `"`, `\`, `\n`, `\t`, and other control
//!   characters (`char::is_control`) as `\uXXXX`; integers are decimal;
//! - keys are bare when they consist of `[A-Za-z0-9_-]+`, quoted otherwise;
//! - a blank line precedes every table header; LF line endings; one trailing newline.
//!
//! Empty `[toolchain]`, `[env]`, `[[build]]`, `[services]`, `[fakes]`, `[[scip]]` and absent
//! optional tables emit nothing; empty inline maps and scalar arrays emit `{}` / `[]`.

use std::borrow::Cow;
use std::collections::BTreeMap;

use sha2::{Digest, Sha256};

use super::scan::normalize_path;
use super::{RecipeSpec, ServiceSpec};
use crate::domain::ids::RecipeHash;

impl RecipeSpec {
    /// The canonical TOML document this spec hashes to.
    pub fn canonical_toml(&self) -> String {
        let mut out = String::new();
        emit(self, &mut out);
        out
    }

    /// `SHA-256(canonical_toml())`.
    pub fn hash(&self) -> RecipeHash {
        let mut hasher = Sha256::new();
        emit(self, &mut hasher);
        RecipeHash::from_bytes(hasher.finalize().into())
    }
}

/// Byte length of `canonical_toml()` without building it.
pub(super) fn canonical_len(spec: &RecipeSpec) -> usize {
    let mut len = ByteCount(0);
    emit(spec, &mut len);
    len.0
}

trait Sink {
    fn put(&mut self, s: &str);
}

impl Sink for String {
    fn put(&mut self, s: &str) {
        self.push_str(s);
    }
}

impl Sink for Sha256 {
    fn put(&mut self, s: &str) {
        Digest::update(self, s.as_bytes());
    }
}

struct ByteCount(usize);

impl Sink for ByteCount {
    fn put(&mut self, s: &str) {
        self.0 += s.len();
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Items {
    Plain,
    Paths,
}

fn emit(spec: &RecipeSpec, out: &mut impl Sink) {
    let mut e = Emitter { out };
    e.int("version", spec.version.into());
    if let Some(description) = &spec.description {
        e.str("description", description);
    }
    e.str_set("ports", &spec.ports, Items::Plain);

    if !spec.toolchain.is_empty() {
        e.header(&["toolchain"]);
        for (name, version) in &spec.toolchain {
            e.str(name, version);
        }
    }
    if !spec.env.is_empty() {
        e.header(&["env"]);
        for (name, value) in &spec.env {
            e.str(name, value);
        }
    }

    for step in &spec.build {
        e.array_header("build");
        e.str("name", &step.name);
        e.str("run", &step.run);
        e.path("cwd", &step.cwd);
        e.int("timeout_s", step.timeout_s.into());
        e.str_map("env", &step.env);
        e.str_set("inputs", &step.inputs, Items::Paths);
    }

    if let Some(db) = &spec.database {
        e.header(&["database"]);
        e.str("provider", db.provider.as_str());
        e.str("engine", &db.engine);
        if let Some(version) = &db.version {
            e.str("version", version);
        }
        e.str("name", &db.name);
        e.str_set("extensions", &db.extensions, Items::Plain);
        if let Some(migrate) = &db.migrate {
            e.str("migrate", migrate);
        }
        if let Some(seed) = &db.seed {
            e.str("seed", seed);
        }
        e.path("cwd", &db.cwd);
        e.int("timeout_s", db.timeout_s.into());
        if let Some(service) = &db.service {
            e.str("service", service);
        }
        if let Some(url) = &db.url {
            e.str("url", url);
        }
    }

    for (name, service) in &spec.services {
        e.service(name, service);
    }

    if let Some(test) = &spec.test {
        e.header(&["test"]);
        e.str("run", &test.run);
        e.path("cwd", &test.cwd);
        e.int("timeout_s", test.timeout_s.into());
        e.str_map("env", &test.env);
    }

    if let Some(browser) = &spec.browser {
        e.header(&["browser"]);
        e.str("base_url", &browser.base_url);
        if let Some(login) = &browser.login {
            e.header(&["browser", "login"]);
            e.str("url", &login.url);
            e.str("username", &login.username);
            e.str("password", &login.password);
        }
    }

    for (name, fake) in &spec.fakes {
        e.header(&["fakes", name]);
        e.str("service", &fake.service);
        if let Some(replaces) = &fake.replaces {
            e.str("replaces", replaces);
        }
        if let Some(notes) = &fake.notes {
            e.str("notes", notes);
        }
    }

    for scip in &spec.scip {
        e.array_header("scip");
        e.str("language", &scip.language);
        e.str("run", &scip.run);
        e.path("cwd", &scip.cwd);
        e.int("timeout_s", scip.timeout_s.into());
    }

    if let Some(fix) = &spec.fix {
        e.header(&["fix"]);
        if let Some(lint) = &fix.lint {
            e.str("lint", lint);
        }
        e.str("lint_mode", fix.lint_mode.as_str());
        e.str_set("generated", &fix.generated, Items::Paths);
        if let Some(dir) = &fix.regression_dir {
            e.path("regression_dir", dir);
        }
    }
}

struct Emitter<'s, S: Sink> {
    out: &'s mut S,
}

impl<S: Sink> Emitter<'_, S> {
    fn service(&mut self, name: &str, service: &ServiceSpec) {
        self.header(&["services", name]);
        if let Some(run) = &service.run {
            self.str("run", run);
        }
        if let Some(compose) = &service.compose {
            self.key("compose");
            self.out.put(" = { ");
            self.key("file");
            self.out.put(" = ");
            self.string(&normalize_path(&compose.file));
            self.out.put(", ");
            self.key("service");
            self.out.put(" = ");
            self.string(&compose.service);
            self.out.put(", ");
            self.key("ports");
            self.out.put(" = ");
            self.inline_map(&compose.ports, |e, port| e.number((*port).into()));
            self.out.put(" }\n");
        }
        self.path("cwd", &service.cwd);
        self.str_map("env", &service.env);
        self.str_set("ports", &service.ports, Items::Plain);
        self.str_set("depends_on", &service.depends_on, Items::Plain);
        self.int("stop_timeout_s", service.stop_timeout_s.into());
        if let Some(health) = &service.health {
            self.header(&["services", name, "health"]);
            if let Some(http) = &health.http {
                self.str("http", http);
            }
            let mut statuses = health.expect_status.clone();
            statuses.sort_unstable();
            statuses.dedup();
            self.key("expect_status");
            self.out.put(" = [");
            for (i, status) in statuses.iter().enumerate() {
                if i > 0 {
                    self.out.put(", ");
                }
                self.number((*status).into());
            }
            self.out.put("]\n");
            if let Some(tcp) = &health.tcp {
                self.str("tcp", tcp);
            }
            if let Some(command) = &health.command {
                self.str("command", command);
            }
            self.int("interval_ms", health.interval_ms.into());
            self.int("timeout_ms", health.timeout_ms.into());
            self.int("start_period_s", health.start_period_s.into());
            self.int("retries", health.retries.into());
        }
    }

    fn header(&mut self, keys: &[&str]) {
        self.out.put("\n[");
        for (i, key) in keys.iter().enumerate() {
            if i > 0 {
                self.out.put(".");
            }
            self.key(key);
        }
        self.out.put("]\n");
    }

    fn array_header(&mut self, key: &str) {
        self.out.put("\n[[");
        self.key(key);
        self.out.put("]]\n");
    }

    fn str(&mut self, key: &str, value: &str) {
        self.key(key);
        self.out.put(" = ");
        self.string(value);
        self.out.put("\n");
    }

    fn path(&mut self, key: &str, value: &str) {
        self.str(key, &normalize_path(value));
    }

    fn int(&mut self, key: &str, value: u64) {
        self.key(key);
        self.out.put(" = ");
        self.number(value);
        self.out.put("\n");
    }

    fn str_map(&mut self, key: &str, map: &BTreeMap<String, String>) {
        self.key(key);
        self.out.put(" = ");
        self.inline_map(map, |e, value| e.string(value));
        self.out.put("\n");
    }

    /// `[]` or `["a", "b"]`: normalized (paths), sorted bytewise, deduplicated.
    fn str_set(&mut self, key: &str, values: &[String], items: Items) {
        let mut sorted: Vec<Cow<'_, str>> = values
            .iter()
            .map(|v| if items == Items::Paths { normalize_path(v) } else { Cow::Borrowed(v.as_str()) })
            .collect();
        sorted.sort_unstable();
        sorted.dedup();
        self.key(key);
        self.out.put(" = [");
        for (i, value) in sorted.iter().enumerate() {
            if i > 0 {
                self.out.put(", ");
            }
            self.string(value);
        }
        self.out.put("]\n");
    }

    /// `{}` or `{ a = …, b = … }` in key order (bytewise for `String` keys).
    fn inline_map<V>(&mut self, map: &BTreeMap<String, V>, mut value: impl FnMut(&mut Self, &V)) {
        if map.is_empty() {
            self.out.put("{}");
            return;
        }
        self.out.put("{ ");
        for (i, (k, v)) in map.iter().enumerate() {
            if i > 0 {
                self.out.put(", ");
            }
            self.key(k);
            self.out.put(" = ");
            value(self, v);
        }
        self.out.put(" }");
    }

    fn key(&mut self, key: &str) {
        let bare = !key.is_empty() && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
        if bare { self.out.put(key) } else { self.string(key) }
    }

    fn string(&mut self, s: &str) {
        self.out.put("\"");
        let mut start = 0;
        for (i, c) in s.char_indices() {
            let escape = match c {
                '"' => "\\\"",
                '\\' => "\\\\",
                '\n' => "\\n",
                '\t' => "\\t",
                c if c.is_control() => "",
                _ => continue,
            };
            self.out.put(&s[start..i]);
            if escape.is_empty() {
                self.unicode_escape(c);
            } else {
                self.out.put(escape);
            }
            start = i + c.len_utf8();
        }
        self.out.put(&s[start..]);
        self.out.put("\"");
    }

    /// `\uXXXX`, uppercase hex. Control characters are all in the BMP.
    fn unicode_escape(&mut self, c: char) {
        const HEX: &[u8; 16] = b"0123456789ABCDEF";
        let code = u32::from(c);
        let mut buf = *b"\\u0000";
        for (i, slot) in buf[2..].iter_mut().enumerate() {
            *slot = HEX[((code >> (12 - 4 * i)) & 0xF) as usize];
        }
        self.out.put(std::str::from_utf8(&buf).unwrap_or_default());
    }

    fn number(&mut self, mut n: u64) {
        let mut buf = [0u8; 20];
        let mut at = buf.len();
        loop {
            at -= 1;
            buf[at] = b'0' + (n % 10) as u8;
            n /= 10;
            if n == 0 {
                break;
            }
        }
        self.out.put(std::str::from_utf8(&buf[at..]).unwrap_or_default());
    }
}

#[cfg(test)]
mod tests {
    use super::super::EXAMPLE_TOML;
    use super::*;

    fn canonical(doc: &str) -> String {
        RecipeSpec::parse(doc).unwrap().canonical_toml()
    }

    fn hash(doc: &str) -> RecipeHash {
        RecipeSpec::parse(doc).unwrap().hash()
    }

    /// The executor.md example's hash. Changing the emitter changes every `RecipeHash`, which
    /// requires a migration recomputing stored hashes (executor.md); update deliberately.
    const EXAMPLE_HASH: &str = "65afdf760b18ae9179b45cf4de9776fa251870d50706953ce91dd2be3c8b3d2c";

    #[test]
    fn golden_hash_of_the_example_recipe() {
        let spec = RecipeSpec::parse(EXAMPLE_TOML).unwrap();
        assert_eq!(spec.hash().to_hex(), EXAMPLE_HASH, "canonical form:\n{}", spec.canonical_toml());
    }

    #[test]
    fn hash_is_sha256_of_canonical_bytes() {
        let spec = RecipeSpec::parse(EXAMPLE_TOML).unwrap();
        let canonical = spec.canonical_toml();
        let digest: [u8; 32] = Sha256::digest(canonical.as_bytes()).into();
        assert_eq!(spec.hash(), RecipeHash::from_bytes(digest));
        assert_eq!(canonical_len(&spec), canonical.len());
    }

    #[test]
    fn example_canonical_form() {
        let want = r#"version = 1
description = "Rails-free example: Node API + React web + Postgres + Redis"
ports = ["api", "redis", "stripe", "web"]

[toolchain]
node = "20"

[env]
DATABASE_URL = "${db.url}"
NODE_ENV = "development"
REDIS_URL = "redis://127.0.0.1:${port.redis}"
SESSION_SECRET = "mage-fake-session-secret"
STRIPE_API_BASE = "${service.stripe-mock.url}"
STRIPE_SECRET_KEY = "sk_test_mage_fake_000000"

[[build]]
name = "deps"
run = "npm ci"
cwd = "."
timeout_s = 1200
env = {}
inputs = ["package-lock.json", "package.json"]

[[build]]
name = "compile"
run = "npm run build"
cwd = "."
timeout_s = 900
env = {}
inputs = []

[database]
provider = "runner"
engine = "postgres"
version = "16"
name = "app"
extensions = ["pgcrypto"]
migrate = "npm run db:migrate"
seed = "npm run db:seed"
cwd = "."
timeout_s = 600

[services.api]
run = "npm run start:api"
cwd = "."
env = { PORT = "${port.api}" }
ports = ["api"]
depends_on = ["redis", "stripe-mock"]
stop_timeout_s = 10

[services.api.health]
http = "http://127.0.0.1:${port.api}/healthz"
expect_status = [200]
interval_ms = 1000
timeout_ms = 2000
start_period_s = 120
retries = 3

[services.redis]
compose = { file = "docker-compose.yml", service = "redis", ports = { redis = 6379 } }
cwd = "."
env = {}
ports = ["redis"]
depends_on = []
stop_timeout_s = 10

[services.redis.health]
expect_status = [200]
tcp = "127.0.0.1:${port.redis}"
interval_ms = 1000
timeout_ms = 2000
start_period_s = 120
retries = 3

[services.stripe-mock]
compose = { file = "docker-compose.yml", service = "stripe-mock", ports = { stripe = 12111 } }
cwd = "."
env = {}
ports = ["stripe"]
depends_on = []
stop_timeout_s = 10

[services.stripe-mock.health]
expect_status = [200]
tcp = "127.0.0.1:${port.stripe}"
interval_ms = 1000
timeout_ms = 2000
start_period_s = 120
retries = 3

[services.web]
run = "npm run start:web"
cwd = "."
env = { API_URL = "${service.api.url}", PORT = "${port.web}" }
ports = ["web"]
depends_on = ["api"]
stop_timeout_s = 10

[services.web.health]
http = "http://127.0.0.1:${port.web}/"
expect_status = [200]
interval_ms = 1000
timeout_ms = 2000
start_period_s = 180
retries = 3

[test]
run = "npm test -- --runInBand"
cwd = "."
timeout_s = 1800
env = {}

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
cwd = "."
timeout_s = 1800

[fix]
lint = "npm run lint"
lint_mode = "advisory"
generated = ["src/generated/**"]
"#;
        assert_eq!(canonical(EXAMPLE_TOML), want);
    }

    #[test]
    fn reordered_keys_and_tables_hash_the_same() {
        let a = r#"version = 1
ports = ["web"]
[env]
A = "1"
B = "2"
[services.web]
run = "x"
ports = ["web"]
[services.web.health]
tcp = "t"
[services.api]
run = "y"
[services.api.health]
command = "c"
[test]
run = "t"
cwd = "sub"
"#;
        let b = r#"ports = ["web"]
version = 1
[test]
cwd = "sub"
run = "t"
[services.api.health]
command = "c"
[services.api]
run = "y"
[services.web]
ports = ["web"]
run = "x"
[services.web.health]
tcp = "t"
[env]
B = "2"
A = "1"
"#;
        assert_eq!(hash(a), hash(b));
    }

    #[test]
    fn omitted_and_explicit_defaults_hash_the_same() {
        let omitted = r#"version = 1
[[build]]
name = "a"
run = "x"
[database]
[services.s]
run = "x"
[services.s.health]
http = "h"
[test]
run = "t"
[[scip]]
language = "go"
run = "r"
[fix]
"#;
        let explicit = r#"version = 1
ports = []
[toolchain]
[env]
[[build]]
name = "a"
run = "x"
cwd = "."
timeout_s = 900
env = {}
inputs = []
[database]
provider = "runner"
engine = "postgres"
name = "app"
extensions = []
cwd = "."
timeout_s = 600
[services.s]
run = "x"
cwd = "."
env = {}
ports = []
depends_on = []
stop_timeout_s = 10
[services.s.health]
http = "h"
expect_status = 200
interval_ms = 1000
timeout_ms = 2000
start_period_s = 120
retries = 3
[test]
run = "t"
cwd = "."
timeout_s = 1800
env = {}
[fakes]
[[scip]]
language = "go"
run = "r"
cwd = "."
timeout_s = 1800
[fix]
lint_mode = "advisory"
generated = []
"#;
        assert_eq!(canonical(omitted), canonical(explicit));
        assert_eq!(hash(omitted), hash(explicit));
    }

    #[test]
    fn set_arrays_reordered_deduplicated_and_path_spellings_hash_the_same() {
        let a = r#"version = 1
ports = ["web", "api"]
[[build]]
name = "b"
run = "x"
cwd = "app"
inputs = ["package.json", "src/**"]
[database]
extensions = ["pgcrypto", "citext"]
[services.a]
run = "x"
[services.a.health]
command = "c"
[services.s]
run = "x"
ports = ["web", "api"]
depends_on = ["a", "b"]
[services.s.health]
http = "h"
expect_status = [302, 200]
[fix]
generated = ["gen/**", "dist/**"]
"#;
        let b = r#"version = 1
ports = ["api", "web", "api"]
[[build]]
name = "b"
run = "x"
cwd = "./app/"
inputs = ["./src//**", "package.json", "./package.json"]
[database]
extensions = ["citext", "pgcrypto"]
[services.a]
run = "x"
[services.a.health]
command = "c"
[services.s]
run = "x"
ports = ["api", "web"]
depends_on = ["b", "a", "a"]
[services.s.health]
http = "h"
expect_status = [200, 302, 200]
[fix]
generated = ["dist/**/", "gen/**"]
"#;
        assert_eq!(canonical(a), canonical(b));
        assert_eq!(hash(a), hash(b));
    }

    #[test]
    fn build_and_scip_order_changes_the_hash() {
        let build = |first: &str, second: &str| {
            format!(
                "version = 1\n[[build]]\nname = \"{first}\"\nrun = \"{first}\"\n[[build]]\nname = \"{second}\"\nrun = \"{second}\"\n"
            )
        };
        assert_ne!(hash(&build("deps", "compile")), hash(&build("compile", "deps")));
        let scip = |first: &str, second: &str| {
            format!(
                "version = 1\n[[scip]]\nlanguage = \"{first}\"\nrun = \"x ${{scip.out}}\"\n[[scip]]\nlanguage = \"{second}\"\nrun = \"x ${{scip.out}}\"\n"
            )
        };
        assert_ne!(hash(&scip("go", "python")), hash(&scip("python", "go")));
    }

    #[test]
    fn value_changes_change_the_hash() {
        let base = hash(EXAMPLE_TOML);
        assert_ne!(base, hash(&EXAMPLE_TOML.replace("timeout_s = 1200", "timeout_s = 1201")));
        assert_ne!(base, hash(&EXAMPLE_TOML.replace("lint_mode = \"advisory\"", "lint_mode = \"required\"")));
        assert_ne!(base, hash(&EXAMPLE_TOML.replace("[fakes.stripe]", "[fakes.stripe2]")));
    }

    #[test]
    fn strings_use_minimal_basic_escaping_and_keys_quote_when_needed() {
        let mut spec = RecipeSpec::parse("version = 1\n").unwrap();
        spec.description = Some("q\" b\\ n\n t\t r\r e\u{1b} del\u{7f} é ☃".to_owned());
        spec.toolchain.insert("a.b".to_owned(), "1".to_owned());
        spec.toolchain.insert(String::new(), "2".to_owned());
        let canonical = spec.canonical_toml();
        assert_eq!(
            canonical,
            "version = 1\ndescription = \"q\\\" b\\\\ n\\n t\\t r\\u000D e\\u001B del\\u007F é ☃\"\nports = []\n\n[toolchain]\n\"\" = \"2\"\n\"a.b\" = \"1\"\n"
        );
        let reparsed = RecipeSpec::parse(&canonical).unwrap();
        assert_eq!(reparsed, spec);
    }

    #[test]
    fn parse_canonical_parse_is_a_fixed_point() {
        let docs = [
            EXAMPLE_TOML.to_owned(),
            "version = 1\n".to_owned(),
            // Database hosted by a service, quoted names, odd strings, every optional key set.
            r#"version = 1
description = "multi\nline \"quoted\" \\ \u0001"
ports = ["pg", "app", "app"]
[toolchain]
python = "3.12"
"weird key" = "x"
[env]
DB = "postgres://app:${db.password}@127.0.0.1:${db.port}/app"
[[build]]
name = "z"
run = "make"
cwd = "./backend//src/"
timeout_s = 1
env = { B = "2", A = "1" }
inputs = ["b/**", "./a/*.py", "b/**"]
[database]
provider = "service"
engine = "mysql"
version = "8"
name = "main"
extensions = ["b", "a"]
migrate = "m"
seed = "s"
cwd = "db/"
timeout_s = 3600
service = "pg"
url = "postgres://app:app@127.0.0.1:${port.pg}/app"
[services.pg]
compose = { file = "./ops//compose.yml", service = "pg", ports = { pg = 5432 } }
ports = ["pg"]
[services.pg.health]
command = "pg_isready"
interval_ms = 5
timeout_ms = 6
start_period_s = 7
retries = 0
[services."app"]
run = "serve"
cwd = "app"
ports = ["app"]
depends_on = ["pg", "pg"]
stop_timeout_s = 1
[services.app.health]
http = "http://127.0.0.1:${port.app}/"
expect_status = [204, 200, 204]
[test]
run = "pytest"
cwd = "./"
timeout_s = 60
env = { CI = "1" }
[browser]
base_url = "${service.app.url}"
[fakes.mail]
service = "app"
notes = "inbox"
[[scip]]
language = "python"
run = "scip-python index . --output ${scip.out}"
cwd = "src"
timeout_s = 10
[fix]
lint = "ruff check"
lint_mode = "required"
generated = ["b/", "a"]
regression_dir = "./tests/regressions/"
"#
            .to_owned(),
            // Invalid but well-typed: canonical form is total.
            "version = 9\nports = [\"Bad Port\"]\n[services.x]\n[[build]]\nname = \"\"\nrun = \"\"\ncwd = \"\"\n"
                .to_owned(),
        ];
        for doc in &docs {
            let first = canonical(doc);
            let reparsed = RecipeSpec::parse(&first).unwrap();
            assert_eq!(reparsed.canonical_toml(), first, "not a fixed point:\n{first}");
            assert_eq!(RecipeSpec::parse(&reparsed.canonical_toml()).unwrap(), reparsed);
        }
    }
}
