//! Error-site templates → regex patterns and literal tokens (knowledge.md "Layer 3: Error sites",
//! "Template" and "Pattern").
//!
//! The indexer runs [`to_pattern`] on every classified literal and catalog message; the server runs
//! [`tokenize`] on `lookup_error` input and matches the stored patterns. Both are versioned by
//! `ErrorsRev`: any change to their output needs an `ErrorsRev` bump in the indexer.

use std::borrow::Cow;
use std::fmt;

use serde::{Deserialize, Serialize};

/// Sites with fewer non-whitespace literal chars are low-specificity.
const MIN_SPECIFICITY: u32 = 8;
/// Sites with fewer literal tokens are low-specificity.
const MIN_TOKENS: usize = 2;

/// Class of a template placeholder; the classifier maps each format convention onto these
/// (knowledge.md "Template" table).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VarClass {
    /// `%s %v %+v %x %T`, `{}`, `{name}`, interpolation holes, ICU placeholders → `.+?`.
    Any,
    /// `%d %i` → `-?\d+`.
    Int,
    /// `%f %g %e` → `-?\d+(?:\.\d+)?`.
    Float,
    /// `%q` → `"[^"]*"`.
    Quoted,
    /// `%w`: the rest of an error chain → `.*`.
    Wrapped,
}

impl VarClass {
    fn regex(self) -> &'static str {
        match self {
            Self::Any => ".+?",
            Self::Int => r"-?\d+",
            Self::Float => r"-?\d+(?:\.\d+)?",
            Self::Quoted => r#""[^"]*""#,
            Self::Wrapped => ".*",
        }
    }

    /// Placeholder in the template display form (`ErrorSiteRecord.template`).
    fn placeholder(self) -> &'static str {
        match self {
            Self::Any => "{}",
            Self::Int => "{int}",
            Self::Float => "{float}",
            Self::Quoted => "{quoted}",
            Self::Wrapped => "{w}",
        }
    }

    /// Every rendering is non-empty (a number, `""`, an error cause), so the separator text of
    /// the neighbouring literal is always followed/preceded by it in real output.
    fn renders_nonempty(self) -> bool {
        !matches!(self, Self::Any)
    }
}

/// One template segment: literal text or a placeholder (`Seg = Lit(String) | Var(VarClass)`).
#[derive(Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum Seg {
    Lit(String),
    Var(VarClass),
}

/// A message template as rewritten by the classifier from a literal site or catalog entry.
///
/// `Display` renders the display form stored in `ErrorSiteRecord.template`: literal text verbatim,
/// placeholders as `{}`, `{int}`, `{float}`, `{quoted}`, `{w}`.
#[derive(Clone, PartialEq, Eq, Hash, Debug, Default, Serialize, Deserialize)]
pub struct Template {
    pub segs: Vec<Seg>,
}

impl fmt::Display for Template {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for seg in &self.segs {
            match seg {
                Seg::Lit(text) => f.write_str(text)?,
                Seg::Var(class) => f.write_str(class.placeholder())?,
            }
        }
        Ok(())
    }
}

/// Output of [`to_pattern`]; the indexer copies it into `ErrorSiteRecord`/`CatalogEntryRecord`.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct ErrorPatternDraft {
    /// Rust `regex` syntax, unanchored.
    pub pattern: String,
    /// Lowercased literal words, sorted, deduped; every one appears in [`tokenize`] of any
    /// rendering of the template (the store's `tokens <@ input_tokens` candidate filter).
    pub tokens: Vec<String>,
    /// Non-whitespace literal chars.
    pub specificity: u32,
    /// `specificity < 8` or fewer than 2 tokens.
    pub low_specificity: bool,
}

/// Lowercased words of `text`: maximal runs of ASCII alphanumerics (every other char separates),
/// at least 2 chars, not all digits; sorted, deduped.
pub fn tokenize(text: &str) -> Vec<String> {
    let mut tokens: Vec<String> =
        words(text).map(|(start, end)| &text[start..end]).filter(|word| is_token(word)).map(str::to_ascii_lowercase).collect();
    tokens.sort_unstable();
    tokens.dedup();
    tokens
}

