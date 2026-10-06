//! Output shaping (executor.md "Confinement and limits"): text cleaning, per-field truncation,
//! and the whole-result cap. Applied in `RunTools`, so every executor answers the same.

use serde_json::Value;

use crate::domain::observe::strip_ansi;

/// Text for the agent: invalid UTF-8 replaced, ANSI sequences removed, control characters other
/// than `\n` and `\t` stripped.
pub fn clean_bytes(bytes: &[u8]) -> String {
    clean_text(&String::from_utf8_lossy(bytes))
}

pub fn clean_text(text: &str) -> String {
    let stripped = strip_ansi(text);
    stripped.chars().filter(|c| !c.is_control() || *c == '\n' || *c == '\t').collect()
}

/// Cuts `text` to at most `max` bytes on a char boundary.
pub fn truncate_bytes(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// The last at most `max` bytes of `text` on a char boundary.
pub fn tail_bytes(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut start = text.len() - max;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}

/// Head and tail of a captured stream joined by the `exec` omission marker when bytes were
/// dropped between them.
pub fn join_captured(head: &[u8], tail: &[u8], omitted: u64, log: &str) -> String {
    let mut out = clean_bytes(head);
    if omitted > 0 {
        out.push_str(&format!("\n…[{omitted} bytes omitted; full output: logs source={log}]…\n"));
    }
    out.push_str(&clean_bytes(tail));
    out
}

/// Marker appended to a field cut by the whole-result cap.
pub const CUT_MARKER: &str = "…[truncated]";

/// Serialized size of `value` in bytes.
pub fn json_len(value: &Value) -> usize {
    serde_json::to_vec(value).map_or(usize::MAX, |v| v.len())
}

/// Enforces `max` serialized bytes: the largest string field is cut first (with
/// [`CUT_MARKER`]), repeatedly; when no string is worth cutting, the longest array loses its
/// tail. Returns whether anything was cut.
pub fn cap_result(value: &mut Value, max: usize) -> bool {
    let mut cut = false;
    for _ in 0..256 {
        let len = json_len(value);
        if len <= max {
            return cut;
        }
        let excess = len - max;
        cut = true;
        if let Some(text) = largest_string(value)
            && text.len() > CUT_MARKER.len() * 2
        {
            // JSON escaping can make a string count more than its byte length; cut generously.
            let keep = text.len().saturating_sub(excess + CUT_MARKER.len() + 16);
            let mut kept = truncate_bytes(text, keep).to_owned();
            kept.push_str(CUT_MARKER);
            *text = kept;
            continue;
        }
        match longest_array(value) {
            Some(items) if !items.is_empty() => {
                let keep = items.len() / 2;
                items.truncate(keep);
            }
            _ => return cut,
        }
    }
    cut
}

fn largest_string(value: &mut Value) -> Option<&mut String> {
    let mut best: Option<&mut String> = None;
    let mut stack = vec![value];
    while let Some(v) = stack.pop() {
        match v {
            Value::String(s) => {
                if best.as_ref().is_none_or(|b| s.len() > b.len()) {
                    best = Some(s);
                }
            }
            Value::Array(items) => stack.extend(items.iter_mut()),
            Value::Object(map) => stack.extend(map.values_mut()),
            _ => {}
        }
    }
    best
}

fn longest_array(value: &mut Value) -> Option<&mut Vec<Value>> {
    let mut best: Option<&mut Vec<Value>> = None;
    let mut stack = vec![value];
    while let Some(v) = stack.pop() {
        match v {
            Value::Array(items) => {
                // Prefer the outermost longest array; its children are visited only if it loses.
                if best.as_ref().is_none_or(|b| items.len() > b.len()) {
                    best = Some(items);
                } else {
                    stack.extend(items.iter_mut());
                }
            }
            Value::Object(map) => stack.extend(map.values_mut()),
            _ => {}
        }
    }
    best
}

/// A short preview of `value` for events and the MCP log (≤ `max` bytes).
pub fn preview(value: &Value, max: usize) -> String {
    let text = match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    let cut = truncate_bytes(&text, max);
    if cut.len() < text.len() { format!("{}…", truncate_bytes(cut, max.saturating_sub(3))) } else { text }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn clean_text_strips_controls_and_ansi_but_keeps_newlines_and_tabs() {
        assert_eq!(clean_bytes(b"a\x1b[31mred\x1b[0m\tb\r\nc\x07\x00d"), "ared\tb\ncd");
        assert_eq!(clean_bytes(&[b'o', b'k', 0xff, 0xfe]), "ok\u{fffd}\u{fffd}");
    }

    #[test]
    fn join_captured_marker() {
        assert_eq!(join_captured(b"head", b"", 0, "exec:3"), "head");
        assert_eq!(join_captured(b"head", b"tail", 10, "exec:3"), "head\n…[10 bytes omitted; full output: logs source=exec:3]…\ntail");
    }

    #[test]
    fn byte_cuts_respect_char_boundaries() {
        assert_eq!(truncate_bytes("héllo", 2), "h");
        assert_eq!(tail_bytes("héllo", 4), "llo");
        assert_eq!(tail_bytes("abc", 10), "abc");
    }

    #[test]
    fn cap_result_cuts_the_largest_string_first() {
        let mut v = json!({"stdout": "a".repeat(10_000), "stderr": "b".repeat(2_000), "exit_code": 0});
        assert!(cap_result(&mut v, 6_000));
        assert!(json_len(&v) <= 6_000);
        assert_eq!(v["stderr"].as_str().unwrap().len(), 2_000, "smaller field untouched");
        assert!(v["stdout"].as_str().unwrap().ends_with(CUT_MARKER));
        assert_eq!(v["exit_code"], 0);

        let mut small = json!({"x": "y"});
        assert!(!cap_result(&mut small, 100));
        assert_eq!(small, json!({"x": "y"}));
    }

    #[test]
    fn cap_result_handles_escaping_heavy_strings() {
        // Every quote doubles in JSON, so byte length underestimates the serialized size.
        let mut v = json!({"body": "\"".repeat(20_000)});
        cap_result(&mut v, 4_096);
        assert!(json_len(&v) <= 4_096);
    }

    #[test]
    fn cap_result_drops_array_tails_when_strings_are_small() {
        let mut v = json!({"rows": (0..5_000).map(|i| json!([i])).collect::<Vec<_>>()});
        assert!(cap_result(&mut v, 2_000));
        assert!(json_len(&v) <= 2_000);
        assert_eq!(v["rows"][0], json!([0]), "head rows kept");
    }

    #[test]
    fn preview_is_bounded() {
        let p = preview(&json!({"command": "x".repeat(10_000)}), 4096);
        assert!(p.len() <= 4096);
        assert!(p.ends_with('…'));
        assert_eq!(preview(&json!("short"), 10), "short");
    }
}
