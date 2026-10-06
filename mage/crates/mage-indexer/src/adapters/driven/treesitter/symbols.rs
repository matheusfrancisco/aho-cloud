//! Symbols and name refs: the symbol table shared by every language, and the query-driven
//! extraction for Go / TypeScript / JavaScript / Python / Rust (knowledge.md "Layer 2: Symbols").

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::time::Instant;

use mage_core::domain::{LineRange, RepoPath};
use mage_core::ports::knowledge::{NameRef, RefKind, SymbolDef, SymbolKind};
use tree_sitter::{Node, QueryCursor, QueryCursorOptions, QueryCursorState, StreamingIterator};

use super::registry::{Family, Queries, Role, RoleQuery};
use super::text::{self, Deadline, ErrorRanges};
use crate::ports::parser::ParseError;

/// A symbol before ords are assigned.
pub(super) struct SymDraft {
    /// Byte range of the declaration (incl. `export` / decorators).
    pub start: usize,
    pub end: usize,
    pub name_byte: usize,
    pub name: String,
    pub kind: SymbolKind,
    pub span: LineRange,
    pub name_pos: (u32, u32),
    pub signature: Option<String>,
    pub doc: Option<String>,
    pub exported: bool,
    /// Semantic container by type name (Go receiver, Rust impl type); `None` = structural parent.
    pub owner_type: Option<String>,
    /// Fixed `qualified` (Clojure); `None` = container chain + name.
    pub qualified: Option<String>,
}

/// A name ref before ords are assigned.
pub(super) struct RefDraft {
    pub byte: usize,
    pub name: String,
    pub qualifier: Option<String>,
    pub kind: RefKind,
    pub pos: (u32, u32),
}

/// Symbols of one blob in ord order, with the structural nesting used for `enclosing`.
pub(super) struct SymbolTable {
    pub defs: Vec<SymbolDef>,
    ranges: Vec<(usize, usize)>,
    parents: Vec<Option<u32>>,
    /// Kept ords are `< limit` (`max_symbols`).
    limit: usize,
}

impl SymbolTable {
    /// Orders drafts by position (outer before inner), assigns ords, containers and qualified
    /// names, keeps the first `limit`.
    pub(super) fn build(mut drafts: Vec<SymDraft>, family: Family, limit: usize) -> Self {
        drafts.sort_by(|a, b| a.start.cmp(&b.start).then(b.end.cmp(&a.end)).then(a.name_byte.cmp(&b.name_byte)));
        let ranges: Vec<(usize, usize)> = drafts.iter().map(|d| (d.start, d.end)).collect();
        let mut parents: Vec<Option<u32>> = Vec::with_capacity(drafts.len());
        let mut stack: Vec<usize> = Vec::new();
        for (i, &(start, end)) in ranges.iter().enumerate() {
            while let Some(&top) = stack.last() {
                let (ts, te) = ranges[top];
                if ts <= start && end <= te && (ts, te) != (start, end) {
                    break;
                }
                stack.pop();
            }
            parents.push(stack.last().map(|&p| ord(p)));
            stack.push(i);
        }
        let containers: Vec<Option<u32>> = drafts
            .iter()
            .enumerate()
            .map(|(i, d)| match &d.owner_type {
                Some(owner) => drafts
                    .iter()
                    .position(|t| {
                        t.name == *owner
                            && matches!(
                                t.kind,
                                SymbolKind::Struct | SymbolKind::Interface | SymbolKind::TypeAlias | SymbolKind::Enum | SymbolKind::Trait | SymbolKind::Class
                            )
                    })
                    .filter(|&t| t != i)
                    .map(ord),
                None => parents[i],
            })
            .collect();
        let sep = family.separator();
        let mut qualified: Vec<Option<String>> = vec![None; drafts.len()];
        for i in 0..drafts.len() {
            qualified_of(i, &drafts, &containers, sep, &mut qualified, 0);
        }
        let mut table = Self { defs: Vec::new(), ranges, parents, limit };
        let defs: Vec<SymbolDef> = drafts
            .into_iter()
            .enumerate()
            .take(limit)
            .map(|(i, d)| SymbolDef {
                ord: ord(i),
                qualified: qualified[i].take().unwrap_or_else(|| d.name.clone()),
                name: d.name,
                kind: d.kind,
                container: table.cap(containers[i]),
                span: d.span,
                name_pos: d.name_pos,
                signature: d.signature,
                doc: d.doc,
                exported: d.exported,
            })
            .collect();
        table.defs = defs;
        table
    }

