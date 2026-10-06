//! Pure string predicates behind the recipe validation rules and canonical path form.

use std::borrow::Cow;
use std::net::{Ipv4Addr, Ipv6Addr};

use super::template;

/// Canonical step 2: collapse `//`, strip leading `./`, drop a trailing `/`. An empty result
/// (`"./"`) is `"."`; a lone `/` is kept so absolute paths stay visible.
pub(super) fn normalize_path(path: &str) -> Cow<'_, str> {
    if !path.contains("//") {
        return Cow::Borrowed(trim_path(path));
    }
    let mut collapsed = String::with_capacity(path.len());
    let mut prev_slash = false;
    for c in path.chars() {
        let slash = c == '/';
        if !(slash && prev_slash) {
            collapsed.push(c);
        }
        prev_slash = slash;
    }
    let trimmed = trim_path(&collapsed);
    if trimmed.len() == collapsed.len() { Cow::Owned(collapsed) } else { Cow::Owned(trimmed.to_owned()) }
}

fn trim_path(path: &str) -> &str {
    let mut s = path;
    while let Some(rest) = s.strip_prefix("./") {
        s = rest;
    }
    if s.len() > 1 {
        s = s.strip_suffix('/').unwrap_or(s);
    }
    if s.is_empty() { "." } else { s }
}

/// `PathNotRelative`: why a path field is not a plain workspace-relative path.
pub(super) fn path_problem(raw: &str) -> Option<&'static str> {
    if raw.is_empty() {
        return Some("is empty");
    }
    if raw.contains('\0') {
        return Some("contains NUL");
    }
    if raw.contains('\\') {
        return Some("contains `\\`");
    }
    if raw.contains('~') {
        return Some("contains `~`");
    }
    if raw.contains("${") {
        return Some("contains `${`; path fields are not templates");
    }
    let path = normalize_path(raw);
    if path.starts_with('/') {
        return Some("is absolute");
    }
    if path.split('/').any(|seg| seg == "..") {
        return Some("has a `..` segment");
    }
    if path.split('/').next() == Some(".git") {
        return Some("is under `.git/`");
    }
    None
}

/// `AbsoluteHostPath`: the first token (split on whitespace, `=`, `:`, quotes) that starts with
/// `/` and is not `/bin/…`, `/usr/bin/…`, or `/dev/null`. URLs are exempt for their path part,
/// except `file:` URLs, whose path is a host path.
pub(super) fn absolute_host_path(value: &str) -> Option<&str> {
    let words = value.split(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '`'));
    for part in words.flat_map(|word| word.split('=')) {
        if let Some(sep) = scheme_end(part) {
            if part[..sep].eq_ignore_ascii_case("file") {
                let rest = &part[sep + 3..];
                if let Some(slash) = rest.find('/') {
                    let path = &rest[slash..];
                    if !allowed_host_path(path) {
                        return Some(path);
                    }
                }
            }
            continue;
        }
        if let Some(token) = part.split(':').find(|t| t.starts_with('/') && !allowed_host_path(t)) {
            return Some(token);
        }
    }
    None
}

fn allowed_host_path(path: &str) -> bool {
    if path == "/dev/null" {
        return true;
    }
    let under = |dir: &str| path == dir || path.strip_prefix(dir).is_some_and(|rest| rest.starts_with('/'));
    (under("/bin") || under("/usr/bin")) && !path.split('/').any(|seg| seg == "..")
}

/// `HardcodedPort`: the loopback host literal followed by `:<digit>`, if any.
pub(super) fn hardcoded_port(value: &str) -> Option<&'static str> {
    const HOSTS: [&str; 4] = ["127.0.0.1:", "localhost:", "0.0.0.0:", "[::1]:"];
    let bytes = value.as_bytes();
    HOSTS.into_iter().find(|host| {
        positions_ignore_case(value, host).any(|i| bytes.get(i + host.len()).is_some_and(u8::is_ascii_digit))
    })
}

