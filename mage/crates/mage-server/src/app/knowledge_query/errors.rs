//! Input preparation of error matching (knowledge.md "Layer 3: Error sites", Matching step 1).
//! Pure: strip ANSI escapes, cap at 4 KiB, pull message fields out of JSON (`"msg"`, `"message"`,
//! `"error"`, `"err"`) and logfmt (`msg="…"`), then segment: the whole input, each line, each
//! extracted message, and for Go-style chains every `": "`-separated suffix. Tokens use the
//! indexer's tokenizer so containment queries compare like with like.

use mage_core::domain::knowledge::tokenize;
use mage_core::ports::knowledge::ErrorQuery;

/// Inputs longer than this are cut (at a char boundary).
pub const MAX_INPUT_BYTES: usize = 4096;
/// `lookup_error`'s candidate cap per query.
pub const MAX_CANDIDATES: u32 = 200;
/// Segments kept per input (the whole input first).
const MAX_SEGMENTS: usize = 64;

const MESSAGE_KEYS: [&str; 4] = ["msg", "message", "error", "err"];

/// The `ErrorQuery` of one input text.
pub fn prepare(input: &str, limit: u32, include_low_specificity: bool) -> ErrorQuery {
    let text = cap(&strip_ansi(input), MAX_INPUT_BYTES);
    let mut segments: Vec<String> = Vec::new();
    let push = |segment: &str, segments: &mut Vec<String>| {
        let segment = segment.trim();
        if !segment.is_empty() && segments.len() < MAX_SEGMENTS && !segments.iter().any(|s| s == segment) {
            segments.push(segment.to_owned());
        }
    };
    push(&text, &mut segments);
    let lines: Vec<&str> = text.lines().collect();
    for line in &lines {
        push(line, &mut segments);
    }
    let mut messages = Vec::new();
    for line in &lines {
        messages.extend(message_fields(line));
    }
    for message in &messages {
        push(message, &mut segments);
    }
    let chained: Vec<String> = lines.iter().map(|l| (*l).to_owned()).chain(messages).collect();
    for candidate in &chained {
        for suffix in chain_suffixes(candidate) {
            push(suffix, &mut segments);
        }
    }
    ErrorQuery { segments, tokens: tokenize(&text), max_candidates: MAX_CANDIDATES, limit, include_low_specificity }
}

/// Removes CSI (`ESC [ … final`) and OSC (`ESC ] … BEL|ESC \`) sequences and stray ESC bytes.
pub fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match chars.peek() {
            Some('[') => {
                chars.next();
                for c in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&c) {
                        break;
                    }
                }
            }
            Some(']') => {
                chars.next();
                while let Some(c) = chars.next() {
                    if c == '\u{7}' {
                        break;
                    }
                    if c == '\u{1b}' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    out
}

fn cap(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

/// Values of `"msg": "…"`-style JSON fields and `msg="…"` / `msg=word` logfmt pairs, unescaped.
pub fn message_fields(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    for key in MESSAGE_KEYS {
        let mut rest = line;
        // JSON: "key" : "value"
        let quoted = format!("\"{key}\"");
        while let Some(at) = rest.find(&quoted) {
            let after = rest[at + quoted.len()..].trim_start();
            rest = &rest[at + quoted.len()..];
            if let Some(value) = after.strip_prefix(':').map(str::trim_start).and_then(|v| v.strip_prefix('"')) {
                if let Some((text, _)) = json_string(value) {
                    out.push(text);
                }
            }
        }
        // logfmt: key="value" or key=value, with `key` at a word boundary.
        let pair = format!("{key}=");
        let mut offset = 0;
        while let Some(found) = line[offset..].find(&pair) {
            let at = offset + found;
            offset = at + pair.len();
            let boundary = at == 0 || !line[..at].chars().next_back().is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '"');
            if !boundary {
                continue;
            }
            let value = &line[at + pair.len()..];
            if let Some(quoted) = value.strip_prefix('"') {
                if let Some((text, _)) = json_string(quoted) {
                    out.push(text);
                }
            } else {
                let word: String = value.chars().take_while(|c| !c.is_whitespace()).collect();
                if !word.is_empty() {
                    out.push(word);
                }
            }
        }
    }
    out
}

/// Reads a JSON-escaped string body up to its closing quote; returns the text and the rest.
fn json_string(body: &str) -> Option<(String, &str)> {
    let mut out = String::new();
    let mut chars = body.char_indices();
    while let Some((i, c)) = chars.next() {
        match c {
            '"' => return Some((out, &body[i + 1..])),
            '\\' => match chars.next()?.1 {
                'n' => out.push('\n'),
                't' => out.push('\t'),
                'r' => out.push('\r'),
                'u' => {
                    let hex: String = (0..4).filter_map(|_| chars.next().map(|(_, c)| c)).collect();
                    if let Some(c) = u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                        out.push(c);
                    }
                }
                other => out.push(other),
            },
            c => out.push(c),
        }
    }
    None
}

/// Every suffix after a `": "` separator (Go error chains: `a: b: c` → `b: c`, `c`).
fn chain_suffixes(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut offset = 0;
    while let Some(found) = text[offset..].find(": ") {
        let at = offset + found + 2;
        out.push(&text[at..]);
        offset = at;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segments_cover_lines_messages_and_chain_suffixes() {
        let input = "\u{1b}[31mERROR\u{1b}[0m request failed\nlevel=error msg=\"failed fetching connection pg-prod: context deadline exceeded\" code=500";
        let q = prepare(input, 10, false);
        assert!(!q.segments[0].contains('\u{1b}'), "ANSI escapes are stripped");
        assert!(q.segments.contains(&"ERROR request failed".to_owned()));
        assert!(q.segments.contains(&"failed fetching connection pg-prod: context deadline exceeded".to_owned()));
        assert!(q.segments.contains(&"context deadline exceeded".to_owned()));
        assert_eq!(q.tokens, tokenize(&strip_ansi(input)));
        assert_eq!((q.limit, q.max_candidates, q.include_low_specificity), (10, MAX_CANDIDATES, false));
    }

    #[test]
    fn json_message_fields_are_unescaped() {
        let line = r#"{"level":"error","message":"Connection \"42\" not found","err": "x\ny"}"#;
        assert_eq!(message_fields(line), vec!["Connection \"42\" not found".to_owned(), "x\ny".to_owned()]);
        assert!(message_fields("errmsg=nope").is_empty(), "keys match at word boundaries only");
        assert_eq!(message_fields("ts=1 err=EOF"), vec!["EOF".to_owned()]);
    }

    #[test]
    fn input_is_capped_at_4_kib() {
        let long = "é".repeat(5000);
        let q = prepare(&long, 10, false);
        assert!(q.segments[0].len() <= MAX_INPUT_BYTES);
    }
}
