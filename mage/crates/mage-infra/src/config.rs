//! Configuration loading shared by every role (server.md "Secrets and per-role config" → "Config").
//!
//! - One TOML file plus `MAGE_*` environment overrides ([`ConfigFile`]). An override names a path
//!   with `__` between segments: `MAGE_SERVER__DATABASE__POOL_MAX=15` sets
//!   `server.database.pool_max`. Variables without `__` (e.g. `MAGE_DATABASE_URL`, the target of a
//!   secret reference) are not overrides. Values parse as TOML scalars/arrays when they can
//!   (`15`, `true`, `["a","b"]`), else they are strings.
//! - Credentials are never inline: fields whose names end in `_secret`, `_key`, `_token` or
//!   `password` must hold a [`SecretSource`] reference (`env:NAME`, `file:/path`,
//!   `gcpsm:projects/…/secrets/…/versions/…`); [`ConfigFile::check_no_inline_secrets`] enforces
//!   it over the whole file and `SecretSource` refuses anything else when deserialized.

use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use base64::Engine;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer};
use toml::{Table, Value};

/// Prefix of override variables.
pub const ENV_PREFIX: &str = "MAGE_";

/// Separator between path segments in override variable names.
pub const ENV_PATH_SEPARATOR: &str = "__";

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read config {path}: {message}")]
    Io { path: String, message: String },
    #[error("config {origin} is not valid TOML: {message}")]
    Parse { origin: String, message: String },
    #[error("config section [{0}] is missing")]
    MissingSection(String),
    #[error("config [{path}]: {message}")]
    Invalid { path: String, message: String },
    #[error("config field {0} holds an inline secret; use env:, file: or gcpsm:")]
    InlineSecret(String),
    #[error("secret {source_ref}: {message}")]
    Secret { source_ref: String, message: String },
}

// ---- secrets ----------------------------------------------------------------------------------

/// Where a credential comes from. Parsed from `env:NAME`, `file:/path` (`~/` expands to the home
/// directory) or `gcpsm:projects/{p}/secrets/{s}/versions/{v}`; an inline value is refused.
#[derive(Clone, PartialEq, Eq)]
pub enum SecretSource {
    Env(String),
    File(PathBuf),
    /// GCP Secret Manager secret version resource name.
    GcpSecretManager(String),
}

/// A resolved credential. `Debug`/`Display` print `***`.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretValue(String);

impl SecretValue {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn into_inner(self) -> String {
        self.0
    }
}

impl fmt::Debug for SecretValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}

impl fmt::Display for SecretValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}

impl SecretSource {
    /// `true` for strings that are secret references (what `FromStr` accepts).
    pub fn is_reference(s: &str) -> bool {
        s.parse::<SecretSource>().is_ok()
    }

    /// Read the credential. `env:` and `file:` are local; `gcpsm:` calls Secret Manager with the
    /// service account of the metadata server (Cloud Run / GCE).
    pub async fn resolve(&self) -> Result<SecretValue, ConfigError> {
        match self {
            Self::GcpSecretManager(name) => SecretManager::default().access(name).await,
            local => local.resolve_local(),
        }
    }

    /// `env:` / `file:` only (synchronous); `gcpsm:` is an error here.
    pub fn resolve_local(&self) -> Result<SecretValue, ConfigError> {
        let fail = |message: String| ConfigError::Secret { source_ref: self.to_string(), message };
        match self {
            Self::Env(name) => match std::env::var(name) {
                Ok(value) if !value.is_empty() => Ok(SecretValue(value)),
                Ok(_) => Err(fail("environment variable is empty".into())),
                Err(e) => Err(fail(e.to_string())),
            },
            Self::File(path) => {
                let value = std::fs::read_to_string(path).map_err(|e| fail(e.to_string()))?;
                let value = value.strip_suffix('\n').map(|v| v.strip_suffix('\r').unwrap_or(v)).unwrap_or(&value).to_owned();
                if value.is_empty() { Err(fail("file is empty".into())) } else { Ok(SecretValue(value)) }
            }
            Self::GcpSecretManager(_) => Err(fail("Secret Manager references need the async `resolve`".into())),
        }
    }
}

