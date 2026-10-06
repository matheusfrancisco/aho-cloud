//! Rust literal rules: string / raw-string literals, macro token trees (args split on top-level
//! commas), attributes (`#[error("…")]`), struct literals as maps, `.to_string()` / `.into()` /
//! `String::from` as transparent conversions.

use mage_core::ports::knowledge::ArgSummary;
use tree_sitter::Node;

use super::go::{binary_plus, negative_int};
use super::{CallStep, LitRules, MAX_SIBLINGS, Parts, Step, call_from_args, other_values};
use crate::adapters::driven::treesitter::text;

pub(in crate::adapters::driven::treesitter) struct RustRules;

/// Macros that panic.
const PANIC_MACROS: &[&str] = &["panic", "unreachable", "todo", "unimplemented"];
/// Zero-arg conversions whose receiver literal keeps the outer context.
const CONVERSIONS: &[&str] = &["to_string", "to_owned", "into", "as_str", "as_ref"];

impl LitRules for RustRules {
    fn is_string(&self, node: Node<'_>) -> bool {
        matches!(node.kind(), "string_literal" | "raw_string_literal")
    }

    fn decode(&self, node: Node<'_>, src: &[u8], parts: &mut Parts) {
        let raw = node.kind() == "raw_string_literal";
        let mut skip_ws = false;
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            match child.kind() {
                "string_content" => {
                    let t = text::text(child, src);
                    if raw {
                        parts.text(t);
                    } else {
                        let t = t.replace('\r', "");
                        parts.text(if skip_ws { t.trim_start() } else { &t });
                    }
                    skip_ws = false;
                }
                "escape_sequence" if !raw => {
                    let esc = text::text(child, src);
                    if esc.starts_with("\\\n") || esc.starts_with("\\\r\n") {
                        skip_ws = true;
                    } else {
                        parts.escaped(esc);
                    }
                }
                _ => {}
            }
        }
    }

    fn concat_operands<'t>(&self, node: Node<'t>, src: &[u8]) -> Option<Vec<Node<'t>>> {
        binary_plus(node, src)
    }

    fn is_paren(&self, node: Node<'_>) -> bool {
        node.kind() == "parenthesized_expression"
    }

    fn wrapper<'t>(&self, _node: Node<'t>, _src: &[u8]) -> Option<Node<'t>> {
        None
    }

    fn step<'t>(&self, node: Node<'t>, src: &[u8]) -> Step<'t> {
        let Some(parent) = node.parent() else { return Step::Stop };
        match parent.kind() {
            "arguments" => {
                let Some(call) = parent.parent().filter(|c| c.kind() == "call_expression") else { return Step::Stop };
                let Some(function) = call.child_by_field_name("function") else { return Step::Stop };
                let callee = text::callee_text(text::text(function, src));
                let args = text::named_children(parent);
                if callee == "String::from" && args.len() == 1 {
                    return Step::Climb(call);
                }
                Step::Call(call_from_args(self, call, callee, &args, node, src))
            }
            "token_tree" => self.token_tree_step(node, parent, src),
            "attribute" => Step::Attribute { name: attribute_name(parent, src), index: None, siblings: Vec::new() },
            "field_expression" if text::is_field(parent, "value", node) => {
                let field = parent.child_by_field_name("field").map(|f| text::text(f, src)).unwrap_or_default();
                let call = parent.parent().filter(|c| c.kind() == "call_expression" && text::is_field(*c, "function", parent));
                match call {
                    Some(call)
                        if CONVERSIONS.contains(&field)
                            && call.child_by_field_name("arguments").is_some_and(|a| text::named_children(a).is_empty()) =>
                    {
                        Step::Climb(call)
                    }
                    _ => Step::Stop,
                }
            }
            "parenthesized_expression"
            | "reference_expression"
            | "try_expression"
            | "await_expression"
            | "array_expression"
            | "tuple_expression"
            | "type_cast_expression" => Step::Climb(parent),
            "field_initializer" => {
                if !text::is_field(parent, "value", node) {
                    return Step::Stop;
                }
                let Some(list) = parent.parent() else { return Step::Stop };
                let Some(next) = list.parent() else { return Step::Stop };
                let key = parent.child_by_field_name("field").map(|f| text::text(f, src).to_string()).unwrap_or_default();
                let values: Vec<Node<'t>> = text::named_children(list)
                    .into_iter()
                    .filter_map(|f| match f.kind() {
                        "field_initializer" => f.child_by_field_name("value"),
                        "shorthand_field_initializer" => Some(f),
                        _ => None,
                    })
                    .collect();
                Step::Map { key, next, others: other_values(self, &values, node, src) }
            }
            "return_expression" => Step::Return,
            _ => Step::Stop,
        }
    }

    fn summarize(&self, node: Node<'_>, src: &[u8]) -> ArgSummary {
        match node.kind() {
            "integer_literal" => text::parse_int(text::text(node, src)).map_or(ArgSummary::Other, ArgSummary::Int),
            "unary_expression" | "negative_literal" => negative_int(node, src, "integer_literal"),
            "identifier" | "scoped_identifier" | "field_expression" | "self" | "shorthand_field_initializer" => {
                ArgSummary::Ident(text::squash(text::text(node, src)))
            }
            "string_literal" | "raw_string_literal" => ArgSummary::Str,
            "parenthesized_expression" | "reference_expression" => match text::named_children(node).as_slice() {
                [inner] => self.summarize(*inner, src),
                _ => ArgSummary::Other,
            },
            _ => ArgSummary::Other,
        }
    }
}

