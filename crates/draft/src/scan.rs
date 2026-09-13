//! Static scans over text the session wrote or passed to tools: outbound hosts, environment
//! variables the code reads, and values that look like credentials (never copied).

use std::sync::LazyLock;

use regex::Regex;

// ───────────────────────────── secret-looking values ─────────────────────────────

/// Well-known credential prefixes (OpenAI/Anthropic, Stripe, GitHub, GitLab, Slack, AWS, Google,
/// npm, Hugging Face, PyPI, SendGrid, Shopify, PEM keys).
const SECRET_PREFIXES: &[&str] = &[
    "sk-",
    "sk_live_",
    "sk_test_",
    "rk_live_",
    "rk_test_",
    "whsec_",
    "ghp_",
    "gho_",
    "ghu_",
    "ghs_",
    "ghr_",
    "github_pat_",
    "glpat-",
    "xoxb-",
    "xoxp-",
    "xoxa-",
    "xapp-",
    "AKIA",
    "ASIA",
    "AIza",
    "npm_",
    "hf_",
    "pypi-",
    "SG.",
    "shpat_",
    "-----BEGIN",
];

/// Words in a variable name that mark its value as a credential.
const SECRET_NAME_WORDS: &[&str] = &["SECRET", "TOKEN", "PASSWORD", "PASSWD", "PASS", "PWD", "KEY", "AUTH", "CREDENTIAL", "PRIVATE", "DSN"];

/// Is `name` (an env or flag name) one whose value is a credential?
pub(crate) fn secret_name(name: &str) -> bool {
    let upper = name.trim_start_matches('-').to_ascii_uppercase().replace('-', "_");
    upper.split('_').any(|part| SECRET_NAME_WORDS.contains(&part)) || upper.ends_with("APIKEY")
}

/// Does `word` contain something that looks like a credential? Conservative: a false positive
/// costs one drafted field, a false negative leaks a secret into a document.
pub(crate) fn looks_secret(word: &str) -> bool {
    let w = word.trim_matches(|c| c == '"' || c == '\'');
    if let Some((k, v)) = w.split_once('=')
        && !v.is_empty()
        && (secret_name(k) || looks_secret(v))
    {
        return true;
    }
    if url_has_password(w) {
        return true;
    }
    if !(w.starts_with('/') || w.starts_with('.') || w.contains("://")) && token_like(w) {
        return true;
    }
    w.split(['/', '?', '&', '=', '#', ':', '@', ',', ';']).any(token_like)
}

fn token_like(s: &str) -> bool {
    if SECRET_PREFIXES.iter().any(|p| s.starts_with(p) && s.len() >= p.len() + 8) {
        return true;
    }
    let parts: Vec<&str> = s.split('.').collect();
    if parts.len() == 3 && s.starts_with("eyJ") && parts.iter().all(|p| p.len() >= 8) {
        return true;
    }
    s.len() >= 20
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'_' | b'-' | b'='))
        && s.bytes().any(|b| b.is_ascii_uppercase())
        && s.bytes().any(|b| b.is_ascii_lowercase())
        && s.bytes().any(|b| b.is_ascii_digit())
        && entropy(s) >= 3.5
}

fn entropy(s: &str) -> f64 {
    let mut counts = [0u32; 256];
    for b in s.bytes() {
        counts[b as usize] += 1;
    }
    let n = s.len() as f64;
    counts.iter().filter(|&&c| c > 0).map(|&c| f64::from(c) / n).map(|p| -p * p.log2()).sum()
}

fn url_has_password(s: &str) -> bool {
    let Some((_, rest)) = s.split_once("://") else { return false };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    authority.rsplit_once('@').is_some_and(|(userinfo, _)| userinfo.contains(':'))
}

/// `argv` for a human-readable note, credentials replaced by `[redacted]`.
pub(crate) fn render_safe(argv: &[String]) -> String {
    let mut out = Vec::with_capacity(argv.len());
    let mut redact_next = false;
    for w in argv {
        let shown = if redact_next {
            redact_next = false;
            redact_value(w)
        } else if let Some((flag, v)) = w.split_once('=').filter(|(f, _)| f.starts_with('-')) {
            if secret_name(flag) || looks_secret(v) || flag == "--build-arg" { format!("{flag}=[redacted]") } else { w.clone() }
        } else if w.starts_with('-') && (secret_name(w) || w == "--build-arg" || w == "--secret") {
            redact_next = true;
            w.clone()
        } else if looks_secret(w) {
            "[redacted]".to_owned()
        } else {
            quote(w)
        };
        out.push(shown);
    }
    out.join(" ")
}

fn redact_value(w: &str) -> String {
    match w.split_once('=') {
        Some((k, _)) => format!("{k}=[redacted]"),
        None => "[redacted]".to_owned(),
    }
}

fn quote(w: &str) -> String {
    if w.is_empty() || w.contains(char::is_whitespace) { format!("'{w}'") } else { w.to_owned() }
}

// ───────────────────────────── hosts ─────────────────────────────