impl FromStr for SecretSource {
    type Err = ConfigError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let invalid = |message: &str| ConfigError::Secret { source_ref: redact_reference(s), message: message.to_owned() };
        if let Some(name) = s.strip_prefix("env:") {
            if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
                return Err(invalid("env: needs a variable name ([A-Za-z0-9_]+)"));
            }
            return Ok(Self::Env(name.to_owned()));
        }
        if let Some(path) = s.strip_prefix("file:") {
            if path.is_empty() {
                return Err(invalid("file: needs a path"));
            }
            return Ok(Self::File(expand_home(path)));
        }
        if let Some(name) = s.strip_prefix("gcpsm:") {
            let segments: Vec<&str> = name.split('/').collect();
            let shaped = segments.len() == 6
                && segments[0] == "projects"
                && segments[2] == "secrets"
                && segments[4] == "versions"
                && segments.iter().all(|seg| !seg.is_empty());
            if !shaped {
                return Err(invalid("gcpsm: needs projects/{project}/secrets/{secret}/versions/{version}"));
            }
            return Ok(Self::GcpSecretManager(name.to_owned()));
        }
        Err(invalid("inline secret values are refused; use env:NAME, file:/path or gcpsm:projects/…"))
    }
}

/// Error text never echoes a value that might be an inline secret.
fn redact_reference(s: &str) -> String {
    match s.split_once(':') {
        Some((scheme @ ("env" | "file" | "gcpsm"), rest)) => format!("{scheme}:{rest}"),
        _ => "<inline value>".to_owned(),
    }
}

impl fmt::Display for SecretSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Env(name) => write!(f, "env:{name}"),
            Self::File(path) => write!(f, "file:{}", path.display()),
            Self::GcpSecretManager(name) => write!(f, "gcpsm:{name}"),
        }
    }
}

impl fmt::Debug for SecretSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SecretSource({self})")
    }
}

impl<'de> Deserialize<'de> for SecretSource {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// `~/x` → `$HOME/x`; other paths unchanged.
pub fn expand_home(path: &str) -> PathBuf {
    match (path.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => Path::new(&home).join(rest),
        _ => PathBuf::from(path),
    }
}

/// Secret Manager `access` through the metadata server's service-account token.
#[derive(Debug, Clone)]
pub struct SecretManager {
    http: reqwest::Client,
    /// `http://metadata.google.internal` (or `GCE_METADATA_HOST`).
    metadata_base: String,
    /// `https://secretmanager.googleapis.com`.
    api_base: String,
}

impl Default for SecretManager {
    fn default() -> Self {
        let metadata_host = std::env::var("GCE_METADATA_HOST").unwrap_or_else(|_| "metadata.google.internal".to_owned());
        Self::with_endpoints(format!("http://{metadata_host}"), "https://secretmanager.googleapis.com".to_owned())
    }
}

#[derive(Deserialize)]
struct MetadataToken {
    access_token: String,
}

#[derive(Deserialize)]
struct AccessResponse {
    payload: AccessPayload,
}

#[derive(Deserialize)]
struct AccessPayload {
    data: String,
}

impl SecretManager {
    pub fn with_endpoints(metadata_base: String, api_base: String) -> Self {
        let http = reqwest::Client::builder().timeout(Duration::from_secs(10)).build().unwrap_or_default();
        Self { http, metadata_base: metadata_base.trim_end_matches('/').to_owned(), api_base: api_base.trim_end_matches('/').to_owned() }
    }

    /// Payload of secret version `name` (`projects/{p}/secrets/{s}/versions/{v}`), UTF-8.
    pub async fn access(&self, name: &str) -> Result<SecretValue, ConfigError> {
        let fail = |message: String| ConfigError::Secret { source_ref: format!("gcpsm:{name}"), message };
        let token: MetadataToken = self
            .http
            .get(format!("{}/computeMetadata/v1/instance/service-accounts/default/token", self.metadata_base))
            .header("Metadata-Flavor", "Google")
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|e| fail(format!("metadata token: {e}")))?
            .json()
            .await
            .map_err(|e| fail(format!("metadata token: {e}")))?;
        let response: AccessResponse = self
            .http
            .get(format!("{}/v1/{name}:access", self.api_base))
            .bearer_auth(&token.access_token)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|e| fail(format!("access: {e}")))?
            .json()
            .await
            .map_err(|e| fail(format!("access response: {e}")))?;
        let bytes = base64::engine::general_purpose::STANDARD.decode(response.payload.data).map_err(|e| fail(format!("payload: {e}")))?;
        String::from_utf8(bytes).map(SecretValue).map_err(|_| fail("payload is not UTF-8".into()))
    }
}

