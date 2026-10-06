//! `FakeParser`: table-driven `Parser` (knowledge.md "Testing": "Fake: table `BlobOid →
//! ParsedBlob`"). Blobs without a table entry parse to an empty `ParsedBlob` with status `Ok`;
//! scripted failures and calls are recorded so use-case tests can assert which blobs were parsed.

use std::collections::HashMap;

use async_trait::async_trait;
use mage_core::domain::{BlobOid, Language, RepoPath};
use mage_core::ports::knowledge::{
    AppFactRecord, AppMapRev, BlobInput, Catalog, FrameworkId, FrameworkScope, ParseLimits, ParsedBlob, ParserRev, StageStatus,
};
use parking_lot::Mutex;

use crate::ports::parser::{ParseError, Parser};

const LANGUAGES: [Language; 9] = [
    Language::Go,
    Language::TypeScript,
    Language::Tsx,
    Language::JavaScript,
    Language::Python,
    Language::Rust,
    Language::Clojure,
    Language::ClojureScript,
    Language::Cljc,
];

#[derive(Default)]
struct State {
    parsed: HashMap<BlobOid, Result<ParsedBlob, ParseError>>,
    app_facts: HashMap<BlobOid, Vec<AppFactRecord>>,
    catalogs: HashMap<BlobOid, Catalog>,
    frameworks: Vec<FrameworkScope>,
    parse_calls: Vec<Vec<BlobOid>>,
    app_fact_calls: Vec<Vec<(BlobOid, Vec<FrameworkId>)>>,
    catalog_calls: Vec<BlobOid>,
}

pub struct FakeParser {
    rev: ParserRev,
    app_rev: AppMapRev,
    state: Mutex<State>,
}

impl Default for FakeParser {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeParser {
    /// `ParserRev(1)`, `AppMapRev(1)`, empty tables.
    pub fn new() -> Self {
        Self { rev: ParserRev(1), app_rev: AppMapRev(1), state: Mutex::new(State::default()) }
    }

    pub fn with_revs(mut self, rev: ParserRev, app_rev: AppMapRev) -> Self {
        self.rev = rev;
        self.app_rev = app_rev;
        self
    }

    /// `parse` result for `oid` (the `lang` of the result is overwritten by the input's).
    pub fn with_parsed(self, oid: BlobOid, parsed: ParsedBlob) -> Self {
        self.state.lock().parsed.insert(oid, Ok(parsed));
        self
    }

    /// `parse` fails for `oid`.
    pub fn with_parse_error(self, oid: BlobOid, err: ParseError) -> Self {
        self.state.lock().parsed.insert(oid, Err(err));
        self
    }

    pub fn with_app_facts(self, oid: BlobOid, facts: Vec<AppFactRecord>) -> Self {
        self.state.lock().app_facts.insert(oid, facts);
        self
    }

    pub fn with_catalog(self, oid: BlobOid, catalog: Catalog) -> Self {
        self.state.lock().catalogs.insert(oid, catalog);
        self
    }

    pub fn with_frameworks(self, frameworks: Vec<FrameworkScope>) -> Self {
        self.state.lock().frameworks = frameworks;
        self
    }

    /// Oids of every `parse` call, one Vec per call.
    pub fn parse_calls(&self) -> Vec<Vec<BlobOid>> {
        self.state.lock().parse_calls.clone()
    }

    /// Every oid handed to `parse`, in call order.
    pub fn parsed_oids(&self) -> Vec<BlobOid> {
        self.state.lock().parse_calls.iter().flatten().copied().collect()
    }

    pub fn app_fact_calls(&self) -> Vec<Vec<(BlobOid, Vec<FrameworkId>)>> {
        self.state.lock().app_fact_calls.clone()
    }