static URL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b(?:https?|wss?)://([^\s/?#'`<>\x22\\]+)").unwrap_or_else(|_| unreachable!("static regex")));

/// Hostnames of the `http(s)://` and `ws(s)://` URLs in `text`, lowercased, without userinfo or
/// port; IP literals, placeholders and reserved names skipped.
pub(crate) fn url_hosts(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for cap in URL.captures_iter(text) {
        let authority = &cap[1];
        let hostport = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
        if hostport.starts_with('[') {
            continue;
        }
        let host = hostport.split(':').next().unwrap_or("").trim_end_matches('.').to_ascii_lowercase();
        if public_hostname(&host) && !out.contains(&host) {
            out.push(host);
        }
    }
    out
}

fn public_hostname(host: &str) -> bool {
    let labels: Vec<&str> = host.split('.').collect();
    labels.len() >= 2
        && labels.iter().all(|l| {
            !l.is_empty()
                && l.len() <= 63
                && !l.starts_with('-')
                && !l.ends_with('-')
                && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
        && !labels.iter().all(|l| l.bytes().all(|b| b.is_ascii_digit()))
        && labels.last().is_some_and(|tld| tld.bytes().any(|b| b.is_ascii_alphabetic()))
        && !RESERVED.iter().any(|d| suffix_match(host, d))
}

/// Names that are never an app's egress: loopback, special-use TLDs, documentation domains and
/// XML/JSON namespace hosts that appear in markup.
const RESERVED: &[&str] = &[
    "localhost",
    "local",
    "internal",
    "test",
    "invalid",
    "example",
    "example.com",
    "example.org",
    "example.net",
    "w3.org",
    "json-schema.org",
    "schema.org",
    "xmlsoap.org",
];

/// Hosts only development touches: package registries and toolchain downloads, model providers
/// (the agent's own traffic), and GitHub (source hosting). Matched with their subdomains.
const DEV_ONLY: &[&str] = &[
    // package registries and toolchains
    "npmjs.org",
    "npmjs.com",
    "yarnpkg.com",
    "pypi.org",
    "pythonhosted.org",
    "crates.io",
    "rust-lang.org",
    "rustup.rs",
    "golang.org",
    "go.dev",
    "rubygems.org",
    "maven.org",
    "repo.maven.apache.org",
    "gradle.org",
    "docker.io",
    "docker.com",
    "ghcr.io",
    "quay.io",
    "debian.org",
    "ubuntu.com",
    "alpinelinux.org",
    "deno.land",
    "jsr.io",
    "packagist.org",
    "nuget.org",
    "anaconda.org",
    "anaconda.com",
    "nodejs.org",
    "bun.sh",
    // model providers
    "openai.com",
    "anthropic.com",
    "generativelanguage.googleapis.com",
    "mistral.ai",
    "cohere.ai",
    "cohere.com",
    "groq.com",
    "openrouter.ai",
    "together.xyz",
    "fireworks.ai",
    "deepseek.com",
    "x.ai",
    "huggingface.co",
    "perplexity.ai",
    // source hosting
    "github.com",
    "githubusercontent.com",
];

/// Is `host` one development uses and a deployed app does not need?
pub(crate) fn dev_only(host: &str) -> bool {
    DEV_ONLY.iter().any(|d| suffix_match(host, d))
}

fn suffix_match(host: &str, domain: &str) -> bool {
    host == domain || host.strip_suffix(domain).is_some_and(|rest| rest.ends_with('.'))
}

// ───────────────────────────── env reads ─────────────────────────────

static ENV_READS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        r"process\.env\.([A-Z][A-Z0-9_]*)\b",
        r#"process\.env\[\s*["'`]([A-Z][A-Z0-9_]*)["'`]\s*\]"#,
        r#"os\.environ\[\s*["']([A-Z][A-Z0-9_]*)["']\s*\]"#,
        r#"os\.environ\.get\(\s*["']([A-Z][A-Z0-9_]*)["']"#,
        r#"os\.getenv\(\s*["']([A-Z][A-Z0-9_]*)["']"#,
        r#"\benv::var(?:_os)?\(\s*"([A-Z][A-Z0-9_]*)""#,
        r#"\bENV(?:\.fetch\(|\[)\s*["']([A-Z][A-Z0-9_]*)["']"#,
        r#"Deno\.env\.get\(\s*["']([A-Z][A-Z0-9_]*)["']"#,
        r#"\bos\.Getenv\(\s*"([A-Z][A-Z0-9_]*)""#,
    ]
    .iter()
    .filter_map(|p| Regex::new(p).ok())
    .collect()
});

/// Variables the platform sets or that never hold a credential.
const PLATFORM_ENV: &[&str] = &[
    "PORT",
    "HOST",
    "HOSTNAME",
    "NODE_ENV",
    "ENV",
    "ENVIRONMENT",
    "DEBUG",
    "LOG_LEVEL",
    "RUST_LOG",
    "RUST_BACKTRACE",
    "PATH",
    "HOME",
    "PWD",
    "USER",
    "SHELL",
    "TERM",
    "TZ",
    "LANG",
    "CI",
    "PYTHONPATH",
    "PYTHONUNBUFFERED",
    "TMPDIR",
];

