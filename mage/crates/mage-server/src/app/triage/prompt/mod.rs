//! Triage prompt rendering (triage.md "System prompt append", "Initial prompt template").
//!
//! Pure and versioned: `spec.prompt_version` selects the template, so a stored run re-renders
//! byte-identically and eval baselines can pin an older version. Untrusted text (ticket fields,
//! earlier model output, index data) is neutralized and wrapped in nonce-delimited blocks.

pub mod v1;

use std::fmt::Write as _;

use mage_core::ports::IdGen;

use crate::domain::triage::{TriageInput, TriageSpec};
use crate::error::AppError;

/// Version written into new specs.
pub const TRIAGE_PROMPT_VERSION: u16 = 1;

/// Rendered harness inputs: `RunSpec.system_append` and `RunSpec.prompt`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedPrompt {
    pub system_append: String,
    pub initial_prompt: String,
}

/// Why a render did not produce a prompt.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RenderError {
    /// The nonce occurs in the untrusted data; draw another one.
    #[error("nonce occurs in untrusted data")]
    NonceCollision,
    #[error("unknown triage prompt version {0}")]
    UnknownVersion(u16),
}

/// Renders the prompts of `spec.prompt_version`. `input` is the snapshot to embed (normally
/// `spec.input`).
pub fn render(input: &TriageInput, spec: &TriageSpec, nonce: [u8; 16]) -> Result<RenderedPrompt, RenderError> {
    match spec.prompt_version {
        1 => v1::render(input, spec, nonce),
        other => Err(RenderError::UnknownVersion(other)),
    }
}

/// [`render`] with a nonce drawn from `ids`, redrawn on the (practically impossible) collision.
pub fn render_with_fresh_nonce(input: &TriageInput, spec: &TriageSpec, ids: &dyn IdGen) -> Result<RenderedPrompt, AppError> {
    const DRAWS: usize = 8;
    for _ in 0..DRAWS {
        match render(input, spec, ids.nonce()) {
            Ok(rendered) => return Ok(rendered),
            Err(RenderError::NonceCollision) => continue,
            Err(e @ RenderError::UnknownVersion(_)) => return Err(AppError::Invalid(e.to_string())),
        }
    }
    Err(AppError::Internal("no prompt nonce free of collisions".into()))
}

/// Untrusted text made safe to embed: NUL and other C0 controls (except `\n`, `\t`), DEL and C1
/// controls removed; Unicode bidi controls and zero-width characters replaced by visible
/// `<U+XXXX>` markers.
pub fn neutralize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\n' | '\t' => out.push(c),
            c if c.is_control() => {}
            c if is_invisible_format(c) => {
                let _ = write!(out, "<U+{:04X}>", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out
}

/// Bidi embedding/override/isolate controls, bidi marks, and zero-width characters.
fn is_invisible_format(c: char) -> bool {
    matches!(
        c,
        '\u{202A}'..='\u{202E}'
            | '\u{2066}'..='\u{2069}'
            | '\u{200B}'..='\u{200F}'
            | '\u{2060}'..='\u{2064}'
            | '\u{061C}'
            | '\u{FEFF}'
            | '\u{180E}'
    )
}

/// [`neutralize`], then every whitespace run (newlines included) collapsed to one space, trimmed,
/// and cut to `max_chars` (with `…`). For values interpolated outside untrusted blocks.
pub fn single_line(text: &str, max_chars: usize) -> String {
    let neutral = neutralize(text);
    let collapsed = neutral.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_chars(&collapsed, max_chars)
}

/// At most `max_chars` Unicode scalar values; a cut string ends with `…`.
pub fn truncate_chars(text: &str, max_chars: usize) -> String {
    match text.char_indices().nth(max_chars) {
        None => text.to_owned(),
        Some((cut, _)) => {
            let mut s = text[..cut.min(text.len())].to_owned();
            // Keep the total within `max_chars` including the marker.
            if max_chars > 0 {
                s.pop();
            }
            s.push('…');
            s
        }
    }
}

/// At most `max_bytes` bytes, cut on a char boundary; a cut string ends with a truncation line.
pub fn truncate_bytes(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_owned();
    }
    let mut cut = max_bytes;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}\n[truncated: {} of {} bytes shown]", &text[..cut], cut, text.len())
}

/// Lowercase hex of the nonce.
pub fn nonce_hex(nonce: [u8; 16]) -> String {
    hex::encode(nonce)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn neutralize_strips_controls_and_marks_bidi_and_zero_width() {
        let raw = "a\u{0}b\u{7}c\r\nd\te\u{7f}f\u{85}g\u{202E}h\u{2066}i\u{200B}j\u{FEFF}k";
        assert_eq!(neutralize(raw), "abc\nd\tefg<U+202E>h<U+2066>i<U+200B>j<U+FEFF>k");
    }

    #[test]
    fn neutralize_keeps_ordinary_unicode() {
        assert_eq!(neutralize("café — naïve 日本 🙂"), "café — naïve 日本 🙂");
    }

    #[test]
    fn single_line_collapses_and_truncates() {
        assert_eq!(single_line("  a\n\n b\tc  ", 100), "a b c");
        assert_eq!(single_line("abcdef", 4), "abc…");
        assert_eq!(single_line("abcd", 4), "abcd");
    }

    #[test]
    fn truncate_bytes_respects_char_boundaries() {
        let s = "ééé";
        let cut = truncate_bytes(s, 3);
        assert!(cut.starts_with('é'));
        assert!(!cut.starts_with("éé"));
        assert!(cut.contains("[truncated: 2 of 6 bytes shown]"));
        assert_eq!(truncate_bytes("short", 64), "short");
    }
}
