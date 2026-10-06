//! `ScipReader` over a local `index.scip` file (knowledge.md
//! "`mage_indexer::adapters::driven::scip`"), decoded with the `scip` crate's `protobuf` types.
//!
//! Streaming: `scip.Index` is read field by field at the top level (metadata = 1, documents = 2,
//! external_symbols = 3, as in the pinned scip.proto) and one `Document` message is decoded at a
//! time, so memory stays bounded by the largest document. A concatenation of several `Index`
//! messages (how streaming SCIP writers emit large indexes) is the same field sequence and reads
//! the same way.
//!
//! `open` makes one framing pass that seeks over payloads: it merges every metadata field,
//! counts documents (`max_documents`) and checks that each field fits in the file. Document
//! passes then decode lazily; external symbols met on the way are buffered until requested.
//! File I/O runs on blocking threads.

use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use mage_core::ports::knowledge::{ScipDocument, ScipMetadata, ScipOccurrence, ScipRange, ScipRelationship, ScipRoles, ScipSymbolInfo};
use protobuf::Message;
use scip::types::{Document, Metadata, Occurrence, SymbolInformation, symbol_information};

use crate::ports::scip::{ScipError, ScipLimits, ScipReader, ScipStream};

const FIELD_METADATA: u64 = 1;
const FIELD_DOCUMENTS: u64 = 2;
const FIELD_EXTERNAL_SYMBOLS: u64 = 3;

const WIRE_VARINT: u8 = 0;
const WIRE_FIXED64: u8 = 1;
const WIRE_LEN: u8 = 2;
const WIRE_FIXED32: u8 = 5;

/// Reads SCIP files from the local filesystem.
#[derive(Debug, Clone, Copy, Default)]
pub struct ScipFileReader;

impl ScipFileReader {
    pub fn new() -> Self {
        ScipFileReader
    }
}

#[async_trait]
impl ScipReader for ScipFileReader {
    async fn open(&self, file: &Path, limits: ScipLimits) -> Result<Box<dyn ScipStream>, ScipError> {
        let path = file.to_path_buf();
        let stream = tokio::task::spawn_blocking(move || FileStream::open(path, limits))
            .await
            .map_err(|e| ScipError::Unavailable(format!("scip open task: {e}")))??;
        Ok(Box::new(stream))
    }
}

/// Top-level fields of `scip.Index`, read sequentially.
struct Fields {
    reader: BufReader<File>,
    pos: u64,
    len: u64,
}

enum Field {
    /// Length-delimited field: number and payload length (payload not read yet).
    Len { number: u64, len: u64 },
    /// Any other wire type; already skipped.
    Skipped,
}

impl Fields {
    fn open(path: &Path) -> Result<Fields, ScipError> {
        let file = File::open(path).map_err(|e| match e.kind() {
            io::ErrorKind::NotFound => ScipError::NotFound,
            _ => ScipError::Unavailable(format!("open {}: {e}", path.display())),
        })?;
        let len = file.metadata().map_err(io_err)?.len();
        Ok(Fields { reader: BufReader::with_capacity(1 << 16, file), pos: 0, len })
    }

    fn rewind(&mut self) -> Result<(), ScipError> {
        self.reader.seek(SeekFrom::Start(0)).map_err(io_err)?;
        self.pos = 0;
        Ok(())
    }

    fn byte(&mut self) -> Result<u8, ScipError> {
        let mut b = [0u8; 1];
        self.reader.read_exact(&mut b).map_err(truncated)?;
        self.pos += 1;
        Ok(b[0])
    }

