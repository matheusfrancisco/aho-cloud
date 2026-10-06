//! Error-site classification (knowledge.md "Layer 3: Error sites", "Classification",
//! "Template"): `LiteralSite` → `ErrorSiteDraft` (first matching rule wins), the literal's parts
//! rewritten into a `Template` with the callee's format convention, then `to_pattern` from
//! `mage_core::domain::knowledge`. Pure; versioned by [`ERRORS_REV`]: any change to the tables or
//! rules below needs a bump.
//!
//! Inputs follow the treesitter adapter's `LiteralContext` contract: `callee` is the
//! whitespace-free callee text (`fmt.Errorf`, `res.status(404).json`, `log/error`, `bail!`),
//! transparent wrappers (`str`, `format`, `fmt.Sprintf`, `.format`, `%`, `+`, template literals)
//! already folded into `parts`, `siblings` include the other values of enclosing maps.
//!
//! Format conventions per language: Go and Clojure printf; Python printf and brace; Rust brace;
//! JS/TS printf for `console.*` calls only (interpolation holes everywhere); catalogs ICU.
//! `I18nUse` sites keep an empty template; their pattern matches the raw key, so an untranslated
//! key leaking into a UI or log still finds the site.

use mage_core::domain::Language;
use mage_core::domain::knowledge::{Seg, Template, VarClass, to_pattern};
use mage_core::ports::knowledge::{
    ArgSummary, Catalog, CatalogEntryRecord, Construct, ErrorSiteKind, ErrorSiteRecord, ErrorsRev, LitPart, LiteralSite, LogLevel,
};

/// Classifier tables + pattern rules revision.
pub const ERRORS_REV: ErrorsRev = ErrorsRev(1);

/// Literals with fewer letters are ignored.
pub const MIN_LETTERS: usize = 3;
/// Literals longer than this (chars of literal text) are ignored.
pub const MAX_LITERAL_CHARS: usize = 512;

/// A classified literal before `to_pattern`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorSiteDraft {
    pub kind: ErrorSiteKind,
    pub level: Option<LogLevel>,
    pub template: Template,
    pub http_status: Option<u16>,
    pub i18n_key: Option<String>,
}

/// How placeholders are written in a literal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Convention {
    /// No placeholders besides interpolation holes.
    Plain,
    Printf,
    Brace,
    /// Python: `%` formatting and `.format` / logging braces.
    PrintfBrace,
    /// i18n catalogs: `{name}`, `{{name}}`, `%{name}`, `{0}`, `{name, plural, …}`.
    Icu,
}

const GO_ERROR_CTORS: [&str; 11] = [
    "errors.New",
    "fmt.Errorf",
    "errors.Errorf",
    "errors.Wrap",
    "errors.Wrapf",
    "errors.WithMessage",
    "errors.WithMessagef",
    "status.Error",
    "status.Errorf",
    "xerrors.New",
    "xerrors.Errorf",
];
const RUST_ERROR_CALLEES: [&str; 12] =
    ["bail!", "anyhow!", "panic!", "ensure!", "unreachable!", "todo!", "unimplemented!", "format_err!", "eyre!", "err!", "expect", "expect_err"];
const CLOJURE_ERROR_CTORS: [&str; 8] =
    ["ex-info", "Error.", "TypeError.", "Exception.", "RuntimeException.", "IllegalArgumentException.", "IllegalStateException.", "ExceptionInfo."];
/// Constructors of HTTP errors: with a status they are `HttpResponse`, not `Throw`.
const HTTP_ERROR_CTORS: [&str; 7] = ["HTTPException", "HTTPError", "HttpError", "NewHTTPError", "HttpException", "ResponseStatusException", "HTTPBadRequest"];
const LOG_METHODS: [&str; 25] = [
    "Error", "Errorf", "Errorw", "Warn", "Warnf", "Warning", "Fatal", "Fatalf", "Panicf", "Info", "Infof", "Printf", "Println", "error",
    "warn", "warning", "exception", "critical", "info", "errorf", "warnf", "infof", "fatal", "fatalf", "Warnw",
];
const LOG_RECEIVERS: [&str; 10] = ["log", "logger", "logging", "slog", "zap", "logrus", "l", "console", "timbre", "tracing"];
const RUST_LOG_MACROS: [&str; 5] = ["error!", "warn!", "info!", "debug!", "trace!"];
const HTTP_MESSAGE_KEYS: [&str; 7] = ["message", "error", "detail", "msg", ":message", ":error", "description"];
const UI_CALLEES: [&str; 6] = ["toast.error", "toast.warning", "notify", "alert", "message.error", "enqueueSnackbar"];
const UI_MAP_KEYS: [&str; 2] = [":text", ":message"];
const DISPATCH_CALLEES: [&str; 2] = ["dispatch", "dispatch-sync"];
const I18N_CALLEES: [&str; 8] = ["t", "i18n.t", "$t", "tr", "gettext", "_", "formatMessage", "i18n/t"];
const I18N_ATTRIBUTE: &str = "i18nKey";

