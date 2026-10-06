//! `UrlTemplate`s from source expressions (Go, JS/TS, Clojure) and string literal decoding.

use mage_core::ports::knowledge::{UrlPart, UrlTemplate};
use tree_sitter::Node;

use super::{named_children, snippet, text};

/// Accumulates parts, merging adjacent literals and dropping empty ones.
#[derive(Default)]
pub(super) struct Builder {
    parts: Vec<UrlPart>,
}

impl Builder {
    pub(super) fn lit(&mut self, s: &str) {
        if s.is_empty() {
            return;
        }
        if let Some(UrlPart::Lit(last)) = self.parts.last_mut() {
            last.push_str(s);
        } else {
            self.parts.push(UrlPart::Lit(s.to_owned()));
        }
    }

    pub(super) fn var(&mut self, name: Option<String>) {
        self.parts.push(UrlPart::Var(name));
    }

    pub(super) fn extend(&mut self, template: &UrlTemplate) {
        for part in &template.parts {
            match part {
                UrlPart::Lit(s) => self.lit(s),
                UrlPart::Var(v) => self.var(v.clone()),
            }
        }
    }

    pub(super) fn finish(self) -> UrlTemplate {
        UrlTemplate { parts: self.parts }
    }
}

pub(super) fn lit_template(s: &str) -> UrlTemplate {
    let mut b = Builder::default();
    b.lit(s);
    b.finish()
}

pub(super) fn var_template(name: &str) -> UrlTemplate {
    UrlTemplate { parts: vec![UrlPart::Var(Some(name.to_owned()))] }
}

/// `a` followed by `b`, adjacent literals merged.
pub(super) fn concat(a: &UrlTemplate, b: &UrlTemplate) -> UrlTemplate {
    let mut out = Builder::default();
    out.extend(a);
    out.extend(b);
    out.finish()
}

/// First literal text when the template starts with a literal.
pub(super) fn leading_lit(t: &UrlTemplate) -> Option<&str> {
    match t.parts.first() {
        Some(UrlPart::Lit(s)) => Some(s),
        _ => None,
    }
}

/// React Router path join: absolute children are kept, relative ones are appended to the parent
/// (the root `/` when there is no parent) with exactly one `/` between them.
pub(super) fn join_route(parent: Option<&UrlTemplate>, child: &UrlTemplate) -> UrlTemplate {
    if leading_lit(child).is_some_and(|s| s.starts_with('/')) {
        return child.clone();
    }
    let root = lit_template("/");
    let parent = parent.unwrap_or(&root);
    if child.parts.is_empty() {
        return parent.clone();
    }
    let mut out = Builder::default();
    out.extend(parent);
    let ends_with_slash = matches!(parent.parts.last(), Some(UrlPart::Lit(s)) if s.ends_with('/'));
    if !ends_with_slash {
        out.lit("/");
    }
    out.extend(child);
    out.finish()
}

/// Decodes `\n \t \r \0 \\ \" \' \` \/ \xHH \uXXXX \u{…}`; other escaped chars stand for themselves.
pub(super) fn unescape(s: &str) -> String {
    if !s.contains('\\') {
        return s.to_owned();
    }
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            None => out.push('\\'),
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('0') => out.push('\0'),
            Some('x') => {
                let hex: String = chars.by_ref().take(2).collect();
                push_code(&mut out, &hex, "x");
            }
            Some('u') => {
                let hex: String = if chars.peek() == Some(&'{') {
                    chars.next();
                    chars.by_ref().take_while(|&c| c != '}').collect()
                } else {
                    chars.by_ref().take(4).collect()
                };
                push_code(&mut out, &hex, "u");
            }
            Some(other) => out.push(other),
        }
    }
    out
}

