//! Python literal rules: strings incl. f-strings (`{expr}` holes, `{{`/`}}`), raw strings,
//! implicit and `+` concatenation, `.format(…)` / `%` wrappers, keyword arguments and dicts as
//! maps, `raise`.

use mage_core::ports::knowledge::ArgSummary;
use tree_sitter::Node;

use super::go::negative_int;
use super::{LitRules, Parts, Step, call_from_args, other_values, walk_content};
use crate::adapters::driven::treesitter::text;

pub(in crate::adapters::driven::treesitter) struct PythonRules;

impl LitRules for PythonRules {
    fn is_string(&self, node: Node<'_>) -> bool {
        node.kind() == "string"
    }

    fn decode(&self, node: Node<'_>, src: &[u8], parts: &mut Parts) {
        let prefix = node
            .child(0)
            .filter(|c| c.kind() == "string_start")
            .map(|c| text::text(c, src).trim_end_matches(['"', '\'']).to_ascii_lowercase())
            .unwrap_or_default();
        let raw = prefix.contains('r');
        let fstring = prefix.contains('f') || prefix.contains('t');
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            match child.kind() {
                "string_content" => walk_content(child, src, parts, |c, parts| match c.kind() {
                    "escape_sequence" if !raw => parts.escaped(text::text(c, src)),
                    "escape_interpolation" if fstring => parts.text(&text::text(c, src)[..1]),
                    _ => parts.text(text::text(c, src)),
                }),
                "interpolation" => match child.child_by_field_name("expression") {
                    Some(expr) => parts.interp(expr, src),
                    None => parts.interp(child, src),
                },
                _ => {}
            }
        }
    }

    fn concat_operands<'t>(&self, node: Node<'t>, src: &[u8]) -> Option<Vec<Node<'t>>> {
        match node.kind() {
            "concatenated_string" => Some(text::named_children(node).into_iter().filter(|c| c.kind() == "string").collect()),
            "binary_operator" => {
                let op = node.child_by_field_name("operator")?;
                if text::text(op, src) != "+" {
                    return None;
                }
                Some(vec![node.child_by_field_name("left")?, node.child_by_field_name("right")?])
            }
            _ => None,
        }
    }

    fn is_paren(&self, node: Node<'_>) -> bool {
        node.kind() == "parenthesized_expression"
    }

    fn wrapper<'t>(&self, node: Node<'t>, src: &[u8]) -> Option<Node<'t>> {
        let parent = node.parent()?;
        match parent.kind() {
            // "…".format(…)
            "attribute" if text::is_field(parent, "object", node) => {
                let attr = parent.child_by_field_name("attribute")?;
                let call = parent.parent().filter(|c| c.kind() == "call" && text::is_field(*c, "function", parent))?;
                (text::text(attr, src) == "format").then_some(call)
            }
            // "…" % args
            "binary_operator" if text::is_field(parent, "left", node) => {
                let op = parent.child_by_field_name("operator")?;
                (text::text(op, src) == "%").then_some(parent)
            }
            _ => None,
        }
    }

    fn step<'t>(&self, node: Node<'t>, src: &[u8]) -> Step<'t> {
        let Some(parent) = node.parent() else { return Step::Stop };
        match parent.kind() {
            "argument_list" => {
                let Some(call) = parent.parent().filter(|c| c.kind() == "call") else { return Step::Stop };
                let Some(function) = call.child_by_field_name("function") else { return Step::Stop };
                Step::Call(call_from_args(self, call, text::callee_text(text::text(function, src)), &text::named_children(parent), node, src))
            }
            "keyword_argument" if text::is_field(parent, "value", node) => {
                let key = parent.child_by_field_name("name").map(|n| text::text(n, src).to_string()).unwrap_or_default();
                Step::Map { key, next: parent, others: Vec::new() }
            }
            "pair" => {
                if !text::is_field(parent, "value", node) {
                    return Step::Stop;
                }
                let Some(dict) = parent.parent() else { return Step::Stop };
                let key = parent.child_by_field_name("key").map(|k| key_text(k, src)).unwrap_or_default();
                let values: Vec<Node<'t>> = text::named_children(dict).into_iter().filter_map(|p| p.child_by_field_name("value")).collect();
                Step::Map { key, next: dict, others: other_values(self, &values, node, src) }
            }
            "parenthesized_expression" | "list" | "tuple" | "set" | "await" | "expression_list" | "boolean_operator" | "list_splat" => Step::Climb(parent),
            "conditional_expression" => {
                // `a if cond else b`: the condition is the second named child.
                let is_cond = text::named_children(parent).get(1).is_some_and(|c| c.id() == node.id());
                if is_cond { Step::Stop } else { Step::Climb(parent) }
            }
            "raise_statement" => Step::Raise,
            "return_statement" => Step::Return,
            _ => Step::Stop,
        }
    }

    fn summarize(&self, node: Node<'_>, src: &[u8]) -> ArgSummary {
        match node.kind() {
            "integer" => text::parse_int(text::text(node, src)).map_or(ArgSummary::Other, ArgSummary::Int),
            "unary_operator" => negative_int(node, src, "integer"),
            "identifier" | "attribute" => ArgSummary::Ident(text::squash(text::text(node, src))),
            "string" | "concatenated_string" => ArgSummary::Str,
            "keyword_argument" => match node.child_by_field_name("value") {
                Some(value) => self.summarize(value, src),
                None => ArgSummary::Other,
            },
            "parenthesized_expression" => match text::named_children(node).as_slice() {
                [inner] => self.summarize(*inner, src),
                _ => ArgSummary::Other,
            },
            _ => ArgSummary::Other,
        }
    }
}

fn key_text(key: Node<'_>, src: &[u8]) -> String {
    if key.kind() == "string" {
        let mut parts = Parts::default();
        PythonRules.decode(key, src, &mut parts);
        return parts
            .0
            .into_iter()
            .filter_map(|p| match p {
                mage_core::ports::knowledge::LitPart::Text(t) => Some(t),
                mage_core::ports::knowledge::LitPart::Hole(_) => None,
            })
            .collect();
    }
    text::squash(text::text(key, src))
}
