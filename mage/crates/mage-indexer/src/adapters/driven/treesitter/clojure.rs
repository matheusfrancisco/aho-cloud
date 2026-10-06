//! Clojure / ClojureScript / CLJC symbols and refs: a custom walker over the purely syntactic
//! grammar (knowledge.md "Layer 2: Symbols", Clojure row). Top-level forms dispatch on their head
//! symbol; refs are list heads, re-frame dispatches and `ns` requires; the `ns` form's alias table
//! resolves `z/foo`, `::foo` and `::z/foo`.

use std::collections::HashMap;

use mage_core::domain::LineRange;
use mage_core::ports::knowledge::{NameRef, RefKind, SymbolKind};
use tree_sitter::Node;

use super::literals::str_value;
use super::registry::Family;
use super::symbols::{RefDraft, SymDraft, SymbolTable, finish_refs};
use super::text::{self, Deadline, ErrorRanges};
use crate::ports::parser::ParseError;

const RE_FRAME: &str = "re-frame.core";
const REGISTRATIONS: &[&str] = &["reg-event-db", "reg-event-fx", "reg-event-ctx", "reg-sub", "reg-fx", "reg-cofx"];
const DISPATCHERS: &[&str] = &["dispatch", "dispatch-sync", "subscribe"];

/// Item children (`value` field) of a collection / list form.
pub(super) fn values<'t>(node: Node<'t>) -> Vec<Node<'t>> {
    let mut cursor = node.walk();
    node.children_by_field_name("value", &mut cursor).collect()
}

/// Name part of a symbol (`foo` in `z/foo`).
pub(super) fn sym_name<'s>(node: Node<'_>, src: &'s [u8]) -> &'s str {
    node.child_by_field_name("name").map_or_else(|| text::text(node, src), |n| text::text(n, src))
}

/// Namespace part of a symbol (`z` in `z/foo`).
fn sym_ns<'s>(node: Node<'_>, src: &'s [u8]) -> Option<&'s str> {
    node.child_by_field_name("namespace").map(|n| text::text(n, src))
}

/// Symbol text without metadata (`z/foo`).
pub(super) fn sym_text(node: Node<'_>, src: &[u8]) -> String {
    match sym_ns(node, src) {
        Some(ns) => format!("{ns}/{}", sym_name(node, src)),
        None => sym_name(node, src).to_string(),
    }
}

/// Per-file namespace context from the `ns` form.
#[derive(Default)]
struct NsContext {
    ns: Option<String>,
    /// alias → lib
    aliases: HashMap<String, String>,
    /// referred var → lib
    refers: HashMap<String, String>,
    /// libs required with `:refer :all`
    refer_all: Vec<String>,
}

impl NsContext {
    /// Lib of a symbol namespace (alias resolved).
    fn resolve_ns<'a>(&'a self, ns: &'a str) -> &'a str {
        self.aliases.get(ns).map_or(ns, String::as_str)
    }

    /// Resolved lib of a symbol: `z/foo` → `x.y`, referred `foo` → its lib.
    fn lib_of(&self, node: Node<'_>, src: &[u8]) -> Option<String> {
        match sym_ns(node, src) {
            Some(ns) => Some(self.resolve_ns(ns).to_string()),
            None => self.refers.get(sym_name(node, src)).cloned(),
        }
    }

    /// Head is `lib/name` (through any alias / refer) for `name ∈ names`.
    fn is_var_of(&self, node: Node<'_>, src: &[u8], lib: &str, names: &[&str]) -> bool {
        if node.kind() != "sym_lit" || !names.contains(&sym_name(node, src)) {
            return false;
        }
        match self.lib_of(node, src) {
            Some(l) => l == lib,
            None => self.refer_all.iter().any(|l| l == lib),
        }
    }

