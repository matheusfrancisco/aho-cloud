//! The acceptance rules. A manifest the cloud refuses gets every reason at once, each with the
//! field it concerns and the fix, like an approval card names its unlock.

use std::collections::BTreeSet;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::manifest::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum RefusalCode {
    ApiVersion,
    ProjectSlug,
    SourceDigest,
    SourceBuildMismatch,
    BuildCommandEmpty,
    NoProcesses,
    ProcessName,
    DuplicateProcess,
    CommandEmpty,
    PublicWithoutPort,
    HealthPath,
    HealthCommandEmpty,
    VolumeMount,
    VolumeSize,
    Hostname,
    DuplicateDomain,
    OidcIssuer,
    MagicLinkDomain,
    SecretName,
    DuplicateSecret,
    SecretReachEmpty,
    SecretReachNotInEgress,
    SecretExpires,
    EgressHost,
    DataClassUnsupported,
    RegionRequired,
    OwnerEmail,
    BudgetZero,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
pub struct Refusal {
    pub code: RefusalCode,
    /// JSON pointer into the manifest, e.g. `/processes/1/port`.
    pub field: String,
    pub message: String,
    pub fix: String,
}

/// Every rule, all refusals at once. `Ok` means the cloud's gate accepts the document.
pub fn validate(m: &Manifest) -> Result<(), Vec<Refusal>> {
    let mut out = Refusals::default();
    if m.api_version != API_VERSION {
        out.push(
            RefusalCode::ApiVersion,
            "/api_version",
            format!("unsupported api_version `{}`", m.api_version),
            format!("set api_version to `{API_VERSION}`"),
        );
    }
    if !is_project_slug(&m.project) {
        out.push(
            RefusalCode::ProjectSlug,
            "/project",
            format!("`{}` is not a valid project slug", m.project),
            format!("use 1–{PROJECT_SLUG_MAX} lowercase letters, digits or `-`, starting with a letter"),
        );
    }
    source_and_build(m, &mut out);
    processes(m, &mut out);
    domains(m, &mut out);
    auth(m, &mut out);
    egress_and_secrets(m, &mut out);
    data(m, &mut out);
    if !is_email(&m.owner.email) {
        out.push(RefusalCode::OwnerEmail, "/owner/email", "owner email is not an address", "give the address whose phone rings");
    }
    for (i, c) in m.owner.escalation.iter().enumerate() {
        if !is_email(&c.email) {
            out.push(
                RefusalCode::OwnerEmail,
                &format!("/owner/escalation/{i}/email"),
                "escalation contact email is not an address",
                "fix or remove the contact",
            );
        }
    }
    if m.budget.ceiling_usd_month == 0 {
        out.push(RefusalCode::BudgetZero, "/budget/ceiling_usd_month", "a zero ceiling can never run", "set a ceiling of at least 1");
    }
    out.finish()
}

#[derive(Default)]
struct Refusals(Vec<Refusal>);

impl Refusals {
    fn push(&mut self, code: RefusalCode, field: &str, message: impl Into<String>, fix: impl Into<String>) {
        self.0.push(Refusal { code, field: field.to_owned(), message: message.into(), fix: fix.into() });
    }

    fn finish(self) -> Result<(), Vec<Refusal>> {
        if self.0.is_empty() { Ok(()) } else { Err(self.0) }
    }
}

fn source_and_build(m: &Manifest, out: &mut Refusals) {
    match &m.source {
        Source::Oci { digest } if !is_prefixed_hex(digest, "sha256:", 64) => out.push(
            RefusalCode::SourceDigest,
            "/source/digest",
            "image digest must be `sha256:` plus 64 lowercase hex",
            "use the digest your registry printed, not a tag",
        ),
        Source::Git { commit, .. } if !is_hex(commit, 40) => out.push(
            RefusalCode::SourceDigest,
            "/source/commit",
            "commit must be a full 40-hex id",
            "use `git rev-parse HEAD`, not a branch name",
        ),
        Source::Archive { hash } if !is_prefixed_hex(hash, "blake3:", 64) => out.push(
            RefusalCode::SourceDigest,
            "/source/hash",
            "archive hash must be `blake3:` plus 64 lowercase hex",
            "upload the archive and use the hash the cloud returned",
        ),
        _ => {}
    }
    let prebuilt_source = matches!(m.source, Source::Oci { .. });
    let prebuilt_build = matches!(m.build, Build::Prebuilt);
    if prebuilt_source != prebuilt_build {
        out.push(
            RefusalCode::SourceBuildMismatch,
            "/build/kind",
            "`build.kind = prebuilt` goes with `source.kind = oci`, and only with it",
            if prebuilt_source { "set build.kind to `prebuilt`" } else { "pick dockerfile, buildpack or command" },
        );
    }
    if let Build::Command { command, base_image } = &m.build
        && (command.is_empty() || base_image.trim().is_empty())
    {
        out.push(RefusalCode::BuildCommandEmpty, "/build", "command builds need a command and a base image", "fill both");
    }
}

fn processes(m: &Manifest, out: &mut Refusals) {
    if m.processes.is_empty() {
        out.push(RefusalCode::NoProcesses, "/processes", "nothing to run", "declare at least one process");
    }
    let mut seen = BTreeSet::new();
    for (i, p) in m.processes.iter().enumerate() {
        let at = |f: &str| format!("/processes/{i}{f}");
        if !is_slug(&p.name, 30) {
            out.push(
                RefusalCode::ProcessName,
                &at("/name"),
                format!("`{}` is not a valid process name", p.name),
                "use a slug like `web` or `worker`",
            );
        }
        if !seen.insert(p.name.as_str()) {
            out.push(
                RefusalCode::DuplicateProcess,
                &at("/name"),
                format!("process `{}` is declared twice", p.name),
                "rename or merge them",
            );
        }
        if p.command.as_ref().is_some_and(|c| c.is_empty()) {
            out.push(RefusalCode::CommandEmpty, &at("/command"), "empty command", "omit `command` to run the image entrypoint");
        }
        if p.expose == Expose::Public && p.port.is_none() {
            out.push(
                RefusalCode::PublicWithoutPort,
                &at("/port"),
                "a public process must listen on a port",
                "set `port`, or make it internal",
            );
        }
        match &p.health {
            Health::Http { path, port } => {
                if !path.starts_with('/') {
                    out.push(RefusalCode::HealthPath, &at("/health/path"), "health path must start with `/`", "e.g. `/healthz`");
                }
                if port.is_none() && p.port.is_none() {
                    out.push(
                        RefusalCode::HealthPath,
                        &at("/health/port"),
                        "HTTP health check has no port to hit",
                        "set the process port or health.port",
                    );
                }
            }
            Health::Command { argv } if argv.is_empty() => {
                out.push(
                    RefusalCode::HealthCommandEmpty,
                    &at("/health/argv"),
                    "empty health command",
                    "give a command that exits 0 when healthy",
                );
            }
            Health::Command { .. } => {}
        }
        if let Some(v) = &p.volume {
            if !v.mount.starts_with('/') || v.mount == "/" {
                out.push(
                    RefusalCode::VolumeMount,
                    &at("/volume/mount"),
                    "volume mount must be an absolute path other than `/`",
                    "e.g. `/data`",
                );
            }
            if v.size_mb == 0 || v.size_mb > 102_400 {
                out.push(RefusalCode::VolumeSize, &at("/volume/size_mb"), "volume size must be 1–102400 MB", "pick a size in range");
            }
        }
    }
}

fn domains(m: &Manifest, out: &mut Refusals) {
    let mut seen = BTreeSet::new();
    for (i, d) in m.domains.iter().enumerate() {
        let at = format!("/domains/{i}/hostname");
        if !is_hostname(&d.hostname, false) {
            out.push(
                RefusalCode::Hostname,
                &at,
                format!("`{}` is not a hostname", d.hostname),
                "lowercase labels, at least two, no scheme or path",
            );
        }
        if !seen.insert(d.hostname.as_str()) {
            out.push(RefusalCode::DuplicateDomain, &at, format!("`{}` is declared twice", d.hostname), "keep one entry");
        }
    }
}

fn auth(m: &Manifest, out: &mut Refusals) {
    match &m.auth {
        Auth::Oidc { provider: OidcProvider::Custom { issuer, client_id } } => {
            if !issuer.starts_with("https://") || client_id.trim().is_empty() {
                out.push(
                    RefusalCode::OidcIssuer,
                    "/auth/provider",
                    "custom OIDC needs an https issuer and a client id",
                    "copy both from your identity provider",
                );
            }
        }
        Auth::MagicLink { allowed_domains } => {
            for (i, d) in allowed_domains.iter().enumerate() {
                if !is_hostname(d, false) {
                    out.push(
                        RefusalCode::MagicLinkDomain,
                        &format!("/auth/allowed_domains/{i}"),
                        format!("`{d}` is not an email domain"),
                        "e.g. `acme.com`",
                    );
                }
            }
        }
        _ => {}
    }
}

fn egress_and_secrets(m: &Manifest, out: &mut Refusals) {
    for (i, host) in m.egress.iter().enumerate() {
        if !is_hostname(host, true) {
            out.push(
                RefusalCode::EgressHost,
                &format!("/egress/{i}"),
                format!("`{host}` is not a host"),
                "a hostname, optionally with a leading `*.`",
            );
        }
    }
    let mut seen = BTreeSet::new();
    for (i, s) in m.secrets.iter().enumerate() {
        let at = |f: &str| format!("/secrets/{i}{f}");
        if !is_env_name(&s.name) {
            out.push(RefusalCode::SecretName, &at("/name"), format!("`{}` is not a secret name", s.name), "use `[A-Z][A-Z0-9_]*`");
        }
        if !seen.insert(s.name.as_str()) {
            out.push(RefusalCode::DuplicateSecret, &at("/name"), format!("secret `{}` is declared twice", s.name), "keep one entry");
        }
        if s.reach.is_empty() {
            out.push(
                RefusalCode::SecretReachEmpty,
                &at("/reach"),
                "a secret nobody may receive is unusable",
                "name the hosts it is sent to",
            );
        }
        for (j, host) in s.reach.iter().enumerate() {
            if !m.egress.iter().any(|e| host_covered(e, host)) {
                out.push(
                    RefusalCode::SecretReachNotInEgress,
                    &at(&format!("/reach/{j}")),
                    format!("`{}` may be sent to `{host}`, which egress does not allow", s.name),
                    format!("add `{host}` to egress or remove it from reach"),
                );
            }
        }
        if let Some(date) = &s.expires
            && time::Date::parse(date, &time::format_description::well_known::Iso8601::DATE).is_err()
        {
            out.push(RefusalCode::SecretExpires, &at("/expires"), format!("`{date}` is not a date"), "use `YYYY-MM-DD`");
        }
    }
}

fn data(m: &Manifest, out: &mut Refusals) {
    match m.data_class {
        DataClass::Payments | DataClass::Health => out.push(
            RefusalCode::DataClassUnsupported,
            "/data_class",
            "payments and health data are not hosted in v1",
            "keep card data with your payment provider, or contact us for a dedicated deployment",
        ),
        DataClass::Pii if m.region.as_deref().is_none_or(|r| r.trim().is_empty()) => out.push(
            RefusalCode::RegionRequired,
            "/region",
            "personal data needs a fixed region",
            "set `region`, e.g. `eu-west` or `us-east`",
        ),
        _ => {}
    }
}

/// `pattern` is an egress entry (`api.x.com` or `*.x.com`); does it allow `host`?
pub fn host_covered(pattern: &str, host: &str) -> bool {
    match pattern.strip_prefix("*.") {
        Some(suffix) => host.len() > suffix.len() + 1 && host.ends_with(suffix) && host.as_bytes()[host.len() - suffix.len() - 1] == b'.',
        None => pattern == host,
    }
}

/// Longest project slug.
pub const PROJECT_SLUG_MAX: usize = 40;

/// The manifest's `project` rule; every service that names a project by slug uses it.
pub fn is_project_slug(s: &str) -> bool {
    is_slug(s, PROJECT_SLUG_MAX)
}

pub(crate) fn is_slug(s: &str, max: usize) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= max
        && b[0].is_ascii_lowercase()
        && !s.ends_with('-')
        && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

/// Lowercase DNS name with at least two labels; `wildcard` allows one leading `*.`.
pub fn is_hostname(s: &str, wildcard: bool) -> bool {
    let body = match s.strip_prefix("*.") {
        Some(rest) if wildcard => rest,
        Some(_) => return false,
        None => s,
    };
    if body.len() > 253 || !body.contains('.') {
        return false;
    }
    body.split('.').all(|label| {
        let b = label.as_bytes();
        !b.is_empty()
            && b.len() <= 63
            && b[0] != b'-'
            && b[b.len() - 1] != b'-'
            && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
    })
}

pub(crate) fn is_env_name(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty() && b[0].is_ascii_uppercase() && b.iter().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || *c == b'_')
}

fn is_email(s: &str) -> bool {
    match s.split_once('@') {
        Some((local, domain)) => {
            !local.is_empty() && !local.contains(char::is_whitespace) && is_hostname(&domain.to_ascii_lowercase(), false)
        }
        None => false,
    }
}

fn is_hex(s: &str, len: usize) -> bool {
    s.len() == len && s.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}

fn is_prefixed_hex(s: &str, prefix: &str, len: usize) -> bool {
    s.strip_prefix(prefix).is_some_and(|h| is_hex(h, len))
}