fn push_code(out: &mut String, hex: &str, marker: &str) {
    match u32::from_str_radix(hex, 16).ok().and_then(char::from_u32) {
        Some(c) => out.push(c),
        None => {
            out.push('\\');
            out.push_str(marker);
            out.push_str(hex);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// String literal contents

/// Decoded content of a Go string literal (`"…"` or `` `…` ``).
pub(super) fn go_string(node: Node<'_>, src: &[u8]) -> Option<String> {
    match node.kind() {
        "interpreted_string_literal" => {
            let mut out = String::new();
            for child in named_children(node) {
                match child.kind() {
                    "escape_sequence" => out.push_str(&unescape(text(child, src))),
                    _ => out.push_str(text(child, src)),
                }
            }
            Some(out)
        }
        "raw_string_literal" => {
            let raw = text(node, src);
            let inner = raw.strip_prefix('`').and_then(|s| s.strip_suffix('`')).unwrap_or(raw);
            Some(inner.replace('\r', ""))
        }
        _ => None,
    }
}

/// Decoded content of a JS string or a substitution-free template literal.
pub(super) fn js_string(node: Node<'_>, src: &[u8]) -> Option<String> {
    match node.kind() {
        "string" | "template_string" => {
            let mut out = String::new();
            for child in named_children(node) {
                match child.kind() {
                    "string_fragment" => out.push_str(text(child, src)),
                    "escape_sequence" => out.push_str(&unescape(text(child, src))),
                    _ => return None,
                }
            }
            Some(out)
        }
        _ => None,
    }
}

/// Decoded content of a Clojure string literal.
pub(super) fn clj_string(node: Node<'_>, src: &[u8]) -> Option<String> {
    if node.kind() != "str_lit" {
        return None;
    }
    let raw = text(node, src);
    let inner = raw.strip_prefix('"').and_then(|s| s.strip_suffix('"')).unwrap_or(raw);
    Some(unescape(inner))
}

// ---------------------------------------------------------------------------------------------
// Go

/// Template of a Go string-ish expression: a string literal, `+` concatenation containing one, or
/// `fmt.Sprintf` with a literal format. `None` for anything else (identifiers, calls, …).
pub(super) fn go_path_template(node: Node<'_>, src: &[u8]) -> Option<UrlTemplate> {
    go_has_literal(node, src).then(|| go_template(node, src))
}

fn go_has_literal(node: Node<'_>, src: &[u8]) -> bool {
    match node.kind() {
        "interpreted_string_literal" | "raw_string_literal" => true,
        "parenthesized_expression" => named_children(node).first().is_some_and(|n| go_has_literal(*n, src)),
        "binary_expression" => {
            go_is_plus(node, src)
                && [node.child_by_field_name("left"), node.child_by_field_name("right")]
                    .into_iter()
                    .flatten()
                    .any(|n| go_has_literal(n, src))
        }
        "call_expression" => go_sprintf_parts(node, src).is_some(),
        _ => false,
    }
}

fn go_is_plus(node: Node<'_>, src: &[u8]) -> bool {
    node.child_by_field_name("operator").is_some_and(|op| text(op, src) == "+")
}

/// Template of any Go expression (unrecognized → `[Var(text)]`).
pub(super) fn go_template(node: Node<'_>, src: &[u8]) -> UrlTemplate {
    let mut b = Builder::default();
    go_push(&mut b, node, src);
    b.finish()
}

fn go_push(b: &mut Builder, node: Node<'_>, src: &[u8]) {
    if let Some(s) = go_string(node, src) {
        b.lit(&s);
        return;
    }
    match node.kind() {
        "parenthesized_expression" => match named_children(node).first() {
            Some(inner) => go_push(b, *inner, src),
            None => b.var(Some(snippet(node, src))),
        },
        "binary_expression" if go_is_plus(node, src) => {
            for side in [node.child_by_field_name("left"), node.child_by_field_name("right")].into_iter().flatten() {
                go_push(b, side, src);
            }
        }
        "call_expression" => match go_sprintf_parts(node, src) {
            Some((format, args)) => push_printf(b, &format, &args, src),
            None => b.var(Some(snippet(node, src))),
        },
        _ => b.var(Some(snippet(node, src))),
    }
}

/// `fmt.Sprintf(<literal>, args…)` → (format, args).
fn go_sprintf_parts<'t>(node: Node<'t>, src: &[u8]) -> Option<(String, Vec<Node<'t>>)> {
    let func = node.child_by_field_name("function")?;
    if func.kind() != "selector_expression" || text(func, src) != "fmt.Sprintf" {
        return None;
    }
    let args = named_children(node.child_by_field_name("arguments")?);
    let (first, rest) = args.split_first()?;
    let format = go_string(*first, src)?;
    Some((format, rest.to_vec()))
}

/// printf-style format: every verb consumes the next argument as a Var; `%%` is a literal `%`.
fn push_printf(b: &mut Builder, format: &str, args: &[Node<'_>], src: &[u8]) {
    let mut args = args.iter();
    let mut lit = String::new();
    let mut chars = format.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '%' {
            lit.push(c);
            continue;
        }
        if chars.peek() == Some(&'%') {
            chars.next();
            lit.push('%');
            continue;
        }
        while chars.peek().is_some_and(|c| "+-# 0123456789.*[]".contains(*c)) {
            chars.next();
        }
        if chars.next().is_none() {
            lit.push('%');
            break;
        }
        b.lit(&lit);
        lit.clear();
        b.var(args.next().map(|a| snippet(*a, src)));
    }
    b.lit(&lit);
}

// ---------------------------------------------------------------------------------------------
// JavaScript / TypeScript

/// Template of any JS expression (unrecognized → `[Var(text)]`).
pub(super) fn js_template(node: Node<'_>, src: &[u8]) -> UrlTemplate {
    let mut b = Builder::default();
    js_push(&mut b, node, src);
    b.finish()
}

/// True for string literals, template literals and `+` concatenations containing one.
pub(super) fn js_is_stringish(node: Node<'_>, src: &[u8]) -> bool {
    match node.kind() {
        "string" | "template_string" => true,
        "parenthesized_expression" => named_children(node).first().is_some_and(|n| js_is_stringish(*n, src)),
        "binary_expression" => {
            js_is_plus(node, src)
                && [node.child_by_field_name("left"), node.child_by_field_name("right")]
                    .into_iter()
                    .flatten()
                    .any(|n| js_is_stringish(n, src))
        }
        _ => false,
    }
}

fn js_is_plus(node: Node<'_>, src: &[u8]) -> bool {
    node.child_by_field_name("operator").is_some_and(|op| text(op, src) == "+")
}

fn js_push(b: &mut Builder, node: Node<'_>, src: &[u8]) {
    match node.kind() {
        "string" => b.lit(&js_string(node, src).unwrap_or_default()),
        "template_string" => {
            for child in named_children(node) {
                match child.kind() {
                    "string_fragment" => b.lit(text(child, src)),
                    "escape_sequence" => b.lit(&unescape(text(child, src))),
                    "template_substitution" => match named_children(child).first() {
                        Some(expr) => b.var(Some(snippet(*expr, src))),
                        None => b.var(None),
                    },
                    _ => {}
                }
            }
        }
        "parenthesized_expression" => match named_children(node).first() {
            Some(inner) => js_push(b, *inner, src),
            None => b.var(Some(snippet(node, src))),
        },
        "binary_expression" if js_is_plus(node, src) => {
            for side in [node.child_by_field_name("left"), node.child_by_field_name("right")].into_iter().flatten() {
                js_push(b, side, src);
            }
        }
        _ => b.var(Some(snippet(node, src))),
    }
}

// ---------------------------------------------------------------------------------------------
// Clojure

/// Values (field `value`) of a Clojure collection / list, skipping metadata, comments and discards.
pub(super) fn clj_values<'t>(node: Node<'t>) -> Vec<Node<'t>> {
    let mut cursor = node.walk();
    node.children_by_field_name("value", &mut cursor).filter(|c| c.kind() != "dis_expr" && c.kind() != "comment").collect()
}

/// Head symbol name of a list (`(str …)` → `str`).
pub(super) fn clj_head<'s>(node: Node<'_>, src: &'s [u8]) -> Option<&'s str> {
    if node.kind() != "list_lit" {
        return None;
    }
    let head = *clj_values(node).first()?;
    (head.kind() == "sym_lit").then(|| text(head, src))
}

/// Template of a Clojure form: string, `(str …)` flattened, symbol → Var, else Var(text).
pub(super) fn clj_template(node: Node<'_>, src: &[u8]) -> UrlTemplate {
    let mut b = Builder::default();
    clj_push(&mut b, node, src);
    b.finish()
}

fn clj_push(b: &mut Builder, node: Node<'_>, src: &[u8]) {
    if let Some(s) = clj_string(node, src) {
        b.lit(&s);
        return;
    }
    if clj_head(node, src) == Some("str") {
        for arg in clj_values(node).into_iter().skip(1) {
            clj_push(b, arg, src);
        }
        return;
    }
    b.var(Some(snippet(node, src)));
}
