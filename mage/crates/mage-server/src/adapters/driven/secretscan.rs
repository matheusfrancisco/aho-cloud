//! `SecretScanner` adapter (fix.md "Security notes"): a `regex` rule set for provider key formats,
//! private keys, JWTs and Mage-issued tokens, plus an entropy-gated rule for values assigned to
//! secret-looking names, and exact matches of caller-supplied known secrets. Recipe fake keys are
//! allow-listed by exact value through `ScanInput::allowed`.
//!
//! Overlapping hits are resolved by position (earliest start wins), then by rule order, so a
//! `sk-ant-…` key is reported as `anthropic_api_key`, not `openai_api_key`.

use regex::Regex;

use crate::ports::secretscan::{KNOWN_SECRET_MIN_BYTES, ScanInput, SecretFinding, SecretScanner};

/// Shortest value the assignment rule considers.
const ASSIGNED_MIN_CHARS: usize = 16;
/// Shannon entropy (bits per char) an assigned value needs to count as a secret.
const ASSIGNED_MIN_ENTROPY: f64 = 3.5;

struct Rule {
    name: &'static str,
    re: Regex,
    /// Capture group holding the secret value (the reported span); 0 = whole match.
    group: usize,
    entropy_gate: bool,
}

pub struct RegexSecretScanner {
    rules: Vec<Rule>,
}

impl RegexSecretScanner {
    pub fn new() -> Result<Self, regex::Error> {
        let rule = |name: &'static str, pattern: &str| -> Result<Rule, regex::Error> {
            Ok(Rule { name, re: Regex::new(pattern)?, group: 0, entropy_gate: false })
        };
        let rules = vec![
            rule(
                "private_key_block",
                r"(?s)-----BEGIN (?:[A-Z0-9]+ )*PRIVATE KEY(?: BLOCK)?-----.*?(?:-----END (?:[A-Z0-9]+ )*PRIVATE KEY(?: BLOCK)?-----|\z)",
            )?,
            rule("gcp_service_account_key", r#""private_key"\s*:\s*"(?:[^"\\\r\n]|\\.){16,}"?"#)?,
            rule("mage_token", r"\bmg[bkrgcxi]_[A-Za-z0-9_.-]{20,}")?,
            rule("aws_access_key_id", r"\b(?:AKIA|ASIA|ABIA|ACCA)[A-Z0-9]{16}\b")?,
            rule("github_token", r"\b(?:ghp|gho|ghu|ghs|ghr)_[A-Za-z0-9]{36,251}\b")?,
            rule("github_pat", r"\bgithub_pat_[A-Za-z0-9_]{22,255}")?,
            rule("slack_token", r"\bxox[abpr]-[A-Za-z0-9-]{10,250}")?,
            rule("anthropic_api_key", r"\bsk-ant-[A-Za-z0-9_-]{20,}")?,
            rule("openai_api_key", r"\bsk-(?:proj-|svcacct-|admin-)?[A-Za-z0-9_-]{32,}")?,
            rule("google_api_key", r"\bAIza[0-9A-Za-z_-]{35}")?,
            rule("jwt", r"\beyJ[A-Za-z0-9_-]{8,}\.eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}")?,
            // `NAME = "value"` / `NAME: 'value'` / `"NAME": "value"` anywhere.
            Rule {
                name: "secret_assignment",
                re: Regex::new(
                    r#"(?i)\b[a-z0-9_.-]*(?:key|secret|token|password|passwd|pwd)["']?[ \t]*(?::=|=>|:|=)[ \t]*["']([^"'\r\n]{16,})["']"#,
                )?,
                group: 1,
                entropy_gate: true,
            },
            // `.env` style: `NAME=value` alone on its line.
            Rule {
                name: "secret_assignment",
                re: Regex::new(
                    r"(?im)^[ \t]*(?:export[ \t]+)?[a-z0-9_]*(?:key|secret|token|password|passwd|pwd)[ \t]*=[ \t]*([^\s\x22'#]{16,})[ \t]*\r?$",
                )?,
                group: 1,
                entropy_gate: true,
            },
        ];
        Ok(Self { rules })
    }

}

