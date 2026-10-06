//! String-literal sites with their call context (knowledge.md "Layer 3: Error sites" feeds on
//! these). One generic driver; each language supplies [`LitRules`]:
//!
//! 1. Every string literal node not already merged into an earlier site seeds a site.
//! 2. The site root climbs through concatenations (`+`, Python implicit concatenation,
//!    Clojure `(str …)`); all string operands are flattened into one `parts` list, other operands
//!    become `Hole::Concat`.
//! 3. A transparent wrapper (`fmt.Sprintf`, Python `.format` / `%`, Clojure `format`) replaces
//!    the root as the node whose context is taken.
//! 4. The context climbs through transparent containers (parens, arrays, ternaries, map values)
//!    to the innermost call / throw / raise / return / attribute.

mod clojure;
mod go;
mod js;
mod python;
mod rust;

use std::collections::HashSet;

use mage_core::ports::knowledge::{ArgSummary, Construct, Hole, LitPart, LiteralContext, LiteralSite};
use tree_sitter::Node;

pub(super) use clojure::{ClojureRules, str_value};
pub(super) use go::GoRules;
pub(super) use js::JsRules;
pub(super) use python::PythonRules;
pub(super) use rust::RustRules;

use super::symbols::SymbolTable;
use super::text::{self, Deadline, ErrorRanges};
use crate::ports::parser::ParseError;

/// Max sibling summaries per site.
const MAX_SIBLINGS: usize = 4;

/// Parts under construction: adjacent text merged, empty text dropped.
#[derive(Default)]
pub(super) struct Parts(Vec<LitPart>);

impl Parts {
    pub(super) fn text(&mut self, s: &str) {
        if s.is_empty() {
            return;
        }
        if let Some(LitPart::Text(last)) = self.0.last_mut() {
            last.push_str(s);
        } else {
            self.0.push(LitPart::Text(s.to_string()));
        }
    }

    pub(super) fn interp(&mut self, node: Node<'_>, src: &[u8]) {
        self.0.push(LitPart::Hole(Hole::Interp { expr: text::expr_text(node, src) }));
    }

    pub(super) fn concat(&mut self, node: Node<'_>, src: &[u8]) {
        self.0.push(LitPart::Hole(Hole::Concat { expr: text::expr_text(node, src) }));
    }

    /// Unescaped text appended.
    pub(super) fn escaped(&mut self, s: &str) {
        let mut out = String::with_capacity(s.len());
        text::unescape_into(s, &mut out);
        self.text(&out);
    }

    fn has_letter(&self) -> bool {
        self.0.iter().any(|p| matches!(p, LitPart::Text(t) if t.chars().any(char::is_alphabetic)))
    }
}

/// The innermost call holding the literal.
pub(super) struct CallStep<'t> {
    /// Node whose own context decides Throw / Raise / Panic (the call expression, the Clojure
    /// list, the Rust macro invocation or nested macro token tree).
    pub node: Node<'t>,
    pub callee: String,
    pub index: Option<u8>,
    /// Other args, source order, at most 4.
    pub siblings: Vec<ArgSummary>,
    /// Constructed class for `new X(…)` / Clojure `(X. …)`.
    pub class: Option<String>,
    /// `panic(…)` / `panic!` family.
    pub panics: bool,
}