    /// Innermost kept symbol whose declaration contains `byte`.
    pub(super) fn enclosing(&self, byte: usize) -> Option<u32> {
        let idx = self.ranges.partition_point(|(start, _)| *start <= byte);
        let mut cur = idx.checked_sub(1).map(ord);
        while let Some(c) = cur {
            let (start, end) = self.ranges[c as usize];
            if start <= byte && byte < end {
                return self.cap(Some(c));
            }
            cur = self.parents[c as usize];
        }
        None
    }

    /// Nearest kept structural ancestor-or-self of `ord`.
    fn cap(&self, mut cur: Option<u32>) -> Option<u32> {
        while let Some(c) = cur {
            if (c as usize) < self.limit {
                return Some(c);
            }
            cur = self.parents[c as usize];
        }
        None
    }
}

fn ord(i: usize) -> u32 {
    u32::try_from(i).unwrap_or(u32::MAX)
}

fn qualified_of(i: usize, drafts: &[SymDraft], containers: &[Option<u32>], sep: &str, memo: &mut [Option<String>], depth: usize) -> String {
    if let Some(q) = &memo[i] {
        return q.clone();
    }
    let d = &drafts[i];
    let q = if let Some(fixed) = &d.qualified {
        fixed.clone()
    } else {
        let prefix = match containers[i] {
            Some(c) if depth < 64 && (c as usize) != i => Some(qualified_of(c as usize, drafts, containers, sep, memo, depth + 1)),
            _ => d.owner_type.clone(),
        };
        match prefix {
            Some(p) => format!("{p}{sep}{}", d.name),
            None => d.name.clone(),
        }
    };
    memo[i] = Some(q.clone());
    q
}

/// Name refs ordered by position, ords assigned, at most `max`.
pub(super) fn finish_refs(mut drafts: Vec<RefDraft>, table: &SymbolTable, max: usize) -> Vec<NameRef> {
    drafts.sort_by(|a, b| a.byte.cmp(&b.byte).then(a.kind.cmp(&b.kind)).then(a.name.cmp(&b.name)));
    drafts.dedup_by(|b, a| a.byte == b.byte && a.kind == b.kind && a.name == b.name && a.qualifier == b.qualifier);
    drafts
        .into_iter()
        .take(max)
        .enumerate()
        .map(|(i, d)| NameRef { ord: ord(i), name: d.name, qualifier: d.qualifier, kind: d.kind, pos: d.pos, enclosing: table.enclosing(d.byte) })
        .collect()
}

