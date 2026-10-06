//! Pure text helpers of the query tools: qualified-name splitting, glossary words, literal runs
//! for degraded searches, language globs, line cutting.

use mage_core::domain::Language;
use mage_core::domain::knowledge::{RoutePattern, RouteSegment};

/// `pkg.Func`, `Type.Method`, `ns/fn`, `Class::method` → (`Some(qualifier)`, name), the qualifier
/// being the segment right before the name. Keywords (`:ns/kw`) and plain names stay whole.
pub fn split_qualified(input: &str) -> (Option<String>, String) {
    let input = input.trim();
    if input.starts_with(':') {
        return (None, input.to_owned());
    }
    let separators = ["::", "/", "."];
    let mut best: Option<(usize, usize)> = None;
    for sep in separators {
        if let Some(at) = input.rfind(sep) {
            if best.is_none_or(|(b, _)| at > b) {
                best = Some((at, sep.len()));
            }
        }
    }
    let Some((at, len)) = best else { return (None, input.to_owned()) };
    let (head, name) = (&input[..at], &input[at + len..]);
    if name.is_empty() || head.is_empty() {
        return (None, input.to_owned());
    }
    let qualifier = head.rsplit(['.', '/', ':']).find(|s| !s.is_empty()).unwrap_or(head);
    (Some(qualifier.to_owned()), name.to_owned())
}

/// Glossary words of a term: split on camelCase, `_`, `-`, `::`, `/`, `.`, whitespace;
/// lowercased; a plural `s` stripped (`connections` → `connection`); words < 2 chars dropped.
pub fn glossary_words(term: &str) -> Vec<String> {
    let mut words = Vec::new();
    for chunk in term.split(|c: char| !c.is_alphanumeric()) {
        let mut current = String::new();
        let chars: Vec<char> = chunk.chars().collect();
        for (i, c) in chars.iter().enumerate() {
            let boundary = i > 0
                && c.is_uppercase()
                && (chars[i - 1].is_lowercase() || chars[i - 1].is_ascii_digit() || chars.get(i + 1).is_some_and(|n| n.is_lowercase()));
            if boundary && !current.is_empty() {
                words.push(std::mem::take(&mut current));
            }
            current.extend(c.to_lowercase());
        }
        if !current.is_empty() {
            words.push(current);
        }
    }
    let mut out: Vec<String> = words.into_iter().filter(|w| w.chars().count() >= 2).map(|w| singular(&w)).collect();
    out.dedup();
    out
}

/// `connections` → `connection`; `status`, `class`, short words unchanged.
pub fn singular(word: &str) -> String {
    if word.len() > 3 && word.ends_with('s') && !word.ends_with("ss") && !word.ends_with("us") && !word.ends_with("is") {
        word[..word.len() - 1].to_owned()
    } else {
        word.to_owned()
    }
}

/// Whether a word of an error message is a value rather than literal text: digits, hex ids,
/// uuids, quoted values.
fn is_variable_word(word: &str) -> bool {
    let core = word.trim_matches(|c: char| !c.is_alphanumeric());
    if core.is_empty() {
        return false;
    }
    core.chars().any(|c| c.is_ascii_digit()) || word.starts_with('"') || word.starts_with('\'') || word.contains('=')
}

/// knowledge.md `lookup_error` degraded path: the longest run of consecutive literal words of
/// the input (values with digits, hex, uuids, quoted strings removed), at least `min_chars`
/// long, trimmed of edge punctuation; verbatim from the input so a literal search can find it.
pub fn longest_literal_run(input: &str, min_chars: usize) -> Option<String> {
    let mut runs: Vec<&str> = Vec::new();
    for line in input.lines() {
        let mut run: Option<(usize, usize)> = None;
        let mut offset = 0;
        for word in line.split(' ') {
            let (start, end) = (offset, offset + word.len());
            offset = end + 1;
            if word.is_empty() {
                continue;
            }
            if is_variable_word(word) {
                runs.extend(run.take().map(|(s, e)| &line[s..e]));
            } else {
                run = Some((run.map_or(start, |(s, _)| s), end));
            }
        }
        runs.extend(run.map(|(s, e)| &line[s..e]));
    }
    runs.into_iter()
        .map(|run| run.trim_matches(|c: char| !c.is_alphanumeric()))
        .fold(None::<&str>, |best, run| if best.is_none_or(|b| run.len() > b.len()) { Some(run) } else { best })
        .filter(|b| b.chars().count() >= min_chars)
        .map(str::to_owned)
}

