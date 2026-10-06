//! SQL table references in string literals, plus Go gorm `db.Table("x")` / `db.Model(&User{})`.

use std::collections::HashSet;
use std::sync::LazyLock;

use mage_core::domain::{Language, RepoPath};
use mage_core::ports::knowledge::{AppFact, AppFactRecord, FrameworkId, SymbolDef, TableOp};
use regex::Regex;
use tree_sitter::Node;

use super::url::{go_string, unescape};
use super::{AppExtractor, ManifestFile, line_of, named_children, record, text, walk_tree};

pub(super) struct SqlStrings;

static VERB: LazyLock<Option<Regex>> = LazyLock::new(|| Regex::new(r"(?i)\b(select|insert|update|delete|with)\b").ok());
/// `FROM|JOIN|INTO|UPDATE <table>`; the table may be quoted (`"users"`, `` `users` ``, `[users]`).
static TABLE: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r#"(?i)\b(FROM|JOIN|INTO|UPDATE)\s+["`\[]?([a-z_][a-z0-9_."`\]\[]*)"#).ok());

impl AppExtractor for SqlStrings {
    fn id(&self) -> FrameworkId {
        FrameworkId::SqlStrings
    }
    fn languages(&self) -> &'static [Language] {
        &[
            Language::Go,
            Language::JavaScript,
            Language::TypeScript,
            Language::Tsx,
            Language::Python,
            Language::Rust,
            Language::Clojure,
            Language::ClojureScript,
            Language::Cljc,
        ]
    }
    fn detect(&self, _root: Option<&RepoPath>, _manifests: &[ManifestFile<'_>]) -> bool {
        true
    }
    fn extract(&self, _path: &RepoPath, tree: &tree_sitter::Tree, src: &[u8], symbols: &[SymbolDef], out: &mut Vec<AppFactRecord>) {
        let mut seen: HashSet<(u32, String, TableOp)> = HashSet::new();
        let mut emit = |table: String, op: TableOp, line: u32| {
            if seen.insert((line, table.clone(), op)) {
                out.push(record(FrameworkId::SqlStrings, AppFact::TableRef { table, op }, line, symbols));
            }
        };
        walk_tree(tree.root_node(), |node| {
            if let Some(content) = string_content(node, src) {
                let line = line_of(node);
                for (table, op) in sql_tables(&content) {
                    emit(table, op, line);
                }
            } else if node.kind() == "call_expression" {
                if let Some(table) = gorm_table(node, src) {
                    emit(table, TableOp::Unknown, line_of(node));
                }
            }
        });
    }
}

/// Text of a string literal in any supported grammar; interpolations become a space.
fn string_content(node: Node<'_>, src: &[u8]) -> Option<String> {
    match node.kind() {
        "str_lit" => {
            let raw = text(node, src);
            Some(raw.strip_prefix('"').and_then(|s| s.strip_suffix('"')).unwrap_or(raw).to_owned())
        }
        "string" | "template_string" | "interpreted_string_literal" | "raw_string_literal" | "string_literal" => {
            let mut out = String::new();
            for child in named_children(node) {
                match child.kind() {
                    "string_fragment" | "string_content" | "interpreted_string_literal_content" | "raw_string_literal_content" => {
                        out.push_str(text(child, src));
                    }
                    "escape_sequence" => out.push_str(&unescape(text(child, src))),
                    "template_substitution" | "interpolation" => out.push(' '),
                    _ => {}
                }
            }
            Some(out)
        }
        _ => None,
    }
}

/// (table, op) references of an SQL-looking string.
fn sql_tables(content: &str) -> Vec<(String, TableOp)> {
    let (Some(verb), Some(table)) = (VERB.as_ref(), TABLE.as_ref()) else { return Vec::new() };
    if !verb.is_match(content) {
        return Vec::new();
    }
    let is_delete = content.trim_start().get(..6).is_some_and(|p| p.eq_ignore_ascii_case("delete"));
    table
        .captures_iter(content)
        .filter_map(|caps| {
            let keyword = caps.get(1)?.as_str().to_ascii_uppercase();
            let name: String = caps.get(2)?.as_str().chars().filter(|c| !matches!(c, '"' | '`' | '[' | ']')).collect();
            let name = name.trim_end_matches('.').to_ascii_lowercase();
            if name.is_empty() {
                return None;
            }
            let op = match keyword.as_str() {
                "INTO" => TableOp::Insert,
                "UPDATE" => TableOp::Update,
                "FROM" if is_delete => TableOp::Delete,
                _ => TableOp::Select,
            };
            Some((name, op))
        })
        .collect()
}

/// Go gorm `x.Table("name")` → `name`; `x.Model(&User{})` → `users`.
fn gorm_table(call: Node<'_>, src: &[u8]) -> Option<String> {
    let func = call.child_by_field_name("function").filter(|f| f.kind() == "selector_expression")?;
    let method = text(func.child_by_field_name("field")?, src);
    let arg = *named_children(call.child_by_field_name("arguments")?).first()?;
    match method {
        "Table" => go_string(arg, src).filter(|t| !t.is_empty()),
        "Model" => {
            let literal = if arg.kind() == "unary_expression" { arg.child_by_field_name("operand")? } else { arg };
            if literal.kind() != "composite_literal" {
                return None;
            }
            let ty = literal.child_by_field_name("type")?;
            let name = match ty.kind() {
                "type_identifier" => text(ty, src),
                "qualified_type" => text(ty.child_by_field_name("name")?, src),
                _ => return None,
            };
            Some(format!("{}s", snake_case(name)))
        }
        _ => None,
    }
}

/// `UserRole` → `user_role`, `HTTPLog` → `http_log`.
fn snake_case(name: &str) -> String {
    let chars: Vec<char> = name.chars().collect();
    let mut out = String::with_capacity(name.len() + 4);
    for (i, &c) in chars.iter().enumerate() {
        if c.is_uppercase() {
            let prev = i.checked_sub(1).map(|p| chars[p]);
            let next = chars.get(i + 1);
            let boundary = prev.is_some_and(|p| p.is_lowercase() || p.is_ascii_digit())
                || (prev.is_some_and(char::is_uppercase) && next.is_some_and(|n| n.is_lowercase()));
            if boundary {
                out.push('_');
            }
            out.extend(c.to_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}
