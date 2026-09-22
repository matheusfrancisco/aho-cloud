//! The taint scan: a secret value found in an uploaded session log taints the release until
//! the secret is rotated. Searches every line for the raw value and the encodings a transcript
//! is likely to carry it in: JSON string escaping, base64 (standard and url-safe, at every byte
//! alignment so a value inside a larger encoded blob is still found), and percent-encoding.

use std::collections::BTreeSet;

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD_NO_PAD, URL_SAFE_NO_PAD};
use cloud_contract::ledger::TaintFinding;

/// Needles shorter than this match too much to mean anything.
const MIN_NEEDLE: usize = 6;

/// One uploaded session log.
pub struct SessionLog<'a> {
    pub session_id: &'a str,
    pub bytes: &'a [u8],
}

/// Every form of `value` the scan searches for, deduplicated, each at least [`MIN_NEEDLE`]
/// bytes.
pub fn needles(value: &str) -> Vec<Vec<u8>> {
    let raw = value.as_bytes();
    let mut out: BTreeSet<Vec<u8>> = BTreeSet::new();
    out.insert(raw.to_vec());
    if let Ok(json) = serde_json::to_string(value) {
        out.insert(json.as_bytes()[1..json.len() - 1].to_vec());
    }
    for engine in [&STANDARD_NO_PAD, &URL_SAFE_NO_PAD] {
        for shift in 0..3usize {
            // Prefix `shift` filler bytes, encode, keep only the characters every bit of which
            // comes from the value: those appear whatever surrounds it in a larger blob.
            let mut padded = vec![0u8; shift];
            padded.extend_from_slice(raw);
            let enc = engine.encode(&padded);
            let start = (8 * shift).div_ceil(6);
            let end = (8 * (shift + raw.len())) / 6;
            if end > start {
                out.insert(enc.as_bytes()[start..end].to_vec());
            }
        }
    }
    for upper in [true, false] {
        out.insert(percent_encode(raw, upper, false));
        out.insert(percent_encode(raw, upper, true));
    }
    out.into_iter().filter(|n| n.len() >= MIN_NEEDLE).collect()
}

/// RFC 3986 percent-encoding of everything but unreserved characters; `form` writes spaces as
/// `+` (application/x-www-form-urlencoded).
fn percent_encode(raw: &[u8], upper: bool, form: bool) -> Vec<u8> {
    let hex: &[u8; 16] = if upper { b"0123456789ABCDEF" } else { b"0123456789abcdef" };
    let mut out = Vec::with_capacity(raw.len() * 3);
    for &b in raw {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => out.push(b),
            b' ' if form => out.push(b'+'),
            _ => out.extend_from_slice(&[b'%', hex[usize::from(b >> 4)], hex[usize::from(b & 15)]]),
        }
    }
    out
}

/// The entry hash of a JSONL log line, when it is an entry.
fn entry_hash(line: &[u8]) -> Option<String> {
    let v: serde_json::Value = serde_json::from_slice(line).ok()?;
    v.get("hash")?.as_str().map(str::to_owned)
}

/// Scans `logs` for `secrets` (`(name, value)`). Findings are ordered and deduplicated; the
/// location is `aho-log:<session>#<entry hash>` (or `#line<N>` for a non-entry line).
pub fn scan(secrets: &[(String, String)], logs: &[SessionLog<'_>]) -> Vec<TaintFinding> {
    let prepared: Vec<(&str, Vec<memchr::memmem::Finder<'static>>)> = secrets
        .iter()
        .filter(|(_, v)| !v.is_empty())
        .map(|(name, value)| (name.as_str(), needles(value).into_iter().map(|n| memchr::memmem::Finder::new(&n).into_owned()).collect()))
        .collect();
    let mut found: BTreeSet<(String, String)> = BTreeSet::new();
    for log in logs {
        for (n, line) in log.bytes.split(|&b| b == b'\n').enumerate() {
            for (name, finders) in &prepared {
                if finders.iter().any(|f| f.find(line).is_some()) {
                    let at = entry_hash(line).unwrap_or_else(|| format!("line{}", n + 1));
                    found.insert(((*name).to_owned(), format!("aho-log:{}#{at}", log.session_id)));
                }
            }
        }
    }
    found.into_iter().map(|(secret, location)| TaintFinding { secret, location }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::{STANDARD, URL_SAFE};

    const SECRET: &str = "sk_live_51H?x/y+z";

    fn finds(haystack: &str) -> bool {
        let logs = [SessionLog { session_id: "s1", bytes: haystack.as_bytes() }];
        !scan(&[("STRIPE_KEY".into(), SECRET.into())], &logs).is_empty()
    }

    #[test]
    fn finds_raw_and_json_escaped_values() {
        assert!(finds(&format!(r#"{{"hash":"aa","kind":{{"text":"key is {SECRET}"}}}}"#)));
        let quoted = "pa\"ss\\word!";
        let json_line = serde_json::json!({"hash": "bb", "out": quoted}).to_string();
        let logs = [SessionLog { session_id: "s", bytes: json_line.as_bytes() }];
        assert_eq!(scan(&[("P".into(), quoted.into())], &logs).len(), 1);
    }

    #[test]
    fn finds_base64_at_every_alignment_and_both_alphabets() {
        for prefix in ["", "a", "ab", "abc"] {
            let blob = format!("{prefix}{SECRET}trailing bytes");
            for enc in [STANDARD.encode(&blob), URL_SAFE.encode(&blob), STANDARD_NO_PAD.encode(&blob)] {
                assert!(finds(&format!(r#"{{"hash":"cc","b64":"{enc}"}}"#)), "prefix {prefix:?}: {enc}");
            }
        }
    }

    #[test]
    fn finds_percent_encoded_values() {
        assert!(finds("GET /cb?key=sk_live_51H%3Fx%2Fy%2Bz HTTP/1.1"));
        assert!(finds("key=sk_live_51H%3fx%2fy%2bz"));
    }

    #[test]
    fn reports_entry_hash_locations_once_per_secret_and_entry() {
        let text = format!("{{\"hash\":\"h1\",\"t\":\"{SECRET} {SECRET}\"}}\n{{\"head\":1}}\nnot json {SECRET}\n");
        let logs = [SessionLog { session_id: "sess", bytes: text.as_bytes() }];
        let f = scan(&[("K".into(), SECRET.into())], &logs);
        let locs: Vec<&str> = f.iter().map(|f| f.location.as_str()).collect();
        assert_eq!(locs, ["aho-log:sess#h1", "aho-log:sess#line3"]);
    }

    #[test]
    fn unrelated_text_and_other_secrets_do_not_match() {
        assert!(!finds(r#"{"hash":"x","text":"sk_live_51H is only a prefix"}"#));
        assert!(!finds(&STANDARD.encode("unrelated")));
        let logs = [SessionLog { session_id: "s", bytes: b"anything at all" }];
        assert!(scan(&[("EMPTY".into(), String::new())], &logs).is_empty());
    }
}