impl RustRules {
    /// `node` is a token inside token tree `tree`.
    fn token_tree_step<'t>(&self, node: Node<'t>, tree: Node<'t>, src: &[u8]) -> Step<'t> {
        let (groups, index) = token_groups(tree, node);
        let siblings: Vec<ArgSummary> = groups
            .iter()
            .enumerate()
            .filter(|(i, _)| Some(*i) != index)
            .take(MAX_SIBLINGS)
            .map(|(_, g)| self.summarize_tokens(g, src))
            .collect();
        let index = index.and_then(|i| u8::try_from(i).ok());
        let Some(holder) = tree.parent() else { return Step::Stop };
        let (call, path) = match holder.kind() {
            "macro_invocation" => match holder.child_by_field_name("macro") {
                Some(m) => (holder, text::squash(text::text(m, src))),
                None => return Step::Stop,
            },
            "attribute" => return Step::Attribute { name: attribute_name(holder, src), index, siblings },
            "token_tree" => match nested_macro_path(tree, src) {
                Some(path) => (tree, path),
                None => return Step::Climb(tree),
            },
            _ => return Step::Stop,
        };
        let last = path.rsplit("::").next().unwrap_or(&path).to_string();
        Step::Call(CallStep {
            node: call,
            callee: text::callee_text(&format!("{path}!")),
            index,
            siblings,
            class: None,
            panics: PANIC_MACROS.contains(&last.as_str()),
        })
    }

    fn summarize_tokens(&self, group: &[Node<'_>], src: &[u8]) -> ArgSummary {
        match group {
            [one] => self.summarize(*one, src),
            [minus, int] if minus.kind() == "-" && int.kind() == "integer_literal" => {
                text::parse_int(text::text(*int, src)).map_or(ArgSummary::Other, |v| ArgSummary::Int(-v))
            }
            _ if !group.is_empty() && group.iter().all(|t| matches!(t.kind(), "identifier" | "::" | "self" | "crate" | "super")) => {
                ArgSummary::Ident(group.iter().map(|t| text::text(*t, src)).collect())
            }
            _ => ArgSummary::Other,
        }
    }
}

/// Top-level comma-separated token groups of a token tree and the group index holding `node`.
fn token_groups<'t>(tree: Node<'t>, node: Node<'t>) -> (Vec<Vec<Node<'t>>>, Option<usize>) {
    let mut cursor = tree.walk();
    let children: Vec<Node<'t>> = tree.children(&mut cursor).collect();
    let inner = match children.as_slice() {
        [open, rest @ .., close] if matches!(open.kind(), "(" | "[" | "{") && matches!(close.kind(), ")" | "]" | "}") => rest,
        all => all,
    };
    let mut groups: Vec<Vec<Node<'t>>> = vec![Vec::new()];
    let mut index = None;
    for child in inner {
        if child.kind() == "," {
            groups.push(Vec::new());
            continue;
        }
        if child.is_extra() {
            continue;
        }
        if child.id() == node.id() {
            index = Some(groups.len() - 1);
        }
        if let Some(last) = groups.last_mut() {
            last.push(*child);
        }
    }
    if groups.last().is_some_and(Vec::is_empty) && groups.len() > 1 {
        groups.pop();
    }
    (groups, index)
}

/// `path ! (…)` inside an outer token tree: the macro path before the `!`.
fn nested_macro_path(tree: Node<'_>, src: &[u8]) -> Option<String> {
    let bang = tree.prev_sibling().filter(|b| b.kind() == "!")?;
    let mut cur = bang.prev_sibling().filter(|n| n.kind() == "identifier")?;
    let mut segments = vec![text::text(cur, src).to_string()];
    while let Some(sep) = cur.prev_sibling().filter(|s| s.kind() == "::") {
        match sep.prev_sibling().filter(|n| n.kind() == "identifier") {
            Some(seg) => {
                segments.push(text::text(seg, src).to_string());
                cur = seg;
            }
            None => break,
        }
    }
    segments.reverse();
    Some(segments.join("::"))
}

fn attribute_name(attribute: Node<'_>, src: &[u8]) -> String {
    text::named_children(attribute)
        .into_iter()
        .find(|n| matches!(n.kind(), "identifier" | "scoped_identifier"))
        .map(|n| text::squash(text::text(n, src)))
        .unwrap_or_default()
}
