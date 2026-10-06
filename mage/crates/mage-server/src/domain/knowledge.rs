//! Pure serde types of the knowledge query side that other server areas persist or embed
//! (knowledge.md "Retrieval pipeline for triage", "Additions": `AnswerSource`, `SimilarResolved`).
//! Triage stores a `CandidateSet` inside its run spec, so these live in the domain, not in
//! `app::knowledge_query` (which re-exports them).

use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use mage_core::domain::budget::duration_ms;
use mage_core::domain::{Citation, CommitSha, Layer, RepoVersionKey, TicketId};
use mage_core::ports::knowledge::SymbolKind;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use time::OffsetDateTime;

/// What answered a knowledge query: a layer, the repo's recipe, or ticket memory. Serializes as
/// the bare name (`"ErrorSites"`, `"Recipe"`, `"Memory"`), the envelope's `layer` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AnswerSource {
    Layer(Layer),
    Recipe,
    Memory,
}

impl AnswerSource {
    pub const fn name(self) -> &'static str {
        match self {
            AnswerSource::Layer(layer) => layer_name(layer),
            AnswerSource::Recipe => "Recipe",
            AnswerSource::Memory => "Memory",
        }
    }
}

/// `Layer` as named in the envelope and in tool errors (`"Files"`, `"AppMap"`, …).
pub const fn layer_name(layer: Layer) -> &'static str {
    match layer {
        Layer::Files => "Files",
        Layer::Symbols => "Symbols",
        Layer::Refs => "Refs",
        Layer::ErrorSites => "ErrorSites",
        Layer::AppMap => "AppMap",
        Layer::Semantic => "Semantic",
        Layer::History => "History",
    }
}

impl From<Layer> for AnswerSource {
    fn from(layer: Layer) -> Self {
        AnswerSource::Layer(layer)
    }
}

impl fmt::Display for AnswerSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for AnswerSource {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "Recipe" => Ok(AnswerSource::Recipe),
            "Memory" => Ok(AnswerSource::Memory),
            _ => Layer::ALL
                .into_iter()
                .find(|layer| layer_name(*layer) == s)
                .map(AnswerSource::Layer)
                .ok_or_else(|| format!("unknown answer source `{s}`")),
        }
    }
}

impl Serialize for AnswerSource {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.name())
    }
}

impl<'de> Deserialize<'de> for AnswerSource {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = <std::borrow::Cow<'de, str>>::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// Ticket text handed to the retriever (triage builds it from the ticket).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TicketContext {
    pub title: String,
    pub body: String,
    pub error_strings: Vec<String>,
    /// Previous deployment of the customer's environment, else previous release, else none.
    pub last_good: Option<CommitSha>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    StackFrame,
    ErrorMatch,
    I18nMatch,
    Route,
    Name,
    Glossary,
    Semantic,
    Graph,
    History,
    SimilarTicket,
}

/// One reason a candidate scored.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Signal {
    pub stage: Stage,
    pub detail: String,
    pub contribution: f32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolSummary {
    pub name: String,
    pub kind: SymbolKind,
    pub signature: Option<String>,
}

/// A candidate location: (path, innermost symbol span or a ±10-line window) at the scope sha.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Candidate {
    pub citation: Citation,
    pub symbol: Option<SymbolSummary>,
    pub score: f32,
    pub signals: Vec<Signal>,
}

/// What one retrieval stage did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StageReport {
    pub stage: Stage,
    pub layer: AnswerSource,
    pub ran: bool,
    pub reason: Option<String>,
    pub hits: u32,
    #[serde(rename = "elapsed_ms", with = "duration_ms")]
    pub elapsed: Duration,
}

/// Output of `Retriever::triage_candidates`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CandidateSet {
    pub version: RepoVersionKey,
    /// Best first, at most `RetrievalLimits.max_candidates`.
    pub candidates: Vec<Candidate>,
    /// At most 5.
    pub similar: Vec<SimilarResolved>,
    pub stages: Vec<StageReport>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SimilarMatch {
    Text,
    ErrorString,
}

/// A resolved ticket similar to a query (`TicketStore::similar_resolved`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SimilarResolved {
    pub ticket: TicketId,
    pub identifier: String,
    pub title: String,
    pub url: String,
    #[serde(with = "time::serde::rfc3339")]
    pub resolved_at: OffsetDateTime,
    pub root_cause: String,
    /// Each at its own sha.
    pub cause_citations: Vec<Citation>,
    pub fix_pr_url: Option<String>,
    pub score: f32,
    pub matched_on: Vec<SimilarMatch>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answer_source_round_trips_by_name() {
        let all = Layer::ALL.into_iter().map(AnswerSource::Layer).chain([AnswerSource::Recipe, AnswerSource::Memory]);
        for source in all {
            let json = serde_json::to_string(&source).unwrap_or_else(|e| panic!("{e}"));
            assert_eq!(json, format!("\"{}\"", source.name()));
            let back: AnswerSource = serde_json::from_str(&json).unwrap_or_else(|e| panic!("{e}"));
            assert_eq!(back, source);
        }
        assert!(serde_json::from_str::<AnswerSource>("\"files\"").is_err(), "names are case-sensitive");
    }
}