    fn varint(&mut self) -> Result<u64, ScipError> {
        let mut value = 0u64;
        for shift in (0..70).step_by(7) {
            let b = self.byte()?;
            value |= u64::from(b & 0x7f) << shift;
            if b & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err(ScipError::Invalid("varint longer than 10 bytes".into()))
    }

    fn skip(&mut self, n: u64) -> Result<(), ScipError> {
        if n > self.len - self.pos {
            return Err(ScipError::Invalid("field runs past the end of the file".into()));
        }
        let n = i64::try_from(n).map_err(|_| ScipError::Invalid("field too long".into()))?;
        self.reader.seek_relative(n).map_err(io_err)?;
        self.pos += n as u64;
        Ok(())
    }

    fn payload(&mut self, len: u64) -> Result<Vec<u8>, ScipError> {
        if len > self.len - self.pos {
            return Err(ScipError::Invalid("field runs past the end of the file".into()));
        }
        let size = usize::try_from(len).map_err(|_| ScipError::Invalid("field too long".into()))?;
        let mut buf = vec![0u8; size];
        self.reader.read_exact(&mut buf).map_err(truncated)?;
        self.pos += len;
        Ok(buf)
    }

    /// Next top-level field, `None` at a clean end of file. Non-length-delimited fields are
    /// skipped here; groups are refused.
    fn next(&mut self) -> Result<Option<Field>, ScipError> {
        if self.pos >= self.len {
            return Ok(None);
        }
        let tag = self.varint()?;
        let number = tag >> 3;
        if number == 0 {
            return Err(ScipError::Invalid("field number 0".into()));
        }
        match (tag & 7) as u8 {
            WIRE_VARINT => {
                self.varint()?;
                Ok(Some(Field::Skipped))
            }
            WIRE_FIXED64 => {
                self.skip(8)?;
                Ok(Some(Field::Skipped))
            }
            WIRE_FIXED32 => {
                self.skip(4)?;
                Ok(Some(Field::Skipped))
            }
            WIRE_LEN => {
                let len = self.varint()?;
                if len > self.len - self.pos {
                    return Err(ScipError::Invalid(format!("field {number} runs past the end of the file")));
                }
                Ok(Some(Field::Len { number, len }))
            }
            other => Err(ScipError::Invalid(format!("unsupported wire type {other} at the top level"))),
        }
    }
}

fn io_err(e: io::Error) -> ScipError {
    ScipError::Unavailable(format!("scip file: {e}"))
}

fn truncated(e: io::Error) -> ScipError {
    match e.kind() {
        io::ErrorKind::UnexpectedEof => ScipError::Invalid("truncated scip file".into()),
        _ => io_err(e),
    }
}

struct FileStream {
    metadata: ScipMetadata,
    limits: ScipLimits,
    /// `None` while a blocking batch holds the reader, or after the end.
    fields: Option<Fields>,
    exhausted: bool,
    external: Vec<ScipSymbolInfo>,
}

impl FileStream {
    fn open(path: PathBuf, limits: ScipLimits) -> Result<FileStream, ScipError> {
        let mut fields = Fields::open(&path)?;
        let mut metadata: Option<Metadata> = None;
        let mut documents: u64 = 0;
        while let Some(field) = fields.next()? {
            let Field::Len { number, len } = field else { continue };
            match number {
                FIELD_METADATA => {
                    let bytes = fields.payload(len)?;
                    metadata
                        .get_or_insert_with(Metadata::new)
                        .merge_from_bytes(&bytes)
                        .map_err(|e| ScipError::Invalid(format!("metadata: {e}")))?;
                }
                FIELD_DOCUMENTS => {
                    documents += 1;
                    if documents > u64::from(limits.max_documents) {
                        return Err(ScipError::LimitExceeded(format!("more than {} documents", limits.max_documents)));
                    }
                    fields.skip(len)?;
                }
                _ => fields.skip(len)?,
            }
        }
        let metadata = metadata.ok_or_else(|| ScipError::Invalid("index has no metadata".into()))?;
        let tool = metadata.tool_info.as_ref();
        let metadata = ScipMetadata {
            tool_name: tool.map(|t| t.name.clone()).unwrap_or_default(),
            tool_version: tool.map(|t| t.version.clone()).unwrap_or_default(),
            project_root: metadata.project_root,
        };
        fields.rewind()?;
        Ok(FileStream { metadata, limits, fields: Some(fields), exhausted: false, external: Vec::new() })
    }
}

/// Reads up to `max` documents; returns them with the external symbols met on the way and
/// whether the end of the file was reached.
fn read_batch(
    fields: &mut Fields,
    max: usize,
    limits: ScipLimits,
) -> Result<(Vec<ScipDocument>, Vec<ScipSymbolInfo>, bool), ScipError> {
    let mut docs = Vec::new();
    let mut external = Vec::new();
    while docs.len() < max {
        let Some(field) = fields.next()? else {
            return Ok((docs, external, true));
        };
        let Field::Len { number, len } = field else { continue };
        match number {
            FIELD_DOCUMENTS => {
                let bytes = fields.payload(len)?;
                let doc = Document::parse_from_bytes(&bytes).map_err(|e| ScipError::Invalid(format!("document: {e}")))?;
                docs.push(convert_document(doc, limits)?);
            }
            FIELD_EXTERNAL_SYMBOLS => {
                let bytes = fields.payload(len)?;
                let info = SymbolInformation::parse_from_bytes(&bytes).map_err(|e| ScipError::Invalid(format!("external symbol: {e}")))?;
                external.push(convert_symbol(info, limits)?);
            }
            _ => fields.skip(len)?,
        }
    }
    Ok((docs, external, false))
}

#[async_trait]
impl ScipStream for FileStream {
    fn metadata(&self) -> &ScipMetadata {
        &self.metadata
    }