impl SecretScanner for RegexSecretScanner {
    fn scan(&self, input: &ScanInput<'_>) -> Vec<SecretFinding> {
        let text = input.text;
        // (start, priority, end, rule): known secrets first, then rules in declaration order.
        let mut hits: Vec<(usize, usize, usize, &'static str)> = Vec::new();
        for known in input.known {
            if known.value.len() < KNOWN_SECRET_MIN_BYTES {
                continue;
            }
            hits.extend(text.match_indices(known.value.as_str()).map(|(at, v)| (at, 0, at + v.len(), known.label)));
        }
        for (priority, rule) in self.rules.iter().enumerate() {
            for caps in rule.re.captures_iter(text) {
                let Some(m) = caps.get(rule.group) else { continue };
                let value = m.as_str();
                if input.allowed.iter().any(|a| a == value) {
                    continue;
                }
                if rule.entropy_gate && !looks_secret(value) {
                    continue;
                }
                hits.push((m.start(), priority + 1, m.end(), rule.name));
            }
        }
        hits.sort_unstable_by_key(|&(start, priority, end, _)| (start, priority, std::cmp::Reverse(end)));

        let mut out = Vec::new();
        let mut covered = 0usize;
        let mut line = 1u32;
        let mut line_start = 0usize;
        let mut scanned = 0usize;
        for (start, _, end, rule) in hits {
            if start < covered {
                continue;
            }
            covered = end;
            for (i, b) in text.as_bytes()[scanned..start].iter().enumerate() {
                if *b == b'\n' {
                    line += 1;
                    line_start = scanned + i + 1;
                }
            }
            scanned = start;
            out.push(SecretFinding {
                rule,
                path: input.path.cloned(),
                line,
                column: u32::try_from(start - line_start + 1).unwrap_or(u32::MAX),
                len: u32::try_from(end - start).unwrap_or(u32::MAX),
            });
        }
        out
    }
}

/// Entropy gate of the assignment rule: long enough, not a reference or placeholder, not a UUID,
/// at least two character classes, and Shannon entropy ≥ [`ASSIGNED_MIN_ENTROPY`].
fn looks_secret(value: &str) -> bool {
    if value.chars().count() < ASSIGNED_MIN_CHARS {
        return false;
    }
    let lower = value.to_ascii_lowercase();
    const PLACEHOLDERS: [&str; 13] = [
        "${", "{{", "%(", "<", "example", "placeholder", "changeme", "your_", "process.env", "os.environ", "getenv", "env[", "env(",
    ];
    if value.starts_with('$') || PLACEHOLDERS.iter().any(|p| lower.contains(p)) || is_identifier_path(value) {
        return false;
    }
    if is_uuid(value) {
        return false;
    }
    let classes = [
        value.bytes().any(|b| b.is_ascii_lowercase()),
        value.bytes().any(|b| b.is_ascii_uppercase()),
        value.bytes().any(|b| b.is_ascii_digit()),
        value.bytes().any(|b| !b.is_ascii_alphanumeric()),
    ];
    if classes.iter().filter(|c| **c).count() < 2 {
        return false;
    }
    shannon_entropy(value) >= ASSIGNED_MIN_ENTROPY
}

/// `config.secrets.api_key`-style references to another variable.
fn is_identifier_path(value: &str) -> bool {
    value.contains('.')
        && value.split('.').all(|part| {
            let mut bytes = part.bytes();
            bytes.next().is_some_and(|b| b.is_ascii_alphabetic() || b == b'_') && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
        })
}

fn is_uuid(value: &str) -> bool {
    let b = value.as_bytes();
    b.len() == 36
        && b.iter().enumerate().all(|(i, c)| if matches!(i, 8 | 13 | 18 | 23) { *c == b'-' } else { c.is_ascii_hexdigit() })
}

fn shannon_entropy(value: &str) -> f64 {
    let mut counts = [0u32; 256];
    let mut total = 0u32;
    for b in value.bytes() {
        counts[usize::from(b)] += 1;
        total += 1;
    }
    let total = f64::from(total.max(1));
    counts.iter().filter(|&&c| c > 0).map(|&c| f64::from(c) / total).map(|p| -p * p.log2()).sum()
}

#[cfg(test)]
mod tests {
    use mage_core::domain::RepoPath;

    use super::*;
    use crate::ports::secretscan::{KnownSecret, redact};
    use crate::testing::contract_secret_scanner;

    fn scanner() -> RegexSecretScanner {
        RegexSecretScanner::new().unwrap()
    }

    fn rules_of(text: &str) -> Vec<&'static str> {
        scanner().scan(&ScanInput::text(text)).into_iter().map(|f| f.rule).collect()
    }

    #[test]
    fn contract_suite_passes() {
        contract_secret_scanner(&scanner());
    }