// ---- TOML file + overrides --------------------------------------------------------------------

/// A parsed config file with overrides applied.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigFile {
    table: Table,
    origin: String,
}

impl ConfigFile {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|e| ConfigError::Io { path: path.display().to_string(), message: e.to_string() })?;
        Self::parse(&text, &path.display().to_string())
    }

    pub fn parse(text: &str, origin: &str) -> Result<Self, ConfigError> {
        let table: Table = text.parse().map_err(|e: toml::de::Error| ConfigError::Parse { origin: origin.to_owned(), message: e.to_string() })?;
        Ok(Self { table, origin: origin.to_owned() })
    }

    /// An empty config (every section from overrides).
    pub fn empty() -> Self {
        Self { table: Table::new(), origin: "<empty>".to_owned() }
    }

    /// Apply `MAGE_A__B__C=value` overrides from the process environment.
    pub fn with_env_overrides(self) -> Result<Self, ConfigError> {
        self.with_overrides(std::env::vars())
    }

    /// Apply overrides from `vars` (name, value); names without the prefix or without `__` are
    /// ignored.
    pub fn with_overrides<I: IntoIterator<Item = (String, String)>>(mut self, vars: I) -> Result<Self, ConfigError> {
        let mut overrides: Vec<(Vec<String>, String)> = vars
            .into_iter()
            .filter_map(|(name, value)| {
                let rest = name.strip_prefix(ENV_PREFIX)?;
                rest.contains(ENV_PATH_SEPARATOR).then(|| (rest.split(ENV_PATH_SEPARATOR).map(str::to_ascii_lowercase).collect(), value))
            })
            .collect();
        overrides.sort();
        for (path, raw) in overrides {
            if path.iter().any(String::is_empty) {
                return Err(ConfigError::Invalid { path: path.join("."), message: "empty segment in override variable".into() });
            }
            set_path(&mut self.table, &path, parse_override(&raw))?;
        }
        Ok(self)
    }

    pub fn table(&self) -> &Table {
        &self.table
    }

    /// The value at a dotted path (`server.database`).
    pub fn get(&self, path: &str) -> Option<&Value> {
        let mut segments = path.split('.');
        let first = self.table.get(segments.next()?)?;
        segments.try_fold(first, |value, segment| value.as_table()?.get(segment))
    }

    pub fn has(&self, path: &str) -> bool {
        self.get(path).is_some()
    }

    /// Deserialize the section at `path`; missing → `MissingSection`.
    pub fn section<T: DeserializeOwned>(&self, path: &str) -> Result<T, ConfigError> {
        self.optional_section(path)?.ok_or_else(|| ConfigError::MissingSection(path.to_owned()))
    }

    pub fn optional_section<T: DeserializeOwned>(&self, path: &str) -> Result<Option<T>, ConfigError> {
        self.get(path)
            .map(|value| value.clone().try_into().map_err(|e: toml::de::Error| ConfigError::Invalid { path: path.to_owned(), message: e.message().to_owned() }))
            .transpose()
    }

    /// Every field named like a credential holds a secret reference (server.md "Config").
    pub fn check_no_inline_secrets(&self) -> Result<(), ConfigError> {
        check_table(&self.table, "")
    }

    pub fn origin(&self) -> &str {
        &self.origin
    }
}

/// Field names that must hold secret references.
pub fn is_secret_field(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    name.ends_with("_secret") || name.ends_with("_key") || name.ends_with("_token") || name.ends_with("password") || name == "secret"
}