    /// Fully-qualified keyword: `::foo` → `:<ns>/foo`, `::z/foo` → `:x.y/foo`.
    fn keyword(&self, node: Node<'_>, src: &[u8]) -> String {
        let auto = node.child_by_field_name("marker").is_some_and(|m| text::text(m, src) == "::");
        let name = node.child_by_field_name("name").map_or("", |n| text::text(n, src));
        let ns = node.child_by_field_name("namespace").map(|n| text::text(n, src));
        match (auto, ns) {
            (true, Some(alias)) => format!(":{}/{name}", self.resolve_ns(alias)),
            (true, None) => match &self.ns {
                Some(current) => format!(":{current}/{name}"),
                None => format!(":{name}"),
            },
            (false, Some(ns)) => format!(":{ns}/{name}"),
            (false, None) => format!(":{name}"),
        }
    }
}

/// Head symbol of a list form.
fn head(list: Node<'_>) -> Option<Node<'_>> {
    values(list).into_iter().next().filter(|h| h.kind() == "sym_lit")
}

/// Top-level forms (comments and `#_` discards excluded).
fn top_forms(root: Node<'_>) -> Vec<Node<'_>> {
    text::named_children(root).into_iter().filter(|n| n.kind() != "dis_expr").collect()
}

/// Reads the `ns` form: name, `:require` aliases / refers, Import refs.
fn read_ns(root: Node<'_>, src: &[u8], refs: &mut Vec<RefDraft>) -> NsContext {
    let mut ctx = NsContext::default();
    let Some(form) = top_forms(root).into_iter().find(|f| f.kind() == "list_lit" && head(*f).is_some_and(|h| sym_text(h, src) == "ns")) else {
        return ctx;
    };
    let vals = values(form);
    ctx.ns = vals.get(1).filter(|n| n.kind() == "sym_lit").map(|n| sym_text(*n, src));
    for clause in vals.iter().skip(2).filter(|c| c.kind() == "list_lit") {
        let items = values(*clause);
        let Some(kw) = items.first().filter(|k| k.kind() == "kwd_lit") else { continue };
        let kw = text::text(*kw, src);
        if !matches!(kw, ":require" | ":require-macros" | ":use") {
            continue;
        }
        for spec in items.iter().skip(1) {
            read_libspec(*spec, src, &mut ctx, refs);
        }
    }
    ctx
}

fn read_libspec(spec: Node<'_>, src: &[u8], ctx: &mut NsContext, refs: &mut Vec<RefDraft>) {
    match spec.kind() {
        "sym_lit" => push_import(spec, sym_text(spec, src), None, refs),
        "str_lit" => push_import(spec, str_value(spec, src), None, refs),
        "vec_lit" | "list_lit" => {
            let items = values(spec);
            let Some(lib_node) = items.first() else { return };
            let lib = match lib_node.kind() {
                "sym_lit" => sym_text(*lib_node, src),
                "str_lit" => str_value(*lib_node, src),
                _ => return,
            };
            let mut alias = None;
            let mut i = 1;
            while i + 1 < items.len() {
                let opt = text::text(items[i], src);
                let arg = items[i + 1];
                match opt {
                    ":as" | ":as-alias" if arg.kind() == "sym_lit" => {
                        let a = sym_text(arg, src);
                        ctx.aliases.insert(a.clone(), lib.clone());
                        alias = Some(a);
                    }
                    ":refer" | ":refer-macros" if arg.kind() == "vec_lit" => {
                        for r in values(arg).into_iter().filter(|r| r.kind() == "sym_lit") {
                            ctx.refers.insert(sym_text(r, src), lib.clone());
                        }
                    }
                    ":refer" if text::text(arg, src) == ":all" => ctx.refer_all.push(lib.clone()),
                    _ => {}
                }
                i += 2;
            }
            push_import(*lib_node, lib, alias, refs);
        }
        _ => {}
    }
}

fn push_import(node: Node<'_>, lib: String, alias: Option<String>, refs: &mut Vec<RefDraft>) {
    refs.push(RefDraft { byte: node.start_byte(), name: lib, qualifier: alias, kind: RefKind::Import, pos: text::pos(node) });
}

/// Symbols and refs of a Clojure-family blob.
pub(super) fn extract(
    root: Node<'_>,
    src: &[u8],
    errors: &ErrorRanges,
    limits: (usize, usize),
    deadline: &mut Deadline,
) -> Result<(SymbolTable, Vec<NameRef>), ParseError> {
    let (max_symbols, max_refs) = limits;
    let mut refs = Vec::new();
    let ctx = read_ns(root, src, &mut refs);
    let mut drafts = Vec::new();
    for form in top_forms(root) {
        if form.kind() == "list_lit" && !errors.contains(form.start_byte()) {
            if let Some(d) = top_symbol(form, src, &ctx) {
                drafts.push(d);
            }
        }
    }
    let table = SymbolTable::build(drafts, Family::Clojure, max_symbols);
    if max_refs == 0 {
        return Ok((table, Vec::new()));
    }
    collect_refs(root, src, &ctx, errors, &mut refs, deadline)?;
    let refs = finish_refs(refs, &table, max_refs);
    Ok((table, refs))
}

fn top_symbol(form: Node<'_>, src: &[u8], ctx: &NsContext) -> Option<SymDraft> {
    let vals = values(form);
    let head = vals.first().filter(|h| h.kind() == "sym_lit")?;
    let head_name = sym_name(*head, src);
    let head_ns = sym_ns(*head, src);
    let name_node = vals.get(1).copied();
    let ns_prefix = |name: &str| match &ctx.ns {
        Some(ns) => format!("{ns}/{name}"),
        None => name.to_string(),
    };

    // re-frame registrations through any alias / refer of re-frame.core.
    if ctx.is_var_of(*head, src, RE_FRAME, REGISTRATIONS) {
        let key = name_node.filter(|n| n.kind() == "kwd_lit")?;
        let name = ctx.keyword(key, src);
        return draft(form, key, name.clone(), name, SymbolKind::Registered, None, true, src);
    }
    // clojure.spec `(s/def ::kw …)`.
    if head_name == "def" && head_ns.is_some_and(|ns| matches!(ns, "s" | "spec") || ctx.resolve_ns(ns).ends_with(".spec.alpha")) {
        let key = name_node.filter(|n| n.kind() == "kwd_lit")?;
        let name = ctx.keyword(key, src);
        return draft(form, key, name.clone(), name, SymbolKind::TypeAlias, None, true, src);
    }
    if head_ns.is_some() {
        return None;
    }
    let name_node = name_node.filter(|n| n.kind() == "sym_lit")?;
    let name = sym_text(name_node, src);
    let doc_at = |i: usize| vals.get(i).filter(|d| d.kind() == "str_lit").map(|d| str_value(*d, src));
    let private = is_private(name_node, src);
    let (kind, doc, exported) = match head_name {
        "ns" => {
            return draft(form, name_node, name.clone(), name, SymbolKind::Module, doc_at(2), true, src);
        }
        "defn" | "defmacro" | "defmulti" => {
            let kind = if head_name == "defmacro" { SymbolKind::Macro } else { SymbolKind::Function };
            (kind, doc_at(2), !private)
        }
        "defn-" => (SymbolKind::Function, doc_at(2), false),
        "defmethod" => (SymbolKind::Function, None, !private),
        "def" | "defonce" => (SymbolKind::Variable, if vals.len() >= 4 { doc_at(2) } else { None }, !private),
        "defprotocol" => (SymbolKind::Interface, doc_at(2), !private),
        "defrecord" | "deftype" => (SymbolKind::Struct, None, !private),
        "deftest" => (SymbolKind::Test, None, false),
        _ => return None,
    };
    let name_pos_node = name_node.child_by_field_name("name").unwrap_or(name_node);
    draft(form, name_pos_node, name.clone(), ns_prefix(&name), kind, doc, exported, src)
}

/// `^:private` / `^{:private true}` metadata on the name symbol.
fn is_private(name: Node<'_>, src: &[u8]) -> bool {
    let mut cursor = name.walk();
    name.children_by_field_name("meta", &mut cursor).any(|m| {
        let t = text::text(m, src);
        t.contains(":private") && !t.contains(":private false")
    })
}

#[allow(clippy::too_many_arguments)]
fn draft(form: Node<'_>, name_node: Node<'_>, name: String, qualified: String, kind: SymbolKind, doc: Option<String>, exported: bool, src: &[u8]) -> Option<SymDraft> {
    let start = text::start_line(form);
    let span = LineRange::new(start, text::end_line(form).max(start)).ok()?;
    let first_line = text::text(form, src).lines().next().unwrap_or("").trim_end();
    Some(SymDraft {
        start: form.start_byte(),
        end: form.end_byte(),
        name_byte: name_node.start_byte(),
        name,
        kind,
        span,
        name_pos: text::pos(name_node),
        signature: (!first_line.is_empty()).then(|| text::head_chars(first_line, text::SIGNATURE_MAX_CHARS)),
        doc: doc.map(|d| d.trim().to_string()).filter(|d| !d.is_empty()).map(|d| text::head_bytes(&d, text::DOC_MAX_BYTES)),
        exported,
        owner_type: None,
        qualified: Some(qualified),
    })
}

/// Call refs (heads of nested lists), Dispatch refs.
fn collect_refs(root: Node<'_>, src: &[u8], ctx: &NsContext, errors: &ErrorRanges, refs: &mut Vec<RefDraft>, deadline: &mut Deadline) -> Result<(), ParseError> {
    let mut cursor = root.walk();
    let mut depth = 0usize;
    loop {
        deadline.tick()?;
        let node = cursor.node();
        let skip = node.is_error() || errors.contains(node.start_byte());
        if !skip {
            match node.kind() {
                "list_lit" | "anon_fn_lit" if depth > 1 || node.kind() == "anon_fn_lit" => list_refs(node, src, ctx, refs),
                "map_lit" => map_refs(node, src, ctx, refs),
                _ => {}
            }
        }
        if !skip && cursor.goto_first_child() {
            depth += 1;
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                break;
            }
            if !cursor.goto_parent() {
                return Ok(());
            }
            depth -= 1;
        }
    }
}

fn list_refs(list: Node<'_>, src: &[u8], ctx: &NsContext, refs: &mut Vec<RefDraft>) {
    let vals = values(list);
    let Some(h) = vals.first().filter(|h| h.kind() == "sym_lit") else { return };
    let name_node = h.child_by_field_name("name").unwrap_or(*h);
    refs.push(RefDraft {
        byte: h.start_byte(),
        name: sym_name(*h, src).to_string(),
        qualifier: ctx.lib_of(*h, src),
        kind: RefKind::Call,
        pos: text::pos(name_node),
    });
    let dispatcher = (sym_ns(*h, src).is_none() && DISPATCHERS.contains(&sym_name(*h, src))) || ctx.is_var_of(*h, src, RE_FRAME, DISPATCHERS);
    if dispatcher {
        if let Some(event) = vals.get(1) {
            push_dispatch(*event, src, ctx, refs);
        }
    }
}

/// `[:kw …]` → Dispatch ref on `:kw`.
fn push_dispatch(event: Node<'_>, src: &[u8], ctx: &NsContext, refs: &mut Vec<RefDraft>) {
    if event.kind() != "vec_lit" {
        return;
    }
    if let Some(kw) = values(event).into_iter().next().filter(|k| k.kind() == "kwd_lit") {
        refs.push(RefDraft { byte: kw.start_byte(), name: ctx.keyword(kw, src), qualifier: None, kind: RefKind::Dispatch, pos: text::pos(kw) });
    }
}

/// `{:dispatch [:kw]}`, `{:dispatch-n [[:a] [:b]]}`, `{:fx [[:dispatch [:kw]]]}`.
fn map_refs(map: Node<'_>, src: &[u8], ctx: &NsContext, refs: &mut Vec<RefDraft>) {
    let vals = values(map);
    for pair in vals.chunks(2) {
        let [key, value] = pair else { continue };
        if key.kind() != "kwd_lit" {
            continue;
        }
        match text::text(*key, src) {
            ":dispatch" => push_dispatch(*value, src, ctx, refs),
            ":dispatch-n" if value.kind() == "vec_lit" => {
                for event in values(*value) {
                    push_dispatch(event, src, ctx, refs);
                }
            }
            ":fx" if value.kind() == "vec_lit" => {
                for effect in values(*value).into_iter().filter(|e| e.kind() == "vec_lit") {
                    let items = values(effect);
                    if let [fx, event, ..] = items.as_slice() {
                        if text::text(*fx, src) == ":dispatch" {
                            push_dispatch(*event, src, ctx, refs);
                        }
                    }
                }
            }
            _ => {}
        }
    }
}