/// Build the matching pattern of a template (knowledge.md "Pattern", steps 1-6).
///
/// 1. Adjacent literals are joined, empty ones dropped, adjacent placeholders merged into one
///    `Any`. Everything before the first and after the last literal holding non-whitespace text is
///    dropped (placeholders and whitespace-only literals at the edges).
/// 2. Literals: regex metacharacters escaped, whitespace runs → `\s+`. At a pattern edge the
///    literal's outer whitespace is trimmed, except next to a dropped placeholder that always
///    renders text (`Int`, `Float`, `Quoted`, `Wrapped`): there the separator is always present
///    in real output and stays. A dropped `Any` may render empty, and logs trim trailing blanks.
/// 3. Placeholders between literals: `Any` → `.+?`, `Int` → `-?\d+`, `Float` →
///    `-?\d+(?:\.\d+)?`, `Quoted` → `"[^"]*"`, `Wrapped` → `.*`.
/// 4. `\b` before the first / after the last literal when it starts / ends with a word char that
///    cannot be glued to a dropped placeholder's text. No `^`/`$`.
/// 5. Tokens: [`tokenize`] words of the literals, skipping word fragments glued to a placeholder
///    (they merge with the rendered value in real output).
/// 6. `low_specificity` when `specificity < 8` or fewer than 2 tokens.
///
/// `None` when no literal holds non-whitespace text: such a template matches anything.
pub fn to_pattern(template: &Template) -> Option<ErrorPatternDraft> {
    let pieces = normalize(&template.segs);
    let first = pieces.iter().position(Piece::has_text)?;
    let last = pieces.iter().rposition(Piece::has_text)?;
    let lead = first.checked_sub(1).and_then(|i| pieces.get(i)).and_then(Piece::var_class);
    let trail = pieces.get(last + 1).and_then(Piece::var_class);
    let keep_lead_ws = lead.is_some_and(VarClass::renders_nonempty);
    let keep_trail_ws = trail.is_some_and(VarClass::renders_nonempty);

    let capacity = pieces
        .iter()
        .map(|piece| match piece {
            Piece::Lit(text) => text.len() + 4,
            Piece::Var(class) => class.regex().len(),
        })
        .sum::<usize>()
        + 4;
    let mut pattern = String::with_capacity(capacity);
    let mut tokens = Vec::new();
    let mut specificity: u32 = 0;
    let mut suffix_boundary = false;

    for (index, piece) in pieces.iter().enumerate().take(last + 1).skip(first) {
        let lit = match piece {
            Piece::Var(class) => {
                pattern.push_str(class.regex());
                continue;
            }
            Piece::Lit(lit) => &**lit,
        };
        let preceded_by_var = index > 0;
        let followed_by_var = index + 1 < pieces.len();

        let mut text = lit;
        if index == first {
            let mut trimmed = false;
            if !keep_lead_ws {
                let rest = text.trim_start();
                trimmed = rest.len() < text.len();
                text = rest;
            }
            let starts_word = text.chars().next().is_some_and(is_word_char);
            if starts_word && (lead.is_none() || trimmed) {
                pattern.push_str(r"\b");
            }
        }
        if index == last {
            let mut trimmed = false;
            if !keep_trail_ws {
                let rest = text.trim_end();
                trimmed = rest.len() < text.len();
                text = rest;
            }
            let ends_word = text.chars().next_back().is_some_and(is_word_char);
            suffix_boundary = ends_word && (trail.is_none() || trimmed);
        }
        push_literal(&mut pattern, text);

        for (start, end) in words(lit) {
            let glued = (start == 0 && preceded_by_var) || (end == lit.len() && followed_by_var);
            let word = &lit[start..end];
            if !glued && is_token(word) {
                tokens.push(word.to_ascii_lowercase());
            }
        }
        let chars = lit.chars().filter(|c| !c.is_whitespace()).count();
        specificity = specificity.saturating_add(u32::try_from(chars).unwrap_or(u32::MAX));
    }
    if suffix_boundary {
        pattern.push_str(r"\b");
    }

    tokens.sort_unstable();
    tokens.dedup();
    let low_specificity = specificity < MIN_SPECIFICITY || tokens.len() < MIN_TOKENS;
    Some(ErrorPatternDraft { pattern, tokens, specificity, low_specificity })
}

