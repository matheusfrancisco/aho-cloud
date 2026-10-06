//! Error string extraction (intake.md 3.2): pure heuristics that pull the error messages a ticket
//! quotes out of its description, kept comments and linked Slack thread.
//!
//! Ranks, best first:
//!
//! 1. Exception/panic headers: `…Error:` / `…Exception(` (from the class name to the end of the
//!    line, or to the end of the enclosing quote), `panicked at` (the line; with the Rust ≥ 1.73
//!    layout `panicked at file:line:col:` the following line), `Traceback (most recent call last)`
//!    (the following non-frame line), a leading Go `panic: `, `FATAL` (as a log level: the rest of
//!    the line after timestamp and logger; elsewhere: from `FATAL` on) and `Unhandled…`.
//! 2. Error-level log lines: logfmt `level=error msg="…"` → `msg`; JSON lines with
//!    `"level":"error"` → `message` / `msg` / `error`; `ERROR ` / `[error]` prefixed lines → the rest
//!    of the line after timestamp and logger.
//! 3. HTTP failures: each 4xx/5xx status in the same line as `METHOD /path` (outside it) →
//!    `"{status} {method} {path}"` (path without query), else the first status with a status
//!    phrase → `"{status} {phrase}"`. Structured log lines also match through their `status` /
//!    `method` / `path` fields. Amounts, versions and ids (`$500`, `1,500`, `404.2`) are not statuses.
//! 4. Quoted UI text: 12–200 chars inside backticks or straight/typographic quotes, containing a
//!    letter, not a path, URL, e-mail address or HTTP request line.
//!
//! Every line is scanned the same way, inside fenced or indented code blocks too; blockquote and
//! list markers are stripped. Stack frames (`at …`, `File "…", line N` plus the source line under
//! it, Go frames) are skipped: they point at code, not at messages. Strings are kept verbatim apart
//! from whitespace collapsing; templating is `lookup_error`'s job.
//!
//! Output: at most [`MAX_ERROR_STRINGS`] strings of at most [`MAX_ERROR_STRING_CHARS`] chars,
//! deduplicated (the best rank wins), ordered by rank, then first seen across description,
//! comments, thread.
//!
//! Bounded work for adversarial input: each line is scanned up to [`MAX_LINE_BYTES`] (the rest of
//! the line is ignored), and at most [`MAX_SCAN_BYTES`] are scanned per call (later lines, comments
//! and messages are ignored). A rank stops collecting once it, or a better rank, holds
//! [`MAX_ERROR_STRINGS`] distinct strings (it can no longer reach the output), and scanning stops
//! when rank 1 is full. All patterns are `regex` automata (linear time), at most 64 logfmt pairs are
//! read per line, and the quote scanner is linear per line, so a call on any input takes well under
//! a second even in a debug build.

use std::borrow::Cow;
use std::ops::Range;
use std::sync::LazyLock;

use regex::Regex;
use serde_json::{Map, Value};

/// Maximum number of extracted strings.
pub const MAX_ERROR_STRINGS: usize = 20;
/// Maximum length of one extracted string, in chars.
pub const MAX_ERROR_STRING_CHARS: usize = 500;
/// Bytes of a single line that are scanned; the rest of a longer line is ignored.
pub const MAX_LINE_BYTES: usize = 16 * 1024;
/// Bytes scanned per call across description, comments and thread; the rest is ignored. Four times
/// the normalized body cap (intake.md 3.1: 64 KiB), which is all triage ever reads.
pub const MAX_SCAN_BYTES: usize = 256 * 1024;

const QUOTED_MIN_CHARS: usize = 12;
const QUOTED_MAX_CHARS: usize = 200;
/// Timestamps, tags and separators skipped before a log level, and meta tokens after it.
const MAX_LOG_TOKENS: usize = 8;
/// logfmt pairs read per line; real log lines carry far fewer.
const MAX_LOGFMT_PAIRS: usize = 64;

const TRACEBACK: &str = "Traceback (most recent call last)";
const PANICKED_AT: &str = "panicked at";

/// Timestamp shapes seen in pasted logs: ISO 8601 / RFC 3339, `2024/03/01 10:00:00`, bare times,
/// syslog `Mar  1 10:00:00`.
const TIMESTAMP: &str = r"\d{4}-\d{2}-\d{2}(?:[T ]\d{2}:\d{2}(?::\d{2})?(?:[.,]\d+)?)?(?:Z|[+-]\d{2}:?\d{2})?|\d{4}/\d{2}/\d{2}(?: \d{2}:\d{2}:\d{2}(?:[.,]\d+)?)?|\d{2}:\d{2}:\d{2}(?:[.,]\d+)?Z?|[A-Z][a-z]{2} +\d{1,2} \d{2}:\d{2}:\d{2}";

/// Word boundaries are ASCII (`(?-u:\b)`): a Unicode `\b` makes the lazy DFA give up on non-ASCII
/// text and fall back to the much slower PikeVM.
fn regex(pattern: &str) -> Regex {
    Regex::new(pattern).expect("static extraction pattern compiles")
}

