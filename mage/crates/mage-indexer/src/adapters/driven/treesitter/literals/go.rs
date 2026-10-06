//! Go literal rules: interpreted + raw strings, `+` concatenation, `fmt.Sprintf` wrapper,
//! composite-literal keyed elements as map values.

use mage_core::ports::knowledge::ArgSummary;
use tree_sitter::Node;

use super::{LitRules, Parts, Step, call_from_args, other_values};
use crate::adapters::driven::treesitter::text;

pub(in crate::adapters::driven::treesitter) struct GoRules;

impl LitRules for GoRules {
    fn is_string(&self, node: Node<'_>) -> bool {
        matches!(node.kind(), "interpreted_string_literal" | "raw_string_literal")
    }

    fn decode(&self, node: Node<'_>, src: &[u8], parts: &mut Parts) {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            match child.kind() {
                "interpreted_string_literal_content" => parts.text(text::text(child, src)),
                "raw_string_literal_content" => parts.text(&text::text(child, src).replace('\r', "")),
                "escape_sequence" => parts.escaped(text::text(child, src)),
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

    fn wrapper<'t>(&self, node: Node<'t>, src: &[u8]) -> Option<Node<'t>> {
        let args = node.parent().filter(|p| p.kind() == "argument_list")?;
        let call = args.parent().filter(|c| c.kind() == "call_expression")?;
        let function = call.child_by_field_name("function")?;
        let first = text::named_children(args).into_iter().next()?;
        (text::squash(text::text(function, src)) == "fmt.Sprintf" && first.id() == node.id()).then_some(call)
    }

    fn step<'t>(&self, node: Node<'t>, src: &[u8]) -> Step<'t> {
        let Some(parent) = node.parent() else { return Step::Stop };
        match parent.kind() {
            "argument_list" => {
                let Some(call) = parent.parent().filter(|c| c.kind() == "call_expression") else { return Step::Stop };
                let Some(function) = call.child_by_field_name("function") else { return Step::Stop };
                let callee = text::callee_text(text::text(function, src));
                let mut step = call_from_args(self, call, callee, &text::named_children(parent), node, src);
                step.panics = step.callee == "panic";
                Step::Call(step)
            }
            "parenthesized_expression" | "literal_element" | "expression_list" => Step::Climb(parent),
            "unary_expression" if parent.child_by_field_name("operator").is_some_and(|o| text::text(o, src) == "&") => Step::Climb(parent),
            "literal_value" => Step::Climb(parent),
            "composite_literal" => Step::Climb(parent),
            "keyed_element" => {
                if !text::is_field(parent, "value", node) {
                    return Step::Stop;
                }
                let Some(map) = parent.parent() else { return Step::Stop };
                let key = parent.child_by_field_name("key").map(|k| map_key(k, src)).unwrap_or_default();
                let values: Vec<Node<'t>> = text::named_children(map)
                    .into_iter()
                    .filter(|e| e.kind() == "keyed_element")
                    .filter_map(|e| e.child_by_field_name("value"))
                    .collect();
                Step::Map { key, next: map, others: other_values(self, &values, node, src) }
            }
            "return_statement" => Step::Return,
            _ => Step::Stop,
        }
    }

    fn summarize(&self, node: Node<'_>, src: &[u8]) -> ArgSummary {
        match node.kind() {
            "int_literal" => text::parse_int(text::text(node, src)).map_or(ArgSummary::Other, ArgSummary::Int),
            "unary_expression" => negative_int(node, src, "int_literal"),
            "identifier" | "selector_expression" | "qualified_type" | "field_identifier" | "type_identifier" => match text::text(node, src) {
                "nil" | "true" | "false" => ArgSummary::Other,
                t => ArgSummary::Ident(text::squash(t)),
            },
            "interpreted_string_literal" | "raw_string_literal" => ArgSummary::Str,
            "literal_element" | "parenthesized_expression" => match text::named_children(node).as_slice() {
                [inner] => self.summarize(*inner, src),
                _ => ArgSummary::Other,
            },
            _ => ArgSummary::Other,
        }
    }
}

/// Key text of a keyed element: string keys unquoted.
fn map_key(key: Node<'_>, src: &[u8]) -> String {
    let inner = match text::named_children(key).as_slice() {
        [inner] if key.kind() == "literal_element" => *inner,
        _ => key,
    };
    match inner.kind() {
        "interpreted_string_literal" => text::unescape(text::unquote(text::text(inner, src))),
        "raw_string_literal" => text::unquote(text::text(inner, src)).to_string(),
        _ => text::squash(text::text(inner, src)),
    }
}

/// `[left, right]` of a `+` binary expression (Go / JS / TS / Rust share the shape).
pub(super) fn binary_plus<'t>(node: Node<'t>, src: &[u8]) -> Option<Vec<Node<'t>>> {
    if node.kind() != "binary_expression" {
        return None;
    }
    let op = node.child_by_field_name("operator")?;
    if text::text(op, src) != "+" {
        return None;
    }
    Some(vec![node.child_by_field_name("left")?, node.child_by_field_name("right")?])
}

/// `-<int>` → `Int(-n)`.
pub(super) fn negative_int(node: Node<'_>, src: &[u8], int_kind: &str) -> ArgSummary {
    let t = text::squash(text::text(node, src));
    let operand = node.child_by_field_name("operand").or_else(|| node.child_by_field_name("argument")).or_else(|| node.named_child(0));
    match (t.strip_prefix('-'), operand) {
        (Some(_), Some(op)) if op.kind() == int_kind => text::parse_int(text::text(op, src)).map_or(ArgSummary::Other, |v| ArgSummary::Int(-v)),
        _ => ArgSummary::Other,
    }
}