/// `LiveSecret`: the live-credential pattern `value` matches, if any. Never echo `value`.
pub(super) fn live_secret(value: &str) -> Option<&'static str> {
    const LITERALS: [&str; 8] =
        ["sk_live_", "rk_live_", "ghp_", "gho_", "ghs_", "github_pat_", "sk-ant-", "\"private_key\":"];
    if let Some(lit) = LITERALS.into_iter().find(|lit| value.contains(lit)) {
        return Some(lit);
    }
    let bytes = value.as_bytes();
    let run =
        |from: usize, n: usize, ok: fn(u8) -> bool| bytes.get(from..from + n).is_some_and(|s| s.iter().all(|&b| ok(b)));
    if value.match_indices("AKIA").any(|(i, _)| run(i + 4, 16, |b| b.is_ascii_digit() || b.is_ascii_uppercase())) {
        return Some("AKIA[0-9A-Z]{16}");
    }
    if value
        .match_indices("xox")
        .any(|(i, _)| matches!(bytes.get(i + 3), Some(b'a' | b'b' | b'p' | b'r')) && bytes.get(i + 4) == Some(&b'-'))
    {
        return Some("xox[abpr]-");
    }
    if value.match_indices("AIza").any(|(i, _)| run(i + 4, 35, |b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'))
    {
        return Some("AIza[0-9A-Za-z_-]{35}");
    }
    const BEGIN: &str = "-----BEGIN ";
    let private_key = value.match_indices(BEGIN).any(|(i, _)| {
        let rest = &value[i + BEGIN.len()..];
        // `[A-Z ]*` is maximal: the regex needs `-` right after `PRIVATE KEY`.
        let len = rest.bytes().take_while(|&b| b.is_ascii_uppercase() || b == b' ').count();
        rest[..len].ends_with("PRIVATE KEY") && rest[len..].starts_with("-----")
    });
    if private_key {
        return Some("-----BEGIN [A-Z ]*PRIVATE KEY-----");
    }
    None
}

/// `NotFakeSecret`: key matches `(?i)(secret|token|password|passwd|api_?key|private_?key|credential|dsn)$`
/// or `(?i)_KEY$`.
pub(super) fn is_secret_key(key: &str) -> bool {
    const SUFFIXES: [&str; 11] = [
        "secret",
        "token",
        "password",
        "passwd",
        "apikey",
        "api_key",
        "privatekey",
        "private_key",
        "credential",
        "dsn",
        "_key",
    ];
    SUFFIXES.into_iter().any(|suffix| ends_with_ignore_case(key, suffix))
}

/// `NotFakeSecret`: the value is a template reference or carries a fake marker.
pub(super) fn looks_fake(value: &str) -> bool {
    const MARKERS: [&str; 7] = ["fake", "test", "mage", "dummy", "example", "changeme", "localhost"];
    template::vars(value).next().is_some()
        || MARKERS.into_iter().any(|marker| positions_ignore_case(value, marker).next().is_some())
}

/// `ReservedEnv`: keys owned by the runner or that change process behaviour.
pub(super) fn is_reserved_env(key: &str) -> bool {
    const EXACT: [&str; 7] = ["PATH", "HOME", "USER", "LOGNAME", "SHELL", "TMPDIR", "GOOGLE_APPLICATION_CREDENTIALS"];
    const PREFIXES: [&str; 7] = ["LD_", "DYLD_", "MAGE_", "ANTHROPIC_", "CLAUDE_", "PI_", "AWS_"];
    EXACT.contains(&key) || PREFIXES.into_iter().any(|p| key.starts_with(p)) || ends_with_ignore_case(key, "_proxy")
}

/// `^[a-z][a-z0-9_-]{0,31}$` (services, build steps, SCIP languages).
pub(super) fn is_name(s: &str) -> bool {
    lower_ident(s, true)
}

/// `^[a-z][a-z0-9_]{0,31}$` (`PortName`; ports become `MAGE_PORT_<NAME>`).
pub(super) fn is_port_name(s: &str) -> bool {
    lower_ident(s, false)
}

fn lower_ident(s: &str, hyphen: bool) -> bool {
    let b = s.as_bytes();
    (1..=32).contains(&b.len())
        && b[0].is_ascii_lowercase()
        && b[1..].iter().all(|&c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_' || (hyphen && c == b'-'))
}

/// `^[A-Z_][A-Z0-9_]{0,127}$`.
pub(super) fn is_env_key(s: &str) -> bool {
    let b = s.as_bytes();
    (1..=128).contains(&b.len())
        && (b[0].is_ascii_uppercase() || b[0] == b'_')
        && b[1..].iter().all(|&c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == b'_')
}

/// `^[a-z0-9_]+$` (database extensions).
pub(super) fn is_extension_name(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_')
}

/// Host of a URL-shaped value (`scheme://[userinfo@]host[:port][/…]`), brackets kept for IPv6.
pub(super) fn url_host(value: &str) -> Option<&str> {
    let sep = scheme_end(value)?;
    let rest = &value[sep + 3..];
    let authority = &rest[..rest.find(['/', '?', '#']).unwrap_or(rest.len())];
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    Some(if host_port.starts_with('[') {
        host_port.find(']').map_or(host_port, |end| &host_port[..=end])
    } else {
        host_port.split(':').next().unwrap_or(host_port)
    })
}

/// Loopback (or unspecified) address, or `localhost`.
pub(super) fn is_loopback_host(host: &str) -> bool {
    let bare = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')).unwrap_or(host);
    bare.eq_ignore_ascii_case("localhost")
        || ends_with_ignore_case(bare, ".localhost")
        || bare.parse::<Ipv4Addr>().is_ok_and(|ip| ip.is_loopback() || ip.is_unspecified())
        || bare.parse::<Ipv6Addr>().is_ok_and(|ip| ip.is_loopback() || ip.is_unspecified())
}

/// `ExternalUrl`: host of a URL-shaped value that is neither loopback nor a template reference.
pub(super) fn external_host(value: &str) -> Option<&str> {
    url_host(value).filter(|host| !host.is_empty() && !host.contains("${") && !is_loopback_host(host))
}

/// Byte index of `://` when `value` starts with a URL scheme (`[A-Za-z][A-Za-z0-9+.-]*`).
fn scheme_end(value: &str) -> Option<usize> {
    let sep = value.find("://")?;
    let scheme = &value.as_bytes()[..sep];
    let valid = scheme.first().is_some_and(u8::is_ascii_alphabetic)
        && scheme.iter().all(|&b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'));
    valid.then_some(sep)
}

fn ends_with_ignore_case(s: &str, suffix: &str) -> bool {
    s.len() >= suffix.len() && s.as_bytes()[s.len() - suffix.len()..].eq_ignore_ascii_case(suffix.as_bytes())
}

/// Start offsets where non-empty ASCII `needle` occurs in `hay`, ignoring ASCII case.
fn positions_ignore_case<'a>(hay: &'a str, needle: &'a str) -> impl Iterator<Item = usize> + 'a {
    let needle = needle.as_bytes();
    hay.as_bytes()
        .windows(needle.len().max(1))
        .enumerate()
        .filter(move |(_, window)| window.eq_ignore_ascii_case(needle))
        .map(|(i, _)| i)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_path_applies_the_three_canonical_rules() {
        let cases = [
            ("frontend", "frontend"),
            ("./frontend/", "frontend"),
            ("././a//b///c/", "a/b/c"),
            (".//a", "a"),
            ("./", "."),
            (".", "."),
            ("", "."),
            ("/", "/"),
            ("//abs//x/", "/abs/x"),
            ("a/./b", "a/./b"),
        ];
        for (raw, want) in cases {
            assert_eq!(normalize_path(raw), want, "{raw:?}");
        }
        assert!(matches!(normalize_path("src/a"), Cow::Borrowed(_)));
    }

    #[test]
    fn path_problem_catches_escapes_and_git_dir() {
        for (raw, why) in [
            ("", "is empty"),
            ("/abs", "is absolute"),
            ("a/../../x", "has a `..` segment"),
            ("~/x", "contains `~`"),
            ("a\\b", "contains `\\`"),
            ("a\0b", "contains NUL"),
            ("./.git/hooks", "is under `.git/`"),
            (".git", "is under `.git/`"),
            ("${ws}/x", "contains `${`; path fields are not templates"),
        ] {
            assert_eq!(path_problem(raw), Some(why), "{raw:?}");
        }
        for ok in [".", "./frontend/", "packages//api", "src/**/*.ts", ".github/x", "a/.git"] {
            assert_eq!(path_problem(ok), None, "{ok:?}");
        }
    }

    #[test]
    fn absolute_host_path_tokenization() {
        assert_eq!(absolute_host_path("cat /etc/passwd"), Some("/etc/passwd"));
        assert_eq!(absolute_host_path("PATH=/opt/bin:/usr/bin"), Some("/opt/bin"));
        assert_eq!(absolute_host_path("x --cfg='/home/me/.rc'"), Some("/home/me/.rc"));
        assert_eq!(absolute_host_path("/bin/../etc/x"), Some("/bin/../etc/x"));
        assert_eq!(absolute_host_path("cat file:///etc/hosts"), Some("/etc/hosts"));
        for ok in [
            "/usr/bin/env node app.js",
            "/bin/sh -c 'x' > /dev/null",
            "${ws}/bin/run --data=${scratch}/d",
            "redis://127.0.0.1:${port.redis}/0",
            "--url=postgres://u:p@127.0.0.1:${port.db}/app",
            "/usr/bin",
        ] {
            assert_eq!(absolute_host_path(ok), None, "{ok:?}");
        }
    }

    #[test]
    fn hardcoded_port_needs_a_digit_after_the_host() {
        assert_eq!(hardcoded_port("serve --addr localhost:3000"), Some("localhost:"));
        assert_eq!(hardcoded_port("http://LOCALHOST:80/"), Some("localhost:"));
        assert_eq!(hardcoded_port("http://[::1]:8080"), Some("[::1]:"));
        assert_eq!(hardcoded_port("0.0.0.0:1"), Some("0.0.0.0:"));
        assert_eq!(hardcoded_port("127.0.0.1:${port.web}"), None);
        assert_eq!(hardcoded_port("127.0.0.1:"), None);
        assert_eq!(hardcoded_port("10.0.0.1:80"), None);
    }

    #[test]
    fn live_secret_patterns_and_their_boundaries() {
        let akia = format!("AKIA{}", "A1B2C3D4E5F6G7H8");
        let aiza = format!("AIza{}", "a".repeat(35));
        for (value, pattern) in [
            ("x sk_live_123", "sk_live_"),
            ("ghp_abc", "ghp_"),
            ("{\"private_key\": \"x\"}", "\"private_key\":"),
            (akia.as_str(), "AKIA[0-9A-Z]{16}"),
            ("xoxb-123", "xox[abpr]-"),
            (aiza.as_str(), "AIza[0-9A-Za-z_-]{35}"),
            ("-----BEGIN PRIVATE KEY-----", "-----BEGIN [A-Z ]*PRIVATE KEY-----"),
            ("-----BEGIN RSA PRIVATE KEY-----\nMII", "-----BEGIN [A-Z ]*PRIVATE KEY-----"),
        ] {
            assert_eq!(live_secret(value), Some(pattern), "{value:?}");
        }
        let short_akia = format!("AKIA{}", "A1B2C3D4E5F6G7H");
        let short_aiza = format!("AIza{}", "a".repeat(34));
        for ok in [
            "sk_test_mage_fake",
            short_akia.as_str(),
            "AKIAa1b2c3d4e5f6g7h8",
            "xoxc-1",
            short_aiza.as_str(),
            "-----BEGIN PUBLIC KEY-----",
            "-----BEGIN PRIVATE KEY",
            "-----BEGIN PRIVATE KEYS-----",
            "-----BEGIN PRIVATE DATA-----",
            "private_key: x",
        ] {
            assert_eq!(live_secret(ok), None, "{ok:?}");
        }
    }

    #[test]
    fn secret_keys_and_fake_values() {
        for key in [
            "SESSION_SECRET",
            "GITHUB_TOKEN",
            "DB_PASSWORD",
            "PASSWD",
            "APIKEY",
            "STRIPE_API_KEY",
            "SENTRY_DSN",
            "X_KEY",
            "AWS_CREDENTIAL",
            "session_secret",
        ] {
            assert!(is_secret_key(key), "{key}");
        }
        for key in ["MONKEY", "TOKENS", "KEY", "DATABASE_URL", "PORT"] {
            assert!(!is_secret_key(key), "{key}");
        }
        for value in ["${db.password}", "x${ws}", "sk_test_1", "Mage-Secret", "DUMMY", "changeme", "http://localhost"] {
            assert!(looks_fake(value), "{value}");
        }
        for value in ["hunter2", "$${db.password}", ""] {
            assert!(!looks_fake(value), "{value}");
        }
    }

    #[test]
    fn reserved_env_keys() {
        for key in [
            "PATH",
            "HOME",
            "TMPDIR",
            "LD_PRELOAD",
            "DYLD_LIBRARY_PATH",
            "HTTPS_PROXY",
            "no_proxy",
            "MAGE_X",
            "ANTHROPIC_API_KEY",
            "CLAUDE_X",
            "PI_X",
            "AWS_REGION",
            "GOOGLE_APPLICATION_CREDENTIALS",
        ] {
            assert!(is_reserved_env(key), "{key}");
        }
        for key in ["PATHS", "MY_PATH", "NODE_ENV", "PROXY", "AWS", "PIP_INDEX"] {
            assert!(!is_reserved_env(key), "{key}");
        }
    }

    #[test]
    fn name_predicates_follow_their_regexes() {
        assert!(is_name("stripe-mock") && is_name("a") && is_name(&format!("a{}", "b".repeat(31))));
        assert!(
            !is_name("")
                && !is_name("Web")
                && !is_name("1a")
                && !is_name("-a")
                && !is_name(&format!("a{}", "b".repeat(32)))
        );
        assert!(is_port_name("redis_2") && !is_port_name("stripe-mock"));
        assert!(is_env_key("_A1") && is_env_key(&"A".repeat(128)) && !is_env_key(&"A".repeat(129)));
        assert!(!is_env_key("1A") && !is_env_key("a") && !is_env_key("A-B") && !is_env_key(""));
        assert!(is_extension_name("pg_trgm") && !is_extension_name("pg-crypto") && !is_extension_name(""));
    }

    #[test]
    fn url_hosts_and_loopback() {
        assert_eq!(url_host("postgres://app:${db.password}@127.0.0.1:${port.db}/app"), Some("127.0.0.1"));
        assert_eq!(url_host("http://[::1]:8080/x"), Some("[::1]"));
        assert_eq!(url_host("https://api.stripe.com?x=1"), Some("api.stripe.com"));
        assert_eq!(url_host("http://${db.host}:5432"), Some("${db.host}"));
        assert_eq!(url_host("/login"), None);
        assert_eq!(url_host("${service.api.url}/x"), None);
        for host in ["127.0.0.1", "127.1.2.3", "localhost", "api.localhost", "[::1]", "0.0.0.0"] {
            assert!(is_loopback_host(host), "{host}");
        }
        for host in ["10.0.0.1", "example.com", "[2001:db8::1]", "localhost.example.com"] {
            assert!(!is_loopback_host(host), "{host}");
        }
        assert_eq!(external_host("https://api.stripe.com/v1"), Some("api.stripe.com"));
        assert_eq!(external_host("http://${db.host}/"), None);
        assert_eq!(external_host("file:///x"), None);
    }
}