static EXCEPTION: LazyLock<Regex> = LazyLock::new(|| regex(r"(?-u:\b)[\w.$]*(?:Error|Exception)(?-u:\b)[:(]"));
static UNHANDLED: LazyLock<Regex> = LazyLock::new(|| regex(r"(?-u:\b)Unhandled"));
static FATAL: LazyLock<Regex> = LazyLock::new(|| regex(r"(?-u:\b)FATAL(?-u:\b)"));

static PY_FRAME: LazyLock<Regex> = LazyLock::new(|| regex(r#"^File "[^"]*", line \d+"#));
/// JS / Java / C# / Rust `at …` frames (with a code location, so prose starting with "at" stays),
/// Go frames, Go file lines, goroutine headers.
static FRAME: LazyLock<Regex> = LazyLock::new(|| {
    regex(
        r"(?x)^(?:
            at\s+(?:
                \S*[/\\]\S*:\d+
              | .*\(.*(?::\d+|Native\ Method|Unknown\ Source|<anonymous>|index\ \d+)\)
              | [\w$.<>`\[\],]+\(.*\)(?:\s+in\s+.*)?
            )\s*
          | (?:[\w.\-]+/)*[\w.\-]+\.(?:\(\*?\w+\)\.)?[\w.\-]+(?:\[[^\]]*\])?\([0-9a-fx{},\ .?]*\)
          | \S+\.go:\d+(?:\s+\+0x[0-9a-f]+)?
          | created\ by\ \S+(?:\ in\ goroutine\ \d+)?
          | goroutine\ \d+\ \[[^\]]*\]:
        )$",
    )
});
/// Python ≥ 3.11 error location markers under a source line.
static CARET_LINE: LazyLock<Regex> = LazyLock::new(|| regex(r"^[\^~\s]+$"));
static LIST_MARKER: LazyLock<Regex> = LazyLock::new(|| regex(r"^(?:[-*+]|\d{1,3}[.)])\s+"));

static LOG_LEVEL: LazyLock<Regex> = LazyLock::new(|| regex(r"^(?:\[(?:ERROR|Error|error|FATAL|Fatal|fatal)\]|(?:ERROR|FATAL)(?-u:\b))"));
static LOG_PREFIX_TOKEN: LazyLock<Regex> =
    LazyLock::new(|| regex(&format!(r"^(?:(?:{TIMESTAMP})(?:\s+|$)|\[[^\]]{{1,64}}\]:?\s*|(?:\d+|[-|]|[\w$]+(?:\.[\w$]+)+)(?:\s+|$))")));
/// Right after the level: Python's `:logger:` or a separator.
static LOG_AFTER_LEVEL: LazyLock<Regex> = LazyLock::new(|| regex(r"^(?::[\w.\-]+:|\s*[:|\-]?)\s*"));
/// Pid + `---` (Spring Boot), `[thread]`, nginx `1234#0:` and `*5`, timestamps.
static LOG_META: LazyLock<Regex> = LazyLock::new(|| regex(&format!(r"^(?:\d+\s+---|---|\[[^\]]{{0,64}}\]|\*\d+|\d+#\d+:|{TIMESTAMP})\s*")));
/// A dotted logger name followed by a spaced separator (`c.a.b.Service : `, `app.billing - `).
static LOG_LOGGER: LazyLock<Regex> = LazyLock::new(|| regex(r"^[\w$]+(?:\.[\w$]+)+(?::\d+)?\s+[:|\-]\s+"));
static LOGFMT_PAIR: LazyLock<Regex> = LazyLock::new(|| regex(r#"(?:^|\s)([\w.\-]+)=("(?:[^"\\]|\\.)*"|[^\s"]*)"#));

static HTTP_REQUEST: LazyLock<Regex> =
    LazyLock::new(|| regex(r#"(?-u:\b)(GET|POST|PUT|PATCH|DELETE|HEAD|OPTIONS)\s+(?:https?://[^\s/]+)?(/[^\s?#"'`<>]*)"#));
static HTTP_STATUS: LazyLock<Regex> = LazyLock::new(|| regex(r"(?-u:\b)[45][0-9][0-9](?-u:\b)"));
static HTTP_PHRASE: LazyLock<Regex> = LazyLock::new(|| {
    regex(
        r"(?i)(?-u:\b)(?:bad request|unauthorized|forbidden|not found|method not allowed|request timeout|precondition failed|payload too large|unsupported media type|unprocessable entity|too many requests|internal server error|not implemented|bad gateway|service unavailable|gateway timeout)(?-u:\b)",
    )
});

const HTTP_METHODS: [&str; 7] = ["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"];
const ERROR_LEVELS: [&str; 6] = ["error", "err", "fatal", "critical", "crit", "panic"];
const LEVEL_KEYS: [&str; 5] = ["level", "lvl", "severity", "levelname", "log.level"];
const MESSAGE_KEYS: [&str; 2] = ["message", "msg"];
const ERROR_KEYS: [&str; 2] = ["error", "err"];
const STATUS_KEYS: [&str; 5] = ["status", "status_code", "statusCode", "http.status_code", "http_status"];
const METHOD_KEYS: [&str; 4] = ["method", "http.method", "http_method", "request_method"];
const PATH_KEYS: [&str; 7] = ["path", "route", "uri", "url", "http.target", "http.url", "request_uri"];

/// Error strings of a ticket, see the module docs. `comments` are the kept (human) comments,
/// oldest first; `thread` the non-bot Slack messages, oldest first.
pub fn error_strings(description: &str, comments: &[&str], thread: &[&str]) -> Vec<String> {
    let mut acc = Collector::default();
    let mut budget = MAX_SCAN_BYTES;
    let texts = std::iter::once(description).chain(comments.iter().copied()).chain(thread.iter().copied());
    for text in texts {
        if budget == 0 || acc.done() {
            break;
        }
        scan_text(text, &mut acc, &mut budget);
    }
    acc.finish()
}

#[derive(Clone, Copy)]
enum Rank {
    Exception = 0,
    ErrorLog = 1,
    Http = 2,
    Quoted = 3,
}

/// Distinct strings per rank, first seen first, each capped at [`MAX_ERROR_STRINGS`].
#[derive(Default)]
struct Collector {
    ranks: [Vec<String>; 4],
}

impl Collector {
    /// A rank can still reach the output while neither it nor a better rank is full.
    fn wants(&self, rank: Rank) -> bool {
        self.ranks[..=rank as usize].iter().all(|r| r.len() < MAX_ERROR_STRINGS)
    }

    fn done(&self) -> bool {
        !self.wants(Rank::Exception)
    }

    fn push(&mut self, rank: Rank, raw: &str) {
        if self.wants(rank) {
            self.push_clean(rank, clean(raw));
        }
    }

    fn push_clean(&mut self, rank: Rank, s: String) {
        let bucket = &mut self.ranks[rank as usize];
        if !s.is_empty() && bucket.len() < MAX_ERROR_STRINGS && !bucket.contains(&s) {
            bucket.push(s);
        }
    }

    fn finish(self) -> Vec<String> {
        let mut out: Vec<String> = Vec::with_capacity(MAX_ERROR_STRINGS);
        for s in self.ranks.into_iter().flatten() {
            if out.len() == MAX_ERROR_STRINGS {
                break;
            }
            if !out.contains(&s) {
                out.push(s);
            }
        }
        out
    }
}

/// Collapses whitespace runs to one space and caps the result at [`MAX_ERROR_STRING_CHARS`].
fn clean(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len().min(MAX_ERROR_STRING_CHARS));
    let mut chars = 0;
    'words: for word in raw.split_whitespace() {
        if chars > 0 {
            if chars == MAX_ERROR_STRING_CHARS {
                break;
            }
            out.push(' ');
            chars += 1;
        }
        for c in word.chars() {
            if chars == MAX_ERROR_STRING_CHARS {
                break 'words;
            }
            out.push(c);
            chars += 1;
        }
    }
    out.truncate(out.trim_end().len());
    out
}

/// The longest prefix of `s` of at most `max` bytes ending on a char boundary.
fn floor_slice(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Multi-line state within one text.
#[derive(Default)]
struct TextState {
    /// After `Traceback (most recent call last)`: the next non-frame line is the exception.
    traceback: bool,
    /// After `panicked at file:line:col:`: the next line is the panic message.
    panic_message: bool,
    /// After a Python frame: a deeper-indented next line is its source line.
    python_frame_indent: Option<usize>,
}

fn scan_text(text: &str, acc: &mut Collector, budget: &mut usize) {
    let mut state = TextState::default();
    for raw in text.lines() {
        if *budget == 0 || acc.done() {
            return;
        }
        let line = floor_slice(raw, MAX_LINE_BYTES.min(*budget));
        *budget = budget.saturating_sub(line.len() + 1);
        scan_line(line, &mut state, acc);
    }
}

/// Strips blockquote and list markers; returns the indentation (tab = 4) and the trimmed body.
fn strip_markup(line: &str) -> (usize, &str) {
    let mut s = line;
    while let Some(rest) = s.trim_start().strip_prefix('>') {
        s = rest.strip_prefix(' ').unwrap_or(rest);
    }
    let indent = s.chars().take_while(|c| c.is_whitespace()).map(|c| if c == '\t' { 4 } else { 1 }).sum();
    let body = s.trim();
    let body = if body.starts_with(['-', '*', '+']) || body.starts_with(|c: char| c.is_ascii_digit()) {
        LIST_MARKER.find(body).map_or(body, |m| &body[m.end()..])
    } else {
        body
    };
    (indent, body)
}

fn scan_line(line: &str, state: &mut TextState, acc: &mut Collector) {
    let (indent, body) = strip_markup(line);
    if body.is_empty() || body.starts_with("```") || body.starts_with("~~~") {
        return;
    }
    if let Some(frame_indent) = state.python_frame_indent.take() {
        if indent > frame_indent {
            return;
        }
    }
    if body.starts_with(['^', '~']) && CARET_LINE.is_match(body) {
        return;
    }
    if body.starts_with("File \"") && PY_FRAME.is_match(body) {
        state.python_frame_indent = Some(indent);
        return;
    }
    if is_frame(body) {
        return;
    }
    if std::mem::take(&mut state.traceback) {
        acc.push(Rank::Exception, body);
    }
    if std::mem::take(&mut state.panic_message) {
        acc.push(Rank::Exception, body);
    }
    if body.contains(TRACEBACK) {
        state.traceback = true;
        return;
    }
    if let Some(record) = LogRecord::parse(body) {
        record.collect(acc);
        return;
    }
    let prefixed = level_prefixed(body);
    match prefixed {
        Some((Level::Fatal, rest)) => acc.push(Rank::Exception, rest),
        Some((Level::Error, rest)) => acc.push(Rank::ErrorLog, rest),
        None => {}
    }
    let fatal_handled = matches!(prefixed, Some((Level::Fatal, _)));
    scan_free_text(body, acc, fatal_handled, Some(&mut state.panic_message));
}

/// Prefilters spare the regex calls on ordinary prose lines (they dominate debug-build cost).
fn is_frame(body: &str) -> bool {
    let candidate = body.starts_with("at ")
        || body.ends_with(')')
        || body.contains(".go:")
        || body.starts_with("goroutine ")
        || body.starts_with("created by ");
    candidate && FRAME.is_match(body)
}

/// Ranks 1, 3 and 4 over one line, or over one message value of a structured log line
/// (`panic_message` is `None` then: values have no following line).
fn scan_free_text(text: &str, acc: &mut Collector, fatal_handled: bool, panic_message: Option<&mut bool>) {
    let mut spans: Option<Vec<Range<usize>>> = None;
    if acc.wants(Rank::Exception) {
        if let Some(at) = text.find(PANICKED_AT) {
            let location = text[at + PANICKED_AT.len()..].trim_end();
            match panic_message {
                Some(pending) if location.ends_with(':') => *pending = true,
                _ => acc.push(Rank::Exception, text),
            }
        } else if let Some(start) = exception_start(text, fatal_handled) {
            let spans = spans.get_or_insert_with(|| quote_spans(text));
            let end = spans.iter().find(|span| span.start <= start && start < span.end).map_or(text.len(), |span| span.end);
            acc.push(Rank::Exception, &text[start..end]);
        }
    }
    if acc.wants(Rank::Http) {
        push_http_failures(text, acc);
    }
    if acc.wants(Rank::Quoted) {
        for span in spans.get_or_insert_with(|| quote_spans(text)).iter() {
            let quoted = &text[span.clone()];
            if quoted.len() < QUOTED_MIN_CHARS {
                continue;
            }
            let quoted = clean(quoted);
            if is_ui_text(&quoted) {
                acc.push_clean(Rank::Quoted, quoted);
                if !acc.wants(Rank::Quoted) {
                    break;
                }
            }
        }
    }
}

/// Start of the earliest exception/panic header in `text`.
fn exception_start(text: &str, fatal_handled: bool) -> Option<usize> {
    let found = |needle: &str, re: &Regex| if text.contains(needle) { re.find(text).map(|m| m.start()) } else { None };
    let exception = if text.contains("Error") || text.contains("Exception") { EXCEPTION.find(text).map(|m| m.start()) } else { None };
    let go_panic = text.starts_with("panic: ").then_some(0);
    let fatal = if fatal_handled { None } else { found("FATAL", &FATAL) };
    [exception, found("Unhandled", &UNHANDLED), go_panic, fatal].into_iter().flatten().min()
}

#[derive(Clone, Copy)]
enum Level {
    Error,
    Fatal,
}

/// `ERROR …` / `[error] …` / `FATAL …` after optional timestamps, tags, pids and loggers → the
/// level and the message after the logger.
fn level_prefixed(body: &str) -> Option<(Level, &str)> {
    let has_level = ["ERROR", "FATAL", "rror]", "atal]"].iter().any(|level| body.contains(level));
    if !has_level {
        return None;
    }
    let mut rest = body;
    for _ in 0..=MAX_LOG_TOKENS {
        if let Some(level) = LOG_LEVEL.find(rest) {
            let kind = if level.as_str().bytes().any(|b| b.eq_ignore_ascii_case(&b'f')) { Level::Fatal } else { Level::Error };
            let message = strip_log_meta(&rest[level.end()..]);
            return (!message.is_empty()).then_some((kind, message));
        }
        rest = &rest[LOG_PREFIX_TOKEN.find(rest)?.end()..];
    }
    None
}

fn strip_log_meta(after_level: &str) -> &str {
    let mut s = &after_level[LOG_AFTER_LEVEL.find(after_level).map_or(0, |m| m.end())..];
    for _ in 0..MAX_LOG_TOKENS {
        match LOG_META.find(s) {
            Some(m) if m.end() > 0 => s = &s[m.end()..],
            _ => break,
        }
    }
    if let Some(logger) = LOG_LOGGER.find(s) {
        s = &s[logger.end()..];
    }
    s.trim()
}

/// Rank 3 for one line: `"{status} {method} {path}"` for every 4xx/5xx status outside the request
/// (a retry line may carry several), else `"{status} {phrase}"` for the first status.
fn push_http_failures(text: &str, acc: &mut Collector) {
    if !text.contains(['4', '5']) {
        return;
    }
    let request = HTTP_REQUEST.captures(text);
    let request_span = request.as_ref().and_then(|c| c.get(0)).map(|m| m.range());
    let mut statuses = HTTP_STATUS.find_iter(text).filter(|m| {
        let inside_request = request_span.as_ref().is_some_and(|r| r.start <= m.start() && m.end() <= r.end);
        !inside_request && standalone_status(text, m.range())
    });
    match request {
        Some(request) => {
            let (method, path) = (&request[1], trim_path(&request[2]));
            for status in statuses {
                acc.push(Rank::Http, &format!("{} {method} {path}", status.as_str()));
            }
        }
        None => {
            if let (Some(status), Some(phrase)) = (statuses.next(), HTTP_PHRASE.find(text)) {
                acc.push(Rank::Http, &format!("{} {}", status.as_str(), phrase.as_str()));
            }
        }
    }
}

/// Excludes numbers that are part of amounts, versions, ids, times or addresses (`$500`, `1.404`,
/// `#503`, `1,500`, `10:450`, `404.2`, `500%`).
fn standalone_status(text: &str, at: Range<usize>) -> bool {
    let bytes = text.as_bytes();
    let before = at.start.checked_sub(1).map(|i| bytes[i]);
    if before.is_some_and(|b| b".:/-#$,_".contains(&b)) {
        return false;
    }
    match (bytes.get(at.end), bytes.get(at.end + 1)) {
        (Some(b'%'), _) => false,
        (Some(b'.' | b':' | b',' | b'-'), Some(next)) => !next.is_ascii_digit(),
        _ => true,
    }
}

fn trim_path(path: &str) -> &str {
    path.trim_end_matches(['.', ',', ';', ':', '!', ')', ']'])
}

/// The path of a URL or path (scheme and host dropped), without query and fragment.
fn url_path(value: &str) -> &str {
    let path = match value.split_once("://") {
        Some((_, rest)) => rest.find('/').map_or("/", |i| &rest[i..]),
        None => value,
    };
    path.split(['?', '#']).next().unwrap_or(path)
}

/// A structured (logfmt or JSON) log line with a level.
struct LogRecord<'a> {
    error_level: bool,
    message: Option<Cow<'a, str>>,
    error: Option<Cow<'a, str>>,
    http: Option<String>,
}

impl<'a> LogRecord<'a> {
    fn parse(body: &'a str) -> Option<Self> {
        if body.starts_with('{') && body.ends_with('}') {
            Self::from_json(body)
        } else if ["level=", "lvl=", "severity="].iter().any(|key| body.contains(key)) {
            Self::from_logfmt(body)
        } else {
            None
        }
    }

    fn from_json(body: &str) -> Option<Self> {
        let map: Map<String, Value> = serde_json::from_str(body).ok()?;
        let level = LEVEL_KEYS.iter().find_map(|k| map.get(*k))?;
        let error_level = match level {
            Value::String(s) => is_error_level(s),
            // pino / bunyan numeric levels: 50 error, 60 fatal.
            Value::Number(n) => n.as_u64().is_some_and(|n| n >= 50),
            _ => false,
        };
        let text = |keys: &[&str]| keys.iter().find_map(|k| json_text(map.get(*k)?)).map(Cow::Owned);
        let scalar = |key: &str| match map.get(key)? {
            Value::String(s) => Some(Cow::Borrowed(s.as_str())),
            Value::Number(n) => Some(Cow::Owned(n.to_string())),
            _ => None,
        };
        Some(Self { error_level, message: text(&MESSAGE_KEYS), error: text(&ERROR_KEYS), http: http_fields(scalar) })
    }

    fn from_logfmt(body: &'a str) -> Option<Self> {
        let pairs: Vec<(&str, &str)> =
            LOGFMT_PAIR.captures_iter(body).take(MAX_LOGFMT_PAIRS).filter_map(|c| Some((c.get(1)?.as_str(), c.get(2)?.as_str()))).collect();
        let get = |key: &str| pairs.iter().find(|(k, _)| *k == key).map(|(_, v)| logfmt_value(v));
        let level = LEVEL_KEYS.iter().find_map(|k| get(k))?;
        let message = MESSAGE_KEYS.iter().find_map(|k| get(k));
        let error = ERROR_KEYS.iter().find_map(|k| get(k));
        if message.is_none() && error.is_none() {
            return None;
        }
        let first_line = |v: Cow<'a, str>| match v {
            Cow::Borrowed(s) => Cow::Borrowed(s.lines().next().unwrap_or("")),
            Cow::Owned(s) => Cow::Owned(s.lines().next().unwrap_or("").to_owned()),
        };
        Some(Self {
            error_level: is_error_level(&level),
            message: message.map(first_line),
            error: error.map(first_line),
            http: http_fields(get),
        })
    }

    fn collect(self, acc: &mut Collector) {
        if self.error_level {
            if let Some(message) = self.message.as_deref().or(self.error.as_deref()) {
                acc.push(Rank::ErrorLog, message);
            }
        }
        if let Some(http) = &self.http {
            acc.push(Rank::Http, http);
        }
        for text in [self.message.as_deref(), self.error.as_deref()].into_iter().flatten() {
            scan_free_text(text, acc, false, None);
        }
    }
}

fn is_error_level(level: &str) -> bool {
    ERROR_LEVELS.iter().any(|l| l.eq_ignore_ascii_case(level))
}

/// First line of a string value, or of `{"message": …}` (nested error objects).
fn json_text(value: &Value) -> Option<String> {
    let s = match value {
        Value::String(s) => s,
        Value::Object(o) => o.get("message")?.as_str()?,
        _ => return None,
    };
    Some(s.lines().next().unwrap_or("").to_owned())
}

/// A logfmt value with quotes removed and `\"`, `\\`, `\n`, `\t` unescaped.
fn logfmt_value(raw: &str) -> Cow<'_, str> {
    let Some(inner) = raw.strip_prefix('"').and_then(|r| r.strip_suffix('"')) else {
        return Cow::Borrowed(raw);
    };
    if !inner.contains('\\') {
        return Cow::Borrowed(inner);
    }
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    Cow::Owned(out)
}

/// `"{status} {method} {path}"` from the fields of a structured log line.
fn http_fields<'v>(get: impl Fn(&str) -> Option<Cow<'v, str>>) -> Option<String> {
    let status = STATUS_KEYS.iter().find_map(|k| get(k))?;
    let status: u16 = status.trim().parse().ok().filter(|s| (400..600).contains(s))?;
    let method = METHOD_KEYS.iter().find_map(|k| get(k))?;
    if !HTTP_METHODS.iter().any(|m| m.eq_ignore_ascii_case(&method)) {
        return None;
    }
    let path = PATH_KEYS.iter().find_map(|k| get(k))?;
    Some(format!("{status} {method} {}", url_path(&path)))
}

/// Content ranges of properly paired quotes in `text`, left to right. Straight single quotes and
/// `’` only open/close at a word boundary so apostrophes (`don't`, `it’s`) do not pair. Linear: a
/// successful pairing skips past its closing quote, a failed one marks that quote kind as having
/// no closer in the rest of the text.
fn quote_spans(text: &str) -> Vec<Range<usize>> {
    const OPENERS: [char; 5] = ['`', '"', '\'', '“', '‘'];
    let mut spans = Vec::new();
    let mut unclosed = [false; OPENERS.len()];
    let mut i = 0;
    while let Some(found) = text[i..].find(OPENERS) {
        if unclosed.iter().all(|u| *u) {
            break;
        }
        let at = i + found;
        let c = text[at..].chars().next().unwrap_or_default();
        let next = at + c.len_utf8();
        if let Some((kind, close)) = quote_opener(c, text[..at].chars().next_back()) {
            if !unclosed[kind] {
                match quote_close(&text[next..], close) {
                    Some(len) => {
                        spans.push(next..next + len);
                        i = next + len + close.len_utf8();
                        continue;
                    }
                    None => unclosed[kind] = true,
                }
            }
        }
        i = next;
    }
    spans
}

/// Quote kind index and closing char for an opening quote.
fn quote_opener(c: char, prev: Option<char>) -> Option<(usize, char)> {
    match c {
        '`' => Some((0, '`')),
        '"' => Some((1, '"')),
        '\'' if !prev.is_some_and(char::is_alphanumeric) => Some((2, '\'')),
        '“' => Some((3, '”')),
        '‘' => Some((4, '’')),
        _ => None,
    }
}

/// Byte length of the content before the closing quote.
fn quote_close(rest: &str, close: char) -> Option<usize> {
    if close != '\'' && close != '’' {
        return rest.find(close);
    }
    rest.match_indices(close).map(|(at, _)| at).find(|&at| !rest[at + close.len_utf8()..].chars().next().is_some_and(char::is_alphanumeric))
}

/// Rank 4 filter: 12–200 chars with a letter; not a path, URL, e-mail address or HTTP request line
/// (`"GET /api/x HTTP/1.1"` in access logs: rank 3 covers it).
fn is_ui_text(s: &str) -> bool {
    let chars = s.chars().count();
    (QUOTED_MIN_CHARS..=QUOTED_MAX_CHARS).contains(&chars)
        && s.chars().any(char::is_alphabetic)
        && !is_path_or_url(s)
        && !is_email(s)
        && !HTTP_REQUEST.find(s).is_some_and(|request| request.start() == 0)
}

fn is_path_or_url(s: &str) -> bool {
    let b = s.as_bytes();
    let windows_drive = b.len() > 2 && b[0].is_ascii_alphabetic() && b[1] == b':' && matches!(b[2], b'\\' | b'/');
    s.contains("://")
        || s.starts_with("www.")
        || s.starts_with('/')
        || s.starts_with("~/")
        || s.starts_with("./")
        || s.starts_with("../")
        || windows_drive
        || (!s.contains(char::is_whitespace) && s.contains(['/', '\\']))
}

fn is_email(s: &str) -> bool {
    !s.contains(char::is_whitespace) && s.split_once('@').is_some_and(|(user, host)| !user.is_empty() && host.contains('.'))
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;

    /// Anonymized tickets: `<!-- description -->`, `<!-- comment -->` and `<!-- thread -->` start
    /// the description, one kept comment and one thread message.
    const CORPUS: &[(&str, &str, &str)] = &[
        ("cross_source", include_str!("testdata/extract/cross_source.md"), include_str!("testdata/extract/cross_source.json")),
        ("go_panic", include_str!("testdata/extract/go_panic.md"), include_str!("testdata/extract/go_panic.json")),
        ("http_failures", include_str!("testdata/extract/http_failures.md"), include_str!("testdata/extract/http_failures.json")),
        ("java_exception", include_str!("testdata/extract/java_exception.md"), include_str!("testdata/extract/java_exception.json")),
        ("logs_logfmt_json", include_str!("testdata/extract/logs_logfmt_json.md"), include_str!("testdata/extract/logs_logfmt_json.json")),
        ("mixed_noise", include_str!("testdata/extract/mixed_noise.md"), include_str!("testdata/extract/mixed_noise.json")),
        ("node_typeerror", include_str!("testdata/extract/node_typeerror.md"), include_str!("testdata/extract/node_typeerror.json")),
        ("python_traceback", include_str!("testdata/extract/python_traceback.md"), include_str!("testdata/extract/python_traceback.json")),
        ("rust_panic", include_str!("testdata/extract/rust_panic.md"), include_str!("testdata/extract/rust_panic.json")),
        ("ui_quoted_text", include_str!("testdata/extract/ui_quoted_text.md"), include_str!("testdata/extract/ui_quoted_text.json")),
    ];

    struct Ticket {
        description: String,
        comments: Vec<String>,
        thread: Vec<String>,
    }

    fn parse_ticket(source: &str) -> Ticket {
        let mut ticket = Ticket { description: String::new(), comments: Vec::new(), thread: Vec::new() };
        let mut section: Option<&str> = None;
        for line in source.lines() {
            match line.trim() {
                "<!-- description -->" => section = Some("description"),
                "<!-- comment -->" => {
                    section = Some("comment");
                    ticket.comments.push(String::new());
                }
                "<!-- thread -->" => {
                    section = Some("thread");
                    ticket.thread.push(String::new());
                }
                _ => {
                    let buf = match section {
                        Some("description") => &mut ticket.description,
                        Some("comment") => ticket.comments.last_mut().expect("comment started"),
                        Some("thread") => ticket.thread.last_mut().expect("thread message started"),
                        _ => panic!("text before the first section marker: {line:?}"),
                    };
                    buf.push_str(line);
                    buf.push('\n');
                }
            }
        }
        ticket
    }

    fn extract(description: &str, comments: &[&str], thread: &[&str]) -> Vec<String> {
        error_strings(description, comments, thread)
    }

    /// Runs `f`, asserting it finishes within the adversarial-input budget (debug build).
    fn bounded(f: impl FnOnce() -> Vec<String>) -> Vec<String> {
        let started = Instant::now();
        let out = f();
        let elapsed = started.elapsed();
        assert!(elapsed < Duration::from_secs(1), "took {elapsed:?}");
        assert!(out.len() <= MAX_ERROR_STRINGS);
        assert!(out.iter().all(|s| s.chars().count() <= MAX_ERROR_STRING_CHARS));
        out
    }

    #[test]
    fn golden_corpus() {
        assert!(CORPUS.len() >= 8);
        for (name, source, expected) in CORPUS {
            let ticket = parse_ticket(source);
            let comments: Vec<&str> = ticket.comments.iter().map(String::as_str).collect();
            let thread: Vec<&str> = ticket.thread.iter().map(String::as_str).collect();
            let expected: Vec<String> = serde_json::from_str(expected).expect("expected output is a JSON string array");
            assert_eq!(extract(&ticket.description, &comments, &thread), expected, "corpus ticket {name}");
        }
    }

    #[test]
    fn duplicates_keep_the_best_rank_and_collapse_whitespace() {
        let out = extract(
            r#"The toast says "Could not save the invoice draft". Then "Session expired, please sign in"."#,
            &["2024-03-01T12:00:00Z ERROR [api] Could not  save the invoice\tdraft"],
            &[r#"again: "Could not save the invoice draft""#],
        );
        assert_eq!(out, ["Could not save the invoice draft", "Session expired, please sign in"]);
    }

    #[test]
    fn ranks_order_the_output_before_sources_do() {
        let out = extract(
            "The banner reads `Unable to load your projects right now`.",
            &[
                "Request log: GET /api/projects returned 502",
                "ERROR projects.loader - upstream fetch failed",
                "First error: BetaError: second exception",
            ],
            &["AlphaError: first exception in the thread"],
        );
        assert_eq!(
            out,
            [
                "BetaError: second exception",
                "AlphaError: first exception in the thread",
                "upstream fetch failed",
                "502 GET /api/projects",
                "Unable to load your projects right now",
            ]
        );
    }

    #[test]
    fn stack_frames_and_python_source_lines_are_skipped() {
        let out = extract(
            "Traceback (most recent call last):\n  File \"/srv/app/jobs.py\", line 12, in run\n    raise TimeoutError(\"job queue stalled for too long\")\nStalledQueue: job queue stalled\n    at Object.<anonymous> (/app/x.js:1:2)\n    at com.acme.Job.run(Job.java:10)\n",
            &[],
            &[],
        );
        assert_eq!(out, ["StalledQueue: job queue stalled"]);
    }

    #[test]
    fn at_most_twenty_strings_best_ranks_first() {
        let description: String = (0..15).map(|i| format!("Job{i}Exception: failure number {i}\n")).collect();
        let quoted: String = (0..10).map(|i| format!("The page shows \"quoted message number {i}\"\n")).collect();
        let out = extract(&description, &[&quoted], &[]);
        let mut expected: Vec<String> = (0..15).map(|i| format!("Job{i}Exception: failure number {i}")).collect();
        expected.extend((0..5).map(|i| format!("quoted message number {i}")));
        assert_eq!(out, expected);
    }

    #[test]
    fn twenty_one_distinct_errors_keep_the_first_twenty() {
        let description: String = (0..21).map(|i| format!("Error: distinct failure {i}\n")).collect();
        let out = extract(&description, &[], &[]);
        let expected: Vec<String> = (0..20).map(|i| format!("Error: distinct failure {i}")).collect();
        assert_eq!(out, expected);
    }

    #[test]
    fn long_strings_are_cut_at_500_chars_on_char_boundaries() {
        let ascii = format!("Error: {}", "x".repeat(600));
        let out = extract(&ascii, &[], &[]);
        assert_eq!(out, [ascii.chars().take(MAX_ERROR_STRING_CHARS).collect::<String>()]);

        let multibyte = format!("Error: {}", "é".repeat(600));
        let out = extract(&multibyte, &[], &[]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].chars().count(), MAX_ERROR_STRING_CHARS);
        assert!(multibyte.starts_with(&out[0]));
    }

    #[test]
    fn one_megabyte_single_line_is_bounded() {
        let line = format!("Error: {}", "a".repeat(1 << 20));
        let out = bounded(|| extract(&line, &[], &[]));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].chars().count(), MAX_ERROR_STRING_CHARS);

        let quotes = "\"".repeat(1 << 20);
        bounded(|| extract(&quotes, &[], &[]));
        let apostrophes = "a' ".repeat(350_000);
        bounded(|| extract(&apostrophes, &[], &[]));
        let mixed = "`x\" 'y “z ‘w ".repeat(70_000);
        bounded(|| extract(&mixed, &[], &[]));
    }

    #[test]
    fn ten_thousand_quoted_strings_are_bounded() {
        let one_line: String = (0..10_000).map(|i| format!("\"quoted ui string {i:05}\" ")).collect();
        let out = bounded(|| extract(&one_line, &[], &[]));
        let expected: Vec<String> = (0..20).map(|i| format!("quoted ui string {i:05}")).collect();
        assert_eq!(out, expected);

        let lines: String = (0..10_000).map(|i| format!("the page says “quoted ui string {i:05}”\n")).collect();
        let out = bounded(|| extract(&lines, &[], &[]));
        assert_eq!(out, expected);

        let same: Vec<String> = (0..10_000).map(|_| "toast: 'Something went wrong here'".to_owned()).collect();
        let same: Vec<&str> = same.iter().map(String::as_str).collect();
        let out = bounded(|| extract("", &same, &same));
        assert_eq!(out, ["Something went wrong here"]);
    }

    #[test]
    fn scanning_stops_at_the_byte_budget() {
        let filler = "nothing to see in this line at all\n".repeat((MAX_SCAN_BYTES / 35) + 1);
        let out = bounded(|| extract(&filler, &["Error: after the budget"], &[]));
        assert!(out.is_empty(), "{out:?}");
    }
}