    /// One positive fixture per rule (fix.md Testing: "positive fixture per rule").
    #[test]
    fn every_rule_has_a_positive_fixture() {
        let pem = "-----BEGIN RSA PRIVATE KEY-----\nMIIEowIBAAKCAQEAu1SU1LfVLPHCozMxH2Mo4lgOEePzNm0tRgeLezV6ffAt0gun\n-----END RSA PRIVATE KEY-----";
        let cases: [(&str, &str); 12] = [
            (pem, "private_key_block"),
            (r#"{"type": "service_account", "private_key": "abcdEFGH1234ijklMNOP5678\n"}"#, "gcp_service_account_key"),
            ("auth = mgk_eyJhbGciOiJFZERTQSJ9.eyJzdWIiOiJydW4ifQ.c2lnbmF0dXJl", "mage_token"),
            ("aws AKIAIOSFODNN7EXAMPLF here", "aws_access_key_id"),
            ("token ghs_16C7e42F292c6912E7710c838347Ae178B4a there", "github_token"),
            ("github_pat_11ABCDEFG0123456789_abcdefghijklmnopqrstuvwxyz0123456789", "github_pat"),
            ("slack xoxb-1234567890-0987654321-AbCdEfGhIjKlMnOp", "slack_token"),
            ("ANTHROPIC sk-ant-api03-AbCdEfGhIjKlMnOpQrStUvWxYz0123456789", "anthropic_api_key"),
            ("openai sk-proj-AbCdEfGhIjKlMnOpQrStUvWxYz0123456789abcd", "openai_api_key"),
            ("maps AIzaSyD-9tSrke72PouQMnMX-a7eZSW0jkFMBWY ok", "google_api_key"),
            ("bearer eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c", "jwt"),
            ("const STRIPE_SECRET = \"rk_live_51H8xKqL2vN9pQ7mZ3cT\";", "secret_assignment"),
        ];
        for (text, rule) in cases {
            assert_eq!(rules_of(text), vec![rule], "fixture for {rule}: {text}");
        }
        assert_eq!(rules_of("export DATABASE_PASSWORD=Zq8!rT2v%Lm9Wx4kP0s\n"), vec!["secret_assignment"]);
    }

    /// fix.md Testing: false-positive corpus (hashes, UUIDs, base64 test data, lockfile hashes).
    #[test]
    fn false_positive_corpus_is_clean() {
        let corpus = [
            "commit 3f9a2c1e8b7d6a5f4e3d2c1b0a9f8e7d6c5b4a39",
            "sha256: 9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08",
            r#"{"id": "123e4567-e89b-12d3-a456-426614174000", "request_id": "6fa459ea-ee8a-3ca4-894e-db77e160355e"}"#,
            r#"const IDEMPOTENCY_KEY = "123e4567-e89b-12d3-a456-426614174000";"#,
            r#"const fixture = "SGVsbG8gV29ybGQhIFRoaXMgaXMgdGVzdCBkYXRhIGZvciBiYXNlNjQ=";"#,
            r#""integrity": "sha512-BbIi7yb1fAKOXPkVnNU84KdoYr5n1N/wQq0ZEpMfnE6uGAqzd6qWVYK9PNHFOWGvN0dR6TUbHuUOGwdPQJiSjw==","#,
            "resolved \"https://registry.yarnpkg.com/lodash/-/lodash-4.17.21.tgz#679591c564c3bffaae8454cf0b3df370c3d6911c\"",
            "checksum = \"a3f5d8e9c1b2a4f6d8e0c2b4a6f8d0e2c4b6a8f0d2e4c6b8a0f2d4e6c8b0a2f4\"",
            "API_KEY = process.env.API_KEY",
            "SESSION_SECRET=config.secrets.session_value",
            r#"const API_KEY = "${STRIPE_API_KEY}";"#,
            r#"password: "your_password_here""#,
            r#"const token = "aaaaaaaaaaaaaaaaaaaaaaaa";"#,
            "const skeleton = 'sk-short';",
            "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9 (header only, base64 json)",
        ];
        for text in corpus {
            assert_eq!(rules_of(text), Vec::<&str>::new(), "false positive: {text}");
        }
    }

    #[test]
    fn overlapping_rules_report_the_most_specific_once() {
        assert_eq!(rules_of("key sk-ant-api03-AbCdEfGhIjKlMnOpQrStUvWxYz0123456789AbCdEfGh"), vec!["anthropic_api_key"]);
        // A Mage token wraps a JWT: reported once as a Mage token.
        assert_eq!(rules_of("mgg_eyJhbGciOiJFZERTQSJ9.eyJzdWIiOiJydW4ifQ.c2lnbmF0dXJlYWJj"), vec!["mage_token"]);
    }

    #[test]
    fn findings_are_located_and_redaction_never_returns_the_value() {
        let secret = "ghp_16C7e42F292c6912E7710c838347Ae178B4a";
        let text = format!("line one\n  let t = \"{secret}\";\n");
        let path = RepoPath::new("src/config.ts").unwrap();
        let findings = scanner().scan(&ScanInput { path: Some(&path), text: &text, known: &[], allowed: &[] });
        assert_eq!(findings.len(), 1);
        let f = &findings[0];
        assert_eq!((f.rule, f.line, f.column, f.len as usize), ("github_token", 2, 12, secret.len()));
        assert_eq!(f.path.as_ref(), Some(&path));
        let rendered = format!("{f} {f:?}");
        assert!(!rendered.contains(secret));
        let redacted = redact(&text, &findings);
        assert!(!redacted.contains(secret));
        assert!(redacted.contains("[redacted: github_token]"));
    }

    #[test]
    fn known_secrets_match_exactly_and_allowed_values_are_skipped() {
        let known = [KnownSecret { label: "gateway_token", value: "opaque-run-token-123".into() }];
        let input = ScanInput { path: None, text: "x opaque-run-token-123 y", known: &known, allowed: &[] };
        let findings = scanner().scan(&input);
        assert_eq!(findings.iter().map(|f| f.rule).collect::<Vec<_>>(), vec!["gateway_token"]);

        let fake = "sk_test_FAKEfakeFAKE1234567890".to_owned();
        let text = format!("STRIPE_SECRET_KEY = \"{fake}\"");
        let allowed = [fake];
        let input = ScanInput { path: None, text: &text, known: &[], allowed: &allowed };
        assert!(scanner().scan(&input).is_empty());
    }
}
