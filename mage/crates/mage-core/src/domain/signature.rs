//! `FailureSignature` and `Verdict` (domain.md).

use serde::{Deserialize, Serialize};

use crate::domain::error::DomainError;

/// Maximum signature value length in bytes (runs.md: "at most 1 KiB").
pub const SIGNATURE_VALUE_MAX_BYTES: usize = 1024;
/// Compiled-size limit for `Matcher::Regex` patterns (agent-supplied).
const REGEX_SIZE_LIMIT: usize = 1 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "snake_case")]
pub enum SignatureKind {
    AssertionText,
    HttpStatus,
    ErrorString,
    ExitCode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "snake_case")]
pub enum Matcher {
    Exact,
    Contains,
    Regex,
}

/// How a repro failure is identified so a clean re-run can be compared.
///
/// Observations extracted by the runner use `matcher = Exact` and carry the observed text in
/// `value`; the expected signature's matcher decides the comparison.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, ts_rs::TS)]
pub struct FailureSignature {
    pub kind: SignatureKind,
    pub value: String,
    pub matcher: Matcher,
}

impl FailureSignature {
    /// Submission-time checks (runs.md SubmitResult): value non-empty after trimming, at most
    /// 1 KiB, and a `Regex` matcher must compile.
    pub fn validate(&self) -> Result<(), DomainError> {
        if self.value.trim().is_empty() {
            return Err(DomainError::invalid("signature value is empty"));
        }
        if self.value.len() > SIGNATURE_VALUE_MAX_BYTES {
            return Err(DomainError::invalid("signature value longer than 1 KiB"));
        }
        if self.matcher == Matcher::Regex {
            self.regex().map_err(|e| DomainError::invalid(format!("signature regex: {e}")))?;
        }
        Ok(())
    }

    fn regex(&self) -> Result<regex::Regex, regex::Error> {
        regex::RegexBuilder::new(&self.value).size_limit(REGEX_SIZE_LIMIT).build()
    }

    /// True when `observed` (an observation of the same kind) satisfies this signature.
    /// Different kinds never match; an uncompilable regex matches nothing.
    pub fn matches(&self, observed: &FailureSignature) -> bool {
        if self.kind != observed.kind {
            return false;
        }
        match self.matcher {
            Matcher::Exact => observed.value == self.value,
            Matcher::Contains => observed.value.contains(self.value.as_str()),
            Matcher::Regex => self.regex().is_ok_and(|re| re.is_match(&observed.value)),
        }
    }

    /// True when any of `observed` matches.
    pub fn matches_any(&self, observed: &[FailureSignature]) -> bool {
        match self.matcher {
            // Compile once for the whole slice.
            Matcher::Regex => match self.regex() {
                Ok(re) => observed.iter().any(|o| o.kind == self.kind && re.is_match(&o.value)),
                Err(_) => false,
            },
            _ => observed.iter().any(|o| self.matches(o)),
        }
    }
}

/// A harness's claim about a diagnosis. Only the repro gate moves a cause to `Reproduced`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Verdict {
    Reproduced,
    NotReproduced,
    Inconclusive { reason: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sig(kind: SignatureKind, matcher: Matcher, value: &str) -> FailureSignature {
        FailureSignature { kind, value: value.to_string(), matcher }
    }

    fn obs(kind: SignatureKind, value: &str) -> FailureSignature {
        sig(kind, Matcher::Exact, value)
    }

    #[test]
    fn exact_requires_equal_value_and_kind() {
        let s = sig(SignatureKind::HttpStatus, Matcher::Exact, "500");
        assert!(s.matches(&obs(SignatureKind::HttpStatus, "500")));
        assert!(!s.matches(&obs(SignatureKind::HttpStatus, "5000")));
        assert!(!s.matches(&obs(SignatureKind::ExitCode, "500")));
        // Case-sensitive, no trimming.
        let a = sig(SignatureKind::ErrorString, Matcher::Exact, "Boom");
        assert!(!a.matches(&obs(SignatureKind::ErrorString, "boom")));
        assert!(!a.matches(&obs(SignatureKind::ErrorString, "Boom ")));
    }

    #[test]
    fn contains_is_substring() {
        let s = sig(SignatureKind::ErrorString, Matcher::Contains, "customer is null");
        assert!(s.matches(&obs(SignatureKind::ErrorString, "TypeError: customer is null at export")));
        assert!(!s.matches(&obs(SignatureKind::ErrorString, "customer was null")));
        assert!(!s.matches(&obs(SignatureKind::AssertionText, "customer is null")));
    }

    #[test]
    fn regex_matches_unanchored_and_bad_regex_matches_nothing() {
        let s = sig(SignatureKind::AssertionText, Matcher::Regex, r"expected \d+ to equal \d+");
        assert!(s.matches(&obs(SignatureKind::AssertionText, "Error: expected 41 to equal 42 (line 3)")));
        assert!(!s.matches(&obs(SignatureKind::AssertionText, "expected x to equal 42")));
        let anchored = sig(SignatureKind::ExitCode, Matcher::Regex, "^1$");
        assert!(anchored.matches(&obs(SignatureKind::ExitCode, "1")));
        assert!(!anchored.matches(&obs(SignatureKind::ExitCode, "11")));
        let bad = sig(SignatureKind::ErrorString, Matcher::Regex, "(unclosed");
        assert!(!bad.matches(&obs(SignatureKind::ErrorString, "(unclosed")));
        assert!(!bad.matches_any(&[obs(SignatureKind::ErrorString, "(unclosed")]));
    }

    #[test]
    fn matches_any_over_candidates() {
        let s = sig(SignatureKind::HttpStatus, Matcher::Exact, "500");
        assert!(!s.matches_any(&[]));
        assert!(s.matches_any(&[obs(SignatureKind::HttpStatus, "200"), obs(SignatureKind::HttpStatus, "500")]));
        let r = sig(SignatureKind::HttpStatus, Matcher::Regex, "^5\\d\\d$");
        assert!(r.matches_any(&[obs(SignatureKind::HttpStatus, "200"), obs(SignatureKind::HttpStatus, "503")]));
        assert!(!r.matches_any(&[obs(SignatureKind::ErrorString, "503")]));
    }

    #[test]
    fn validate_rules() {
        assert!(sig(SignatureKind::ErrorString, Matcher::Exact, "x").validate().is_ok());
        assert!(sig(SignatureKind::ErrorString, Matcher::Exact, "  ").validate().is_err());
        assert!(sig(SignatureKind::ErrorString, Matcher::Exact, &"a".repeat(1024)).validate().is_ok());
        assert!(sig(SignatureKind::ErrorString, Matcher::Exact, &"a".repeat(1025)).validate().is_err());
        assert!(sig(SignatureKind::ErrorString, Matcher::Regex, "a(").validate().is_err());
        assert!(sig(SignatureKind::ErrorString, Matcher::Regex, "a+").validate().is_ok());
    }

    #[test]
    fn verdict_serde_shape() {
        let v = Verdict::Inconclusive { reason: "app never booted".into() };
        let json = serde_json::to_value(&v).unwrap();
        assert_eq!(json, serde_json::json!({"type": "inconclusive", "reason": "app never booted"}));
        assert_eq!(serde_json::to_value(Verdict::Reproduced).unwrap(), serde_json::json!({"type": "reproduced"}));
    }
}