/// How a node relates to its parent.
pub(super) enum Step<'t> {
    /// Transparent container: continue from this node.
    Climb(Node<'t>),
    /// Value under `key` of a map / object / struct literal / keyword argument; `others` are the
    /// other values of that map; continue from `next`.
    Map { key: String, next: Node<'t>, others: Vec<ArgSummary> },
    Call(CallStep<'t>),
    Throw,
    Raise,
    Return,
    Attribute { name: String, index: Option<u8>, siblings: Vec<ArgSummary> },
    Stop,
}

pub(super) trait LitRules {
    fn is_string(&self, node: Node<'_>) -> bool;
    /// Appends the decoded content of a string node.
    fn decode(&self, node: Node<'_>, src: &[u8], parts: &mut Parts);
    /// Operands when `node` is a concatenation.
    fn concat_operands<'t>(&self, node: Node<'t>, src: &[u8]) -> Option<Vec<Node<'t>>>;
    fn is_paren(&self, node: Node<'_>) -> bool;
    /// Transparent wrapper whose format string / receiver is `node`.
    fn wrapper<'t>(&self, node: Node<'t>, src: &[u8]) -> Option<Node<'t>>;
    /// Relation of `node` to its parent.
    fn step<'t>(&self, node: Node<'t>, src: &[u8]) -> Step<'t>;
    fn summarize(&self, node: Node<'_>, src: &[u8]) -> ArgSummary;
}

/// Literal sites of a tree in document order (first literal of each site), at most `max`.
pub(super) fn extract<R: LitRules>(
    rules: &R,
    root: Node<'_>,
    src: &[u8],
    symbols: &SymbolTable,
    errors: &ErrorRanges,
    max: usize,
    deadline: &mut Deadline,
) -> Result<Vec<LiteralSite>, ParseError> {
    let mut consumed: HashSet<usize> = HashSet::new();
    let mut out = Vec::new();
    if max == 0 {
        return Ok(out);
    }
    let mut cursor = root.walk();
    loop {
        deadline.tick()?;
        let node = cursor.node();
        // Facts inside error nodes are dropped: their subtrees are skipped.
        if !node.is_error() {
            if rules.is_string(node) && !consumed.contains(&node.id()) && !errors.contains(node.start_byte()) {
                if let Some(mut site) = site(rules, node, src, &mut consumed, symbols) {
                    site.ord = u32::try_from(out.len()).unwrap_or(u32::MAX);
                    out.push(site);
                    if out.len() >= max {
                        return Ok(out);
                    }
                }
            }
            if cursor.goto_first_child() {
                continue;
            }
        }
        loop {
            if cursor.goto_next_sibling() {
                break;
            }
            if !cursor.goto_parent() {
                return Ok(out);
            }
        }
    }
}

fn site<R: LitRules>(rules: &R, lit: Node<'_>, src: &[u8], consumed: &mut HashSet<usize>, symbols: &SymbolTable) -> Option<LiteralSite> {
    let mut root = lit;
    while let Some(parent) = concat_parent(rules, root, src) {
        root = parent;
    }
    let mut parts = Parts::default();
    flatten(rules, root, src, consumed, &mut parts);
    consumed.insert(lit.id());
    if !parts.has_letter() {
        return None;
    }
    let ctx_node = rules.wrapper(root, src).unwrap_or(root);
    Some(LiteralSite {
        ord: 0,
        parts: parts.0,
        pos: text::pos(lit),
        enclosing: symbols.enclosing(lit.start_byte()),
        context: context(rules, ctx_node, src),
    })
}

/// The concatenation directly holding `node` (through one parenthesis level).
fn concat_parent<'t, R: LitRules>(rules: &R, node: Node<'t>, src: &[u8]) -> Option<Node<'t>> {
    let mut child = node;
    let mut parent = node.parent()?;
    if rules.is_paren(parent) {
        child = parent;
        parent = parent.parent()?;
    }
    let operands = rules.concat_operands(parent, src)?;
    operands.iter().any(|o| o.id() == child.id()).then_some(parent)
}

fn flatten<R: LitRules>(rules: &R, node: Node<'_>, src: &[u8], consumed: &mut HashSet<usize>, parts: &mut Parts) {
    if rules.is_string(node) {
        consumed.insert(node.id());
        rules.decode(node, src, parts);
        return;
    }
    if let Some(operands) = rules.concat_operands(node, src) {
        for op in operands {
            flatten(rules, op, src, consumed, parts);
        }
        return;
    }
    if rules.is_paren(node) {
        let inner = text::named_children(node);
        if let [inner] = inner.as_slice() {
            if rules.concat_operands(*inner, src).is_some() && has_string_operand(rules, *inner, src) {
                flatten(rules, *inner, src, consumed, parts);
                return;
            }
        }
    }
    parts.concat(node, src);
}

fn has_string_operand<R: LitRules>(rules: &R, node: Node<'_>, src: &[u8]) -> bool {
    if rules.is_string(node) {
        return true;
    }
    if let Some(operands) = rules.concat_operands(node, src) {
        return operands.iter().any(|op| has_string_operand(rules, *op, src));
    }
    if rules.is_paren(node) {
        return text::named_children(node).iter().any(|n| has_string_operand(rules, *n, src));
    }
    false
}

fn context<R: LitRules>(rules: &R, start: Node<'_>, src: &[u8]) -> LiteralContext {
    let mut cur = start;
    let mut map_key: Option<String> = None;
    let mut map_siblings: Vec<ArgSummary> = Vec::new();
    let outcome = loop {
        match rules.step(cur, src) {
            Step::Climb(next) => cur = next,
            Step::Map { key, next, others } => {
                if map_key.is_none() {
                    map_key = Some(key);
                }
                map_siblings.extend(others);
                cur = next;
            }
            other => break other,
        }
    };
    let mut ctx = LiteralContext {
        callee: None,
        arg_index: None,
        siblings: Vec::new(),
        construct: Construct::Other,
        exception_type: None,
        map_key,
        attribute: None,
    };
    match outcome {
        Step::Call(call) => {
            let holder = if call.panics { Holder::Panic } else { holder(rules, call.node, src) };
            ctx.construct = match holder {
                Holder::Throw => Construct::Throw,
                Holder::Raise => Construct::Raise,
                Holder::Panic => Construct::Panic,
                Holder::None => Construct::Call,
            };
            ctx.exception_type = match holder {
                Holder::Throw | Holder::Raise => call.class.clone().or_else(|| Some(call.callee.clone())),
                Holder::Panic | Holder::None => call.class.clone(),
            };
            ctx.callee = Some(call.callee);
            ctx.arg_index = call.index;
            ctx.siblings = call.siblings;
        }
        Step::Throw => ctx.construct = Construct::Throw,
        Step::Raise => ctx.construct = Construct::Raise,
        Step::Return => ctx.construct = if ctx.map_key.is_some() { Construct::MapValue } else { Construct::Return },
        Step::Attribute { name, index, siblings } => {
            ctx.construct = Construct::Attribute;
            ctx.attribute = Some(name);
            ctx.arg_index = index;
            ctx.siblings = siblings;
        }
        Step::Stop | Step::Climb(_) | Step::Map { .. } => {
            ctx.construct = if ctx.map_key.is_some() { Construct::MapValue } else { Construct::Other };
        }
    }
    ctx.siblings.extend(map_siblings);
    ctx.siblings.truncate(MAX_SIBLINGS);
    ctx
}

#[derive(Clone, Copy)]
enum Holder {
    Throw,
    Raise,
    Panic,
    None,
}

/// What directly holds a call: a throw / raise operand or a `panic(…)` argument.
fn holder<R: LitRules>(rules: &R, call: Node<'_>, src: &[u8]) -> Holder {
    let mut cur = call;
    loop {
        match rules.step(cur, src) {
            Step::Climb(next) => cur = next,
            Step::Throw => return Holder::Throw,
            Step::Raise => return Holder::Raise,
            Step::Call(outer) if outer.panics => return Holder::Panic,
            _ => return Holder::None,
        }
    }
}

/// Call step from an argument list: `child` is one of `args` (comments excluded).
pub(super) fn call_from_args<'t, R: LitRules + ?Sized>(
    rules: &R,
    call: Node<'t>,
    callee: String,
    args: &[Node<'t>],
    child: Node<'t>,
    src: &[u8],
) -> CallStep<'t> {
    let index = args.iter().position(|a| a.id() == child.id());
    let siblings = args
        .iter()
        .filter(|a| a.id() != child.id())
        .take(MAX_SIBLINGS)
        .map(|a| rules.summarize(*a, src))
        .collect();
    CallStep { node: call, callee, index: index.and_then(|i| u8::try_from(i).ok()), siblings, class: None, panics: false }
}

/// Summaries of the other values of a map, at most 4.
pub(super) fn other_values<R: LitRules + ?Sized>(rules: &R, values: &[Node<'_>], except: Node<'_>, src: &[u8]) -> Vec<ArgSummary> {
    values
        .iter()
        .filter(|v| v.id() != except.id())
        .take(MAX_SIBLINGS)
        .map(|v| rules.summarize(*v, src))
        .collect()
}

/// Copies the text of `node` between its children, letting `child` handle each child.
pub(super) fn walk_content(node: Node<'_>, src: &[u8], parts: &mut Parts, mut child: impl FnMut(Node<'_>, &mut Parts)) {
    let mut at = node.start_byte();
    let mut cursor = node.walk();
    for c in node.children(&mut cursor) {
        parts.text(text::slice(src, at, c.start_byte()));
        child(c, parts);
        at = c.end_byte();
    }
    parts.text(text::slice(src, at, node.end_byte()));
}