/// A template segment after step 1 normalization: literals and placeholders strictly alternate.
enum Piece<'a> {
    Lit(Cow<'a, str>),
    Var(VarClass),
}

impl Piece<'_> {
    fn has_text(&self) -> bool {
        matches!(self, Piece::Lit(text) if text.chars().any(|c| !c.is_whitespace()))
    }

    fn var_class(&self) -> Option<VarClass> {
        match self {
            Piece::Var(class) => Some(*class),
            Piece::Lit(_) => None,
        }
    }
}

fn normalize(segs: &[Seg]) -> Vec<Piece<'_>> {
    let mut out: Vec<Piece<'_>> = Vec::with_capacity(segs.len());
    for seg in segs {
        match seg {
            Seg::Lit(text) if text.is_empty() => {}
            Seg::Lit(text) => match out.last_mut() {
                Some(Piece::Lit(prev)) => prev.to_mut().push_str(text),
                _ => out.push(Piece::Lit(Cow::Borrowed(text))),
            },
            Seg::Var(class) => match out.last_mut() {
                Some(Piece::Var(prev)) => *prev = VarClass::Any,
                _ => out.push(Piece::Var(*class)),
            },
        }
    }
    out
}

/// Escaped literal with whitespace runs collapsed to `\s+`.
fn push_literal(out: &mut String, text: &str) {
    let mut in_space = false;
    for c in text.chars() {
        if c.is_whitespace() {
            if !in_space {
                out.push_str(r"\s+");
                in_space = true;
            }
            continue;
        }
        in_space = false;
        if is_meta_character(c) {
            out.push('\\');
        }
        out.push(c);
    }
}

/// `regex_syntax::is_meta_character` (regex-syntax 0.8): the chars `regex::escape` escapes.
fn is_meta_character(c: char) -> bool {
    matches!(c, '\\' | '.' | '+' | '*' | '?' | '(' | ')' | '|' | '[' | ']' | '{' | '}' | '^' | '$' | '#' | '&' | '-' | '~')
}

/// Conservative subset of the `regex` crate's Unicode `\w`: every char accepted here is a word
/// char for `\b`, so an emitted `\b` never rejects a rendering. (Alphabetic ⊂ `\w`; other `\w`
/// chars just get no `\b`.)
fn is_word_char(c: char) -> bool {
    c == '_' || c.is_ascii_digit() || c.is_alphabetic()
}

/// Byte ranges of maximal ASCII-alphanumeric runs. Non-ASCII bytes are never alphanumeric, so the
/// ranges are always char boundaries.
fn words(text: &str) -> impl Iterator<Item = (usize, usize)> + '_ {
    let bytes = text.as_bytes();
    let mut i = 0;
    std::iter::from_fn(move || {
        while i < bytes.len() && !bytes[i].is_ascii_alphanumeric() {
            i += 1;
        }
        if i == bytes.len() {
            return None;
        }
        let start = i;
        while i < bytes.len() && bytes[i].is_ascii_alphanumeric() {
            i += 1;
        }
        Some((start, i))
    })
}

