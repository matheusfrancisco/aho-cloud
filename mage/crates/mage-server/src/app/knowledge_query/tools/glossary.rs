//! `glossary` (layer Symbols, plus AppMap and ErrorSites catalogs when Ready; knowledge.md).
//! Definitions would come from Semantic `glossary_terms`, which v1 does not store.

use std::collections::BTreeMap;

use mage_core::domain::{Layer, RepoPath};
use mage_core::ports::knowledge::{GlossaryHit, GlossarySource};
use serde_json::{Value, json};

use super::super::envelope::{Envelope, ToolError};
use super::super::inputs::{GlossaryInput, check_len, check_range};
use super::super::scope::KnowledgeScope;
use super::super::service::{Ctx, Service, check_scope};
use super::super::text::glossary_words;

const REFS_PER_SOURCE: usize = 5;

/// Output name of a glossary source.
fn source_name(source: GlossarySource) -> &'static str {
    match source {
        GlossarySource::Symbol => "type",
        GlossarySource::Table => "table",
        GlossarySource::Endpoint => "endpoint",
        GlossarySource::Page => "page",
        GlossarySource::Event => "event",
        GlossarySource::CatalogKey => "i18n_key",
        GlossarySource::CatalogText => "ui_label",
    }
}

/// Layer a glossary source is read from (a source is used only when that layer is Ready).
fn source_layer(source: GlossarySource) -> Layer {
    match source {
        GlossarySource::Symbol | GlossarySource::Event => Layer::Symbols,
        GlossarySource::Table | GlossarySource::Endpoint | GlossarySource::Page => Layer::AppMap,
        GlossarySource::CatalogKey | GlossarySource::CatalogText => Layer::ErrorSites,
    }
}

/// Grouping key of a hit: its words, singular, joined (`Connections`, `connection` → `connection`).
fn term_key(text: &str) -> String {
    glossary_words(text).join(" ")
}

struct Term {
    weight: f32,
    /// Original spellings by frequency.
    spellings: BTreeMap<String, u32>,
    refs: BTreeMap<GlossarySource, Vec<(RepoPath, u32)>>,
}

/// Groups hits into terms, heaviest first.
pub(crate) fn group_terms(hits: &[GlossaryHit]) -> Vec<(String, Vec<GlossarySource>, Vec<(GlossarySource, Vec<(RepoPath, u32)>)>)> {
    let mut terms: BTreeMap<String, Term> = BTreeMap::new();
    for hit in hits {
        let key = term_key(&hit.text);
        if key.is_empty() {
            continue;
        }
        let term = terms.entry(key).or_insert_with(|| Term { weight: 0.0, spellings: BTreeMap::new(), refs: BTreeMap::new() });
        term.weight += hit.weight;
        *term.spellings.entry(hit.text.clone()).or_default() += 1;
        let refs = term.refs.entry(hit.source).or_default();
        if refs.len() < REFS_PER_SOURCE && !refs.contains(&hit.at) {
            refs.push(hit.at.clone());
        }
    }
    let mut out: Vec<(f32, String, Vec<GlossarySource>, Vec<(GlossarySource, Vec<(RepoPath, u32)>)>)> = terms
        .into_values()
        .map(|t| {
            let name = t.spellings.iter().max_by(|a, b| a.1.cmp(b.1).then(b.0.len().cmp(&a.0.len()))).map(|(s, _)| s.clone()).unwrap_or_default();
            let sources = t.refs.keys().copied().collect();
            (t.weight, name, sources, t.refs.into_iter().collect())
        })
        .collect();
    out.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal).then(a.1.cmp(&b.1)));
    out.into_iter().map(|(_, name, sources, refs)| (name, sources, refs)).collect()
}

impl Service {
    pub(crate) async fn glossary(&self, scope: &KnowledgeScope, input: GlossaryInput) -> Result<Envelope, ToolError> {
        check_scope(scope, input.repo.as_deref(), input.sha.as_deref())?;
        check_range("limit", input.limit, 1, 50)?;
        if let Some(term) = &input.term {
            check_len("term", term, 1, 128)?;
        }
        let ctx = self.ctx(scope).await?;
        ctx.require(Layer::Symbols, "use search_code")?;
        let (org, key) = (ctx.org(), ctx.key());
        let fetch = input.limit.saturating_mul(10);
        let hits = match &input.term {
            Some(term) => {
                let words = glossary_words(term);
                if words.is_empty() {
                    return Err(ToolError::invalid("term has no words"));
                }
                self.knowledge.glossary_hits(org, &key, &words, fetch).await?
            }
            None => self.knowledge.glossary_top(org, &key, fetch).await?,
        };
        let hits: Vec<GlossaryHit> = hits.into_iter().filter(|h| ctx.ready(source_layer(h.source))).collect();
        let mut env = ctx.envelope(Layer::Symbols);
        let mut contributing = Vec::new();
        for layer in [Layer::AppMap, Layer::ErrorSites] {
            if hits.iter().any(|h| source_layer(h.source) == layer) {
                contributing.push(layer);
            } else if !ctx.ready(layer) {
                env.note(format!("{} terms omitted: {}", crate::domain::knowledge::layer_name(layer), ctx.not_ready_reason(layer)));
            }
        }
        if !contributing.is_empty() {
            let names: Vec<&str> = contributing.iter().map(|l| crate::domain::knowledge::layer_name(*l)).collect();
            env.note(format!("contributing layers: Symbols, {}", names.join(", ")));
        }
        env.items = group_terms(&hits).into_iter().take(input.limit as usize).map(|(term, sources, refs)| term_item(&ctx, term, &sources, &refs)).collect();
        Ok(env)
    }
}

fn term_item(ctx: &Ctx<'_>, term: String, sources: &[GlossarySource], refs: &[(GlossarySource, Vec<(RepoPath, u32)>)]) -> Value {
    let refs: Vec<Value> = refs
        .iter()
        .flat_map(|(source, at)| at.iter().map(move |(path, line)| (source, path, line)))
        .map(|(source, path, line)| {
            let mut cite = ctx.cite(path, *line, *line);
            cite["source"] = json!(source_name(*source));
            cite
        })
        .collect();
    json!({ "term": term, "sources": sources.iter().map(|s| source_name(*s)).collect::<Vec<_>>(), "refs": refs })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(source: GlossarySource, text: &str, path: &str, line: u32, weight: f32) -> GlossaryHit {
        GlossaryHit { source, text: text.into(), at: (RepoPath::new(path).unwrap_or_else(|e| panic!("{e}")), line), weight }
    }

    #[test]
    fn hits_group_by_singular_words_and_rank_by_weight() {
        let hits = vec![
            hit(GlossarySource::Table, "connections", "db/schema.sql", 3, 2.0),
            hit(GlossarySource::Symbol, "Connection", "api/conn.go", 10, 1.0),
            hit(GlossarySource::Symbol, "Connection", "api/conn.go", 10, 1.0),
            hit(GlossarySource::Endpoint, "/api/review", "api/routes.go", 5, 0.5),
        ];
        let terms = group_terms(&hits);
        assert_eq!(terms.len(), 2);
        let (name, sources, refs) = &terms[0];
        assert_eq!(name, "Connection", "most frequent spelling wins");
        assert_eq!(sources, &vec![GlossarySource::Symbol, GlossarySource::Table]);
        assert_eq!(refs.iter().map(|(_, at)| at.len()).sum::<usize>(), 2, "duplicate locations collapse");
        assert_eq!(terms[1].0, "/api/review");
    }
}
