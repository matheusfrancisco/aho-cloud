//! `SecretScanner` port (fix.md Additions, "Security notes": no secrets in patches, regression
//! scripts, briefs or PR bodies). Synchronous and pure over its input; the rule set is swappable
//! (adapter `crate::adapters::driven::secretscan`, fake `crate::testing::FakeSecretScanner`).
//!
//! Findings carry the rule and the location only, never the matched value, so they can be logged,
//! stored in gate feedback and shown to the agent. [`redact`] replaces the located spans with
//! `[redacted: <rule>]`.

use mage_core::domain::RepoPath;

/// Known secrets shorter than this are not matched (they would hit by accident).
pub const KNOWN_SECRET_MIN_BYTES: usize = 8;

/// A value the caller knows is secret (e.g. a run's own Mage-issued tokens); matched exactly
/// when at least [`KNOWN_SECRET_MIN_BYTES`] long, reported with `label` as the rule.
#[derive(Clone, PartialEq, Eq)]
pub struct KnownSecret {
    /// Rule name reported for a hit, e.g. `gateway_token`.
    pub label: &'static str,
    pub value: String,
}

impl std::fmt::Debug for KnownSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KnownSecret").field("label", &self.label).field("value", &"***").finish()
    }
}

/// One text to scan.
#[derive(Debug, Clone, Copy)]
pub struct ScanInput<'a> {
    /// Reported on findings (patch file, regression file); `None` for briefs and PR bodies.
    pub path: Option<&'a RepoPath>,
    pub text: &'a str,
    /// Exact values that are always secret.
    pub known: &'a [KnownSecret],
    /// Exact values that are never secret (recipe fake keys, allow-listed by exact value).
    pub allowed: &'a [String],
}

impl<'a> ScanInput<'a> {
    pub fn text(text: &'a str) -> Self {
        Self { path: None, text, known: &[], allowed: &[] }
    }
}

/// A located hit. `line` and `column` (byte column) are 1-based; `len` is the byte length of the
/// matched span, which may continue past the line (e.g. a PEM block).
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize)]
pub struct SecretFinding {
    pub rule: &'static str,
    pub path: Option<RepoPath>,
    pub line: u32,
    pub column: u32,
    pub len: u32,
}

impl std::fmt::Display for SecretFinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.path {
            Some(path) => write!(f, "{} at {path}:{}:{}", self.rule, self.line, self.column),
            None => write!(f, "{} at line {}:{}", self.rule, self.line, self.column),
        }
    }
}

pub trait SecretScanner: Send + Sync {
    /// Every finding in `input.text`, ordered by position, non-overlapping.
    fn scan(&self, input: &ScanInput<'_>) -> Vec<SecretFinding>;
}

/// `text` with every finding's span replaced by `[redacted: <rule>]`. Findings that do not
/// address a valid span of `text` are ignored; the result never contains a located value.
pub fn redact(text: &str, findings: &[SecretFinding]) -> String {
    // Byte offset of each line start.
    let mut starts = vec![0usize];
    starts.extend(text.match_indices('\n').map(|(i, _)| i + 1));
    let mut spans: Vec<(usize, usize, &'static str)> = findings
        .iter()
        .filter_map(|f| {
            let line_start = *starts.get(usize::try_from(f.line).ok()?.checked_sub(1)?)?;
            let start = line_start + usize::try_from(f.column).ok()?.checked_sub(1)?;
            let end = start + usize::try_from(f.len).ok()?;
            (end <= text.len() && text.is_char_boundary(start) && text.is_char_boundary(end)).then_some((start, end, f.rule))
        })
        .collect();
    spans.sort_unstable();
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for (start, end, rule) in spans {
        if start < at {
            // Overlaps an earlier span: extend the redaction to cover it.
            at = at.max(end);
            continue;
        }
        out.push_str(&text[at..start]);
        out.push_str("[redacted: ");
        out.push_str(rule);
        out.push(']');
        at = end;
    }
    out.push_str(&text[at..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finding(line: u32, column: u32, len: u32) -> SecretFinding {
        SecretFinding { rule: "test_rule", path: None, line, column, len }
    }

    #[test]
    fn redact_replaces_located_spans_on_any_line() {
        let text = "first line\nkey = SECRETVALUE end\nlast";
        let out = redact(text, &[finding(2, 7, 11)]);
        assert_eq!(out, "first line\nkey = [redacted: test_rule] end\nlast");
    }

    #[test]
    fn redact_merges_overlaps_and_ignores_bad_spans() {
        let text = "abcdefghij";
        let out = redact(text, &[finding(1, 2, 4), finding(1, 4, 4), finding(3, 1, 1), finding(1, 9, 50)]);
        assert_eq!(out, "a[redacted: test_rule]hij");
    }
}
