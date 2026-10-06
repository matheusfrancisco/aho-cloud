//! Markdown helpers shared by the fix brief, the PR body and the commit message (fix.md
//! "Template", "Security notes: Untrusted text"): untrusted text is HTML-stripped, has `@`
//! mentions neutralised (`@\u{200B}name`) and is length-capped before it is quoted or fenced, so
//! pasting into Linear or GitHub never pings anyone or renders markup.

use mage_core::domain::{CommitSha, LineRange, RepoPath};
use url::Url;

/// Zero-width space inserted after `@` so `@name` does not mention anyone.
const ZWSP: char = '\u{200B}';

/// `@name` → `@\u{200B}name` when the `@` starts a word and is followed by an ASCII alphanumeric
/// (e-mail addresses are left alone).
pub fn neutralize_mentions(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev: Option<char> = None;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        out.push(c);
        if c == '@' {
            let starts_word = prev.is_none_or(|p| !(p.is_alphanumeric() || p == '_' || p == '.' || p == '-'));
            let next_is_name = chars.peek().is_some_and(char::is_ascii_alphanumeric);
            if starts_word && next_is_name {
                out.push(ZWSP);
            }
        }
        prev = Some(c);
    }
    out
}

/// Removes HTML tags and comments (`<b>`, `</div>`, `<img …>`, `<!-- … -->`); a lone `<` stays.
pub fn strip_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(open) = rest.find('<') {
        out.push_str(&rest[..open]);
        let after = &rest[open..];
        if let Some(body) = after.strip_prefix("<!--") {
            match body.find("-->") {
                Some(end) => {
                    rest = &body[end + 3..];
                    continue;
                }
                None => return out,
            }
        }
        let tag_like = after[1..].chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '/' || c == '!');
        match (tag_like, after.find('>')) {
            (true, Some(close)) => rest = &after[close + 1..],
            _ => {
                out.push('<');
                rest = &after[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// At most `max` chars, ending in `…` when cut.
pub fn cap_chars(s: &str, max: usize) -> String {
    let mut chars = s.char_indices();
    match chars.nth(max) {
        None => s.to_owned(),
        Some((cut, _)) => {
            let mut out = s[..cut].trim_end().to_owned();
            out.push('…');
            out
        }
    }
}

/// Untrusted prose: HTML stripped, mentions neutralised, trimmed, capped at `max` chars.
pub fn untrusted(s: &str, max: usize) -> String {
    cap_chars(neutralize_mentions(&strip_html(s)).trim(), max)
}

/// Every line prefixed with `> ` (blank lines as `>`).
pub fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 16);
    for (i, line) in s.lines().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        if line.trim().is_empty() {
            out.push('>');
        } else {
            out.push_str("> ");
            out.push_str(line);
        }
    }
    if out.is_empty() {
        out.push('>');
    }
    out
}

fn longest_run(s: &str, ch: char) -> usize {
    let (mut best, mut run) = (0, 0);
    for c in s.chars() {
        if c == ch {
            run += 1;
            best = best.max(run);
        } else {
            run = 0;
        }
    }
    best
}

/// A fenced code block whose fence is longer than any backtick run inside `body`.
pub fn fence(lang: &str, body: &str) -> String {
    let ticks = "`".repeat((longest_run(body, '`') + 1).max(3));
    let mut out = format!("{ticks}{lang}\n{body}");
    if !body.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&ticks);
    out
}

/// Inline code for `s` (single line), with enough backticks to contain it.
pub fn code(s: &str) -> String {
    let s = s.replace(['\r', '\n'], " ");
    let ticks = "`".repeat(longest_run(&s, '`') + 1);
    if s.starts_with('`') || s.ends_with('`') { format!("{ticks} {s} {ticks}") } else { format!("{ticks}{s}{ticks}") }
}

/// Text safe inside a markdown table cell.
pub fn cell(s: &str) -> String {
    s.replace(['\r', '\n'], " ").replace('|', "\\|")
}

/// First 7 hex chars of a commit.
pub fn short(sha: &CommitSha) -> String {
    let mut hex = sha.to_hex();
    hex.truncate(7);
    hex
}

/// `{web}{owner}/{name}/blob/{sha}/{path}#L{a}-L{b}`.
pub fn permalink(web: &Url, owner: &str, name: &str, sha: &CommitSha, path: &RepoPath, lines: &LineRange) -> String {
    let base = web.as_str().trim_end_matches('/');
    format!("{base}/{owner}/{name}/blob/{sha}/{path}#L{}-L{}", lines.start(), lines.end())
}

/// `{ui}{path}` with exactly one `/` between them.
pub fn ui_link(ui: &Url, path: &str) -> String {
    format!("{}/{}", ui.as_str().trim_end_matches('/'), path.trim_start_matches('/'))
}

/// The first paragraph of `s` (up to the first blank line).
pub fn first_paragraph(s: &str) -> &str {
    let s = s.trim();
    match s.find("\n\n").or_else(|| s.find("\r\n\r\n")) {
        Some(end) => s[..end].trim_end(),
        None => s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mentions_are_neutralised_but_emails_are_not() {
        assert_eq!(neutralize_mentions("ping @alice and @bob-x, mail a@b.com, @ alone, @@x"), "ping @\u{200B}alice and @\u{200B}bob-x, mail a@b.com, @ alone, @@\u{200B}x");
        assert_eq!(neutralize_mentions("(@team)"), "(@\u{200B}team)");
    }

    #[test]
    fn html_is_stripped_and_lone_brackets_kept() {
        assert_eq!(strip_html("a <b>bold</b> <img src=x onerror=alert(1)> c"), "a bold  c");
        assert_eq!(strip_html("x < y and <!-- hidden --> z"), "x < y and  z");
        assert_eq!(strip_html("1 <2 and 3> 2"), "1 <2 and 3> 2");
        assert_eq!(strip_html("<!-- unterminated"), "");
    }

    #[test]
    fn fences_and_code_spans_contain_backticks() {
        assert_eq!(fence("ts", "a ``` b"), "````ts\na ``` b\n````");
        assert_eq!(fence("", "x\n"), "```\nx\n```");
        assert_eq!(code("a`b"), "``a`b``");
        assert_eq!(code("`x`"), "`` `x` ``");
    }

    #[test]
    fn quote_caps_and_cells() {
        assert_eq!(quote("a\n\nb"), "> a\n>\n> b");
        assert_eq!(quote(""), ">");
        assert_eq!(cap_chars("héllo world", 5), "héllo…");
        assert_eq!(cap_chars("short", 10), "short");
        assert_eq!(cell("a|b\nc"), "a\\|b c");
        assert_eq!(first_paragraph("one\nline\n\ntwo"), "one\nline");
    }
}
