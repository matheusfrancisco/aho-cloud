//! Node position / text helpers shared by the extractors.

use std::time::Instant;

use tree_sitter::Node;

use crate::ports::parser::ParseError;

/// Max chars of a hole expression and of a callee.
pub(super) const EXPR_MAX_CHARS: usize = 64;
/// Max chars of a symbol signature.
pub(super) const SIGNATURE_MAX_CHARS: usize = 200;
/// Max bytes of a symbol doc.
pub(super) const DOC_MAX_BYTES: usize = 1024;

/// Source text of a node (empty on invalid UTF-8).
pub(super) fn text<'s>(node: Node<'_>, src: &'s [u8]) -> &'s str {
    node.utf8_text(src).unwrap_or("")
}

/// UTF-8 text of a byte range (empty when out of bounds or invalid).
pub(super) fn slice(src: &[u8], start: usize, end: usize) -> &str {
    src.get(start..end).and_then(|b| std::str::from_utf8(b).ok()).unwrap_or("")
}

fn to_u32(v: usize) -> u32 {
    u32::try_from(v).unwrap_or(u32::MAX - 1)
}

/// 1-based (line, column) of the node start; column in UTF-8 bytes.
pub(super) fn pos(node: Node<'_>) -> (u32, u32) {
    let p = node.start_position();
    (to_u32(p.row) + 1, to_u32(p.column) + 1)
}

/// 1-based start line.
pub(super) fn start_line(node: Node<'_>) -> u32 {
    to_u32(node.start_position().row) + 1
}

/// 1-based inclusive end line; a node ending at column 0 of a later line ends on the line before.
pub(super) fn end_line(node: Node<'_>) -> u32 {
    let start = node.start_position();
    let end = node.end_position();
    if end.column == 0 && end.row > start.row { to_u32(end.row) } else { to_u32(end.row) + 1 }
}

/// Text with every whitespace char removed.
pub(super) fn squash(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}

/// Whitespace runs collapsed to one space, trimmed.
pub(super) fn collapse_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for word in s.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
    }
    out
}

/// First `max` chars.
pub(super) fn head_chars(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => s[..i].to_string(),
        None => s.to_string(),
    }
}

/// Last `max` chars.
pub(super) fn tail_chars(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        return s.to_string();
    }
    match s.char_indices().nth(count - max) {
        Some((i, _)) => s[i..].to_string(),
        None => s.to_string(),
    }
}

/// Longest prefix of at most `max` bytes ending on a char boundary.
pub(super) fn head_bytes(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

/// Hole expression text: whitespace collapsed, first 64 chars.
pub(super) fn expr_text(node: Node<'_>, src: &[u8]) -> String {
    head_chars(&collapse_ws(text(node, src)), EXPR_MAX_CHARS)
}

/// Callee text: whitespace removed, last 64 chars.
pub(super) fn callee_text(s: &str) -> String {
    tail_chars(&squash(s), EXPR_MAX_CHARS)
}

/// Named children that are not comments.
pub(super) fn named_children<'t>(node: Node<'t>) -> Vec<Node<'t>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).filter(|n| !n.is_extra() && n.kind() != "comment").collect()
}

/// `node` is the child of `parent` under `field`.
pub(super) fn is_field(parent: Node<'_>, field: &str, node: Node<'_>) -> bool {
    parent.child_by_field_name(field).is_some_and(|c| c.id() == node.id())
}

/// Strip one pair of matching quotes (`"`, `'`, `` ` ``).
pub(super) fn unquote(s: &str) -> &str {
    for q in ['"', '\'', '`'] {
        if s.len() >= 2 && s.starts_with(q) && s.ends_with(q) {
            return &s[1..s.len() - 1];
        }
    }
    s
}

/// Decodes C-family backslash escapes (`\n \t \r \a \b \f \v \0 \\ \" \' \xHH \uHHHH \u{H..}
/// \UHHHHHHHH \ooo`, line continuations). Unknown escapes are kept verbatim.
pub(super) fn unescape_into(s: &str, out: &mut String) {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < s.len() {
        let Some(c) = s[i..].chars().next() else { break };
        if c != '\\' {
            out.push(c);
            i += c.len_utf8();
            continue;
        }
        let Some(e) = s[i + 1..].chars().next() else {
            out.push('\\');
            break;
        };
        let mut next = i + 1 + e.len_utf8();
        match e {
            'n' => out.push('\n'),
            't' => out.push('\t'),
            'r' => out.push('\r'),
            'a' => out.push('\u{07}'),
            'b' => out.push('\u{08}'),
            'f' => out.push('\u{0c}'),
            'v' => out.push('\u{0b}'),
            '\\' | '"' | '\'' | '`' | '$' => out.push(e),
            '\n' => {}
            '\r' => {
                if bytes.get(next) == Some(&b'\n') {
                    next += 1;
                }
            }
            'x' => {
                let (v, used) = hex_prefix(&s[next..], 2);
                match v.and_then(char::from_u32) {
                    Some(ch) if used > 0 => {
                        out.push(ch);
                        next += used;
                    }
                    _ => out.push_str(&s[i..next]),
                }
            }
            'u' if s[next..].starts_with('{') => match s[next..].find('}') {
                Some(close) => {
                    let hex = &s[next + 1..next + close];
                    match u32::from_str_radix(hex, 16).ok().and_then(char::from_u32) {
                        Some(ch) => out.push(ch),
                        None => out.push_str(&s[i..next + close + 1]),
                    }
                    next += close + 1;
                }
                None => out.push_str(&s[i..next]),
            },
            'u' => {
                let (v, used) = hex_prefix(&s[next..], 4);
                match v {
                    Some(hi) if used == 4 && (0xD800..0xDC00).contains(&hi) => {
                        // Surrogate pair `\uD83D\uDE00`.
                        let rest = &s[next + 4..];
                        let lo = rest.strip_prefix("\\u").map(|r| hex_prefix(r, 4));
                        match lo {
                            Some((Some(lo), 4)) if (0xDC00..0xE000).contains(&lo) => {
                                let cp = 0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00);
                                out.push(char::from_u32(cp).unwrap_or('\u{FFFD}'));
                                next += 10;
                            }
                            _ => {
                                out.push('\u{FFFD}');
                                next += 4;
                            }
                        }
                    }
                    Some(cp) if used == 4 => {
                        out.push(char::from_u32(cp).unwrap_or('\u{FFFD}'));
                        next += 4;
                    }
                    _ => out.push_str(&s[i..next]),
                }
            }
            'U' => {
                let (v, used) = hex_prefix(&s[next..], 8);
                match v.and_then(char::from_u32) {
                    Some(ch) if used == 8 => {
                        out.push(ch);
                        next += 8;
                    }
                    _ => out.push_str(&s[i..next]),
                }
            }
            '0'..='7' => {
                let mut v = e.to_digit(8).unwrap_or(0);
                let mut used = 0;
                while used < 2 {
                    match bytes.get(next + used) {
                        Some(b @ b'0'..=b'7') => {
                            v = v * 8 + u32::from(b - b'0');
                            used += 1;
                        }
                        _ => break,
                    }
                }
                out.push(char::from_u32(v).unwrap_or('\u{FFFD}'));
                next += used;
            }
            _ => out.push_str(&s[i..next]),
        }
        i = next;
    }
}

