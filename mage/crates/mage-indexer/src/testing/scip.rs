//! `ScipReader` fake ([`FakeScipReader`]: decoded indexes registered by path) and the port
//! contract suite [`contract_scip_reader`] (documents in order and in batches, external symbols
//! after the documents, limits), run against the fake and
//! `adapters::driven::scip::ScipFileReader`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use mage_core::ports::knowledge::{ScipDocument, ScipMetadata, ScipOccurrence, ScipRange, ScipRelationship, ScipRoles, ScipSymbolInfo};

use crate::ports::scip::{ScipError, ScipLimits, ScipReader, ScipStream};

/// One decoded index: what a reader must yield for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScipFixture {
    pub metadata: ScipMetadata,
    pub documents: Vec<ScipDocument>,
    pub external_symbols: Vec<ScipSymbolInfo>,
}

/// Indexes by file path; `open` of an unregistered path is `NotFound`. Enforces `ScipLimits`
/// like the real reader (document count at `open`, per-document checks when read).
#[derive(Default)]
pub struct FakeScipReader {
    files: Mutex<HashMap<PathBuf, ScipFixture>>,
}

impl FakeScipReader {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&self, path: impl Into<PathBuf>, index: ScipFixture) {
        self.files.lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert(path.into(), index);
    }
}

struct FakeStream {
    index: ScipFixture,
    limits: ScipLimits,
    next: usize,
}

fn check_doc(doc: &ScipDocument, limits: ScipLimits) -> Result<(), ScipError> {
    if doc.occurrences.len() > limits.max_occurrences_per_doc as usize {
        return Err(ScipError::LimitExceeded(format!("{}: too many occurrences", doc.relative_path)));
    }
    let long = |s: &str| s.len() > limits.max_symbol_len as usize;
    let symbol_too_long = |s: &ScipSymbolInfo| long(&s.symbol) || s.relationships.iter().any(|r| long(&r.symbol));
    if doc.occurrences.iter().any(|o| long(&o.symbol)) || doc.symbols.iter().any(symbol_too_long) {
        return Err(ScipError::LimitExceeded(format!("{}: symbol too long", doc.relative_path)));
    }
    Ok(())
}

#[async_trait]
impl ScipReader for FakeScipReader {
    async fn open(&self, file: &Path, limits: ScipLimits) -> Result<Box<dyn ScipStream>, ScipError> {
        let index = self.files.lock().unwrap_or_else(std::sync::PoisonError::into_inner).get(file).cloned().ok_or(ScipError::NotFound)?;
        if index.documents.len() > limits.max_documents as usize {
            return Err(ScipError::LimitExceeded(format!("more than {} documents", limits.max_documents)));
        }
        Ok(Box::new(FakeStream { index, limits, next: 0 }))
    }
}

#[async_trait]
impl ScipStream for FakeStream {
    fn metadata(&self) -> &ScipMetadata {
        &self.index.metadata
    }

    async fn next_documents(&mut self, max: usize) -> Result<Vec<ScipDocument>, ScipError> {
        let end = (self.next + max).min(self.index.documents.len());
        let batch = self.index.documents[self.next..end].to_vec();
        for doc in &batch {
            check_doc(doc, self.limits)?;
        }
        self.next = end;
        Ok(batch)
    }

    async fn external_symbols(&mut self) -> Result<Vec<ScipSymbolInfo>, ScipError> {
        if self.next < self.index.documents.len() {
            return Err(ScipError::Invalid("external_symbols before the documents are exhausted".into()));
        }
        let long = |s: &str| s.len() > self.limits.max_symbol_len as usize;
        if self.index.external_symbols.iter().any(|s| long(&s.symbol)) {
            return Err(ScipError::LimitExceeded("external symbol too long".into()));
        }
        Ok(std::mem::take(&mut self.index.external_symbols))
    }
}

// ---------------------------------------------------------------------------------------------
// Contract suite
// ---------------------------------------------------------------------------------------------