fn is_token(word: &str) -> bool {
    word.len() >= 2 && !word.bytes().all(|b| b.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use regex::Regex;

    use super::*;
    use Seg::{Lit, Var};
    use VarClass::{Any, Float, Int, Quoted, Wrapped};

    fn lit(text: &str) -> Seg {
        Lit(text.to_owned())
    }

    fn template(segs: Vec<Seg>) -> Template {
        Template { segs }
    }

    fn draft(segs: Vec<Seg>) -> ErrorPatternDraft {
        to_pattern(&template(segs)).unwrap()
    }

    fn is_subset(small: &[String], big: &[String]) -> bool {
        small.iter().all(|t| big.contains(t))
    }

    /// knowledge.md "Pattern" examples: (template, display form, pattern, matching input).
    #[test]
    fn doc_examples_produce_documented_patterns_that_match() {
        let cases: Vec<(Vec<Seg>, &str, &str, &str)> = vec![
            (
                // Go fmt.Errorf("failed fetching connection %s: %w", name, err)
                vec![lit("failed fetching connection "), Var(Any), lit(": "), Var(Wrapped)],
                "failed fetching connection {}: {w}",
                r"\bfailed\s+fetching\s+connection\s+.+?:\s+",
                r#"level=error msg="failed fetching connection pg-prod: context deadline exceeded""#,
            ),
            (
                // Go http.Error(w, "session expired", http.StatusUnauthorized)
                vec![lit("session expired")],
                "session expired",
                r"\bsession\s+expired\b",
                "401 Unauthorized: session expired",
            ),
            (
                // TS throw new Error(`Connection ${id} not found`)
                vec![lit("Connection "), Var(Any), lit(" not found")],
                "Connection {} not found",
                r"\bConnection\s+.+?\s+not\s+found\b",
                "Uncaught Error: Connection 42 not found",
            ),
            (
                // Python logger.error("retry %d/%d failed for %s", n, m, host)
                vec![lit("retry "), Var(Int), lit("/"), Var(Int), lit(" failed for "), Var(Any)],
                "retry {int}/{int} failed for {}",
                r"\bretry\s+-?\d+/-?\d+\s+failed\s+for\b",
                "ERROR retry 3/5 failed for db-1",
            ),
            (
                // Rust #[error("invalid token: {0}")]
                vec![lit("invalid token: "), Var(Any)],
                "invalid token: {}",
                r"\binvalid\s+token:",
                "Error: invalid token: expired",
            ),
            (
                // CLJS (ex-info (str "Plugin " plugin-name " is not enabled") {})
                vec![lit("Plugin "), Var(Any), lit(" is not enabled")],
                "Plugin {} is not enabled",
                r"\bPlugin\s+.+?\s+is\s+not\s+enabled\b",
                "Plugin slack is not enabled",
            ),
            (
                // catalog "errors.conn_not_found": "Connection {{name}} not found"
                vec![lit("Connection "), Var(Any), lit(" not found")],
                "Connection {} not found",
                r"\bConnection\s+.+?\s+not\s+found\b",
                "Connection prod-db not found",
            ),
        ];
        for (segs, display, expected, input) in cases {
            let template = template(segs);
            assert_eq!(template.to_string(), display);
            let draft = to_pattern(&template).unwrap();
            assert_eq!(draft.pattern, expected, "template {display}");
            let re = Regex::new(&draft.pattern).unwrap();
            assert!(re.is_match(input), "{expected} vs {input}");
            assert!(is_subset(&draft.tokens, &tokenize(input)), "{:?} ⊄ {input}", draft.tokens);
        }
    }

    #[test]
    fn tokens_specificity_and_low_specificity_boundaries() {
        let d = draft(vec![lit("failed fetching connection "), Var(Any), lit(": "), Var(Wrapped)]);
        assert_eq!(d.tokens, ["connection", "failed", "fetching"]);
        assert_eq!(d.specificity, 25);
        assert!(!d.low_specificity);

        // Digits-only and single-char words are not tokens; specificity still counts them.
        let d = draft(vec![lit("retry "), Var(Int), lit("/"), Var(Int), lit(" failed for 2 x"), Var(Any)]);
        assert_eq!(d.tokens, ["failed", "for", "retry"]);
        assert_eq!(d.specificity, 5 + 1 + 6 + 3 + 1 + 1);

        // 7 non-whitespace chars → low; 8 → not low.
        assert!(draft(vec![lit("abc defg")]).low_specificity);
        let d = draft(vec![lit("abcd efgh")]);
        assert_eq!((d.specificity, d.low_specificity), (8, false));
        // One token → low however long.
        let d = draft(vec![lit("unauthorized!!")]);
        assert_eq!((d.tokens.len(), d.low_specificity), (1, true));
        // Repeated words dedupe before counting.
        assert!(draft(vec![lit("timeout timeout")]).low_specificity);
    }

    #[test]
    fn adjacent_segments_are_normalized() {
        // Adjacent placeholders (also across an empty literal) merge into one `Any`.
        assert_eq!(draft(vec![lit("code "), Var(Int), Var(Float), lit(" rejected now")]).pattern, r"\bcode\s+.+?\s+rejected\s+now\b");
        assert_eq!(
            draft(vec![lit("code "), Var(Int), lit(""), Var(Quoted), lit(" rejected now")]).pattern,
            r"\bcode\s+.+?\s+rejected\s+now\b"
        );
        // A single placeholder keeps its class.
        assert_eq!(draft(vec![lit("code "), Var(Quoted), lit(" rejected now")]).pattern, r#"\bcode\s+"[^"]*"\s+rejected\s+now\b"#);
        // Adjacent literals join; a whitespace run across them is one `\s+`.
        assert_eq!(draft(vec![lit("session "), lit(" expired")]).pattern, r"\bsession\s+expired\b");
    }

    #[test]
    fn edges_drop_placeholders_and_whitespace_only_literals() {
        // Leading and trailing placeholders are dropped; Any-adjacent edge whitespace is trimmed.
        assert_eq!(draft(vec![Var(Any), lit(" connection refused "), Var(Any)]).pattern, r"\bconnection\s+refused\b");
        // Whitespace-only edge literals carry nothing: the placeholders behind them are edges
        // too. The kept literal's neighbour is `Any`, so its leading space is trimmed.
        assert_eq!(draft(vec![Var(Int), lit(" "), Var(Any), lit(" connection refused")]).pattern, r"\bconnection\s+refused\b");
        // Same shape with an `Int` neighbour keeps the separator.
        assert_eq!(draft(vec![Var(Any), lit(" "), Var(Int), lit(" connection refused")]).pattern, r"\s+connection\s+refused\b");
        // Whitespace-only literals between kept literals stay as `\s+`.
        assert_eq!(draft(vec![lit("a-b "), Var(Int), lit(" "), Var(Int), lit(" c-d")]).pattern, r"\ba\-b\s+-?\d+\s+-?\d+\s+c\-d\b");
        // Message-edge whitespace without a placeholder is trimmed too.
        assert_eq!(draft(vec![lit("  session expired\n")]).pattern, r"\bsession\s+expired\b");
    }

    #[test]
    fn separator_next_to_a_dropped_nonempty_placeholder_is_kept() {
        assert_eq!(draft(vec![lit("retry in "), Var(Int)]).pattern, r"\bretry\s+in\s+");
        assert_eq!(draft(vec![lit("retry in "), Var(Any)]).pattern, r"\bretry\s+in\b");
        assert_eq!(draft(vec![Var(Quoted), lit(" is not a valid name")]).pattern, r"\s+is\s+not\s+a\s+valid\s+name\b");
    }

    #[test]
    fn literal_glued_to_a_dropped_placeholder_gets_no_boundary_and_no_fragment_token() {
        let d = draft(vec![Var(Any), lit("Error: boom failed")]);
        assert_eq!(d.pattern, r"Error:\s+boom\s+failed\b");
        assert_eq!(d.tokens, ["boom", "failed"]);
        let re = Regex::new(&d.pattern).unwrap();
        assert!(re.is_match("XError: boom failed"));

        let d = draft(vec![lit("session expired at"), Var(Int)]);
        assert_eq!(d.pattern, r"\bsession\s+expired\s+at");
        assert_eq!(d.tokens, ["expired", "session"]);
        assert!(Regex::new(&d.pattern).unwrap().is_match("session expired at1700000000"));
    }

    #[test]
    fn metacharacters_are_escaped() {
        let text = r"cost $5.00 (max) [a|b] {x} ^~&#-*+? \ end";
        let d = draft(vec![lit(text)]);
        assert_eq!(d.pattern, r"\bcost\s+\$5\.00\s+\(max\)\s+\[a\|b\]\s+\{x\}\s+\^\~\&\#\-\*\+\?\s+\\\s+end\b");
        let re = Regex::new(&d.pattern).unwrap();
        assert!(re.is_match(text));
        assert!(!re.is_match("cost $5000 (max) [a|b] {x} ^~&#-*+? \\ end"));
    }

    #[test]
    fn templates_without_literal_text_have_no_pattern() {
        assert_eq!(to_pattern(&template(vec![])), None);
        assert_eq!(to_pattern(&template(vec![Var(Any)])), None);
        assert_eq!(to_pattern(&template(vec![Var(Any), lit("  "), Var(Int)])), None);
        assert_eq!(to_pattern(&template(vec![lit(" \t\n")])), None);
    }

    #[test]
    fn display_uses_one_placeholder_per_class() {
        let t =
            template(vec![lit("a "), Var(Any), lit(" "), Var(Int), lit(" "), Var(Float), lit(" "), Var(Quoted), lit(": "), Var(Wrapped)]);
        assert_eq!(t.to_string(), "a {} {int} {float} {quoted}: {w}");
    }

    #[test]
    fn tokenize_cases() {
        assert_eq!(
            tokenize(r#"level=error msg="failed fetching connection pg-prod: context deadline exceeded""#),
            ["connection", "context", "deadline", "error", "exceeded", "failed", "fetching", "level", "msg", "pg", "prod"]
        );
        // Single chars and digit-only runs dropped; mixed alphanumerics kept; lowercased, deduped.
        assert_eq!(tokenize("ERROR retry 3/5 failed for db-1 Retry x 0xFF"), ["0xff", "db", "error", "failed", "for", "retry"]);
        // Underscore and non-ASCII chars separate words.
        assert_eq!(tokenize("conn_not_found café"), ["caf", "conn", "found", "not"]);
        assert!(tokenize("").is_empty());
        assert!(tokenize("1 22 333 - a").is_empty());
    }

    /// Deterministic xorshift64* for render-and-match checks.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }

        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }

        /// No `"` (Quoted cannot hold it) and no newline (`.` does not match it).
        fn text(&mut self, min: u64, max: u64) -> String {
            const CHARS: &[char] =
                &['a', 'Z', '0', '9', ' ', '\t', '-', '_', ':', '.', '/', '\'', '(', ']', '{', '$', '^', '*', '\\', 'é', '漢'];
            let len = min + self.below(max - min + 1);
            (0..len).map(|_| CHARS[self.below(CHARS.len() as u64) as usize]).collect()
        }

        fn render(&mut self, class: VarClass) -> String {
            match class {
                Any => self.text(1, 12),
                Int => (self.next() as i64 >> self.below(63)).to_string(),
                Float => {
                    let whole = (self.next() as i64 >> (20 + self.below(43))).to_string();
                    match self.below(3) {
                        0 => whole,
                        _ => format!("{whole}.{}", self.below(100_000)),
                    }
                }
                Quoted => format!("\"{}\"", self.text(0, 8)),
                Wrapped => self.text(0, 16),
            }
        }
    }

    /// knowledge.md Testing: a template rendered with random values per `VarClass` matches its
    /// pattern; patterns compile; literal tokens are a subset of the rendering's tokens.
    #[test]
    fn rendered_templates_match_their_pattern_for_every_class() {
        let shapes: [fn(VarClass) -> Vec<Seg>; 8] = [
            |c| vec![lit("connection "), Var(c), lit(" not found")],
            |c| vec![lit("id="), Var(c), lit(";done")],
            |c| vec![lit("user"), Var(c), lit("name lookup failed")],
            |c| vec![Var(c), lit(" happened at "), Var(c)],
            |c| vec![Var(c), lit("Error: retry "), Var(c), lit(" of task "), Var(Any), Var(c)],
            |c| vec![lit("x "), Var(c), Var(Any), lit(" y z")],
            |c| vec![lit("value "), Var(c), lit(" "), Var(c), lit(" out of range")],
            |c| vec![lit("failed: "), Var(c)],
        ];
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        for class in [Any, Int, Float, Quoted, Wrapped] {
            for shape in &shapes {
                let template = template(shape(class));
                let draft = to_pattern(&template).unwrap();
                let re = Regex::new(&draft.pattern).unwrap();
                for _ in 0..200 {
                    let rendered: String = template
                        .segs
                        .iter()
                        .map(|seg| match seg {
                            Lit(text) => text.clone(),
                            Var(c) => rng.render(*c),
                        })
                        .collect();
                    assert!(re.is_match(&rendered), "{class:?} {} /{}/ vs {rendered:?}", template, draft.pattern);
                    assert!(is_subset(&draft.tokens, &tokenize(&rendered)), "{:?} ⊄ tokens of {rendered:?}", draft.tokens);
                }
            }
        }
    }
}