pub(super) fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    unescape_into(s, &mut out);
    out
}

/// Value of up to `max` leading hex digits and how many bytes they took.
fn hex_prefix(s: &str, max: usize) -> (Option<u32>, usize) {
    let used = s.bytes().take(max).take_while(u8::is_ascii_hexdigit).count();
    if used == 0 {
        return (None, 0);
    }
    (u32::from_str_radix(&s[..used], 16).ok(), used)
}

/// Integer literal value: decimal / `0x` / `0o` / `0b`, `_` separators, Rust type suffixes.
pub(super) fn parse_int(s: &str) -> Option<i64> {
    let mut t: String = s.chars().filter(|c| *c != '_').collect();
    for suffix in ["i128", "u128", "isize", "usize", "i64", "u64", "i32", "u32", "i16", "u16", "i8", "u8", "n"] {
        if t.len() > suffix.len() && t.ends_with(suffix) && !t.starts_with("0x") && !t.starts_with("0X") {
            t.truncate(t.len() - suffix.len());
            break;
        }
    }
    let lower = t.to_ascii_lowercase();
    if let Some(h) = lower.strip_prefix("0x") {
        return i64::from_str_radix(h, 16).ok();
    }
    if let Some(o) = lower.strip_prefix("0o") {
        return i64::from_str_radix(o, 8).ok();
    }
    if let Some(b) = lower.strip_prefix("0b") {
        return i64::from_str_radix(b, 2).ok();
    }
    if lower.is_empty() || !lower.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    lower.parse().ok()
}

/// Per-blob wall-clock deadline.
pub(super) struct Deadline {
    at: Instant,
    ticks: u32,
}

impl Deadline {
    pub(super) fn new(at: Instant) -> Self {
        Self { at, ticks: 0 }
    }

    pub(super) fn instant(&self) -> Instant {
        self.at
    }

    pub(super) fn check(&self) -> Result<(), ParseError> {
        if Instant::now() >= self.at { Err(ParseError::Timeout) } else { Ok(()) }
    }

    /// Cheap periodic check for long walks.
    pub(super) fn tick(&mut self) -> Result<(), ParseError> {
        self.ticks = self.ticks.wrapping_add(1);
        if self.ticks % 4096 == 0 { self.check() } else { Ok(()) }
    }
}

/// Sorted outermost `ERROR` node byte ranges; facts starting inside them are dropped.
#[derive(Default)]
pub(super) struct ErrorRanges(Vec<(usize, usize)>);

impl ErrorRanges {
    /// Walks the tree: returns the number of `ERROR` + `MISSING` nodes and the outermost error ranges.
    pub(super) fn scan(root: Node<'_>) -> (u32, Self) {
        if !root.has_error() {
            return (0, Self::default());
        }
        let mut count = 0u32;
        let mut ranges = Vec::new();
        let mut cursor = root.walk();
        let mut inside_error_depth: Option<usize> = None;
        let mut depth = 0usize;
        loop {
            let node = cursor.node();
            if node.is_error() || node.is_missing() {
                count = count.saturating_add(1);
            }
            if node.is_error() && inside_error_depth.is_none() {
                inside_error_depth = Some(depth);
                ranges.push((node.start_byte(), node.end_byte()));
            }
            let descend = node.has_error() || inside_error_depth.is_some();
            if descend && cursor.goto_first_child() {
                depth += 1;
                continue;
            }
            loop {
                if inside_error_depth == Some(depth) {
                    inside_error_depth = None;
                }
                if cursor.goto_next_sibling() {
                    break;
                }
                if !cursor.goto_parent() {
                    return (count, Self(ranges));
                }
                depth -= 1;
            }
        }
    }

    pub(super) fn contains(&self, byte: usize) -> bool {
        let idx = self.0.partition_point(|(start, _)| *start <= byte);
        idx > 0 && byte < self.0[idx - 1].1
    }
}