/// A small TypeScript-like index: three documents, a cross-file relationship, one external
/// symbol, single- and multi-line ranges.
pub fn sample_scip_fixture() -> ScipFixture {
    let connect = "scip-typescript npm app 1.0.0 src/`conn.ts`/connect().";
    let range = |sl, sc, el, ec| ScipRange { start_line: sl, start_col: sc, end_line: el, end_col: ec };
    ScipFixture {
        metadata: ScipMetadata { tool_name: "scip-typescript".into(), tool_version: "0.3.15".into(), project_root: "file:///work/repo".into() },
        documents: vec![
            ScipDocument {
                relative_path: "src/conn.ts".into(),
                language: "TypeScript".into(),
                occurrences: vec![
                    ScipOccurrence { range: range(10, 16, 10, 23), symbol: connect.into(), roles: ScipRoles(ScipRoles::DEFINITION), enclosing: Some(range(10, 0, 15, 1)) },
                    ScipOccurrence { range: range(21, 4, 22, 2), symbol: connect.into(), roles: ScipRoles(ScipRoles::READ_ACCESS), enclosing: None },
                ],
                symbols: vec![ScipSymbolInfo {
                    symbol: connect.into(),
                    display_name: Some("connect".into()),
                    kind: Some("Function".into()),
                    documentation: Some("Opens a connection.".into()),
                    relationships: vec![ScipRelationship {
                        symbol: "scip-typescript npm app 1.0.0 src/`api.ts`/Api#connect().".into(),
                        is_reference: false,
                        is_implementation: true,
                        is_type_definition: false,
                        is_definition: false,
                    }],
                }],
            },
            ScipDocument {
                relative_path: "src/api.ts".into(),
                language: "TypeScript".into(),
                occurrences: vec![ScipOccurrence { range: range(3, 9, 3, 16), symbol: connect.into(), roles: ScipRoles(ScipRoles::IMPORT), enclosing: None }],
                symbols: Vec::new(),
            },
            ScipDocument { relative_path: "src/empty.ts".into(), language: "TypeScript".into(), occurrences: Vec::new(), symbols: Vec::new() },
        ],
        external_symbols: vec![ScipSymbolInfo {
            symbol: "scip-typescript npm react 18.2.0 `index.d.ts`/useState().".into(),
            display_name: Some("useState".into()),
            kind: None,
            documentation: None,
            relationships: Vec::new(),
        }],
    }
}

/// Generic `ScipReader` contract. Each scenario gets a fresh temp dir and calls
/// `make(dir, fixture)`, which must return a reader and the file path whose `open` yields
/// `fixture`. Panics on the first violated expectation.
pub async fn contract_scip_reader<F, Fut>(make: F)
where
    F: Fn(&Path, &ScipFixture) -> Fut,
    Fut: Future<Output = (Arc<dyn ScipReader>, PathBuf)>,
{
    let limits = ScipLimits::INGEST;
    let fixture = sample_scip_fixture();

    // Round trip in batches, then external symbols.
    let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
    let (reader, path) = make(dir.path(), &fixture).await;
    let mut stream = reader.open(&path, limits).await.unwrap_or_else(|e| panic!("open: {e}"));
    assert_eq!(stream.metadata(), &fixture.metadata);
    let first = stream.next_documents(2).await.unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(first, fixture.documents[..2]);
    assert!(matches!(stream.external_symbols().await, Err(ScipError::Invalid(_))), "external symbols before the end");
    let second = stream.next_documents(2).await.unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(second, fixture.documents[2..]);
    assert!(stream.next_documents(2).await.unwrap_or_else(|e| panic!("{e}")).is_empty(), "end of documents");
    assert_eq!(stream.external_symbols().await.unwrap_or_else(|e| panic!("{e}")), fixture.external_symbols);

    // Missing file.
    let gone = dir.path().join("missing.scip");
    assert!(matches!(reader.open(&gone, limits).await, Err(ScipError::NotFound)));

    // Limits: each violation is LimitExceeded, at open or when the document is read.
    let violations = [
        ("documents", ScipLimits { max_documents: 2, ..limits }),
        ("occurrences", ScipLimits { max_occurrences_per_doc: 1, ..limits }),
        ("symbol length", ScipLimits { max_symbol_len: 20, ..limits }),
    ];
    for (name, tight) in violations {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let (reader, path) = make(dir.path(), &fixture).await;
        let outcome = match reader.open(&path, tight).await {
            Err(e) => Err(e),
            Ok(mut stream) => loop {
                match stream.next_documents(10).await {
                    Err(e) => break Err(e),
                    Ok(docs) if docs.is_empty() => break stream.external_symbols().await.map(|_| ()),
                    Ok(_) => {}
                }
            },
        };
        assert!(matches!(outcome, Err(ScipError::LimitExceeded(_))), "{name}: {outcome:?}");
    }
    // Exactly at the limits is fine.
    let longest = fixture
        .documents
        .iter()
        .flat_map(|d| {
            let occ = d.occurrences.iter().map(|o| o.symbol.len());
            let sym = d.symbols.iter().flat_map(|s| std::iter::once(s.symbol.len()).chain(s.relationships.iter().map(|r| r.symbol.len())));
            occ.chain(sym).collect::<Vec<_>>()
        })
        .max()
        .unwrap_or(0);
    let exact = ScipLimits { max_documents: 3, max_occurrences_per_doc: 2, max_symbol_len: longest as u32 };
    let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
    let (reader, path) = make(dir.path(), &fixture).await;
    let mut stream = reader.open(&path, exact).await.unwrap_or_else(|e| panic!("open at limits: {e}"));
    assert_eq!(stream.next_documents(10).await.unwrap_or_else(|e| panic!("{e}")).len(), 3);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fake_scip_reader_passes_the_contract() {
        contract_scip_reader(|dir, fixture| {
            let fake = FakeScipReader::new();
            let path = dir.join("index.scip");
            fake.insert(&path, fixture.clone());
            async move { (Arc::new(fake) as Arc<dyn ScipReader>, path) }
        })
        .await;
    }
}
