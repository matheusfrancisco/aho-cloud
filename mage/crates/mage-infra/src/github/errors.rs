//! HTTP status → `SourceHostError` (intake.md "Error mapping").

use std::time::Duration;

use mage_core::ports::source_host::SourceHostError;
use parking_lot::Mutex;
use serde::Deserialize;
use time::OffsetDateTime;

/// First wait after a secondary rate limit; doubled on each consecutive one.
const SECONDARY_BASE: Duration = Duration::from_secs(60);
/// Ceiling of the doubled secondary wait.
const SECONDARY_MAX: Duration = Duration::from_secs(60 * 60);
/// Longest error message kept from a non-JSON body.
const MAX_MESSAGE: usize = 300;

/// What a non-success response says about the failure.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ErrorSignals<'a> {
    pub status: u16,
    pub ratelimit_remaining: Option<&'a str>,
    /// Unix seconds.
    pub ratelimit_reset: Option<&'a str>,
    /// Seconds.
    pub retry_after: Option<&'a str>,
    pub message: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Failure {
    Error(SourceHostError),
    /// A secondary rate limit without a reset time: the caller picks the backoff.
    SecondaryLimit,
}

pub(crate) fn classify(signals: &ErrorSignals<'_>, now: OffsetDateTime) -> Failure {
    let status = signals.status;
    let message = signals.message;
    match status {
        401 => Failure::Error(SourceHostError::Unauthorized),
        403 | 429 => {
            let exhausted = signals.ratelimit_remaining.is_some_and(|r| r.trim() == "0");
            let reset = signals.ratelimit_reset.and_then(|r| r.trim().parse::<i64>().ok()).and_then(|r| OffsetDateTime::from_unix_timestamp(r).ok());
            if let (true, Some(retry_at)) = (exhausted, reset) {
                return Failure::Error(SourceHostError::RateLimited { retry_at });
            }
            if let Some(secs) = signals.retry_after.and_then(|r| r.trim().parse::<u64>().ok()) {
                return Failure::Error(SourceHostError::RateLimited { retry_at: now + Duration::from_secs(secs) });
            }
            let lower = message.to_ascii_lowercase();
            if status == 429 || exhausted || lower.contains("rate limit") || lower.contains("abuse") {
                Failure::SecondaryLimit
            } else {
                Failure::Error(SourceHostError::Unauthorized)
            }
        }
        404 | 410 => Failure::Error(SourceHostError::NotFound),
        409 => Failure::Error(SourceHostError::Conflict),
        422 => Failure::Error(SourceHostError::Invalid(message.to_owned())),
        400..=499 => Failure::Error(SourceHostError::Invalid(format!("GitHub {status}: {message}"))),
        _ => Failure::Error(SourceHostError::Unavailable(format!("GitHub {status}: {message}"))),
    }
}

/// Consecutive secondary limits double the wait; any success resets it.
#[derive(Debug, Default)]
pub(crate) struct SecondaryBackoff {
    last: Mutex<Option<Duration>>,
}

impl SecondaryBackoff {
    pub fn next(&self) -> Duration {
        let mut last = self.last.lock();
        let wait = last.map_or(SECONDARY_BASE, |prev| (prev * 2).min(SECONDARY_MAX));
        *last = Some(wait);
        wait
    }

    pub fn reset(&self) {
        *self.last.lock() = None;
    }
}

#[derive(Deserialize)]
struct ErrorBody {
    #[serde(default)]
    message: String,
    #[serde(default)]
    errors: Vec<serde_json::Value>,
}

