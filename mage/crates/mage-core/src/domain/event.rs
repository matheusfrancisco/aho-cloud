//! `RunEvent` (domain.md "RunEvent"): the per-run event log.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::domain::budget::Usage;
use crate::domain::gate::{GateKind, GateStatus};
use crate::domain::ids::{ArtifactId, RunId};
use crate::domain::run::RunStatus;

/// Maximum preview size in bytes; longer previews are truncated, not rejected.
pub const PREVIEW_MAX_BYTES: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum RunEventKind {
    Status(RunStatus),
    AssistantText(String),
    ToolCall { name: String, args_preview: String },
    ToolResult { name: String, ok: bool, preview: String },
    Usage(Usage),
    Notice(String),
    GateStarted(GateKind),
    GateResult(GateKind, GateStatus),
    Artifact(ArtifactId),
    Error(String),
}

impl RunEventKind {
    /// Truncates `args_preview` / `preview` to [`PREVIEW_MAX_BYTES`] on a char boundary.
    pub fn truncate_previews(&mut self) {
        match self {
            Self::ToolCall { args_preview: p, .. } | Self::ToolResult { preview: p, .. } => truncate_utf8(p, PREVIEW_MAX_BYTES),
            _ => {}
        }
    }
}

/// Truncates `s` to at most `max` bytes without splitting a character.
pub fn truncate_utf8(s: &mut String, max: usize) {
    if s.len() <= max {
        return;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
}

/// `seq` is assigned by the server, strictly increasing per run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
pub struct RunEvent {
    pub run_id: RunId,
    pub seq: u64,
    #[serde(with = "time::serde::rfc3339")]
    #[ts(type = "string")]
    pub at: OffsetDateTime,
    pub kind: RunEventKind,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn previews_truncate_on_char_boundary() {
        // 'é' is 2 bytes; 4095 ASCII + 'é' crosses the limit mid-character.
        let mut kind =
            RunEventKind::ToolResult { name: "read".into(), ok: true, preview: format!("{}é{}", "a".repeat(4095), "b".repeat(10)) };
        kind.truncate_previews();
        let RunEventKind::ToolResult { preview, .. } = &kind else { unreachable!() };
        assert_eq!(preview.len(), 4095);
        let mut exact = RunEventKind::ToolCall { name: "x".into(), args_preview: "a".repeat(4096) };
        exact.truncate_previews();
        let RunEventKind::ToolCall { args_preview, .. } = &exact else { unreachable!() };
        assert_eq!(args_preview.len(), 4096);
        // Other kinds are untouched.
        let mut text = RunEventKind::AssistantText("a".repeat(5000));
        text.truncate_previews();
        assert_eq!(text, RunEventKind::AssistantText("a".repeat(5000)));
    }

    #[test]
    fn event_kind_serde_shape() {
        let k = RunEventKind::Status(RunStatus::AwaitingGate);
        assert_eq!(serde_json::to_value(&k).unwrap(), serde_json::json!({"type": "status", "data": "awaiting_gate"}));
        let g = RunEventKind::GateStarted(GateKind::Repro);
        let back: RunEventKind = serde_json::from_value(serde_json::to_value(&g).unwrap()).unwrap();
        assert_eq!(back, g);
    }
}