/// HTTP status names (Go `StatusNotFound`, Rust/Python `NOT_FOUND`), canonicalized by
/// [`canonical_status_name`].
const STATUS_NAMES: [(&str, u16); 34] = [
    ("ok", 200),
    ("created", 201),
    ("accepted", 202),
    ("nocontent", 204),
    ("movedpermanently", 301),
    ("found", 302),
    ("seeother", 303),
    ("notmodified", 304),
    ("temporaryredirect", 307),
    ("permanentredirect", 308),
    ("badrequest", 400),
    ("unauthorized", 401),
    ("paymentrequired", 402),
    ("forbidden", 403),
    ("notfound", 404),
    ("methodnotallowed", 405),
    ("notacceptable", 406),
    ("requesttimeout", 408),
    ("conflict", 409),
    ("gone", 410),
    ("preconditionfailed", 412),
    ("requestentitytoolarge", 413),
    ("payloadtoolarge", 413),
    ("unsupportedmediatype", 415),
    ("unprocessableentity", 422),
    ("unprocessablecontent", 422),
    ("toomanyrequests", 429),
    ("internalservererror", 500),
    ("notimplemented", 501),
    ("badgateway", 502),
    ("serviceunavailable", 503),
    ("gatewaytimeout", 504),
    ("locked", 423),
    ("failedependency", 424),
];

/// Classify one literal site (knowledge.md "Classification"); `None` = not an error site.
pub fn classify(lang: Language, site: &LiteralSite) -> Option<ErrorSiteDraft> {
    let text = literal_text(&site.parts);
    let letters = text.chars().filter(|c| c.is_alphabetic()).count();
    if letters < MIN_LETTERS || text.chars().count() > MAX_LITERAL_CHARS {
        return None;
    }
    let ctx = &site.context;
    let callee = Callee::new(ctx.callee.as_deref().unwrap_or(""));
    let status = http_status(site, &callee);
    let draft = |kind, level, http_status| ErrorSiteDraft {
        kind,
        level,
        template: template(&site.parts, convention(lang, &callee)),
        http_status,
        i18n_key: None,
    };

    // Throw
    let http_ctor = HTTP_ERROR_CTORS.contains(&callee.last());
    if !(http_ctor && status.is_some()) && is_throw(lang, site, &callee) {
        return Some(draft(ErrorSiteKind::Throw, None, status.and_then(StatusSignal::code)));
    }
    // Log
    if let Some(level) = log_level(lang, &callee) {
        return Some(draft(ErrorSiteKind::Log, Some(level), None));
    }
    // HttpResponse
    if let Some(status) = status {
        let message_key = ctx.map_key.as_deref().is_none_or(|key| HTTP_MESSAGE_KEYS.contains(&key));
        if message_key {
            return Some(draft(ErrorSiteKind::HttpResponse, None, status.code()));
        }
    }
    // UiMessage
    let ui_callee = UI_CALLEES.iter().any(|c| callee.is(c));
    let ui_map = ctx.map_key.as_deref().is_some_and(|key| UI_MAP_KEYS.contains(&key))
        && ctx.siblings.iter().any(|s| matches!(s, ArgSummary::Ident(id) if id == ":error"))
        && DISPATCH_CALLEES.contains(&callee.last());
    if ui_callee || ui_map {
        return Some(draft(ErrorSiteKind::UiMessage, None, None));
    }
    // I18nUse
    let i18n_call = I18N_CALLEES.iter().any(|c| callee.is(c)) && (ctx.arg_index == Some(0) || ctx.map_key.as_deref() == Some("id"));
    if i18n_call || ctx.attribute.as_deref() == Some(I18N_ATTRIBUTE) {
        let key = match site.parts.as_slice() {
            [LitPart::Text(key)] if !key.trim().is_empty() => key.clone(),
            _ => return None,
        };
        return Some(ErrorSiteDraft { kind: ErrorSiteKind::I18nUse, level: None, template: Template::default(), http_status: None, i18n_key: Some(key) });
    }
    None
}

/// Classified sites of one blob as stored records (`ord` = position among the blob's error
/// sites); sites whose template has no literal text are dropped.
pub fn error_site_records(lang: Language, sites: &[LiteralSite]) -> Vec<ErrorSiteRecord> {
    let mut out = Vec::new();
    for site in sites {
        let Some(draft) = classify(lang, site) else { continue };
        let matched = match &draft.i18n_key {
            Some(key) => to_pattern(&Template { segs: vec![Seg::Lit(key.clone())] }),
            None => to_pattern(&draft.template),
        };
        let Some(pattern) = matched else { continue };
        let ord = u32::try_from(out.len()).unwrap_or(u32::MAX);
        out.push(ErrorSiteRecord {
            ord,
            literal_ord: site.ord,
            kind: draft.kind,
            level: draft.level,
            template: draft.template.to_string(),
            pattern: pattern.pattern,
            tokens: pattern.tokens,
            specificity: pattern.specificity,
            pos: site.pos,
            enclosing: site.enclosing,
            http_status: draft.http_status,
            i18n_key: draft.i18n_key,
        });
    }
    out
}

/// Catalog messages as stored records (ICU convention); messages without literal text dropped.
pub fn catalog_records(catalog: &Catalog) -> Vec<CatalogEntryRecord> {
    catalog
        .entries
        .iter()
        .filter_map(|entry| {
            let template = Template { segs: parse_icu(&entry.message) };
            let pattern = to_pattern(&template)?;
            Some(CatalogEntryRecord {
                key: entry.key.clone(),
                locale: entry.locale.clone(),
                template: template.to_string(),
                pattern: pattern.pattern,
                tokens: pattern.tokens,
                specificity: pattern.specificity,
                line: entry.line,
            })
        })
        .collect()
}

