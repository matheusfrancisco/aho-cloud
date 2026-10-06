//! Input extraction of the triage retrieval stages (knowledge.md "Retrieval pipeline for triage"):
//! pure functions over the ticket text, std + `regex` only.

use std::collections::BTreeSet;
use std::sync::LazyLock;

use regex::Regex;

use super::super::text::{glossary_words, singular};

/// A source location mentioned in a stack trace or log line.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Frame {
    /// As written (`/app/gateway/api/conn.go`, `webpack:///./src/app.tsx`).
    pub path: String,
    pub line: Option<u32>,
}

impl Frame {
    /// `/`-separated non-empty segments after dropping URL schemes (`webpack:///./`).
    pub fn segments(&self) -> Vec<&str> {
        let path = self.path.rsplit_once("://").map_or(self.path.as_str(), |(_, rest)| rest);
        path.split(['/', '\\']).filter(|s| !s.is_empty() && *s != "." && *s != "..").collect()
    }

    pub fn basename(&self) -> &str {
        self.segments().last().copied().unwrap_or("")
    }
}

const SOURCE_EXTENSIONS: &str = "go|ts|tsx|js|jsx|mjs|cjs|py|rs|clj|cljs|cljc|rb|java|kt|scala|cs|php|ex|exs|swift";

static FRAME: LazyLock<Option<Regex>> = LazyLock::new(|| {
    Regex::new(&format!(r"([A-Za-z0-9_@~.:/\\-]*[A-Za-z0-9_-]\.(?:{SOURCE_EXTENSIONS}))(?::(\d+))?(?::\d+)?\b")).ok()
});
static PYTHON_FRAME: LazyLock<Option<Regex>> = LazyLock::new(|| Regex::new(r#"File "([^"]+)", line (\d+)"#).ok());
static URL: LazyLock<Option<Regex>> = LazyLock::new(|| Regex::new(r#"https?://[^\s"'<>()\[\]]+"#).ok());
static PATH: LazyLock<Option<Regex>> = LazyLock::new(|| Regex::new(r#"(?:^|[\s"'(`])(/[A-Za-z0-9_.~-]+(?:/[A-Za-z0-9_.~:{}-]+)+)"#).ok());
static QUOTED: LazyLock<Option<Regex>> = LazyLock::new(|| Regex::new(r#""([^"\n]{2,200})"|'([^'\n]{2,200})'|“([^”\n]{2,200})”"#).ok());
static WORD: LazyLock<Option<Regex>> = LazyLock::new(|| Regex::new(r"[:A-Za-z_][A-Za-z0-9_.:/-]*[A-Za-z0-9_]").ok());

/// Words that mark a body line as an error report.
const ERROR_WORDS: [&str; 10] = ["error", "exception", "failed", "denied", "invalid", "not found", "timeout", "panic", "unauthorized", "fatal"];

const STOPWORDS: [&str; 64] = [
    "a", "about", "after", "all", "also", "an", "and", "any", "are", "as", "at", "be", "been", "but", "by", "can", "cannot", "could", "does",
    "doesn", "for", "from", "get", "gets", "has", "have", "how", "i", "if", "in", "into", "is", "isn", "it", "its", "not", "of", "on", "or",
    "our", "should", "some", "that", "the", "their", "then", "there", "this", "to", "user", "users", "was", "we", "when", "where", "which",
    "while", "will", "with", "without", "won", "would", "you", "your",
];

pub fn frames(text: &str) -> Vec<Frame> {
    let mut out = BTreeSet::new();
    if let Some(re) = PYTHON_FRAME.as_ref() {
        for c in re.captures_iter(text) {
            out.insert(Frame { path: c[1].to_owned(), line: c[2].parse().ok() });
        }
    }
    if let Some(re) = FRAME.as_ref() {
        for c in re.captures_iter(text) {
            let path = c[1].trim_start_matches(['(', '@']).to_owned();
            out.insert(Frame { path, line: c.get(2).and_then(|m| m.as_str().parse().ok()) });
        }
    }
    out.into_iter().collect()
}

/// `error_strings`, body lines with error words, quoted strings of ≥ 3 words.
pub fn error_lines(body: &str, error_strings: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |s: &str| {
        let s = s.trim();
        if s.len() >= 8 && !out.iter().any(|o| o == s) {
            out.push(s.to_owned());
        }
    };
    for s in error_strings {
        push(s);
    }
    for line in body.lines() {
        let lower = line.to_lowercase();
        if ERROR_WORDS.iter().any(|w| lower.contains(w)) {
            push(line);
        }
    }
    for quoted in quoted_strings(body) {
        if quoted.split_whitespace().count() >= 3 {
            push(&quoted);
        }
    }
    out
}

pub fn quoted_strings(text: &str) -> Vec<String> {
    let Some(re) = QUOTED.as_ref() else { return Vec::new() };
    re.captures_iter(text).filter_map(|c| c.get(1).or_else(|| c.get(2)).or_else(|| c.get(3)).map(|m| m.as_str().trim().to_owned())).collect()
}

/// Short UI strings (1-6 words) quoted in the text.
pub fn ui_texts(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for quoted in quoted_strings(text) {
        let words = quoted.split_whitespace().count();
        if (1..=6).contains(&words) && quoted.chars().any(char::is_alphabetic) && !out.contains(&quoted) {
            out.push(quoted);
        }
    }
    out
}

/// URL paths in the text: full URLs and absolute paths with at least two segments.
pub fn url_paths(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if let Some(re) = URL.as_ref() {
        for m in re.find_iter(text) {
            let url = m.as_str().trim_end_matches(['.', ',', ';', ':']);
            if !out.iter().any(|o| o == url) {
                out.push(url.to_owned());
            }
        }
    }
    if let Some(re) = PATH.as_ref() {
        for c in re.captures_iter(text) {
            let path = c[1].trim_end_matches(['.', ',', ';', ':']);
            let looks_like_file = path.rsplit('/').next().is_some_and(|last| last.contains('.') && !last.ends_with('.'));
            if !looks_like_file && !out.iter().any(|o| o.ends_with(path)) {
                out.push(path.to_owned());
            }
        }
    }
    out
}

/// Code identifiers: camelCase, PascalCase with an inner capital, snake_case, kebab-case, dotted,
/// `ns/name`, `:keywords`; at least 4 chars. Plain dictionary-looking words are left to the glossary.
pub fn identifiers(text: &str) -> Vec<String> {
    let Some(re) = WORD.as_ref() else { return Vec::new() };
    let mut out: Vec<String> = Vec::new();
    for m in re.find_iter(text) {
        let word = m.as_str().trim_end_matches(['.', ':', '/', '-']);
        let inside_path = text[..m.start()].ends_with('/');
        if word.len() < 4 || word.contains("://") || inside_path {
            continue;
        }
        let chars: Vec<char> = word.chars().collect();
        let inner_capital = chars.windows(2).any(|w| w[0].is_lowercase() && w[1].is_uppercase());
        let joined = |sep: char| chars.windows(3).any(|w| w[1] == sep && w[0].is_alphanumeric() && w[2].is_alphanumeric());
        let structured = inner_capital || joined('_') || joined('-') || joined('.') || joined('/') || (word.starts_with(':') && word.len() > 4);
        if structured && !out.iter().any(|o| o == word) {
            out.push(word.to_owned());
        }
    }
    out
}

/// Title nouns for the glossary stage: title words minus stopwords and identifiers, singular.
pub fn nouns(title: &str, identifiers: &[String]) -> Vec<String> {
    let taken: BTreeSet<String> = identifiers.iter().flat_map(|i| glossary_words(i)).collect();
    let mut out: Vec<String> = Vec::new();
    for word in title.split(|c: char| !c.is_alphanumeric()) {
        let lower = word.to_lowercase();
        if lower.len() < 3 || lower.chars().all(|c| c.is_ascii_digit()) || STOPWORDS.contains(&lower.as_str()) {
            continue;
        }
        let noun = singular(&lower);
        if !taken.contains(&noun) && !out.contains(&noun) {
            out.push(noun);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_from_common_trace_formats() {
        let text = "panic: boom\n\tgoroutine 1\n\t/app/gateway/api/conn.go:123 +0x1f\n\
                    at fetchConnections (webpack:///./src/webapp/api.ts:45:12)\n\
                    File \"/srv/app/views.py\", line 12, in handler\n\
                    webapp/connections/events.cljs:88";
        let found = frames(text);
        let has = |path: &str, line: Option<u32>| found.iter().any(|f| f.path == path && f.line == line);
        assert!(has("/app/gateway/api/conn.go", Some(123)), "{found:?}");
        assert!(found.iter().any(|f| f.basename() == "api.ts" && f.line == Some(45)), "{found:?}");
        assert!(has("/srv/app/views.py", Some(12)), "{found:?}");
        assert!(has("webapp/connections/events.cljs", Some(88)), "{found:?}");
        let webpack = found.iter().find(|f| f.basename() == "api.ts").map(Frame::segments).unwrap_or_default();
        assert_eq!(webpack, vec!["src", "webapp", "api.ts"], "scheme and ./ fall away");
    }

    #[test]
    fn error_lines_keep_reports_and_long_quotes() {
        let body = "Steps: open the page\nThen I get Error: connection refused\nThe toast said \"could not save the connection\" twice";
        let lines = error_lines(body, &["pq: password authentication failed".to_owned()]);
        assert_eq!(lines[0], "pq: password authentication failed");
        assert!(lines.contains(&"Then I get Error: connection refused".to_owned()));
        assert!(lines.contains(&"could not save the connection".to_owned()));
        assert!(!lines.iter().any(|l| l.starts_with("Steps")));
    }

    #[test]
    fn urls_identifiers_ui_texts_and_nouns() {
        let text = "Clicking \"Save\" on https://app.hoop.dev/connections/abc/edit calls /api/connections/abc and fails in \
                    reviewService or review_requests or :connections/get or http.Error";
        let urls = url_paths(text);
        assert!(urls.contains(&"https://app.hoop.dev/connections/abc/edit".to_owned()), "{urls:?}");
        assert!(urls.contains(&"/api/connections/abc".to_owned()), "{urls:?}");
        let ids = identifiers(text);
        for want in ["reviewService", "review_requests", ":connections/get", "http.Error"] {
            assert!(ids.contains(&want.to_owned()), "{want} in {ids:?}");
        }
        assert!(!ids.contains(&"Clicking".to_owned()));
        assert_eq!(ui_texts(text), vec!["Save".to_owned()]);
        assert_eq!(nouns("Users cannot save connections after review", &[]), vec!["save", "connection", "review"]);
        assert_eq!(nouns("Review fails", &["reviewService".to_owned()]), vec!["fail"], "identifier words are not repeated");
        assert!(!ids.iter().any(|i| i.starts_with("api/")), "URL paths are not identifiers: {ids:?}");
    }
}
