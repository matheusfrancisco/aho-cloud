//! Redaction of spec secrets from every string leaving the adapter (H-ISO-8), and the text
//! helpers for previews and event chunks.

use crate::ports::harness::HarnessSpec;

pub const REDACTED: &str = "[redacted]";
/// Max bytes of a tool preview and of one `AssistantText` event.
pub const PREVIEW_MAX: usize = 4096;

/// Replaces the exact bytes of every secret with `[redacted]`. Secrets are ASCII
/// (`[A-Za-z0-9_.-]`), so byte replacement never splits a UTF-8 character.
#[derive(Debug, Clone, Default)]
pub struct Redactor {
    /// Longest first, so a secret containing another one is replaced whole.
    secrets: Vec<String>,
}

impl Redactor {
    pub fn new<I, S>(secrets: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut secrets: Vec<String> = secrets.into_iter().map(Into::into).filter(|s| !s.is_empty()).collect();
        secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
        secrets.dedup();
        Self { secrets }
    }

    pub fn for_spec(spec: &HarnessSpec) -> Self {
        Self::new(spec.secrets().into_iter().map(|s| s.expose().to_owned()))
    }

    pub fn redact(&self, text: &str) -> String {
        let mut out = text.to_owned();
        for secret in &self.secrets {
            if out.contains(secret.as_str()) {
                out = out.replace(secret.as_str(), REDACTED);
            }
        }
        out
    }

    pub fn redact_bytes(&self, bytes: &[u8]) -> Vec<u8> {
        let mut out = bytes.to_vec();
        for secret in &self.secrets {
            out = replace_bytes(&out, secret.as_bytes(), REDACTED.as_bytes());
        }
        out
    }

    /// Length of the longest suffix of `text` that is a proper prefix of some secret: those bytes
    /// must be held back until the next chunk shows whether a secret continues.
    pub fn holdback(&self, text: &[u8]) -> usize {
        let mut best = 0;
        for secret in &self.secrets {
            let s = secret.as_bytes();
            let max = (s.len() - 1).min(text.len());
            for len in (best + 1..=max).rev() {
                if text[text.len() - len..] == s[..len] {
                    best = len;
                    break;
                }
            }
        }
        best
    }

    pub fn is_empty(&self) -> bool {
        self.secrets.is_empty()
    }
}

fn replace_bytes(haystack: &[u8], needle: &[u8], with: &[u8]) -> Vec<u8> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return haystack.to_vec();
    }
    let mut out = Vec::with_capacity(haystack.len());
    let mut i = 0;
    while i < haystack.len() {
        if haystack[i..].starts_with(needle) {
            out.extend_from_slice(with);
            i += needle.len();
        } else {
            out.push(haystack[i]);
            i += 1;
        }
    }
    out
}

/// Redacts a byte stream that arrives in arbitrary chunks: a secret split across chunks is still
/// replaced, because a possible secret prefix at the end of a chunk is carried over.
#[derive(Debug)]
pub struct StreamRedactor {
    redactor: Redactor,
    carry: Vec<u8>,
}

impl StreamRedactor {
    pub fn new(redactor: Redactor) -> Self {
        Self { redactor, carry: Vec::new() }
    }

    /// Returns the bytes that are safe to emit now.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<u8> {
        self.carry.extend_from_slice(chunk);
        let redacted = self.redactor.redact_bytes(&self.carry);
        let hold = self.redactor.holdback(&redacted);
        let emit = redacted[..redacted.len() - hold].to_vec();
        self.carry = redacted[redacted.len() - hold..].to_vec();
        emit
    }

    /// End of stream: everything left.
    pub fn finish(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.carry)
    }
}

/// UTF-8-safe truncation to [`PREVIEW_MAX`] bytes including a `…[+N bytes]` suffix.
pub fn preview(text: &str) -> String {
    if text.len() <= PREVIEW_MAX {
        return text.to_owned();
    }
    // "…[+" (5 bytes) + up to 20 digits + " bytes]" (7 bytes).
    let mut keep = PREVIEW_MAX - 32;
    while !text.is_char_boundary(keep) {
        keep -= 1;
    }
    format!("{}…[+{} bytes]", &text[..keep], text.len() - keep)
}

/// Splits `text` into pieces of at most `max` bytes on char boundaries.
pub fn chunks(text: &str, max: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while rest.len() > max {
        let mut cut = max;
        while !rest.is_char_boundary(cut) {
            cut -= 1;
        }
        out.push(rest[..cut].to_owned());
        rest = &rest[cut..];
    }
    if !rest.is_empty() {
        out.push(rest.to_owned());
    }
    out
}

/// The last `max` bytes of `text`, starting on a char boundary.
pub fn tail(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut start = text.len() - max;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "mgg_abcdefghijklmnop";

    #[test]
    fn redacts_every_occurrence_longest_first() {
        let r = Redactor::new([SECRET, "abcdefghijklmnopq_inner"]);
        assert_eq!(r.redact(&format!("a {SECRET} b {SECRET}")), "a [redacted] b [redacted]");
        let nested = Redactor::new(["tok_0123456789abcdef", "tok_0123456789abcdef_longer"]);
        assert_eq!(nested.redact("x tok_0123456789abcdef_longer y"), "x [redacted] y");
        assert_eq!(Redactor::default().redact("plain"), "plain");
    }

    #[test]
    fn secret_split_across_chunks_is_redacted() {
        let r = Redactor::new([SECRET]);
        for split in 1..SECRET.len() {
            let mut s = StreamRedactor::new(r.clone());
            let mut out = s.push(format!("before {}", &SECRET[..split]).as_bytes());
            out.extend(s.push(format!("{} after", &SECRET[split..]).as_bytes()));
            out.extend(s.finish());
            assert_eq!(String::from_utf8(out).unwrap(), "before [redacted] after", "split at {split}");
        }
    }

    #[test]
    fn holdback_only_keeps_possible_prefixes() {
        let r = Redactor::new([SECRET]);
        let mut s = StreamRedactor::new(r);
        // "mg" could start the secret: held back; "x" cannot.
        assert_eq!(s.push(b"hello mg"), b"hello ");
        assert_eq!(s.push(b"x"), b"mgx");
        assert_eq!(s.finish(), b"");
        // A dangling prefix at the very end is emitted by finish.
        let mut s = StreamRedactor::new(Redactor::new([SECRET]));
        assert_eq!(s.push(b"end mgg_ab"), b"end ");
        assert_eq!(s.finish(), b"mgg_ab");
    }

    #[test]
    fn preview_is_utf8_safe_and_bounded() {
        assert_eq!(preview("short"), "short");
        let long = "é".repeat(5000);
        let p = preview(&long);
        assert!(p.len() <= PREVIEW_MAX, "{}", p.len());
        let suffix_at = p.find('…').unwrap();
        let kept = &p[..suffix_at];
        assert!(p.ends_with(&format!("…[+{} bytes]", long.len() - kept.len())));
        assert_eq!(kept.chars().count() * 2, kept.len());
    }

    #[test]
    fn chunks_respect_max_and_boundaries() {
        let text = format!("{}{}", "a".repeat(4095), "é".repeat(3));
        let parts = chunks(&text, 4096);
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].len(), 4095);
        assert_eq!(parts.concat(), text);
        assert!(chunks("", 10).is_empty());
        assert_eq!(tail("abcdé", 2), "é");
        assert_eq!(tail("abcdé", 3), "dé");
    }
}
