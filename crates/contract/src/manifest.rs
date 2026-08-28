//! `deploy.aho.cloud/v1`: the one document the cloud accepts or refuses. Nothing here names aho;
//! a project built with any tool can write it by hand.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

pub const API_VERSION: &str = "deploy.aho.cloud/v1";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// Always [`API_VERSION`].
    pub api_version: String,
    /// Project slug: lowercase letters, digits and `-`, 1–40 chars, starting with a letter.
    pub project: String,
    pub source: Source,
    pub build: Build,
    pub processes: Vec<Process>,
    #[serde(default)]
    pub domains: Vec<Domain>,
    pub auth: Auth,
    /// Secret names and where each may be sent. Values never appear in a manifest.
    #[serde(default)]
    pub secrets: Vec<Secret>,
    /// Every outbound host the app may reach (a leading `*.` matches subdomains). Must cover
    /// every secret's `reach`.
    #[serde(default)]
    pub egress: Vec<String>,
    pub data_class: DataClass,
    /// Required when `data_class` is not `none`.
    #[serde(default)]
    pub region: Option<String>,
    pub owner: Owner,
    pub budget: Budget,
}

/// What gets built or run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Source {
    /// A prebuilt image, by digest (`sha256:<64 hex>`).
    Oci { digest: String },
    /// A git commit (40 hex) in a repository the cloud can fetch.
    Git { repo: String, commit: String },
    /// A source archive uploaded to the cloud, by content hash (`blake3:<64 hex>`).
    Archive { hash: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Build {
    /// Only valid with `source.kind = oci`.
    Prebuilt,
    Dockerfile {
        #[serde(default = "default_dockerfile")]
        path: String,
        #[serde(default = "default_context")]
        context: String,
    },
    Buildpack {
        builder: String,
    },
    /// Run `command` inside `base_image` with the source at `/workspace`; the result is the image.
    Command {
        command: Vec<String>,
        base_image: String,
    },
}

fn default_dockerfile() -> String {
    "Dockerfile".into()
}

fn default_context() -> String {
    ".".into()
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct Process {
    /// Unique per manifest; same rules as the project slug.
    pub name: String,
    /// `None` runs the image's own entrypoint.
    #[serde(default)]
    pub command: Option<Vec<String>>,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub expose: Expose,
    pub health: Health,
    #[serde(default)]
    pub volume: Option<Volume>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum Expose {
    /// Reachable through the edge on the project's domains.
    Public,
    /// Reachable only from the project's other processes.
    #[default]
    Internal,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Health {
    /// `GET path` on the process port (or `port`) must answer 2xx.
    Http {
        path: String,
        #[serde(default)]
        port: Option<u16>,
    },
    /// Exit status 0 means healthy.
    Command { argv: Vec<String> },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct Volume {
    /// Absolute path inside the process.
    pub mount: String,
    pub size_mb: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct Domain {
    pub hostname: String,
    pub verification: DomainMode,
}

/// How the customer points the domain at us.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum DomainMode {
    /// `CNAME <project>.edge.<cloud>` plus a TXT record; HTTP-01 certificate.
    Cname,
    /// A/AAAA (or ALIAS) to the edge's stable addresses plus a TXT record.
    Apex,
    /// The subzone's NS records point at us; DNS-01 wildcard certificate.
    NsDelegated,
}

/// Authentication at the edge. Authorization stays in the app.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum Auth {
    /// No edge login; every route behaves as `passthrough` unless `edge.toml` says otherwise.
    None,
    /// One shared password per environment.
    Password,
    /// Email magic links, optionally limited to `allowed_domains` (e.g. `acme.com`).
    MagicLink {
        #[serde(default)]
        allowed_domains: Vec<String>,
    },
    Oidc {
        provider: OidcProvider,
    },
    /// The app authenticates its own users; the edge forwards untouched.
    AppOwned,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum OidcProvider {
    Google,
    Github,
    Microsoft,
    /// Any OIDC issuer, including a SAML identity provider behind an OIDC broker.
    Custom {
        issuer: String,
        client_id: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct Secret {
    /// Environment-variable style: `[A-Z][A-Z0-9_]*`.
    pub name: String,
    /// Hosts this secret may be sent to. Each must be covered by `egress`.
    pub reach: Vec<String>,
    /// ISO date (`YYYY-MM-DD`) the credential stops working, if it expires.
    #[serde(default)]
    pub expires: Option<String>,
}

/// Ordered from least to most regulated; raising it is a widening.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum DataClass {
    None,
    Pii,
    Payments,
    Health,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct Owner {
    pub name: String,
    pub email: String,
    #[serde(default)]
    pub phone: Option<String>,
    /// Called in order after the owner. The platform is always last and is not listed.
    #[serde(default)]
    pub escalation: Vec<Contact>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct Contact {
    pub name: String,
    pub email: String,
    #[serde(default)]
    pub phone: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct Budget {
    /// Whole US dollars per month; at least 1.
    pub ceiling_usd_month: u32,
    pub degrade: Degrade,
}

/// What happens when the ceiling is reached.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum Degrade {
    /// Stop every process; the edge serves a "paused" page until the owner raises the ceiling.
    Sleep,
}

impl Manifest {
    /// Parses JSON. Structural errors only; call [`crate::validate`] for the rules.
    pub fn from_json(text: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(text)
    }

    /// Content hash over the canonical form: `blake3:<hex>`.
    pub fn hash(&self) -> String {
        crate::canonical::hash(self)
    }

    pub fn process(&self, name: &str) -> Option<&Process> {
        self.processes.iter().find(|p| p.name == name)
    }

    pub fn secret(&self, name: &str) -> Option<&Secret> {
        self.secrets.iter().find(|s| s.name == name)
    }
}
