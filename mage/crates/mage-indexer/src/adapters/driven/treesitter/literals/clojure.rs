//! Clojure / CLJS / CLJC literal rules: `str_lit`, `(str …)` concatenation, `(format "…" …)`
//! wrapper, list forms as calls (head symbol = callee), maps (keyword keys keep the colon),
//! `(throw …)`, constructor forms `(js/Error. …)` / `(new X …)`.

use mage_core::ports::knowledge::ArgSummary;
use tree_sitter::Node;

use super::{LitRules, Parts, Step, call_from_args, other_values};
use crate::adapters::driven::treesitter::clojure::{sym_name, sym_text, values};
use crate::adapters::driven::treesitter::text;

pub(in crate::adapters::driven::treesitter) struct ClojureRules;

/// Control forms whose value positions keep the enclosing context.
const TRANSPARENT_HEADS: &[&str] = &[
    "if", "if-not", "when", "when-not", "cond", "condp", "case", "do", "let", "let*", "if-let", "when-let", "if-some", "when-some", "or", "and",
];
/// Definition forms: a literal directly inside is not a call argument.
const STOP_HEADS: &[&str] = &[
    "defn",
    "defn-",
    "def",
    "defonce",
    "defmacro",
    "defmethod",
    "defmulti",
    "fn",
    "fn*",
    "letfn",
    "deftest",
    "ns",
    "defprotocol",
    "defrecord",
    "deftype",
    "extend-protocol",
    "extend-type",
    "reify",
    "comment",
];

/// Head symbol text of a list form (`None` when the head is not a symbol).
fn head_sym(list: Node<'_>, src: &[u8]) -> Option<String> {
    values(list).first().filter(|h| h.kind() == "sym_lit").map(|h| sym_text(*h, src))
}

impl LitRules for ClojureRules {
    fn is_string(&self, node: Node<'_>) -> bool {
        node.kind() == "str_lit"
    }

    fn decode(&self, node: Node<'_>, src: &[u8], parts: &mut Parts) {
        parts.escaped(str_body(text::text(node, src)));
    }

    fn concat_operands<'t>(&self, node: Node<'t>, src: &[u8]) -> Option<Vec<Node<'t>>> {
        if node.kind() != "list_lit" {
            return None;
        }
        let head = head_sym(node, src)?;
        matches!(head.as_str(), "str" | "clojure.core/str" | "cljs.core/str").then(|| values(node).into_iter().skip(1).collect())
    }

    fn is_paren(&self, _node: Node<'_>) -> bool {
        false
    }

    fn wrapper<'t>(&self, node: Node<'t>, src: &[u8]) -> Option<Node<'t>> {
        let list = node.parent().filter(|p| p.kind() == "list_lit")?;
        let vals = values(list);
        let head = vals.first().filter(|h| h.kind() == "sym_lit")?;
        (sym_name(*head, src) == "format" && vals.get(1).is_some_and(|f| f.id() == node.id())).then_some(list)
    }

    fn step<'t>(&self, node: Node<'t>, src: &[u8]) -> Step<'t> {
        let Some(parent) = node.parent() else { return Step::Stop };
        match parent.kind() {
            "list_lit" | "anon_fn_lit" => {
                let vals = values(parent);
                let Some(pos) = vals.iter().position(|v| v.id() == node.id()) else { return Step::Stop };
                if pos == 0 {
                    return Step::Stop;
                }
                let head = vals[0];
                let head_text = if head.kind() == "sym_lit" { sym_text(head, src) } else { text::squash(text::text(head, src)) };
                match head_text.as_str() {
                    "throw" => return Step::Throw,
                    h if TRANSPARENT_HEADS.contains(&h) => return Step::Climb(parent),
                    h if STOP_HEADS.contains(&h) => return Step::Stop,
                    _ => {}
                }
                let mut step = call_from_args(self, parent, text::callee_text(&head_text), &vals[1..], node, src);
                step.class = ctor_class(head, &vals, src);
                Step::Call(step)
            }
            "map_lit" | "ns_map_lit" => {
                let vals = values(parent);
                let Some(pos) = vals.iter().position(|v| v.id() == node.id()) else { return Step::Stop };
                if pos % 2 == 0 {
                    return Step::Stop;
                }
                let key = key_text(vals[pos - 1], src);
                let map_values: Vec<Node<'t>> = vals.iter().skip(1).step_by(2).copied().collect();
                Step::Map { key, next: parent, others: other_values(self, &map_values, node, src) }
            }
            "vec_lit" | "set_lit" | "read_cond_lit" | "splicing_read_cond_lit" | "quoting_lit" | "syn_quoting_lit" | "unquoting_lit"
            | "unquote_splicing_lit" | "derefing_lit" | "tagged_or_ctor_lit" => Step::Climb(parent),
            _ => Step::Stop,
        }
    }

    fn summarize(&self, node: Node<'_>, src: &[u8]) -> ArgSummary {
        match node.kind() {
            "num_lit" => text::parse_int(text::text(node, src)).map_or(ArgSummary::Other, ArgSummary::Int),
            "kwd_lit" => ArgSummary::Ident(text::text(node, src).to_string()),
            "sym_lit" => ArgSummary::Ident(sym_text(node, src)),
            "str_lit" => ArgSummary::Str,
            _ => ArgSummary::Other,
        }
    }
}

/// Content between the quotes of a `str_lit`.
pub(in crate::adapters::driven::treesitter) fn str_body(s: &str) -> &str {
    let s = s.strip_prefix('"').unwrap_or(s);
    s.strip_suffix('"').unwrap_or(s)
}

/// Decoded `str_lit` content.
pub(in crate::adapters::driven::treesitter) fn str_value(node: Node<'_>, src: &[u8]) -> String {
    text::unescape(str_body(text::text(node, src)))
}

/// Constructed class of `(X. …)` / `(new X …)`: the symbol name without namespace and dot.
fn ctor_class(head: Node<'_>, vals: &[Node<'_>], src: &[u8]) -> Option<String> {
    if head.kind() != "sym_lit" {
        return None;
    }
    let name = sym_name(head, src);
    if name == "new" && head.child_by_field_name("namespace").is_none() {
        return vals.get(1).filter(|c| c.kind() == "sym_lit").map(|c| sym_name(*c, src).to_string());
    }
    match name.strip_suffix('.') {
        Some(class) if !class.is_empty() && !class.ends_with('.') => Some(class.to_string()),
        _ => None,
    }
}

/// Map key text: keywords keep the colon, string keys unquoted.
fn key_text(key: Node<'_>, src: &[u8]) -> String {
    match key.kind() {
        "str_lit" => str_value(key, src),
        "sym_lit" => sym_text(key, src),
        _ => text::squash(text::text(key, src)),
    }
}