fn check_table(table: &Table, prefix: &str) -> Result<(), ConfigError> {
    for (key, value) in table {
        let path = if prefix.is_empty() { key.clone() } else { format!("{prefix}.{key}") };
        if is_secret_field(key) {
            check_reference(value, &path)?;
        } else {
            check_nested(value, &path)?;
        }
    }
    Ok(())
}

fn check_nested(value: &Value, path: &str) -> Result<(), ConfigError> {
    match value {
        Value::Table(t) => check_table(t, path),
        Value::Array(items) => items.iter().enumerate().try_for_each(|(i, v)| check_nested(v, &format!("{path}[{i}]"))),
        _ => Ok(()),
    }
}

fn check_reference(value: &Value, path: &str) -> Result<(), ConfigError> {
    match value {
        Value::String(s) if SecretSource::is_reference(s) => Ok(()),
        Value::Array(items) => items.iter().enumerate().try_for_each(|(i, v)| check_reference(v, &format!("{path}[{i}]"))),
        _ => Err(ConfigError::InlineSecret(path.to_owned())),
    }
}

fn parse_override(raw: &str) -> Value {
    format!("v = {raw}").parse::<Table>().ok().and_then(|mut t| t.remove("v")).unwrap_or_else(|| Value::String(raw.to_owned()))
}