/// Literal text of the parts (holes left out).
fn literal_text(parts: &[LitPart]) -> String {
    parts
        .iter()
        .filter_map(|part| match part {
            LitPart::Text(text) => Some(text.as_str()),
            LitPart::Hole(_) => None,
        })
        .collect()
}

/// Callee text split into segments on `.`, `/` and `::` outside parentheses.
struct Callee<'a> {
    raw: &'a str,
    /// Segments with call arguments stripped (`status(404)` → `status`).
    names: Vec<&'a str>,
    /// Segments as written.
    segments: Vec<&'a str>,
}

impl<'a> Callee<'a> {
    fn new(raw: &'a str) -> Self {
        let segments = split_callee(raw);
        let names = segments.iter().map(|s| s.split('(').next().unwrap_or(s)).collect();
        Self { raw, names, segments }
    }

    fn last(&self) -> &'a str {
        self.names.last().copied().unwrap_or("")
    }

    /// Segments before the last.
    fn receivers(&self) -> &[&'a str] {
        &self.names[..self.names.len().saturating_sub(1)]
    }

    /// The callee is `name`, or ends with it on a segment boundary (`this.toast.error` is
    /// `toast.error`).
    fn is(&self, name: &str) -> bool {
        self.raw == name
            || self.raw.strip_suffix(name).is_some_and(|rest| rest.ends_with('.') || rest.ends_with('/') || rest.ends_with("::"))
    }
}

fn split_callee(raw: &str) -> Vec<&str> {
    let bytes = raw.as_bytes();
    let mut out = Vec::new();
    let (mut depth, mut start, mut i) = (0i32, 0usize, 0usize);
    while i < bytes.len() {
        match bytes[i] {
            b'(' | b'[' => depth += 1,
            b')' | b']' => depth -= 1,
            b'.' | b'/' if depth == 0 => {
                // `js/Error.`: a trailing dot belongs to the constructor name.
                if bytes[i] == b'.' && i + 1 == bytes.len() {
                    i += 1;
                    continue;
                }
                out.push(&raw[start..i]);
                start = i + 1;
            }
            b':' if depth == 0 && bytes.get(i + 1) == Some(&b':') => {
                out.push(&raw[start..i]);
                start = i + 2;
                i += 1;
            }
            _ => {}
        }
        i += 1;
    }
    out.push(&raw[start..]);
    out.retain(|s| !s.is_empty());
    out
}

fn is_throw(lang: Language, site: &LiteralSite, callee: &Callee<'_>) -> bool {
    let ctx = &site.context;
    if matches!(ctx.construct, Construct::Throw | Construct::Raise | Construct::Panic) || ctx.attribute.as_deref() == Some("error") {
        return true;
    }
    let last = callee.last();
    match lang {
        Language::Go => GO_ERROR_CTORS.contains(&callee.raw) || callee.raw == "panic",
        Language::Rust => RUST_ERROR_CALLEES.contains(&last),
        Language::Clojure | Language::ClojureScript | Language::Cljc => CLOJURE_ERROR_CTORS.contains(&last),
        Language::Python | Language::JavaScript | Language::TypeScript | Language::Tsx => {
            ctx.exception_type.is_some() || is_error_class(last)
        }
        _ => ctx.exception_type.is_some(),
    }
}

/// `TypeError`, `ValueError`, `HttpException`: capitalized names ending in Error/Exception.
fn is_error_class(name: &str) -> bool {
    name.chars().next().is_some_and(char::is_uppercase) && (name.ends_with("Error") || name.ends_with("Exception"))
}