    async fn next_documents(&mut self, max: usize) -> Result<Vec<ScipDocument>, ScipError> {
        if self.exhausted || max == 0 {
            return Ok(Vec::new());
        }
        let mut fields = self.fields.take().ok_or_else(|| ScipError::Unavailable("scip stream failed earlier".into()))?;
        let limits = self.limits;
        let (fields, result) = tokio::task::spawn_blocking(move || {
            let result = read_batch(&mut fields, max, limits);
            (fields, result)
        })
        .await
        .map_err(|e| ScipError::Unavailable(format!("scip read task: {e}")))?;
        let (docs, external, end) = result?;
        self.external.extend(external);
        if end {
            self.exhausted = true;
        } else {
            self.fields = Some(fields);
        }
        Ok(docs)
    }

    async fn external_symbols(&mut self) -> Result<Vec<ScipSymbolInfo>, ScipError> {
        if !self.exhausted {
            return Err(ScipError::Invalid("external_symbols before the documents are exhausted".into()));
        }
        Ok(std::mem::take(&mut self.external))
    }
}

fn check_symbol(symbol: &str, limits: ScipLimits) -> Result<(), ScipError> {
    if symbol.len() > limits.max_symbol_len as usize {
        return Err(ScipError::LimitExceeded(format!("symbol of {} bytes (max {})", symbol.len(), limits.max_symbol_len)));
    }
    Ok(())
}

/// SCIP `[line, start, end]` or `[start_line, start, end_line, end]`, 0-based lines → 1-based.
/// `None` when malformed (wrong arity, negative, or end before start).
fn convert_range(range: &[i32]) -> Option<ScipRange> {
    let unsigned = |v: i32| u32::try_from(v).ok();
    let (start_line, start_col, end_line, end_col) = match *range {
        [line, start, end] => (unsigned(line)?, unsigned(start)?, unsigned(line)?, unsigned(end)?),
        [start_line, start, end_line, end] => (unsigned(start_line)?, unsigned(start)?, unsigned(end_line)?, unsigned(end)?),
        _ => return None,
    };
    if (end_line, end_col) < (start_line, start_col) {
        return None;
    }
    Some(ScipRange { start_line: start_line.checked_add(1)?, start_col, end_line: end_line.checked_add(1)?, end_col })
}

fn convert_occurrence(occ: &Occurrence, limits: ScipLimits) -> Result<Option<ScipOccurrence>, ScipError> {
    check_symbol(&occ.symbol, limits)?;
    let Some(range) = convert_range(&occ.range) else { return Ok(None) };
    let enclosing = if occ.enclosing_range.is_empty() { None } else { convert_range(&occ.enclosing_range) };
    Ok(Some(ScipOccurrence { range, symbol: occ.symbol.clone(), roles: ScipRoles(occ.symbol_roles as u32), enclosing }))
}

fn convert_symbol(info: SymbolInformation, limits: ScipLimits) -> Result<ScipSymbolInfo, ScipError> {
    check_symbol(&info.symbol, limits)?;
    let mut relationships = Vec::with_capacity(info.relationships.len());
    for rel in info.relationships {
        check_symbol(&rel.symbol, limits)?;
        relationships.push(ScipRelationship {
            symbol: rel.symbol,
            is_reference: rel.is_reference,
            is_implementation: rel.is_implementation,
            is_type_definition: rel.is_type_definition,
            is_definition: rel.is_definition,
        });
    }
    let kind = match info.kind.enum_value() {
        Ok(symbol_information::Kind::UnspecifiedKind) | Err(_) => None,
        Ok(kind) => Some(format!("{kind:?}")),
    };
    let documentation = info.documentation.join("\n\n");
    Ok(ScipSymbolInfo {
        symbol: info.symbol,
        display_name: (!info.display_name.is_empty()).then_some(info.display_name),
        kind,
        documentation: (!documentation.is_empty()).then_some(documentation),
        relationships,
    })
}

fn convert_document(doc: Document, limits: ScipLimits) -> Result<ScipDocument, ScipError> {
    if doc.occurrences.len() > limits.max_occurrences_per_doc as usize {
        return Err(ScipError::LimitExceeded(format!(
            "{}: {} occurrences (max {})",
            doc.relative_path,
            doc.occurrences.len(),
            limits.max_occurrences_per_doc
        )));
    }
    let mut occurrences = Vec::with_capacity(doc.occurrences.len());
    for occ in &doc.occurrences {
        if let Some(occ) = convert_occurrence(occ, limits)? {
            occurrences.push(occ);
        }
    }
    let symbols = doc.symbols.into_iter().map(|s| convert_symbol(s, limits)).collect::<Result<Vec<_>, _>>()?;
    Ok(ScipDocument { relative_path: doc.relative_path, language: doc.language, occurrences, symbols })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use scip::types::{Index, Relationship, ToolInfo};

    use super::*;
    use crate::testing::{ScipFixture, contract_scip_reader};

    /// Encodes a decoded fixture as SCIP (1-based lines back to 0-based; single-line ranges in
    /// the 3-element form).
    pub(crate) fn encode(fixture: &ScipFixture) -> Index {
        let range = |r: &ScipRange| -> Vec<i32> {
            let n = |v: u32| i32::try_from(v).unwrap_or_else(|e| panic!("{e}"));
            if r.start_line == r.end_line {
                vec![n(r.start_line - 1), n(r.start_col), n(r.end_col)]
            } else {
                vec![n(r.start_line - 1), n(r.start_col), n(r.end_line - 1), n(r.end_col)]
            }
        };
        let symbol = |s: &ScipSymbolInfo| {
            let mut info = SymbolInformation::new();
            info.symbol = s.symbol.clone();
            info.display_name = s.display_name.clone().unwrap_or_default();
            info.documentation = s.documentation.iter().cloned().collect();
            if let Some(kind) = &s.kind {
                let found = <symbol_information::Kind as protobuf::EnumFull>::enum_descriptor()
                    .values()
                    .find(|v| v.name() == kind)
                    .unwrap_or_else(|| panic!("unknown kind {kind}"));
                info.kind = protobuf::EnumOrUnknown::from_i32(found.value());
            }
            info.relationships = s
                .relationships
                .iter()
                .map(|r| {
                    let mut rel = Relationship::new();
                    rel.symbol = r.symbol.clone();
                    rel.is_reference = r.is_reference;
                    rel.is_implementation = r.is_implementation;
                    rel.is_type_definition = r.is_type_definition;
                    rel.is_definition = r.is_definition;
                    rel
                })
                .collect();
            info
        };
        let mut index = Index::new();
        let mut meta = Metadata::new();
        let mut tool = ToolInfo::new();
        tool.name = fixture.metadata.tool_name.clone();
        tool.version = fixture.metadata.tool_version.clone();
        meta.tool_info = protobuf::MessageField::some(tool);
        meta.project_root = fixture.metadata.project_root.clone();
        index.metadata = protobuf::MessageField::some(meta);
        for d in &fixture.documents {
            let mut doc = Document::new();
            doc.relative_path = d.relative_path.clone();
            doc.language = d.language.clone();
            doc.occurrences = d
                .occurrences
                .iter()
                .map(|o| {
                    let mut occ = Occurrence::new();
                    occ.range = range(&o.range);
                    occ.symbol = o.symbol.clone();
                    occ.symbol_roles = o.roles.0 as i32;
                    occ.enclosing_range = o.enclosing.as_ref().map(range).unwrap_or_default();
                    occ
                })
                .collect();
            doc.symbols = d.symbols.iter().map(symbol).collect();
            index.documents.push(doc);
        }
        index.external_symbols = fixture.external_symbols.iter().map(symbol).collect();
        index
    }

    fn write(path: &Path, bytes: &[u8]) {
        std::fs::write(path, bytes).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
    }

    #[tokio::test]
    async fn file_reader_passes_the_scip_reader_contract() {
        contract_scip_reader(|dir, fixture| {
            let path = dir.join("index.scip");
            let bytes = encode(fixture).write_to_bytes().unwrap_or_else(|e| panic!("{e}"));
            write(&path, &bytes);
            async move { (Arc::new(ScipFileReader::new()) as Arc<dyn ScipReader>, path) }
        })
        .await;
    }

    fn occurrence(range: Vec<i32>, symbol: &str, roles: i32) -> Occurrence {
        let mut occ = Occurrence::new();
        occ.range = range;
        occ.symbol = symbol.into();
        occ.symbol_roles = roles;
        occ
    }

    /// A small index built with the scip crate types, written the way streaming indexers do:
    /// two concatenated `Index` messages, metadata only in the first, external symbols in the
    /// middle of the documents, plus an unknown top-level field.
    #[tokio::test]
    async fn decodes_a_generated_streamed_index() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let mut first = Index::new();
        let mut meta = Metadata::new();
        let mut tool = ToolInfo::new();
        tool.name = "scip-typescript".into();
        tool.version = "0.3.15".into();
        meta.tool_info = protobuf::MessageField::some(tool);
        meta.project_root = "file:///work/repo".into();
        first.metadata = protobuf::MessageField::some(meta);
        let mut doc = Document::new();
        doc.relative_path = "src/conn.ts".into();
        doc.language = "TypeScript".into();
        let mut def = occurrence(vec![9, 16, 23], "scip-typescript npm app 1.0.0 src/`conn.ts`/connect().", 1);
        def.enclosing_range = vec![9, 0, 14, 1];
        doc.occurrences = vec![
            def,
            occurrence(vec![20, 4, 21, 2], "scip-typescript npm app 1.0.0 src/`conn.ts`/connect().", 8),
            occurrence(vec![3, 2, 7], "local 4", 0),
            occurrence(vec![1, 2], "malformed range", 0),
            occurrence(vec![5, 9, 4, 1], "end before start", 0),
        ];
        let mut info = SymbolInformation::new();
        info.symbol = "scip-typescript npm app 1.0.0 src/`conn.ts`/connect().".into();
        info.display_name = "connect".into();
        info.documentation = vec!["```ts\nfunction connect(): void\n```".into(), "Opens a connection.".into()];
        info.kind = protobuf::EnumOrUnknown::new(symbol_information::Kind::Function);
        let mut rel = Relationship::new();
        rel.symbol = "scip-typescript npm app 1.0.0 src/`api.ts`/Api#connect().".into();
        rel.is_implementation = true;
        info.relationships = vec![rel];
        doc.symbols = vec![info];
        first.documents.push(doc);
        let mut external = SymbolInformation::new();
        external.symbol = "scip-typescript npm react 18.2.0 `index.d.ts`/useState().".into();
        first.external_symbols.push(external);

        let mut second = Index::new();
        let mut doc2 = Document::new();
        doc2.relative_path = "src/api.ts".into();
        doc2.language = "TypeScript".into();
        second.documents.push(doc2);

        let mut bytes = first.write_to_bytes().unwrap_or_else(|e| panic!("{e}"));
        // Unknown field 15 (varint) and 14 (length-delimited): skipped.
        bytes.extend_from_slice(&[15 << 3, 0x96, 0x01, (14 << 3) | 2, 3, b'a', b'b', b'c']);
        bytes.extend(second.write_to_bytes().unwrap_or_else(|e| panic!("{e}")));
        let path = dir.path().join("typescript.scip");
        write(&path, &bytes);

        let mut stream = ScipFileReader::new().open(&path, ScipLimits::INGEST).await.unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            stream.metadata(),
            &ScipMetadata { tool_name: "scip-typescript".into(), tool_version: "0.3.15".into(), project_root: "file:///work/repo".into() }
        );
        let batch = stream.next_documents(1).await.unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(batch.len(), 1);
        let doc = &batch[0];
        assert_eq!((doc.relative_path.as_str(), doc.language.as_str()), ("src/conn.ts", "TypeScript"));
        assert_eq!(
            doc.occurrences.iter().map(|o| (o.range, o.roles, o.enclosing)).collect::<Vec<_>>(),
            [
                (
                    ScipRange { start_line: 10, start_col: 16, end_line: 10, end_col: 23 },
                    ScipRoles(1),
                    Some(ScipRange { start_line: 10, start_col: 0, end_line: 15, end_col: 1 })
                ),
                (ScipRange { start_line: 21, start_col: 4, end_line: 22, end_col: 2 }, ScipRoles(8), None),
                (ScipRange { start_line: 4, start_col: 2, end_line: 4, end_col: 7 }, ScipRoles(0), None),
            ],
            "0-based lines become 1-based; malformed ranges are dropped; locals are kept for the ingest to drop"
        );
        assert_eq!(
            doc.symbols,
            [ScipSymbolInfo {
                symbol: "scip-typescript npm app 1.0.0 src/`conn.ts`/connect().".into(),
                display_name: Some("connect".into()),
                kind: Some("Function".into()),
                documentation: Some("```ts\nfunction connect(): void\n```\n\nOpens a connection.".into()),
                relationships: vec![ScipRelationship {
                    symbol: "scip-typescript npm app 1.0.0 src/`api.ts`/Api#connect().".into(),
                    is_reference: false,
                    is_implementation: true,
                    is_type_definition: false,
                    is_definition: false,
                }],
            }]
        );
        assert!(matches!(stream.external_symbols().await, Err(ScipError::Invalid(_))), "documents not exhausted yet");
        let batch = stream.next_documents(10).await.unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(batch.iter().map(|d| d.relative_path.as_str()).collect::<Vec<_>>(), ["src/api.ts"]);
        assert!(stream.next_documents(10).await.unwrap_or_else(|e| panic!("{e}")).is_empty());
        let external = stream.external_symbols().await.unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(external.iter().map(|s| s.symbol.as_str()).collect::<Vec<_>>(), ["scip-typescript npm react 18.2.0 `index.d.ts`/useState()."]);
    }

    #[tokio::test]
    async fn corrupt_files_are_invalid() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let reader = ScipFileReader::new();
        let cases: [(&str, &[u8]); 4] = [
            ("truncated length", &[(2 << 3) | 2, 0x7f, 1, 2]),
            ("group wire type", &[(1 << 3) | 3]),
            ("unterminated varint", &[0x80, 0x80]),
            ("no metadata", &[]),
        ];
        for (name, bytes) in cases {
            let path = dir.path().join(format!("{}.scip", name.replace(' ', "_")));
            write(&path, bytes);
            let result = reader.open(&path, ScipLimits::INGEST).await;
            assert!(matches!(result, Err(ScipError::Invalid(_))), "{name}: {:?}", result.err());
        }
        let mut doc = Index::new();
        doc.metadata = protobuf::MessageField::some(Metadata::new());
        let mut bytes = doc.write_to_bytes().unwrap_or_else(|e| panic!("{e}"));
        // A document field whose payload is not a Document message.
        bytes.extend_from_slice(&[(2 << 3) | 2, 2, 0xff, 0xff]);
        let path = dir.path().join("bad_doc.scip");
        write(&path, &bytes);
        let mut stream = reader.open(&path, ScipLimits::INGEST).await.unwrap_or_else(|e| panic!("{e}"));
        assert!(matches!(stream.next_documents(5).await, Err(ScipError::Invalid(_))));
    }
}