fn set_path(table: &mut Table, path: &[String], value: Value) -> Result<(), ConfigError> {
    let Some((last, parents)) = path.split_last() else { return Ok(()) };
    let mut current = table;
    for (depth, segment) in parents.iter().enumerate() {
        let entry = current.entry(segment.clone()).or_insert_with(|| Value::Table(Table::new()));
        current = entry.as_table_mut().ok_or_else(|| ConfigError::Invalid {
            path: path[..=depth].join("."),
            message: "override descends into a non-table value".into(),
        })?;
    }
    current.insert(last.clone(), value);
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::io::Write;

    use super::*;

    #[test]
    fn secret_sources_parse_and_refuse_inline_values() {
        assert_eq!("env:MAGE_DATABASE_URL".parse::<SecretSource>(), Ok(SecretSource::Env("MAGE_DATABASE_URL".into())));
        assert_eq!("file:/etc/mage/key.pem".parse::<SecretSource>(), Ok(SecretSource::File("/etc/mage/key.pem".into())));
        let sm = "gcpsm:projects/p/secrets/github-read-app-key/versions/latest";
        assert_eq!(sm.parse::<SecretSource>().map(|s| s.to_string()), Ok(sm.to_owned()));
        for bad in ["hunter2", "env:", "env:A-B", "file:", "gcpsm:projects/p/secrets/s", "postgres://u:pw@h/db"] {
            let err = bad.parse::<SecretSource>().unwrap_err();
            assert!(!err.to_string().contains("hunter2") && !err.to_string().contains(":pw@"), "error echoes the value: {err}");
        }
    }

    #[test]
    fn local_sources_resolve_and_print_redacted() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        writeln!(file, "-----BEGIN KEY-----").unwrap();
        let source = SecretSource::File(file.path().to_owned());
        let value = source.resolve_local().unwrap();
        assert_eq!(value.expose(), "-----BEGIN KEY-----", "one trailing newline is dropped");
        assert_eq!(format!("{value:?} {value}"), "*** ***");
        assert!(matches!(SecretSource::Env("MAGE_TEST_SURELY_UNSET_VAR".into()).resolve_local(), Err(ConfigError::Secret { .. })));
        assert!(matches!(SecretSource::File("/nonexistent/mage".into()).resolve_local(), Err(ConfigError::Secret { .. })));
    }

    #[tokio::test]
    async fn secret_manager_reads_the_payload_with_the_metadata_token() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/computeMetadata/v1/instance/service-accounts/default/token"))
            .and(header("Metadata-Flavor", "Google"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"access_token": "ya29.t", "expires_in": 3599, "token_type": "Bearer"})))
            .mount(&server)
            .await;
        let name = "projects/p/secrets/github-read-app-key/versions/latest";
        Mock::given(method("GET"))
            .and(path(format!("/v1/{name}:access")))
            .and(header("authorization", "Bearer ya29.t"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "name": name,
                "payload": {"data": base64::engine::general_purpose::STANDARD.encode("-----BEGIN RSA PRIVATE KEY-----")}
            })))
            .mount(&server)
            .await;
        let manager = SecretManager::with_endpoints(server.uri(), server.uri());
        assert_eq!(manager.access(name).await.map(SecretValue::into_inner), Ok("-----BEGIN RSA PRIVATE KEY-----".to_owned()));
        let missing = manager.access("projects/p/secrets/absent/versions/1").await;
        assert!(matches!(missing, Err(ConfigError::Secret { .. })), "{missing:?}");
    }

    #[test]
    fn overrides_set_nested_paths_with_typed_values() {
        let file = ConfigFile::parse("[server]\nbind = \"0.0.0.0:8080\"\n[server.database]\npool_max = 15\n", "test").unwrap();
        let vars = [
            ("MAGE_SERVER__DATABASE__POOL_MAX".to_owned(), "20".to_owned()),
            ("MAGE_SERVER__BIND".to_owned(), "127.0.0.1:9090".to_owned()),
            ("MAGE_SERVER__JOBS__KINDS".to_owned(), "[\"intake.\", \"triage.\"]".to_owned()),
            ("MAGE_DATABASE_URL".to_owned(), "postgres://ignored".to_owned()),
            ("OTHER__X".to_owned(), "1".to_owned()),
        ];
        let file = file.with_overrides(vars).unwrap();
        assert_eq!(file.get("server.database.pool_max"), Some(&Value::Integer(20)));
        assert_eq!(file.get("server.bind"), Some(&Value::String("127.0.0.1:9090".into())), "unparseable TOML stays a string");
        assert_eq!(file.get("server.jobs.kinds").and_then(Value::as_array).map(Vec::len), Some(2));
        assert!(!file.has("database_url"), "variables without __ are not overrides");

        let bad = ConfigFile::parse("[server]\nbind = 1\n", "t").unwrap().with_overrides([("MAGE_SERVER__BIND__X".to_owned(), "1".to_owned())]);
        assert!(matches!(bad, Err(ConfigError::Invalid { .. })));
    }

    #[test]
    fn sections_deserialize_or_report_missing() {
        #[derive(Debug, Deserialize, PartialEq)]
        struct Db {
            url_source: SecretSource,
            pool_max: u32,
        }
        let file = ConfigFile::parse("[server.database]\nurl_source = \"env:MAGE_DATABASE_URL\"\npool_max = 15\n", "t").unwrap();
        assert_eq!(file.section::<Db>("server.database"), Ok(Db { url_source: SecretSource::Env("MAGE_DATABASE_URL".into()), pool_max: 15 }));
        assert_eq!(file.section::<Db>("indexer.database"), Err(ConfigError::MissingSection("indexer.database".into())));
        let inline = ConfigFile::parse("[server.database]\nurl_source = \"postgres://x\"\npool_max = 1\n", "t").unwrap();
        assert!(matches!(inline.section::<Db>("server.database"), Err(ConfigError::Invalid { .. })));
    }

    #[test]
    fn credential_named_fields_must_be_references() {
        let ok = ConfigFile::parse(
            "[server.github]\nclient_id = \"Iv1.x\"\napp_private_key = \"gcpsm:projects/p/secrets/k/versions/latest\"\n\
             [[server.extra]]\napi_token = \"env:TOKEN\"\n",
            "t",
        )
        .unwrap();
        assert_eq!(ok.check_no_inline_secrets(), Ok(()));
        let bad = ConfigFile::parse("[server.github]\napp_private_key = \"-----BEGIN RSA\"\n", "t").unwrap();
        assert_eq!(bad.check_no_inline_secrets(), Err(ConfigError::InlineSecret("server.github.app_private_key".into())));
        let nested = ConfigFile::parse("[[runner.hosts]]\npassword = \"pw\"\n", "t").unwrap();
        assert_eq!(nested.check_no_inline_secrets(), Err(ConfigError::InlineSecret("runner.hosts[0].password".into())));
        assert!(is_secret_field("webhook_secret") && is_secret_field("bot_token") && !is_secret_field("key_file"));
    }
}