/// One environment read found in code: the variable name, the matched expression and its line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct EnvRead {
    pub name: String,
    pub expr: String,
    pub line: usize,
}

/// Environment variables `code` reads, in order of first appearance.
pub(crate) fn env_reads(code: &str) -> Vec<EnvRead> {
    let mut hits: Vec<(usize, EnvRead)> = Vec::new();
    for re in ENV_READS.iter() {
        for cap in re.captures_iter(code) {
            let (Some(all), Some(name)) = (cap.get(0), cap.get(1)) else { continue };
            if PLATFORM_ENV.contains(&name.as_str()) {
                continue;
            }
            let line = code[..all.start()].matches('\n').count() + 1;
            hits.push((all.start(), EnvRead { name: name.as_str().to_owned(), expr: all.as_str().to_owned(), line }));
        }
    }
    hits.sort_by_key(|(at, _)| *at);
    let mut out: Vec<EnvRead> = Vec::new();
    for (_, r) in hits {
        if !out.iter().any(|o| o.name == r.name) {
            out.push(r);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_are_recognised() {
        for s in [
            "sk-proj-abcdEFGH12345678",
            "sk_live_51HxYzAbCdEf",
            "ghp_0123456789abcdefABCDEF",
            "AKIAIOSFODNN7EXAMPLE",
            "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U",
            "API_TOKEN=hunter2",
            "--password=hunter2",
            "https://bob:hunter2@db.acme.io/prod",
            "https://hooks.slack.com/services/T0000/B0000/Xa9bQ2cR7dS4eT1fU8gV",
            "Zm9vYmFyYmF6cXV4MTIzNDU2Nzg5MEFCQ0RFRg",
        ] {
            assert!(looks_secret(s), "{s}");
        }
    }

    #[test]
    fn ordinary_words_are_not_credentials() {
        for s in [
            "npm",
            "run",
            "build",
            "--port=3000",
            "Dockerfile",
            "./cmd/server",
            "sha256:4f53cda18c2baa0c0354bb5f9a3ecbe5ed12ab4d8e11ba873c2f11161202b945",
            "PORT=8080",
            "https://api.stripe.com/v1/charges",
            "node_modules/.bin/next",
        ] {
            assert!(!looks_secret(s), "{s}");
        }
    }

    #[test]
    fn notes_redact_credentials_and_build_args() {
        let argv: Vec<String> =
            ["docker", "build", "--build-arg", "NPM_TOKEN=npm_abcdefghijklmnop", "--secret=id=x,src=y", "-t", "app", "."]
                .map(String::from)
                .to_vec();
        assert_eq!(render_safe(&argv), "docker build --build-arg NPM_TOKEN=[redacted] --secret=[redacted] -t app .");
    }

    #[test]
    fn hosts_drop_userinfo_ports_ips_and_reserved_names() {
        let text = "fetch('https://api.stripe.com/v1/x'); ws://Realtime.Acme.io:8443/s https://u:p@db.acme.io http://127.0.0.1:3000 http://localhost:8080 <svg xmlns=\"http://www.w3.org/2000/svg\"> https://${API_HOST}/x http://[::1]:80/";
        assert_eq!(url_hosts(text), vec!["api.stripe.com", "realtime.acme.io", "db.acme.io"]);
    }

    #[test]
    fn dev_hosts_match_with_subdomains_only() {
        assert!(
            dev_only("registry.npmjs.org") && dev_only("api.openai.com") && dev_only("github.com") && dev_only("raw.githubusercontent.com")
        );
        assert!(!dev_only("api.stripe.com") && !dev_only("notgithub.com") && !dev_only("fox.ai"));
    }

    #[test]
    fn env_reads_cover_each_language_and_skip_platform_variables() {
        let code = r#"
const a = process.env.STRIPE_KEY; const b = process.env["DB_URL"];
x = os.environ["PY_SECRET"]; y = os.getenv('PY_OTHER'); z = os.environ.get("PY_THIRD")
let r = std::env::var("RUST_TOKEN"); let s = env::var_os("RUST_OTHER");
ENV["RUBY_KEY"]; ENV.fetch("RUBY_OTHER"); Deno.env.get("DENO_KEY"); os.Getenv("GO_KEY")
const port = process.env.PORT; const again = process.env.STRIPE_KEY;
"#;
        let names: Vec<String> = env_reads(code).into_iter().map(|r| r.name).collect();
        assert_eq!(
            names,
            [
                "STRIPE_KEY",
                "DB_URL",
                "PY_SECRET",
                "PY_OTHER",
                "PY_THIRD",
                "RUST_TOKEN",
                "RUST_OTHER",
                "RUBY_KEY",
                "RUBY_OTHER",
                "DENO_KEY",
                "GO_KEY"
            ]
        );
        assert_eq!(env_reads(code)[0].line, 2);
    }
}
