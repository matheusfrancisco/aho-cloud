//! A manifest before a human confirmed it: every field carries its evidence and how strong that
//! evidence is. The aho `deploy` capability drafts from a session log; `cloud-importers` drafts
//! from repo files; both produce this type, so the confirm screen is one screen.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use ts_rs::TS;

use crate::manifest::Manifest;

/// Weakest to strongest machine evidence; `Declared` is a human's word and needs no confirmation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum Strength {
    /// From a static scan of files.
    Inferred,
    /// From runtime observation that is not hash-chained (e.g. aho `Net/Flow`).
    Observed,
    /// From hash-chained log entries.
    Chained,
    /// Typed by a human.
    Declared,
}

impl Strength {
    /// Inferred and observed fields must be confirmed before the draft becomes a manifest.
    pub fn needs_confirmation(self) -> bool {
        matches!(self, Strength::Inferred | Strength::Observed)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
pub struct Evidence {
    /// `aho-log:<session>#<entry-hash>`, `file:<path>:<line>`, or `net:<flow>`.
    pub uri: String,
    pub note: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
pub struct DraftField {
    #[ts(type = "unknown")]
    pub value: Value,
    pub strength: Strength,
    #[serde(default)]
    pub evidence: Vec<Evidence>,
    #[serde(default)]
    pub confirmed: bool,
}

/// Fields keyed by JSON pointer into the manifest (`/processes/0/port`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
pub struct Draft {
    pub fields: BTreeMap<String, DraftField>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DraftError {
    #[error("field `{0}` is not a JSON pointer")]
    Pointer(String),
    #[error("field `{0}` conflicts with `{1}`")]
    Conflict(String, String),
    #[error("unconfirmed fields: {}", .0.join(", "))]
    Unconfirmed(Vec<String>),
    #[error("draft is not a manifest yet: {0}")]
    Shape(String),
}

impl Draft {
    /// Records a machine-derived value. A stronger existing value is kept; an equal or weaker one
    /// is replaced and its evidence merged.
    pub fn offer(&mut self, pointer: &str, value: Value, strength: Strength, evidence: Evidence) {
        match self.fields.get_mut(pointer) {
            Some(f) if f.strength > strength => {}
            Some(f) if f.value == value => {
                f.strength = f.strength.max(strength);
                f.evidence.push(evidence);
            }
            _ => {
                self.fields.insert(pointer.to_owned(), DraftField { value, strength, evidence: vec![evidence], confirmed: false });
            }
        }
    }

    /// A human typed or corrected the value.
    pub fn declare(&mut self, pointer: &str, value: Value) {
        let evidence = self.fields.remove(pointer).map(|f| f.evidence).unwrap_or_default();
        self.fields.insert(pointer.to_owned(), DraftField { value, strength: Strength::Declared, evidence, confirmed: true });
    }

    /// A human accepted the drafted value as is. Returns false if there is no such field.
    pub fn confirm(&mut self, pointer: &str) -> bool {
        self.fields.get_mut(pointer).map(|f| f.confirmed = true).is_some()
    }

    pub fn unconfirmed(&self) -> Vec<String> {
        self.fields.iter().filter(|(_, f)| f.strength.needs_confirmation() && !f.confirmed).map(|(k, _)| k.clone()).collect()
    }

    /// The JSON the fields assemble into, regardless of confirmation.
    pub fn to_value(&self) -> Result<Value, DraftError> {
        let mut root = Value::Object(Default::default());
        for (pointer, field) in &self.fields {
            insert(&mut root, pointer, field.value.clone())?;
        }
        Ok(root)
    }

    /// The manifest, once every weak field is confirmed. Does not run [`crate::validate`].
    pub fn into_manifest(&self) -> Result<Manifest, DraftError> {
        let pending = self.unconfirmed();
        if !pending.is_empty() {
            return Err(DraftError::Unconfirmed(pending));
        }
        serde_json::from_value(self.to_value()?).map_err(|e| DraftError::Shape(e.to_string()))
    }
}

/// Inserts `value` at `pointer`, creating objects for names and arrays for numeric segments.
fn insert(root: &mut Value, pointer: &str, value: Value) -> Result<(), DraftError> {
    let Some(rest) = pointer.strip_prefix('/') else {
        return Err(DraftError::Pointer(pointer.to_owned()));
    };
    let segments: Vec<String> = rest.split('/').map(|s| s.replace("~1", "/").replace("~0", "~")).collect();
    let mut node = root;
    for (i, seg) in segments.iter().enumerate() {
        let last = i + 1 == segments.len();
        let next_is_index = segments.get(i + 1).is_some_and(|s| s.parse::<usize>().is_ok());
        let fresh = || if next_is_index { Value::Array(Vec::new()) } else { Value::Object(Default::default()) };
        node = match node {
            Value::Object(map) => {
                if last {
                    if map.contains_key(seg) {
                        return Err(DraftError::Conflict(pointer.to_owned(), seg.clone()));
                    }
                    map.insert(seg.clone(), value);
                    return Ok(());
                }
                map.entry(seg.clone()).or_insert_with(fresh)
            }
            Value::Array(items) => {
                let idx: usize = seg.parse().map_err(|_| DraftError::Pointer(pointer.to_owned()))?;
                while items.len() <= idx {
                    items.push(Value::Null);
                }
                if last {
                    items[idx] = value;
                    return Ok(());
                }
                if items[idx].is_null() {
                    items[idx] = fresh();
                }
                &mut items[idx]
            }
            _ => return Err(DraftError::Conflict(pointer.to_owned(), segments[..i].join("/"))),
        };
    }
    Err(DraftError::Pointer(pointer.to_owned()))
}