    pub fn catalog_calls(&self) -> Vec<BlobOid> {
        self.state.lock().catalog_calls.clone()
    }
}

/// Extension / file-name rules of the real adapter, reduced to the languages tests use.
fn language_of(path: &RepoPath, head: &[u8]) -> Option<Language> {
    let by_ext = match path.extension().map(str::to_ascii_lowercase).as_deref() {
        Some("go") => Some(Language::Go),
        Some("ts" | "mts" | "cts") => Some(Language::TypeScript),
        Some("tsx") => Some(Language::Tsx),
        Some("js" | "jsx" | "mjs" | "cjs") => Some(Language::JavaScript),
        Some("py") => Some(Language::Python),
        Some("rs") => Some(Language::Rust),
        Some("clj" | "edn") => Some(Language::Clojure),
        Some("cljs") => Some(Language::ClojureScript),
        Some("cljc") => Some(Language::Cljc),
        Some("json") => Some(Language::Json),
        Some("yaml" | "yml") => Some(Language::Yaml),
        Some("toml") => Some(Language::Toml),
        Some("md") => Some(Language::Markdown),
        Some("sql") => Some(Language::Sql),
        Some("sh") => Some(Language::Shell),
        _ => None,
    };
    by_ext.or_else(|| {
        let first = head.split(|b| *b == b'\n').next().unwrap_or_default();
        let shebang = std::str::from_utf8(first).ok()?.strip_prefix("#!")?;
        if shebang.contains("python") {
            Some(Language::Python)
        } else if shebang.contains("node") {
            Some(Language::JavaScript)
        } else if shebang.contains("sh") {
            Some(Language::Shell)
        } else {
            None
        }
    })
}

#[async_trait]
impl Parser for FakeParser {
    fn rev(&self) -> ParserRev {
        self.rev
    }

    fn app_rev(&self) -> AppMapRev {
        self.app_rev
    }

    fn languages(&self) -> &[Language] {
        &LANGUAGES
    }

    fn detect_language(&self, path: &RepoPath, head: &[u8]) -> Option<Language> {
        language_of(path, head)
    }

    async fn parse(&self, blobs: Vec<BlobInput>, limits: ParseLimits) -> Vec<Result<ParsedBlob, ParseError>> {
        let mut state = self.state.lock();
        state.parse_calls.push(blobs.iter().map(|b| b.oid).collect());
        blobs
            .into_iter()
            .map(|blob| {
                if !LANGUAGES.contains(&blob.lang) {
                    return Err(ParseError::Unsupported(blob.lang));
                }
                if blob.bytes.len() as u64 > limits.max_bytes {
                    return Err(ParseError::TooLarge);
                }
                match state.parsed.get(&blob.oid) {
                    Some(Ok(parsed)) => Ok(ParsedBlob { lang: blob.lang, ..parsed.clone() }),
                    Some(Err(err)) => Err(err.clone()),
                    None => Ok(ParsedBlob { lang: blob.lang, status: StageStatus::Ok, symbols: Vec::new(), refs: Vec::new(), literals: Vec::new() }),
                }
            })
            .collect()
    }

    async fn catalog(&self, blob: BlobInput) -> Result<Option<Catalog>, ParseError> {
        let mut state = self.state.lock();
        state.catalog_calls.push(blob.oid);
        Ok(state.catalogs.get(&blob.oid).cloned())
    }

    async fn detect_frameworks(&self, _manifests: Vec<BlobInput>) -> Result<Vec<FrameworkScope>, ParseError> {
        Ok(self.state.lock().frameworks.clone())
    }

    async fn app_facts(&self, blobs: Vec<(BlobInput, Vec<FrameworkId>)>, limits: ParseLimits) -> Vec<Result<Vec<AppFactRecord>, ParseError>> {
        let mut state = self.state.lock();
        state.app_fact_calls.push(blobs.iter().map(|(b, f)| (b.oid, f.clone())).collect());
        blobs
            .into_iter()
            .map(|(blob, _)| {
                if blob.bytes.len() as u64 > limits.max_bytes {
                    return Err(ParseError::TooLarge);
                }
                Ok(state.app_facts.get(&blob.oid).cloned().unwrap_or_default())
            })
            .collect()
    }
}