/// Matches of `query` over `root` as `(role, node)` capture lists.
fn run_query<'t>(q: &RoleQuery, root: Node<'t>, src: &[u8], deadline: &Deadline, mut f: impl FnMut(&[(Role, Node<'t>)])) -> Result<(), ParseError> {
    let timed_out = Cell::new(false);
    let at: Instant = deadline.instant();
    let mut progress = |_: &QueryCursorState| {
        let late = Instant::now() >= at;
        if late {
            timed_out.set(true);
        }
        late
    };
    let mut cursor = QueryCursor::new();
    let options = QueryCursorOptions::new().progress_callback(&mut progress);
    let mut matches = cursor.matches_with_options(&q.query, root, src, options);
    let mut captures: Vec<(Role, Node<'t>)> = Vec::new();
    while let Some(m) = matches.next() {
        captures.clear();
        captures.extend(m.captures.iter().filter_map(|c| q.roles.get(c.index as usize).map(|r| (*r, c.node))));
        f(&captures);
    }
    drop(matches);
    if timed_out.get() { Err(ParseError::Timeout) } else { deadline.check() }
}

/// Symbols and refs of a query-driven family; no refs query runs when `max_refs` is 0.
#[allow(clippy::too_many_arguments)]
pub(super) fn extract(
    family: Family,
    queries: &Queries,
    root: Node<'_>,
    src: &[u8],
    path: &RepoPath,
    errors: &ErrorRanges,
    limits: (usize, usize),
    deadline: &Deadline,
) -> Result<(SymbolTable, Vec<NameRef>), ParseError> {
    let (max_symbols, max_refs) = limits;
    let mut seen: HashSet<(usize, usize)> = HashSet::new();
    let mut name_nodes: HashSet<usize> = HashSet::new();
    let mut drafts = Vec::new();
    run_query(&queries.symbols, root, src, deadline, |caps| {
        let def = caps.iter().find_map(|(r, n)| match r {
            Role::Def(kind) => Some((*kind, *n)),
            _ => None,
        });
        let name = caps.iter().find(|(r, _)| *r == Role::Name).map(|(_, n)| *n);
        let (Some((kind, def)), Some(name)) = (def, name) else { return };
        if errors.contains(def.start_byte()) {
            return;
        }
        // Go `const A, B = 1, 2`: one spec, several names; the query engine reports the first.
        let names: Vec<Node<'_>> = if matches!(def.kind(), "const_spec" | "var_spec") {
            let mut cursor = def.walk();
            def.children_by_field_name("name", &mut cursor).filter(|n| n.kind() == "identifier").collect()
        } else {
            vec![name]
        };
        for name in names {
            if !seen.insert((def.id(), name.id())) {
                continue;
            }
            name_nodes.insert(name.id());
            if let Some(d) = draft_symbol(family, def, name, kind, src, path) {
                drafts.push(d);
            }
        }
    })?;
    let table = SymbolTable::build(drafts, family, max_symbols);
    if max_refs == 0 {
        return Ok((table, Vec::new()));
    }

    let mut refs: Vec<RefDraft> = Vec::new();
    let mut by_name: HashMap<(usize, RefKind), usize> = HashMap::new();
    run_query(&queries.refs, root, src, deadline, |caps| {
        let r = caps.iter().find_map(|(r, n)| match r {
            Role::Ref(kind) => Some((*kind, *n)),
            _ => None,
        });
        let name = caps.iter().find(|(r, _)| *r == Role::Name).map(|(_, n)| *n);
        let qualifier = caps.iter().find(|(r, _)| *r == Role::Qualifier).map(|(_, n)| *n);
        let (Some((kind, node)), Some(name)) = (r, name) else { return };
        if errors.contains(node.start_byte()) || (kind != RefKind::Import && name_nodes.contains(&name.id())) {
            return;
        }
        let Some(draft) = draft_ref(family, kind, node, name, qualifier, src) else { return };
        match by_name.get(&(name.id(), kind)) {
            Some(&existing) => {
                if refs[existing].qualifier.is_none() && draft.qualifier.is_some() {
                    refs[existing] = draft;
                }
            }
            None => {
                by_name.insert((name.id(), kind), refs.len());
                refs.push(draft);
            }
        }
    })?;
    let refs = finish_refs(refs, &table, max_refs);
    Ok((table, refs))
}

// ---------------------------------------------------------------------------------------------
// Symbols

fn draft_symbol(family: Family, def: Node<'_>, name_node: Node<'_>, kind: SymbolKind, src: &[u8], path: &RepoPath) -> Option<SymDraft> {
    let name = text::text(name_node, src).to_string();
    if name.is_empty() {
        return None;
    }
    let mut kind = kind;
    let mut owner_type = None;
    match family {
        Family::Go => {
            if def.kind() == "type_spec" {
                kind = match def.child_by_field_name("type").map(|t| t.kind()) {
                    Some("struct_type") => SymbolKind::Struct,
                    Some("interface_type") => SymbolKind::Interface,
                    _ => SymbolKind::TypeAlias,
                };
            }
            if kind == SymbolKind::Function && name.starts_with("Test") && path.file_name().ends_with("_test.go") {
                kind = SymbolKind::Test;
            }
            if kind == SymbolKind::Method {
                owner_type = def
                    .child_by_field_name("receiver")
                    .and_then(|r| text::named_children(r).into_iter().next())
                    .and_then(|p| p.child_by_field_name("type"))
                    .map(|t| base_type_name(t, src));
            }
        }
        Family::Rust => {
            let holder = def.parent().filter(|p| p.kind() == "declaration_list").and_then(|p| p.parent());
            match holder.map(|h| (h.kind(), h)) {
                Some(("impl_item", imp)) if kind == SymbolKind::Function => {
                    kind = SymbolKind::Method;
                    owner_type = imp.child_by_field_name("type").map(|t| base_type_name(t, src));
                }
                Some(("trait_item", _)) if kind == SymbolKind::Function => kind = SymbolKind::Method,
                _ => {}
            }
            if def.kind() == "function_item" && rust_attributes(def, src).iter().any(|a| a == "test" || a.ends_with("::test")) {
                kind = SymbolKind::Test;
            }
        }
        Family::Python => {
            if kind == SymbolKind::Function {
                let outer = def.parent().filter(|p| p.kind() == "decorated_definition").unwrap_or(def);
                let in_class = outer.parent().filter(|b| b.kind() == "block").and_then(|b| b.parent()).is_some_and(|c| c.kind() == "class_definition");
                if in_class {
                    kind = SymbolKind::Method;
                }
            }
            if kind == SymbolKind::Variable && is_upper_case(&name) {
                kind = SymbolKind::Constant;
            }
        }
        Family::TypeScript | Family::JavaScript | Family::Clojure => {}
    }
    let outer = outer_node(family, def);
    let start_line = text::start_line(outer);
    let span = LineRange::new(start_line, text::end_line(outer).max(start_line)).ok()?;
    Some(SymDraft {
        start: outer.start_byte(),
        end: outer.end_byte(),
        name_byte: name_node.start_byte(),
        exported: exported(family, def, &name, kind, src),
        signature: signature(outer, def, src),
        doc: doc(family, outer, def, src),
        name,
        kind,
        span,
        name_pos: text::pos(name_node),
        owner_type,
        qualified: None,
    })
}

fn is_upper_case(name: &str) -> bool {
    name.chars().any(|c| c.is_ascii_uppercase()) && name.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

/// Type name without pointer / generics / path (`*Server` → `Server`, `Foo<T>` → `Foo`).
fn base_type_name(node: Node<'_>, src: &[u8]) -> String {
    match node.kind() {
        "pointer_type" | "reference_type" | "parenthesized_type" => match text::named_children(node).into_iter().find(|c| c.kind() != "lifetime" && c.kind() != "mutable_specifier") {
            Some(inner) => base_type_name(inner, src),
            None => text::squash(text::text(node, src)),
        },
        "generic_type" => match node.child_by_field_name("type") {
            Some(inner) => base_type_name(inner, src),
            None => text::squash(text::text(node, src)),
        },
        "scoped_type_identifier" | "qualified_type" => match node.child_by_field_name("name") {
            Some(inner) => text::text(inner, src).to_string(),
            None => text::squash(text::text(node, src)),
        },
        _ => text::squash(text::text(node, src)),
    }
}

/// Paths of the `#[…]` attributes directly before a Rust item.
fn rust_attributes(def: Node<'_>, src: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = def.prev_sibling();
    while let Some(n) = cur {
        match n.kind() {
            "attribute_item" => {
                if let Some(attr) = text::named_children(n).into_iter().find(|a| a.kind() == "attribute") {
                    if let Some(path) = text::named_children(attr).into_iter().next() {
                        out.push(text::squash(text::text(path, src)));
                    }
                }
            }
            "line_comment" | "block_comment" => {}
            _ => break,
        }
        cur = n.prev_sibling();
    }
    out
}

/// Declaration incl. syntactic wrappers (`export`, decorators, single-spec Go declarations).
fn outer_node<'t>(family: Family, def: Node<'t>) -> Node<'t> {
    let Some(parent) = def.parent() else { return def };
    match family {
        Family::Go => {
            if matches!(parent.kind(), "type_declaration" | "const_declaration" | "var_declaration")
                && text::named_children(parent).iter().filter(|c| c.kind().ends_with("_spec") || c.kind() == "type_alias").count() == 1
            {
                parent
            } else {
                def
            }
        }
        Family::TypeScript | Family::JavaScript => {
            if parent.kind() == "export_statement" {
                parent
            } else {
                def
            }
        }
        Family::Python => {
            if parent.kind() == "decorated_definition" {
                parent
            } else {
                def
            }
        }
        Family::Rust | Family::Clojure => def,
    }
}

/// Text from the declaration start to the end of the line holding the definition keyword.
fn signature(outer: Node<'_>, def: Node<'_>, src: &[u8]) -> Option<String> {
    let line_end = src[def.start_byte().min(src.len())..].iter().position(|b| *b == b'\n').map_or(src.len(), |p| def.start_byte() + p);
    let sig = text::slice(src, outer.start_byte(), line_end.min(outer.end_byte().max(def.end_byte())));
    let sig = sig.trim_end();
    (!sig.is_empty()).then(|| text::head_chars(sig, text::SIGNATURE_MAX_CHARS))
}

fn exported(family: Family, def: Node<'_>, name: &str, kind: SymbolKind, src: &[u8]) -> bool {
    match family {
        Family::Go => kind == SymbolKind::Module || name.chars().next().is_some_and(char::is_uppercase),
        Family::TypeScript | Family::JavaScript => {
            let mut cur = def.parent();
            while let Some(n) = cur {
                if n.kind() == "export_statement" {
                    return true;
                }
                cur = n.parent();
            }
            false
        }
        Family::Python => !name.starts_with('_'),
        Family::Rust => {
            if def.kind() == "macro_definition" {
                return rust_attributes(def, src).iter().any(|a| a == "macro_export");
            }
            text::named_children(def).iter().any(|c| c.kind() == "visibility_modifier" && text::text(*c, src) == "pub")
        }
        Family::Clojure => true,
    }
}

fn doc(family: Family, outer: Node<'_>, def: Node<'_>, src: &[u8]) -> Option<String> {
    let doc = match family {
        Family::Go => go_doc(outer, src),
        Family::TypeScript | Family::JavaScript => js_doc(outer, src),
        Family::Python => python_doc(def, src),
        Family::Rust => rust_doc(def, src),
        Family::Clojure => None,
    }?;
    let doc = doc.trim();
    (!doc.is_empty()).then(|| text::head_bytes(doc, text::DOC_MAX_BYTES))
}

/// Contiguous `//` comment lines directly above the declaration.
fn go_doc(outer: Node<'_>, src: &[u8]) -> Option<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut next_line = text::start_line(outer);
    let mut cur = outer.prev_sibling();
    while let Some(c) = cur.filter(|c| c.kind() == "comment") {
        if text::end_line(c) + 1 != next_line {
            break;
        }
        let t = text::text(c, src);
        if let Some(body) = t.strip_prefix("//") {
            lines.push(body.strip_prefix(' ').unwrap_or(body).trim_end().to_string());
        } else {
            lines.push(strip_block_comment(t));
        }
        next_line = text::start_line(c);
        cur = c.prev_sibling();
    }
    lines.reverse();
    (!lines.is_empty()).then(|| lines.join("\n"))
}

/// The `/** … */` comment directly above the declaration.
fn js_doc(outer: Node<'_>, src: &[u8]) -> Option<String> {
    let c = outer.prev_sibling().filter(|c| c.kind() == "comment")?;
    let t = text::text(c, src);
    if !t.starts_with("/**") || text::end_line(c) + 1 < text::start_line(outer) {
        return None;
    }
    Some(strip_block_comment(t))
}

fn strip_block_comment(t: &str) -> String {
    let body = t.trim_start_matches("/**").trim_start_matches("/*").trim_end_matches("*/");
    body.lines()
        .map(|l| {
            let l = l.trim();
            let l = l.strip_prefix('*').unwrap_or(l);
            l.strip_prefix(' ').unwrap_or(l).trim_end()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// First statement string of a function / class body.
fn python_doc(def: Node<'_>, src: &[u8]) -> Option<String> {
    let body = def.child_by_field_name("body")?;
    let first = text::named_children(body).into_iter().next().filter(|s| s.kind() == "expression_statement")?;
    let string = text::named_children(first).into_iter().next().filter(|s| s.kind() == "string")?;
    let content: String = text::named_children(string).into_iter().filter(|c| c.kind() == "string_content").map(|c| text::text(c, src)).collect();
    Some(content.lines().map(str::trim).collect::<Vec<_>>().join("\n"))
}

/// `///` / `/** */` outer doc comments before the item (attributes in between skipped).
fn rust_doc(def: Node<'_>, src: &[u8]) -> Option<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut cur = def.prev_sibling();
    while let Some(n) = cur {
        match n.kind() {
            "attribute_item" => {}
            "line_comment" | "block_comment" if n.child_by_field_name("outer").is_some() => {
                let body = n.child_by_field_name("doc").map(|d| text::text(d, src)).unwrap_or("");
                let body: Vec<&str> = body.lines().collect();
                for l in body.into_iter().rev() {
                    lines.push(l.strip_prefix(' ').unwrap_or(l).trim_end().to_string());
                }
            }
            _ => break,
        }
        cur = n.prev_sibling();
    }
    lines.reverse();
    (!lines.is_empty()).then(|| lines.join("\n"))
}

// ---------------------------------------------------------------------------------------------
// Refs

/// Go predeclared types (not refs).
const GO_BUILTIN_TYPES: &[&str] = &[
    "any", "bool", "byte", "comparable", "complex64", "complex128", "error", "float32", "float64", "int", "int8", "int16", "int32", "int64", "rune",
    "string", "uint", "uint8", "uint16", "uint32", "uint64", "uintptr",
];

fn draft_ref(family: Family, kind: RefKind, node: Node<'_>, name: Node<'_>, qualifier: Option<Node<'_>>, src: &[u8]) -> Option<RefDraft> {
    let (name_text, qualifier) = if kind == RefKind::Import {
        let path = match name.kind() {
            "interpreted_string_literal" | "raw_string_literal" | "string" => text::unescape(text::unquote(text::text(name, src))),
            _ if family == Family::Rust => text::collapse_ws(text::text(name, src)),
            _ => text::squash(text::text(name, src)),
        };
        let alias = match family {
            Family::Go => Some(match node.child_by_field_name("name") {
                Some(alias) => text::text(alias, src).to_string(),
                None => path.rsplit('/').next().unwrap_or(&path).to_string(),
            }),
            _ => qualifier.map(|q| text::text(q, src).to_string()),
        };
        (path, alias)
    } else {
        (text::squash(text::text(name, src)), qualifier.map(|q| text::callee_text(text::text(q, src))))
    };
    if name_text.is_empty() || (family == Family::Go && kind == RefKind::Type && GO_BUILTIN_TYPES.contains(&name_text.as_str())) {
        return None;
    }
    Some(RefDraft { byte: name.start_byte(), name: name_text, qualifier, kind, pos: text::pos(name) })
}
