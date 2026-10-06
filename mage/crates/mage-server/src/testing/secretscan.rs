//! `SecretScanner` fake and contract suite (fix.md Testing: "`SecretScanner` contract").
//!
//! The fake flags caller-configured literals (each with a rule name) plus the input's known
//! secrets, with the same location, ordering and overlap semantics as the real adapter, so use
//! case tests can plant a "secret" deterministically.

use crate::ports::secretscan::{KNOWN_SECRET_MIN_BYTES, KnownSecret, ScanInput, SecretFinding, SecretScanner, redact};

#[derive(Debug, Clone, Default)]
pub struct FakeSecretScanner {
    literals: Vec<(&'static str, String)>,
}

impl FakeSecretScanner {
    pub fn new() -> Self {
        Self::default()
    }

    /// Also flag every occurrence of `literal` as `rule`.
    pub fn flagging(mut self, rule: &'static str, literal: impl Into<String>) -> Self {
        self.literals.push((rule, literal.into()));
        self
    }
}

impl SecretScanner for FakeSecretScanner {
    fn scan(&self, input: &ScanInput<'_>) -> Vec<SecretFinding> {
        let text = input.text;
        let mut hits: Vec<(usize, usize, usize, &'static str)> = Vec::new();
        for KnownSecret { label, value } in input.known {
            if value.len() >= KNOWN_SECRET_MIN_BYTES {
                hits.extend(text.match_indices(value.as_str()).map(|(at, v)| (at, 0, at + v.len(), *label)));
            }
        }
        for (rule, literal) in &self.literals {
            if literal.is_empty() || input.allowed.iter().any(|a| a == literal) {
                continue;
            }
            hits.extend(text.match_indices(literal.as_str()).map(|(at, v)| (at, 1, at + v.len(), *rule)));
        }
        hits.sort_unstable_by_key(|&(start, priority, end, _)| (start, priority, std::cmp::Reverse(end)));
        let mut out = Vec::new();
        let mut covered = 0;
        for (start, _, end, rule) in hits {
            if start < covered {
                continue;
            }
            covered = end;
            let line_start = text[..start].rfind('\n').map_or(0, |i| i + 1);
            out.push(SecretFinding {
                rule,
                path: input.path.cloned(),
                line: u32::try_from(text[..start].matches('\n').count() + 1).unwrap_or(u32::MAX),
                column: u32::try_from(start - line_start + 1).unwrap_or(u32::MAX),
                len: u32::try_from(end - start).unwrap_or(u32::MAX),
            });
        }
        out
    }
}

/// Generic suite every `SecretScanner` passes. Uses only known secrets, so it holds for any rule
/// set; rule-specific fixtures live with the adapter.
pub fn contract_secret_scanner(scanner: &dyn SecretScanner) {
    let path = mage_core::domain::RepoPath::new("src/app/config.ts").unwrap_or_else(|e| panic!("{e}"));
    let token = KnownSecret { label: "gateway_token", value: "run-bound-opaque-0001".into() };
    let bearer = KnownSecret { label: "run_mcp_bearer", value: "mcp-bearer-opaque-0002".into() };

    // Empty and benign texts have no findings.
    assert!(scanner.scan(&ScanInput::text("")).is_empty(), "empty text");
    assert!(scanner.scan(&ScanInput::text("export function add(a, b) {\n  return a + b;\n}\n")).is_empty(), "benign code");

    // Known secrets: exact, located (1-based line, byte column), path propagated, sorted.
    let text = format!("line one\nline two\n    use({}) and {}\nagain {}\n", token.value, bearer.value, token.value);
    let known = [token.clone(), bearer.clone()];
    let findings = scanner.scan(&ScanInput { path: Some(&path), text: &text, known: &known, allowed: &[] });
    let located: Vec<(&str, u32, u32, u32)> = findings.iter().map(|f| (f.rule, f.line, f.column, f.len)).collect();
    let len = |s: &KnownSecret| u32::try_from(s.value.len()).unwrap_or(u32::MAX);
    assert_eq!(
        located,
        vec![("gateway_token", 3, 9, len(&token)), ("run_mcp_bearer", 3, 36, len(&bearer)), ("gateway_token", 4, 7, len(&token))],
        "known secret locations"
    );
    assert!(findings.iter().all(|f| f.path.as_ref() == Some(&path)), "path on every finding");

    // Redaction removes every value and keeps the rest; findings never print the value.
    let redacted = redact(&text, &findings);
    assert!(!redacted.contains(&token.value) && !redacted.contains(&bearer.value), "redaction leaks a value");
    assert!(redacted.starts_with("line one\nline two\n    use([redacted: gateway_token])"), "redaction keeps context: {redacted}");
    for f in &findings {
        let shown = format!("{f} {f:?}");
        assert!(!shown.contains(&token.value) && !shown.contains(&bearer.value), "finding prints its value");
    }

    // Overlapping known secrets yield one finding; too-short known values are ignored.
    let outer = KnownSecret { label: "outer", value: "AAAA-overlap-BBBB-CCCC".into() };
    let inner = KnownSecret { label: "inner", value: "overlap-BBBB".into() };
    let short = KnownSecret { label: "short", value: "x".repeat(KNOWN_SECRET_MIN_BYTES - 1) };
    let text = format!("{} {}", outer.value, short.value);
    let known = [inner, outer, short];
    let findings = scanner.scan(&ScanInput { path: None, text: &text, known: &known, allowed: &[] });
    assert_eq!(findings.iter().map(|f| (f.rule, f.column)).collect::<Vec<_>>(), vec![("outer", 1)], "overlap / short known");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fake_passes_the_contract() {
        contract_secret_scanner(&FakeSecretScanner::new());
    }

    #[test]
    fn fake_flags_literals_unless_allowed() {
        let scanner = FakeSecretScanner::new().flagging("planted", "TOPSECRET-123");
        assert_eq!(scanner.scan(&ScanInput::text("a TOPSECRET-123 b")).len(), 1);
        let allowed = ["TOPSECRET-123".to_owned()];
        assert!(scanner.scan(&ScanInput { path: None, text: "a TOPSECRET-123", known: &[], allowed: &allowed }).is_empty());
    }
}