/// The human message of an error body: GitHub's `message` plus each `errors[]` entry's `message`
/// or `code`; a short prefix of a non-JSON body otherwise.
pub(crate) fn error_message(body: &[u8]) -> String {
    match serde_json::from_slice::<ErrorBody>(body) {
        Ok(parsed) => {
            let details: Vec<&str> = parsed
                .errors
                .iter()
                .filter_map(|e| e.get("message").or_else(|| e.get("code")).and_then(serde_json::Value::as_str).or_else(|| e.as_str()))
                .collect();
            if details.is_empty() { parsed.message } else { format!("{}: {}", parsed.message, details.join("; ")) }
        }
        Err(_) => {
            let text = String::from_utf8_lossy(body);
            let text = text.trim();
            match text.char_indices().nth(MAX_MESSAGE) {
                Some((cut, _)) => text[..cut].to_owned(),
                None => text.to_owned(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: OffsetDateTime = time::macros::datetime!(2026-01-01 00:00:00 UTC);

    fn signals(status: u16, message: &str) -> ErrorSignals<'_> {
        ErrorSignals { status, ratelimit_remaining: None, ratelimit_reset: None, retry_after: None, message }
    }

    #[test]
    fn primary_limit_waits_until_the_reset() {
        let s = ErrorSignals { ratelimit_remaining: Some("0"), ratelimit_reset: Some("1767225900"), ..signals(403, "API rate limit exceeded") };
        let retry_at = OffsetDateTime::from_unix_timestamp(1_767_225_900).unwrap_or(NOW);
        assert_eq!(classify(&s, NOW), Failure::Error(SourceHostError::RateLimited { retry_at }));
        let s = ErrorSignals { status: 429, ..s };
        assert_eq!(classify(&s, NOW), Failure::Error(SourceHostError::RateLimited { retry_at }));
    }

    #[test]
    fn retry_after_counts_from_now() {
        let s = ErrorSignals { retry_after: Some("30"), ..signals(429, "") };
        assert_eq!(classify(&s, NOW), Failure::Error(SourceHostError::RateLimited { retry_at: NOW + Duration::from_secs(30) }));
        let s = ErrorSignals { retry_after: Some("5"), ..signals(403, "You have exceeded a secondary rate limit") };
        assert_eq!(classify(&s, NOW), Failure::Error(SourceHostError::RateLimited { retry_at: NOW + Duration::from_secs(5) }));
    }

    #[test]
    fn secondary_limits_and_plain_forbidden() {
        assert_eq!(classify(&signals(403, "You have exceeded a secondary rate limit."), NOW), Failure::SecondaryLimit);
        assert_eq!(classify(&signals(403, "abuse detection mechanism"), NOW), Failure::SecondaryLimit);
        assert_eq!(classify(&signals(429, ""), NOW), Failure::SecondaryLimit);
        assert_eq!(classify(&signals(403, "Resource not accessible by integration"), NOW), Failure::Error(SourceHostError::Unauthorized));
        let no_reset = ErrorSignals { ratelimit_remaining: Some("0"), ..signals(403, "") };
        assert_eq!(classify(&no_reset, NOW), Failure::SecondaryLimit, "exhausted without a reset time still backs off");
    }

    #[test]
    fn status_codes() {
        assert_eq!(classify(&signals(401, "Bad credentials"), NOW), Failure::Error(SourceHostError::Unauthorized));
        assert_eq!(classify(&signals(404, ""), NOW), Failure::Error(SourceHostError::NotFound));
        assert_eq!(classify(&signals(409, ""), NOW), Failure::Error(SourceHostError::Conflict));
        assert_eq!(classify(&signals(422, "Validation Failed"), NOW), Failure::Error(SourceHostError::Invalid("Validation Failed".into())));
        assert!(matches!(classify(&signals(400, "bad"), NOW), Failure::Error(SourceHostError::Invalid(_))));
        assert!(matches!(classify(&signals(502, ""), NOW), Failure::Error(SourceHostError::Unavailable(_))));
    }

    #[test]
    fn backoff_doubles_until_reset() {
        let backoff = SecondaryBackoff::default();
        assert_eq!(backoff.next(), Duration::from_secs(60));
        assert_eq!(backoff.next(), Duration::from_secs(120));
        assert_eq!(backoff.next(), Duration::from_secs(240));
        backoff.reset();
        assert_eq!(backoff.next(), Duration::from_secs(60));
        for _ in 0..20 {
            backoff.next();
        }
        assert_eq!(backoff.next(), SECONDARY_MAX);
    }

    #[test]
    fn error_messages() {
        assert_eq!(error_message(br#"{"message":"Not Found","documentation_url":"x"}"#), "Not Found");
        assert_eq!(
            error_message(br#"{"message":"Validation Failed","errors":[{"resource":"Issue","code":"missing_field"},{"message":"title is long"}]}"#),
            "Validation Failed: missing_field; title is long"
        );
        assert_eq!(error_message(b"  upstream timeout \n"), "upstream timeout");
        assert_eq!(error_message(&[b'x'; 1000]).len(), MAX_MESSAGE);
    }
}
