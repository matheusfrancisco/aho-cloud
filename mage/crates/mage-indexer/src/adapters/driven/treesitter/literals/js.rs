//! JavaScript / TypeScript / TSX literal rules: strings, template literals (`${…}` holes),
//! `+` concatenation, objects as maps, `new X(…)`, `throw`, JSX attributes.

use mage_core::ports::knowledge::ArgSummary;
use tree_sitter::Node;

use super::go::{binary_plus, negative_int};
use super::{LitRules, Parts, Step, call_from_args, other_values};
use crate::adapters::driven::treesitter::text;

pub(in crate::adapters::driven::treesitter) struct JsRules;

impl LitRules for JsRules {
    fn is_string(&self, node: Node<'_>) -> bool {
        matches!(node.kind(), "string" | "template_string")
    }

    fn decode(&self, node: Node<'_>, src: &[u8], parts: &mut Parts) {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            match child.kind() {
                "string_fragment" | "html_character_reference" => parts.text(text::text(child, src)),
                "escape_sequence" => parts.escaped(text::text(child, src)),
                "template_substitution" => match text::named_children(child).first() {
                    Some(expr) => parts.interp(*expr, src),
                    None => parts.interp(child, src),
                },
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
                let Some(call) = parent.parent() else { return Step::Stop };
                let args = text::named_children(parent);
                match call.kind() {
                    "call_expression" => {
                        let Some(function) = call.child_by_field_name("function") else { return Step::Stop };
                        Step::Call(call_from_args(self, call, text::callee_text(text::text(function, src)), &args, node, src))
                    }
                    "new_expression" => {
                        let Some(ctor) = call.child_by_field_name("constructor") else { return Step::Stop };
                        let callee = text::callee_text(text::text(ctor, src));
                        let mut step = call_from_args(self, call, callee.clone(), &args, node, src);
                        step.class = Some(callee);
                        Step::Call(step)
                    }
                    _ => Step::Stop,
                }
            }
            // Tagged template: the template literal is the call's arguments.
            "call_expression" if text::is_field(parent, "arguments", node) => match parent.child_by_field_name("function") {
                Some(function) => Step::Call(call_from_args(self, parent, text::callee_text(text::text(function, src)), &[node], node, src)),
                None => Step::Stop,
            },
            "parenthesized_expression"
            | "await_expression"
            | "array"
            | "spread_element"
            | "as_expression"
            | "satisfies_expression"
            | "non_null_expression"
            | "type_assertion"
            | "jsx_expression"
            | "sequence_expression" => Step::Climb(parent),
            "ternary_expression" if !text::is_field(parent, "condition", node) => Step::Climb(parent),
            "binary_expression" => match parent.child_by_field_name("operator").map(|o| text::text(o, src)) {
                Some("||" | "??" | "&&") => Step::Climb(parent),
                _ => Step::Stop,
            },
            "pair" => {
                if !text::is_field(parent, "value", node) {
                    return Step::Stop;
                }
                let Some(object) = parent.parent() else { return Step::Stop };
                let key = parent.child_by_field_name("key").map(|k| key_text(k, src)).unwrap_or_default();
                let values: Vec<Node<'t>> = text::named_children(object)
                    .into_iter()
                    .filter_map(|p| match p.kind() {
                        "pair" => p.child_by_field_name("value"),
                        "shorthand_property_identifier" => Some(p),
                        _ => None,
                    })
                    .collect();
                Step::Map { key, next: object, others: other_values(self, &values, node, src) }
            }
            "jsx_attribute" => {
                let name = text::named_children(parent).first().map(|n| text::text(*n, src).to_string()).unwrap_or_default();
                Step::Attribute { name, index: None, siblings: Vec::new() }
            }
            "throw_statement" => Step::Throw,
            "return_statement" => Step::Return,
            "arrow_function" if text::is_field(parent, "body", node) => Step::Return,
            _ => Step::Stop,
        }
    }

    fn summarize(&self, node: Node<'_>, src: &[u8]) -> ArgSummary {
        match node.kind() {
            "number" => text::parse_int(text::text(node, src)).map_or(ArgSummary::Other, ArgSummary::Int),
            "unary_expression" => negative_int(node, src, "number"),
            "identifier" | "member_expression" | "property_identifier" | "shorthand_property_identifier" | "this" => match text::text(node, src) {
                "undefined" => ArgSummary::Other,
                t => ArgSummary::Ident(text::squash(t)),
            },
            "string" | "template_string" => ArgSummary::Str,
            "parenthesized_expression" => match text::named_children(node).as_slice() {
                [inner] => self.summarize(*inner, src),
                _ => ArgSummary::Other,
            },
            _ => ArgSummary::Other,
        }
    }
}

/// Object key text: quoted keys unquoted, computed keys verbatim.
fn key_text(key: Node<'_>, src: &[u8]) -> String {
    match key.kind() {
        "string" => {
            let mut parts = Parts::default();
            JsRules.decode(key, src, &mut parts);
            parts
                .0
                .into_iter()
                .map(|p| match p {
                    mage_core::ports::knowledge::LitPart::Text(t) => t,
                    mage_core::ports::knowledge::LitPart::Hole(_) => String::new(),
                })
                .collect()
        }
        _ => text::squash(text::text(key, src)),
    }
}