fn log_level(lang: Language, callee: &Callee<'_>) -> Option<LogLevel> {
    let last = callee.last();
    if lang == Language::Rust && RUST_LOG_MACROS.contains(&last) {
        return Some(match last {
            "error!" => LogLevel::Error,
            "warn!" => LogLevel::Warn,
            "info!" => LogLevel::Info,
            _ => LogLevel::Debug,
        });
    }
    if !LOG_METHODS.contains(&last) {
        return None;
    }
    let receiver = callee.receivers().iter().any(|r| LOG_RECEIVERS.iter().any(|known| r.eq_ignore_ascii_case(known)));
    if !receiver {
        return None;
    }
    Some(match last {
        "Fatal" | "Fatalf" | "Panicf" | "critical" | "fatal" | "fatalf" => LogLevel::Fatal,
        "Warn" | "Warnf" | "Warning" | "warn" | "warning" | "warnf" | "Warnw" => LogLevel::Warn,
        "Info" | "Infof" | "Printf" | "Println" | "info" | "infof" => LogLevel::Info,
        _ => LogLevel::Error,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatusSignal {
    Code(u16),
    /// A status constant whose value is unknown.
    Unknown,
}

impl StatusSignal {
    fn code(self) -> Option<u16> {
        match self {
            Self::Code(code) => Some(code),
            Self::Unknown => None,
        }
    }
}

/// An error status among the siblings (`404`, `http.StatusNotFound`, `StatusCode::NOT_FOUND`,
/// `HTTPStatus.NOT_FOUND`, `status.HTTP_404_NOT_FOUND`) or in the callee chain
/// (`res.status(404).json`).
fn http_status(site: &LiteralSite, callee: &Callee<'_>) -> Option<StatusSignal> {
    for sibling in &site.context.siblings {
        match sibling {
            ArgSummary::Int(n) if (400..=599).contains(n) => return u16::try_from(*n).ok().map(StatusSignal::Code),
            ArgSummary::Ident(ident) => match status_constant(ident) {
                Some(StatusSignal::Code(code)) if code < 400 => {}
                Some(signal) => return Some(signal),
                None => {}
            },
            _ => {}
        }
    }
    callee.segments.iter().find_map(|segment| {
        let (name, args) = segment.split_once('(')?;
        if !matches!(name, "status" | "Status" | "WriteHeader" | "code" | "sendStatus") {
            return None;
        }
        let code: u16 = args.strip_suffix(')')?.trim().parse().ok()?;
        (400..=599).contains(&code).then_some(StatusSignal::Code(code))
    })
}

fn status_constant(ident: &str) -> Option<StatusSignal> {
    let last = split_callee(ident).last().copied().unwrap_or(ident);
    if let Some(rest) = last.strip_prefix("HTTP_") {
        // Django REST framework: `status.HTTP_404_NOT_FOUND`.
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        if let Ok(code) = digits.parse::<u16>() {
            return Some(StatusSignal::Code(code));
        }
    }
    let name = if let Some(rest) = last.strip_prefix("Status").filter(|r| r.chars().next().is_some_and(char::is_uppercase)) {
        rest
    } else if ident.contains("StatusCode::") || ident.contains("HTTPStatus.") || ident.contains("HttpStatus.") {
        last
    } else {
        return None;
    };
    let canonical = canonical_status_name(name);
    Some(STATUS_NAMES.iter().find(|(known, _)| *known == canonical).map_or(StatusSignal::Unknown, |(_, code)| StatusSignal::Code(*code)))
}

/// `NotFound` / `NOT_FOUND` → `notfound`.
fn canonical_status_name(name: &str) -> String {
    name.chars().filter(char::is_ascii_alphanumeric).map(|c| c.to_ascii_lowercase()).collect()
}

fn convention(lang: Language, callee: &Callee<'_>) -> Convention {
    match lang {
        Language::Go | Language::Clojure | Language::ClojureScript | Language::Cljc => Convention::Printf,
        Language::Python => Convention::PrintfBrace,
        Language::Rust => Convention::Brace,
        Language::JavaScript | Language::TypeScript | Language::Tsx => {
            if callee.raw.starts_with("console.") {
                Convention::Printf
            } else {
                Convention::Plain
            }
        }
        _ => Convention::Plain,
    }
}

/// Rewrite literal parts into template segments (holes become `Any`).
pub fn template(parts: &[LitPart], convention: Convention) -> Template {
    let mut segs = Vec::new();
    for part in parts {
        match part {
            LitPart::Hole(_) => segs.push(Seg::Var(VarClass::Any)),
            LitPart::Text(text) => segs.extend(match convention {
                Convention::Plain => vec![Seg::Lit(text.clone())],
                Convention::Printf => parse_printf(text),
                Convention::Brace => parse_brace(text),
                Convention::PrintfBrace => parse_printf(text)
                    .into_iter()
                    .flat_map(|seg| match seg {
                        Seg::Lit(lit) => parse_brace(&lit),
                        var => vec![var],
                    })
                    .collect(),
                Convention::Icu => parse_icu(text),
            }),
        }
    }
    Template { segs }
}

/// Pushes literal text, merging with a preceding literal.
fn push_lit(segs: &mut Vec<Seg>, text: &str) {
    if text.is_empty() {
        return;
    }
    match segs.last_mut() {
        Some(Seg::Lit(prev)) => prev.push_str(text),
        _ => segs.push(Seg::Lit(text.to_owned())),
    }
}

/// printf: `%d %i` Int, `%f %g %e` Float, `%q` Quoted, `%w` Wrapped, other verbs and `%(name)s`
/// Any, `%%` a literal `%`. Malformed directives stay literal.
fn parse_printf(text: &str) -> Vec<Seg> {
    let mut segs = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find('%') {
        push_lit(&mut segs, &rest[..at]);
        let after = &rest[at + 1..];
        if let Some(tail) = after.strip_prefix('%') {
            push_lit(&mut segs, "%");
            rest = tail;
            continue;
        }
        match printf_directive(after) {
            Some((class, len)) => {
                segs.push(Seg::Var(class));
                rest = &after[len..];
            }
            None => {
                push_lit(&mut segs, "%");
                rest = after;
            }
        }
    }
    push_lit(&mut segs, rest);
    segs
}

/// Directive after `%`: `(name)`? flags* width? (.precision)? length? verb → (class, bytes).
fn printf_directive(s: &str) -> Option<(VarClass, usize)> {
    let bytes = s.as_bytes();
    let mut i = 0;
    if bytes.first() == Some(&b'(') {
        let close = s.find(')')?;
        if !s[1..close].bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
            return None;
        }
        i = close + 1;
    }
    while i < bytes.len() && matches!(bytes[i], b'-' | b'+' | b' ' | b'#' | b'0') {
        i += 1;
    }
    while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'*') {
        i += 1;
    }
    if bytes.get(i) == Some(&b'.') {
        i += 1;
        while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'*') {
            i += 1;
        }
    }
    while i < bytes.len() && matches!(bytes[i], b'h' | b'l' | b'L' | b'j' | b'z' | b't') {
        i += 1;
    }
    let class = match bytes.get(i)? {
        b'd' | b'i' => VarClass::Int,
        b'f' | b'F' | b'g' | b'G' | b'e' | b'E' => VarClass::Float,
        b'q' => VarClass::Quoted,
        b'w' => VarClass::Wrapped,
        b's' | b'v' | b'x' | b'X' | b'T' | b'o' | b'c' | b'u' | b'r' | b'a' | b'p' | b'b' | b'U' => VarClass::Any,
        _ => return None,
    };
    Some((class, i + 1))
}

