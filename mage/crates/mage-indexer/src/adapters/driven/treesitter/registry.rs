//! Grammar registry: `Language → (tree_sitter::Language, compiled queries)`, built once.

use mage_core::domain::Language;
use mage_core::ports::knowledge::{RefKind, SymbolKind};
use tree_sitter::Query;

use crate::ports::parser::ParseError;

/// Extraction family of a grammar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Family {
    Go,
    TypeScript,
    JavaScript,
    Python,
    Rust,
    Clojure,
}

impl Family {
    /// Separator of `SymbolDef::qualified` segments.
    pub(super) fn separator(self) -> &'static str {
        match self {
            Self::Rust => "::",
            Self::Clojure => "/",
            Self::Go | Self::TypeScript | Self::JavaScript | Self::Python => ".",
        }
    }
}

/// Meaning of a query capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Role {
    Name,
    Qualifier,
    Def(SymbolKind),
    Ref(RefKind),
    /// `@_x` helper captures used by predicates.
    Ignore,
}

/// A compiled query with the role of each capture index.
pub(super) struct RoleQuery {
    pub query: Query,
    pub roles: Vec<Role>,
}

impl RoleQuery {
    fn new(language: &tree_sitter::Language, source: &str, what: &str) -> Result<Self, ParseError> {
        let query = Query::new(language, source).map_err(|e| ParseError::Unavailable(format!("{what} query: {e}")))?;
        let roles = query.capture_names().iter().map(|name| role(name)).collect::<Option<Vec<_>>>();
        let roles = roles.ok_or_else(|| ParseError::Unavailable(format!("{what} query: unknown capture name")))?;
        Ok(Self { query, roles })
    }
}

fn role(name: &str) -> Option<Role> {
    if name.starts_with('_') {
        return Some(Role::Ignore);
    }
    match name {
        "name" => return Some(Role::Name),
        "qualifier" => return Some(Role::Qualifier),
        _ => {}
    }
    if let Some(kind) = name.strip_prefix("definition.") {
        let kind = match kind {
            "module" => SymbolKind::Module,
            "function" => SymbolKind::Function,
            "method" => SymbolKind::Method,
            "class" => SymbolKind::Class,
            "struct" => SymbolKind::Struct,
            "interface" => SymbolKind::Interface,
            "trait" => SymbolKind::Trait,
            "enum" => SymbolKind::Enum,
            // Go `type_spec`: refined from the spec's type node.
            "type" | "type_alias" => SymbolKind::TypeAlias,
            "constant" => SymbolKind::Constant,
            "variable" => SymbolKind::Variable,
            "field" => SymbolKind::Field,
            "macro" => SymbolKind::Macro,
            "test" => SymbolKind::Test,
            _ => return None,
        };
        return Some(Role::Def(kind));
    }
    let kind = match name.strip_prefix("reference.")? {
        "call" => RefKind::Call,
        "type" => RefKind::Type,
        "read" => RefKind::Read,
        "import" => RefKind::Import,
        "dispatch" => RefKind::Dispatch,
        "render" => RefKind::Render,
        _ => return None,
    };
    Some(Role::Ref(kind))
}

/// Symbol and ref queries of a query-driven family.
pub(super) struct Queries {
    pub symbols: RoleQuery,
    pub refs: RoleQuery,
}

pub(super) struct CodeGrammar {
    pub family: Family,
    pub language: tree_sitter::Language,
    /// `None` for the Clojure walker.
    pub queries: Option<Queries>,
}

impl CodeGrammar {
    fn with_queries(family: Family, language: tree_sitter::Language, symbols: &str, refs: &str, what: &str) -> Result<Self, ParseError> {
        let queries = Queries {
            symbols: RoleQuery::new(&language, symbols, &format!("{what} symbols"))?,
            refs: RoleQuery::new(&language, refs, &format!("{what} refs"))?,
        };
        Ok(Self { family, language, queries: Some(queries) })
    }
}

const GO_SYMBOLS: &str = include_str!("queries/go/symbols.scm");
const GO_REFS: &str = include_str!("queries/go/refs.scm");
const JS_SYMBOLS: &str = include_str!("queries/javascript/symbols.scm");
const JS_REFS: &str = include_str!("queries/javascript/refs.scm");
const TS_SYMBOLS: &str = include_str!("queries/typescript/symbols.scm");
const TS_REFS: &str = include_str!("queries/typescript/refs.scm");
const JSX_REFS: &str = include_str!("queries/jsx/refs.scm");
const PY_SYMBOLS: &str = include_str!("queries/python/symbols.scm");
const PY_REFS: &str = include_str!("queries/python/refs.scm");
const RS_SYMBOLS: &str = include_str!("queries/rust/symbols.scm");
const RS_REFS: &str = include_str!("queries/rust/refs.scm");

pub(super) struct Registry {
    go: CodeGrammar,
    typescript: CodeGrammar,
    tsx: CodeGrammar,
    javascript: CodeGrammar,
    python: CodeGrammar,
    rust: CodeGrammar,
    clojure: CodeGrammar,
    pub json: tree_sitter::Language,
    pub yaml: tree_sitter::Language,
}

impl Registry {
    pub(super) fn new() -> Result<Self, ParseError> {
        let js_refs = format!("{JS_REFS}\n{JSX_REFS}");
        let tsx_refs = format!("{TS_REFS}\n{JSX_REFS}");
        Ok(Self {
            go: CodeGrammar::with_queries(Family::Go, tree_sitter_go::LANGUAGE.into(), GO_SYMBOLS, GO_REFS, "go")?,
            typescript: CodeGrammar::with_queries(
                Family::TypeScript,
                tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
                TS_SYMBOLS,
                TS_REFS,
                "typescript",
            )?,
            tsx: CodeGrammar::with_queries(Family::TypeScript, tree_sitter_typescript::LANGUAGE_TSX.into(), TS_SYMBOLS, &tsx_refs, "tsx")?,
            javascript: CodeGrammar::with_queries(Family::JavaScript, tree_sitter_javascript::LANGUAGE.into(), JS_SYMBOLS, &js_refs, "javascript")?,
            python: CodeGrammar::with_queries(Family::Python, tree_sitter_python::LANGUAGE.into(), PY_SYMBOLS, PY_REFS, "python")?,
            rust: CodeGrammar::with_queries(Family::Rust, tree_sitter_rust::LANGUAGE.into(), RS_SYMBOLS, RS_REFS, "rust")?,
            clojure: CodeGrammar { family: Family::Clojure, language: tree_sitter_clojure_orchard::LANGUAGE.into(), queries: None },
            json: tree_sitter_json::LANGUAGE.into(),
            yaml: tree_sitter_yaml::LANGUAGE.into(),
        })
    }

    /// Grammar with symbol / literal extraction for `lang`.
    pub(super) fn code(&self, lang: Language) -> Option<&CodeGrammar> {
        match lang {
            Language::Go => Some(&self.go),
            Language::TypeScript => Some(&self.typescript),
            Language::Tsx => Some(&self.tsx),
            Language::JavaScript => Some(&self.javascript),
            Language::Python => Some(&self.python),
            Language::Rust => Some(&self.rust),
            Language::Clojure | Language::ClojureScript | Language::Cljc => Some(&self.clojure),
            _ => None,
        }
    }

    /// The Clojure grammar (EDN catalogs).
    pub(super) fn clojure(&self) -> &tree_sitter::Language {
        &self.clojure.language
    }
}