/// knowledge.md `route_info` degraded path: the longest run of consecutive literal segments
/// (`/api/connections/{}/edit` → `/api/connections`). Segments holding a digit (ids, hashes,
/// uuids in concrete URLs) count as parameters.
pub fn longest_static_run(pattern: &RoutePattern) -> Option<String> {
    let mut best = String::new();
    let mut current = String::new();
    for segment in pattern.segments() {
        match segment {
            RouteSegment::Lit(text) if !text.chars().any(|c| c.is_ascii_digit()) => {
                current.push('/');
                current.push_str(text);
                if current.len() > best.len() {
                    best = current.clone();
                }
            }
            RouteSegment::Lit(_) | RouteSegment::Param => current.clear(),
        }
    }
    (!best.is_empty()).then_some(best)
}

/// Path globs of a language's files (search_code `lang` filter).
pub fn language_globs(lang: Language) -> &'static [&'static str] {
    match lang {
        Language::Go => &["**/*.go"],
        Language::TypeScript => &["**/*.ts", "**/*.mts", "**/*.cts"],
        Language::Tsx => &["**/*.tsx"],
        Language::JavaScript => &["**/*.js", "**/*.jsx", "**/*.mjs", "**/*.cjs"],
        Language::Python => &["**/*.py"],
        Language::Rust => &["**/*.rs"],
        Language::Java => &["**/*.java"],
        Language::Ruby => &["**/*.rb"],
        Language::Clojure => &["**/*.clj"],
        Language::ClojureScript => &["**/*.cljs"],
        Language::Cljc => &["**/*.cljc"],
        Language::Sql => &["**/*.sql"],
        Language::Json => &["**/*.json"],
        Language::Yaml => &["**/*.yml", "**/*.yaml"],
        Language::Toml => &["**/*.toml"],
        Language::Markdown => &["**/*.md"],
        Language::Html => &["**/*.html", "**/*.htm"],
        Language::Css => &["**/*.css", "**/*.scss"],
        Language::Shell => &["**/*.sh", "**/*.bash"],
        Language::Proto => &["**/*.proto"],
        Language::Other => &[],
    }
}

/// knowledge.md `read_file`: lines longer than `max` chars end with `…[+N chars]`.
pub fn cut_line(line: &str, max: usize) -> String {
    let count = line.chars().count();
    if count <= max {
        return line.to_owned();
    }
    let kept: String = line.chars().take(max).collect();
    format!("{kept}…[+{} chars]", count - max)
}

/// First `max` chars of `text`.
pub fn head(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qualified_names_split_on_the_last_separator() {
        assert_eq!(split_qualified("pkg.Func"), (Some("pkg".into()), "Func".into()));
        assert_eq!(split_qualified("api.Server.Connect"), (Some("Server".into()), "Connect".into()));
        assert_eq!(split_qualified("webapp.events/fetch"), (Some("events".into()), "fetch".into()));
        assert_eq!(split_qualified("Conn::open"), (Some("Conn".into()), "open".into()));
        assert_eq!(split_qualified(":connections/get"), (None, ":connections/get".into()));
        assert_eq!(split_qualified("Connect"), (None, "Connect".into()));
    }

    #[test]
    fn glossary_words_split_case_and_separators() {
        assert_eq!(glossary_words("ConnectionStatus"), vec!["connection", "status"]);
        assert_eq!(glossary_words("api_keys/HTTPServer"), vec!["api", "key", "http", "server"]);
        assert_eq!(glossary_words("review-requests"), vec!["review", "request"]);
    }

    #[test]
    fn literal_runs_skip_values() {
        let line = "level=error msg=x failed fetching connection pg-prod-2: timeout after 30s";
        assert_eq!(longest_literal_run(line, 12).as_deref(), Some("failed fetching connection"));
        assert_eq!(longest_literal_run("id 42 not ok", 12), None);
    }

    #[test]
    fn static_runs_of_routes() {
        let pattern = RoutePattern::from_url("https://app.example.com/api/connections/abc123/edit?x=1");
        // `abc123` holds a digit: an id, not route text.
        assert_eq!(longest_static_run(&pattern).as_deref(), Some("/api/connections"));
        let canonical: RoutePattern = "/api/connections/{}/edit".parse().unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(longest_static_run(&canonical).as_deref(), Some("/api/connections"));
    }

    #[test]
    fn long_lines_are_cut_with_a_marker() {
        assert_eq!(cut_line("abcdef", 4), "abcd…[+2 chars]");
        assert_eq!(cut_line("abc", 4), "abc");
    }
}