/// brace: `{}`, `{0}`, `{name}`, `{:?}`, `{name:>8}` Any; `{{` / `}}` literal braces.
fn parse_brace(text: &str) -> Vec<Seg> {
    let mut segs = Vec::new();
    let mut lit = String::new();
    let mut chars = text.char_indices().peekable();
    while let Some((at, c)) = chars.next() {
        match c {
            '{' if chars.peek().map(|(_, c)| *c) == Some('{') => {
                chars.next();
                lit.push('{');
            }
            '}' if chars.peek().map(|(_, c)| *c) == Some('}') => {
                chars.next();
                lit.push('}');
            }
            '{' => match text[at + 1..].find('}') {
                Some(len) if text[at + 1..at + 1 + len].chars().all(is_brace_spec_char) => {
                    push_lit(&mut segs, &lit);
                    lit.clear();
                    segs.push(Seg::Var(VarClass::Any));
                    for _ in 0..=len {
                        chars.next();
                    }
                }
                _ => lit.push('{'),
            },
            other => lit.push(other),
        }
    }
    push_lit(&mut segs, &lit);
    segs
}

fn is_brace_spec_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | ':' | '?' | '<' | '>' | '^' | '#' | '+' | '-' | '=' | '!' | ' ' | '[' | ']')
}

/// ICU / i18n: `%{name}` (Rails), `{{name}}` (i18next), `{name}`, `{0}` and nested
/// `{name, plural, …}` are Any.
fn parse_icu(text: &str) -> Vec<Seg> {
    let mut segs = Vec::new();
    let bytes = text.as_bytes();
    let (mut i, mut lit_start) = (0usize, 0usize);
    while i < bytes.len() {
        let open = if bytes[i] == b'%' && bytes.get(i + 1) == Some(&b'{') {
            Some(i + 1)
        } else if bytes[i] == b'{' {
            Some(i)
        } else {
            None
        };
        let Some(open) = open else {
            i += 1;
            continue;
        };
        let mut depth = 0usize;
        let mut close = None;
        for (j, b) in bytes.iter().enumerate().skip(open) {
            match b {
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        close = Some(j);
                        break;
                    }
                }
                _ => {}
            }
        }
        match close {
            Some(close) => {
                push_lit(&mut segs, &text[lit_start..i]);
                segs.push(Seg::Var(VarClass::Any));
                i = close + 1;
                lit_start = i;
            }
            None => i += 1,
        }
    }
    push_lit(&mut segs, &text[lit_start..]);
    segs
}

#[cfg(test)]
mod tests {
    use mage_core::ports::knowledge::{CatalogEntry, CatalogFormat, Hole, LiteralContext};
    use regex::Regex;

    use super::*;

    fn ctx(callee: Option<&str>) -> LiteralContext {
        LiteralContext {
            callee: callee.map(str::to_owned),
            arg_index: Some(0),
            siblings: Vec::new(),
            construct: if callee.is_some() { Construct::Call } else { Construct::Other },
            exception_type: None,
            map_key: None,
            attribute: None,
        }
    }

    fn site(parts: Vec<LitPart>, context: LiteralContext) -> LiteralSite {
        LiteralSite { ord: 7, parts, pos: (3, 9), enclosing: Some(1), context }
    }

    fn text(s: &str) -> LitPart {
        LitPart::Text(s.to_owned())
    }

    fn hole(expr: &str) -> LitPart {
        LitPart::Hole(Hole::Interp { expr: expr.to_owned() })
    }

    fn record(lang: Language, s: LiteralSite) -> ErrorSiteRecord {
        let mut records = error_site_records(lang, &[s]);
        assert_eq!(records.len(), 1, "expected one error site");
        records.remove(0)
    }

    fn assert_matches(pattern: &str, input: &str) {
        let re = Regex::new(pattern).unwrap();
        assert!(re.is_match(input), "{pattern} should match {input:?}");
    }

    #[test]
    fn go_errorf_is_a_throw_with_printf_placeholders() {
        let mut c = ctx(Some("fmt.Errorf"));
        c.siblings = vec![ArgSummary::Ident("name".into()), ArgSummary::Ident("err".into())];
        let r = record(Language::Go, site(vec![text("failed fetching connection %s: %w")], c));
        assert_eq!(r.kind, ErrorSiteKind::Throw);
        assert_eq!(r.template, "failed fetching connection {}: {w}");
        assert_eq!(r.pattern, r"\bfailed\s+fetching\s+connection\s+.+?:\s+");
        assert_eq!(r.tokens, vec!["connection", "failed", "fetching"]);
        assert_eq!((r.literal_ord, r.pos, r.enclosing, r.ord), (7, (3, 9), Some(1), 0));
        assert_matches(&r.pattern, r#"level=error msg="failed fetching connection pg-prod: context deadline exceeded""#);
    }

    #[test]
    fn go_http_error_with_named_status_is_an_http_response() {
        let mut c = ctx(Some("http.Error"));
        c.arg_index = Some(1);
        c.siblings = vec![ArgSummary::Ident("w".into()), ArgSummary::Ident("http.StatusUnauthorized".into())];
        let r = record(Language::Go, site(vec![text("session expired")], c));
        assert_eq!((r.kind, r.http_status), (ErrorSiteKind::HttpResponse, Some(401)));
        assert_eq!(r.pattern, r"\bsession\s+expired\b");
        assert_matches(&r.pattern, "401 Unauthorized: session expired");
    }

    #[test]
    fn http_response_shapes_across_frameworks() {
        // gin c.JSON(http.StatusNotFound, gin.H{"message": "…"})
        let mut gin = ctx(Some("c.JSON"));
        gin.arg_index = Some(1);
        gin.map_key = Some("message".into());
        gin.siblings = vec![ArgSummary::Ident("http.StatusNotFound".into())];
        let r = record(Language::Go, site(vec![text("connection not found")], gin.clone()));
        assert_eq!((r.kind, r.http_status), (ErrorSiteKind::HttpResponse, Some(404)));
        // A 2xx status is not an error response.
        gin.siblings = vec![ArgSummary::Ident("http.StatusOK".into())];
        assert!(error_site_records(Language::Go, &[site(vec![text("connection created")], gin)]).is_empty());

        // Express res.status(404).json({message: "…"})
        let mut express = ctx(Some("res.status(404).json"));
        express.map_key = Some("message".into());
        let r = record(Language::JavaScript, site(vec![text("user not found")], express));
        assert_eq!((r.kind, r.http_status), (ErrorSiteKind::HttpResponse, Some(404)));

        // FastAPI raise HTTPException(status_code=404, detail="…")
        let mut fastapi = ctx(Some("HTTPException"));
        fastapi.construct = Construct::Raise;
        fastapi.arg_index = Some(1);
        fastapi.map_key = Some("detail".into());
        fastapi.exception_type = Some("HTTPException".into());
        fastapi.siblings = vec![ArgSummary::Int(404)];
        let r = record(Language::Python, site(vec![text("item not found")], fastapi));
        assert_eq!((r.kind, r.http_status), (ErrorSiteKind::HttpResponse, Some(404)));

        // Ring {:status 404 :body {:message "…"}}
        let mut ring = ctx(None);
        ring.construct = Construct::MapValue;
        ring.map_key = Some(":message".into());
        ring.siblings = vec![ArgSummary::Int(404)];
        let r = record(Language::Clojure, site(vec![text("connection not found")], ring));
        assert_eq!((r.kind, r.http_status), (ErrorSiteKind::HttpResponse, Some(404)));

        // Rust StatusCode::FORBIDDEN, Django status.HTTP_409_CONFLICT
        let mut axum = ctx(Some("error_response"));
        axum.siblings = vec![ArgSummary::Ident("StatusCode::FORBIDDEN".into())];
        assert_eq!(record(Language::Rust, site(vec![text("access denied")], axum)).http_status, Some(403));
        let mut drf = ctx(Some("Response"));
        drf.map_key = Some("error".into());
        drf.siblings = vec![ArgSummary::Ident("status.HTTP_409_CONFLICT".into())];
        assert_eq!(record(Language::Python, site(vec![text("already exists")], drf)).http_status, Some(409));
    }

    #[test]
    fn ts_template_literal_throw() {
        let mut c = ctx(Some("Error"));
        c.construct = Construct::Throw;
        c.exception_type = Some("Error".into());
        let r = record(Language::TypeScript, site(vec![text("Connection "), hole("id"), text(" not found")], c));
        assert_eq!(r.kind, ErrorSiteKind::Throw);
        assert_eq!(r.template, "Connection {} not found");
        assert_eq!(r.pattern, r"\bConnection\s+.+?\s+not\s+found\b");
        assert_matches(&r.pattern, "Uncaught Error: Connection 42 not found");
    }

    #[test]
    fn python_logger_uses_printf_classes() {
        let mut c = ctx(Some("logger.error"));
        c.siblings = vec![ArgSummary::Ident("n".into()), ArgSummary::Ident("m".into()), ArgSummary::Ident("host".into())];
        let r = record(Language::Python, site(vec![text("retry %d/%d failed for %s")], c));
        assert_eq!((r.kind, r.level), (ErrorSiteKind::Log, Some(LogLevel::Error)));
        assert_eq!(r.template, "retry {int}/{int} failed for {}");
        assert_eq!(r.pattern, r"\bretry\s+-?\d+/-?\d+\s+failed\s+for\b");
        assert_matches(&r.pattern, "ERROR retry 3/5 failed for db-1");
    }

    #[test]
    fn rust_error_attribute_and_macros() {
        let mut attr = ctx(None);
        attr.construct = Construct::Attribute;
        attr.attribute = Some("error".into());
        let r = record(Language::Rust, site(vec![text("invalid token: {0}")], attr));
        assert_eq!(r.kind, ErrorSiteKind::Throw);
        assert_eq!(r.template, "invalid token: {}");
        assert_eq!(r.pattern, r"\binvalid\s+token:");
        assert_matches(&r.pattern, "Error: invalid token: expired");

        assert_eq!(record(Language::Rust, site(vec![text("bad header {:?}")], ctx(Some("bail!")))).kind, ErrorSiteKind::Throw);
        assert_eq!(record(Language::Rust, site(vec![text("config must load")], ctx(Some("cfg.expect")))).kind, ErrorSiteKind::Throw);
        let warn = record(Language::Rust, site(vec![text("lease lost for {}")], ctx(Some("tracing::warn!"))));
        assert_eq!((warn.kind, warn.level, warn.template.as_str()), (ErrorSiteKind::Log, Some(LogLevel::Warn), "lease lost for {}"));
    }

    #[test]
    fn cljs_ex_info_with_str_concatenation() {
        let parts = vec![text("Plugin "), LitPart::Hole(Hole::Concat { expr: "plugin-name".into() }), text(" is not enabled")];
        let r = record(Language::ClojureScript, site(parts, ctx(Some("ex-info"))));
        assert_eq!(r.kind, ErrorSiteKind::Throw);
        assert_eq!(r.template, "Plugin {} is not enabled");
        assert_eq!(r.pattern, r"\bPlugin\s+.+?\s+is\s+not\s+enabled\b");
        assert_matches(&r.pattern, "Plugin slack is not enabled");
        assert_eq!(record(Language::ClojureScript, site(vec![text("boom happened")], ctx(Some("js/Error.")))).kind, ErrorSiteKind::Throw);
    }

    #[test]
    fn log_calls_need_a_known_receiver() {
        let cases = [
            (Language::Go, "log.Printf", Some(LogLevel::Info)),
            (Language::Go, "slog.Error", Some(LogLevel::Error)),
            (Language::Go, "s.logger.Warnf", Some(LogLevel::Warn)),
            (Language::Go, "log.WithError(err).Fatal", Some(LogLevel::Fatal)),
            (Language::Go, "zap.L().Error", Some(LogLevel::Error)),
            (Language::JavaScript, "console.error", Some(LogLevel::Error)),
            (Language::Python, "logging.critical", Some(LogLevel::Fatal)),
            (Language::Clojure, "log/error", Some(LogLevel::Error)),
            (Language::Clojure, "timbre/warn", Some(LogLevel::Warn)),
            (Language::ClojureScript, "js/console.error", Some(LogLevel::Error)),
            (Language::Go, "handler.Error", None),
            (Language::JavaScript, "console.log", None),
        ];
        for (lang, callee, level) in cases {
            let got = classify(lang, &site(vec![text("something went wrong")], ctx(Some(callee))));
            assert_eq!(got.as_ref().and_then(|d| d.level), level, "{callee}");
            assert_eq!(got.map(|d| d.kind), level.map(|_| ErrorSiteKind::Log), "{callee}");
        }
    }

    #[test]
    fn ui_messages() {
        assert_eq!(classify(Language::Tsx, &site(vec![text("Could not save")], ctx(Some("toast.error")))).map(|d| d.kind), Some(ErrorSiteKind::UiMessage));
        let mut snackbar = ctx(Some("rf/dispatch"));
        snackbar.construct = Construct::Call;
        snackbar.map_key = Some(":text".into());
        snackbar.siblings = vec![ArgSummary::Ident(":error".into())];
        let d = classify(Language::ClojureScript, &site(vec![text("Failed to connect")], snackbar.clone())).unwrap();
        assert_eq!(d.kind, ErrorSiteKind::UiMessage);
        snackbar.siblings = vec![ArgSummary::Ident(":info".into())];
        assert_eq!(classify(Language::ClojureScript, &site(vec![text("Saved the thing")], snackbar)), None);
    }

    #[test]
    fn i18n_uses_carry_the_key_and_an_empty_template() {
        let r = record(Language::Tsx, site(vec![text("errors.connection_not_found")], ctx(Some("t"))));
        assert_eq!(r.kind, ErrorSiteKind::I18nUse);
        assert_eq!(r.i18n_key.as_deref(), Some("errors.connection_not_found"));
        assert_eq!(r.template, "");
        assert_matches(&r.pattern, "missing translation errors.connection_not_found");
        let mut second_arg = ctx(Some("t"));
        second_arg.arg_index = Some(1);
        assert_eq!(classify(Language::Tsx, &site(vec![text("errors.other_key")], second_arg)), None);
        let mut jsx = ctx(None);
        jsx.construct = Construct::Attribute;
        jsx.attribute = Some("i18nKey".into());
        assert_eq!(record(Language::Tsx, site(vec![text("welcome.title")], jsx)).i18n_key.as_deref(), Some("welcome.title"));
    }

    #[test]
    fn short_letterless_and_huge_literals_are_ignored() {
        for parts in [vec![text("ok")], vec![text("404: %d")], vec![text(&"a".repeat(MAX_LITERAL_CHARS + 1))]] {
            assert_eq!(classify(Language::Go, &site(parts, ctx(Some("errors.New")))), None);
        }
        assert!(classify(Language::Go, &site(vec![text(&"a".repeat(MAX_LITERAL_CHARS))], ctx(Some("errors.New")))).is_some());
        // A template without literal text has no pattern and is dropped.
        let mut c = ctx(Some("errors.New"));
        c.construct = Construct::Call;
        assert!(error_site_records(Language::Go, &[site(vec![text("%s %s abc")], c.clone()), site(vec![text("%s")], c)]).len() == 1);
    }

    #[test]
    fn plain_string_calls_are_not_error_sites() {
        assert_eq!(classify(Language::Go, &site(vec![text("connection string")], ctx(Some("strings.Contains")))), None);
        assert_eq!(classify(Language::TypeScript, &site(vec![text("showError value")], ctx(Some("showError")))), None);
    }

    #[test]
    fn printf_and_brace_parsers() {
        use VarClass::*;
        let lit = |s: &str| Seg::Lit(s.to_owned());
        assert_eq!(parse_printf("100%% of %5.2f in %(name)s %q %+v %"), vec![
            lit("100% of "),
            Seg::Var(Float),
            lit(" in "),
            Seg::Var(Any),
            lit(" "),
            Seg::Var(Quoted),
            lit(" "),
            Seg::Var(Any),
            lit(" %")
        ]);
        assert_eq!(parse_brace("{{x}} {} {0} {name:>8} {:?} {not closed"), vec![
            lit("{x} "),
            Seg::Var(Any),
            lit(" "),
            Seg::Var(Any),
            lit(" "),
            Seg::Var(Any),
            lit(" "),
            Seg::Var(Any),
            lit(" {not closed")
        ]);
        assert_eq!(parse_icu("Hi {{name}}, %{count} {n, plural, one {# item} other {# items}} left"), vec![
            lit("Hi "),
            Seg::Var(Any),
            lit(", "),
            Seg::Var(Any),
            lit(" "),
            Seg::Var(Any),
            lit(" left")
        ]);
        // Python takes both conventions.
        assert_eq!(template(&[text("bad %s for {}")], Convention::PrintfBrace).to_string(), "bad {} for {}");
    }

    #[test]
    fn catalog_entries_use_icu_placeholders() {
        let catalog = Catalog {
            format: CatalogFormat::Json,
            entries: vec![
                CatalogEntry { key: "errors.conn_not_found".into(), locale: Some("en".into()), message: "Connection {{name}} not found".into(), line: 3 },
                CatalogEntry { key: "count".into(), locale: None, message: "{count}".into(), line: 4 },
            ],
        };
        let records = catalog_records(&catalog);
        assert_eq!(records.len(), 1);
        let r = &records[0];
        assert_eq!((r.key.as_str(), r.locale.as_deref(), r.line), ("errors.conn_not_found", Some("en"), 3));
        assert_eq!(r.template, "Connection {} not found");
        assert_eq!(r.pattern, r"\bConnection\s+.+?\s+not\s+found\b");
    }

    /// Property: rendering a classified template with values of each placeholder class yields text
    /// its pattern matches (tokens of the site all appear in the rendering).
    #[test]
    fn rendered_templates_match_their_patterns() {
        let samples: [(&str, Language, &str); 6] = [
            ("fmt.Errorf", Language::Go, "dial %s:%d failed after %.1fs: %w"),
            ("errors.Wrapf", Language::Go, "loading %q from %v"),
            ("logger.warning", Language::Python, "user %(user)s hit limit {} of %d"),
            ("anyhow!", Language::Rust, "cannot open {path:?} ({})"),
            ("log/error", Language::Clojure, "sync of %s failed"),
            ("console.error", Language::JavaScript, "request %s returned %d"),
        ];
        let values: [(VarClass, [&str; 3]); 5] = [
            (VarClass::Any, ["x", "pg-prod 01", "{weird} value"]),
            (VarClass::Int, ["0", "-12", "4096"]),
            (VarClass::Float, ["1", "2.5", "-0.25"]),
            (VarClass::Quoted, ["\"\"", "\"a b\"", "\"x\""]),
            (VarClass::Wrapped, ["eof", "context deadline exceeded: dial tcp", "x"]),
        ];
        for (callee, lang, literal) in samples {
            let r = record(lang, site(vec![text(literal)], ctx(Some(callee))));
            let draft = classify(lang, &site(vec![text(literal)], ctx(Some(callee)))).unwrap();
            for pick in 0..3 {
                let rendered: String = draft
                    .template
                    .segs
                    .iter()
                    .map(|seg| match seg {
                        Seg::Lit(lit) => lit.clone(),
                        Seg::Var(class) => values.iter().find(|(c, _)| c == class).map(|(_, v)| v[pick].to_owned()).unwrap(),
                    })
                    .collect();
                let line = format!("2026-10-05T10:00:00Z ERROR {rendered} (trace=abc)");
                assert_matches(&r.pattern, &line);
                let input_tokens = mage_core::domain::knowledge::tokenize(&line);
                assert!(r.tokens.iter().all(|t| input_tokens.contains(t)), "{:?} ⊄ {:?}", r.tokens, input_tokens);
            }
        }
    }
}
