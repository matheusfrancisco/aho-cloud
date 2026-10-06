//! Run MCP result tools (executor.md "Tool schemas", triage.md "`submit_triage` schema", fix.md
//! `submit_fix`): the checked-in JSON Schemas the runner serves verbatim as `inputSchema`, and the
//! tool inputs and outputs as serde types.
//!
//! The runner validates a call against the schema (and the 256 KiB cap), turns it into a
//! [`crate::wire::runner::WireRunResult`], and returns the server's `SubmitAck.message` in the
//! output. `submit_triage`'s input is [`TriageSubmission`], forwarded unchanged.

use serde::{Deserialize, Serialize};

use crate::domain::fix::PatchChange;
use crate::domain::recipe::RecipeIssue;
use crate::domain::{ArtifactId, FailureSignature, GateKind, RecipeHash, RepoPath, RunKind, Verdict};
use crate::wire::runner::CitationDraftDto;
#[cfg(doc)]
use crate::wire::runner::TriageSubmission;

/// Largest accepted tool call input, in bytes.
pub const MAX_INPUT_BYTES: usize = 256 * 1024;

pub const SUBMIT_TRIAGE: &str = "submit_triage";
pub const SUBMIT_RECIPE: &str = "submit_recipe";
pub const SUBMIT_DIAGNOSIS: &str = "submit_diagnosis";
pub const SUBMIT_FIX: &str = "submit_fix";

/// Knowledge MCP tool names (knowledge.md), in the order the server lists them. The runner's
/// fail-closed harness tool check and the server's tool table both use this list.
pub const KNOWLEDGE_TOOLS: [&str; 15] = [
    "repo_overview",
    "search_code",
    "read_file",
    "find_symbol",
    "references",
    "call_graph",
    "lookup_error",
    "app_map",
    "route_info",
    "explain",
    "glossary",
    "changes_between",
    "blame",
    "similar_tickets",
    "run_recipe",
];

/// JSON Schema (draft 2020-12) of `submit_triage` (triage.md).
pub const SUBMIT_TRIAGE_SCHEMA: &str = include_str!("schemas/submit_triage.json");
/// JSON Schema of `submit_recipe` (Learn).
pub const SUBMIT_RECIPE_SCHEMA: &str = include_str!("schemas/submit_recipe.json");
/// JSON Schema of `submit_diagnosis` (Diagnose), with fix.md's `suggested_fix` and `fix_sites`.
pub const SUBMIT_DIAGNOSIS_SCHEMA: &str = include_str!("schemas/submit_diagnosis.json");
/// JSON Schema of `submit_fix` (Fix).
pub const SUBMIT_FIX_SCHEMA: &str = include_str!("schemas/submit_fix.json");

/// A result tool: its MCP name and `inputSchema`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubmitTool {
    pub name: &'static str,
    pub input_schema: &'static str,
}

/// The one result tool of a harness run kind; Verify runs have no harness.
pub const fn submit_tool(kind: RunKind) -> Option<SubmitTool> {
    match kind {
        RunKind::Learn => Some(SubmitTool { name: SUBMIT_RECIPE, input_schema: SUBMIT_RECIPE_SCHEMA }),
        RunKind::Triage => Some(SubmitTool { name: SUBMIT_TRIAGE, input_schema: SUBMIT_TRIAGE_SCHEMA }),
        RunKind::Diagnose => Some(SubmitTool { name: SUBMIT_DIAGNOSIS, input_schema: SUBMIT_DIAGNOSIS_SCHEMA }),
        RunKind::Fix => Some(SubmitTool { name: SUBMIT_FIX, input_schema: SUBMIT_FIX_SCHEMA }),
        RunKind::Verify => None,
    }
}

// ---------------------------------------------------------------------------------------------
// submit_recipe
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubmitRecipeInput {
    pub toml: String,
    /// Markdown.
    pub notes: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubmitRecipeOutput {
    pub submitted: bool,
    pub recipe_hash: RecipeHash,
    pub warnings: Vec<RecipeIssue>,
    pub message: String,
}

// ---------------------------------------------------------------------------------------------
// submit_diagnosis
// ---------------------------------------------------------------------------------------------

/// The agent's claim; only the repro gate moves a cause to `Reproduced`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosisVerdict {
    Reproduced,
    NotReproduced,
    Inconclusive,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubmitDiagnosisInput {
    pub verdict: DiagnosisVerdict,
    /// Markdown.
    pub summary: String,
    /// Required for `inconclusive`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Workspace path of the repro script; required unless `inconclusive`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script: Option<String>,
    /// Required unless `inconclusive`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<FailureSignature>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suggested_fix: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fix_sites: Vec<CitationDraftDto>,
}

impl SubmitDiagnosisInput {
    /// The domain verdict, enforcing the schema's conditional requirements (an `if`/`then` a
    /// lenient validator may skip): `reason` for `inconclusive`, `script` and `signature`
    /// otherwise.
    pub fn verdict(&self) -> Result<Verdict, String> {
        match self.verdict {
            DiagnosisVerdict::Inconclusive => match self.reason.as_deref().map(str::trim) {
                Some(reason) if !reason.is_empty() => Ok(Verdict::Inconclusive { reason: reason.to_owned() }),
                _ => Err("reason is required when the verdict is inconclusive".into()),
            },
            verdict => {
                if self.script.is_none() || self.signature.is_none() {
                    return Err("script and signature are required unless the verdict is inconclusive".into());
                }
                Ok(match verdict {
                    DiagnosisVerdict::Reproduced => Verdict::Reproduced,
                    _ => Verdict::NotReproduced,
                })
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubmitDiagnosisOutput {
    pub submitted: bool,
    /// `repro` when a gate follows (`reproduced`), else `null`.
    pub gate: Option<GateKind>,
    pub script_artifact: Option<ArtifactId>,
    pub message: String,
}

// ---------------------------------------------------------------------------------------------
// submit_fix
// ---------------------------------------------------------------------------------------------

/// The agent describes its change; the patch is always computed from the fork.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubmitFixInput {
    /// Markdown.
    pub summary: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PatchedFile {
    pub path: RepoPath,
    pub change: PatchChange,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubmitFixOutput {
    pub submitted: bool,
    pub patch_artifact: ArtifactId,
    pub files: Vec<PatchedFile>,
    /// Paths dropped by the exclusion policy.
    pub excluded: Vec<RepoPath>,
    pub message: String,
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use serde::de::DeserializeOwned;
    use serde_json::{Value, json};

    use super::*;
    use crate::domain::{Matcher, ReproMethod, SignatureKind};
    use crate::wire::runner::{CauseDraftDto, TriageSubmission};

    fn schema(text: &str) -> Value {
        serde_json::from_str(text).unwrap_or_else(|e| panic!("schema is not JSON: {e}"))
    }

    fn keys(v: &Value) -> BTreeSet<String> {
        v.as_object().map(|o| o.keys().cloned().collect()).unwrap_or_default()
    }

    fn strings(v: &Value) -> BTreeSet<String> {
        v.as_array().map(|a| a.iter().filter_map(|s| s.as_str().map(String::from)).collect()).unwrap_or_default()
    }

    /// The schema object and the DTO agree on property names (from a fully populated value) and
    /// on which ones are required (from a minimal value).
    fn agrees<T: Serialize + DeserializeOwned>(object: &Value, full: &Value, minimal: &Value) {
        assert_eq!(object["type"], "object");
        assert_eq!(object["additionalProperties"], false);
        let parsed: T = serde_json::from_value(full.clone()).unwrap_or_else(|e| panic!("full sample: {e}"));
        assert_eq!(keys(&serde_json::to_value(parsed).unwrap_or_default()), keys(&object["properties"]), "properties");
        let parsed: T = serde_json::from_value(minimal.clone()).unwrap_or_else(|e| panic!("minimal sample: {e}"));
        assert_eq!(keys(&serde_json::to_value(parsed).unwrap_or_default()), strings(&object["required"]), "required");
    }

    #[test]
    fn every_schema_is_a_draft_2020_12_object_named_after_its_tool() {
        for kind in [RunKind::Learn, RunKind::Triage, RunKind::Diagnose, RunKind::Fix] {
            let tool = submit_tool(kind).unwrap_or_else(|| panic!("{kind:?} has a submit tool"));
            assert!(tool.input_schema.len() <= MAX_INPUT_BYTES);
            let s = schema(tool.input_schema);
            assert_eq!(s["$schema"], "https://json-schema.org/draft/2020-12/schema");
            assert_eq!(s["title"], tool.name);
            assert_eq!(s["$id"], format!("mage:{}:v1", tool.name));
            assert_eq!(s["type"], "object");
        }
        assert_eq!(submit_tool(RunKind::Verify), None);
    }

    #[test]
    fn submit_triage_schema_mirrors_the_submission_dtos() {
        let s = schema(SUBMIT_TRIAGE_SCHEMA);
        let citation_full = json!({ "path": "a.ts", "start_line": 1, "end_line": 2, "quote": "x = 1;", "note": "n" });
        let citation_min = json!({ "path": "a.ts", "start_line": 1, "end_line": 2 });
        agrees::<CitationDraftDto>(&s["$defs"]["citation"], &citation_full, &citation_min);
        let cause = json!({
            "title": "t", "explanation": "e", "evidence": [citation_min], "repro_steps": [], "confirm_if": "c",
            "rule_out_if": "r", "confidence": 0.5, "method": "unknown"
        });
        agrees::<CauseDraftDto>(&s["$defs"]["cause"], &cause, &cause);
        let submission = json!({ "summary": "s", "causes": [cause], "missing_info": [] });
        agrees::<TriageSubmission>(&s, &submission, &submission);
        for method in strings(&s["$defs"]["cause"]["properties"]["method"]["enum"]) {
            assert!(serde_json::from_value::<ReproMethod>(json!(method)).is_ok(), "{method}");
        }
    }

    #[test]
    fn submit_diagnosis_schema_mirrors_its_input() {
        let s = schema(SUBMIT_DIAGNOSIS_SCHEMA);
        let full = json!({
            "verdict": "reproduced", "summary": "s", "reason": "r", "script": "e2e/bug.spec.ts",
            "signature": { "kind": "assertion_text", "value": "expected 3", "matcher": "contains" },
            "suggested_fix": "guard it", "fix_sites": [{ "path": "a.ts", "start_line": 1, "end_line": 2 }]
        });
        agrees::<SubmitDiagnosisInput>(&s, &full, &json!({ "verdict": "inconclusive", "summary": "s" }));
        agrees::<CitationDraftDto>(
            &s["$defs"]["citation"],
            &json!({ "path": "a.ts", "start_line": 1, "end_line": 2, "quote": "x = 1;", "note": "n" }),
            &json!({ "path": "a.ts", "start_line": 1, "end_line": 2 }),
        );
        for verdict in strings(&s["properties"]["verdict"]["enum"]) {
            assert!(serde_json::from_value::<DiagnosisVerdict>(json!(verdict)).is_ok(), "{verdict}");
        }
        let signature = &s["properties"]["signature"];
        assert_eq!(strings(&signature["required"]), keys(&signature["properties"]));
        for kind in strings(&signature["properties"]["kind"]["enum"]) {
            assert!(serde_json::from_value::<SignatureKind>(json!(kind)).is_ok(), "{kind}");
        }
        for matcher in strings(&signature["properties"]["matcher"]["enum"]) {
            assert!(serde_json::from_value::<Matcher>(json!(matcher)).is_ok(), "{matcher}");
        }
    }

    #[test]
    fn submit_recipe_and_fix_schemas_mirror_their_inputs() {
        let recipe = json!({ "toml": "version = 1\n", "notes": "" });
        agrees::<SubmitRecipeInput>(&schema(SUBMIT_RECIPE_SCHEMA), &recipe, &recipe);
        let fix = json!({ "summary": "guarded the null" });
        agrees::<SubmitFixInput>(&schema(SUBMIT_FIX_SCHEMA), &fix, &fix);
        assert!(
            serde_json::from_value::<SubmitFixInput>(json!({ "summary": "s", "patch": "diff --git" })).is_err(),
            "the agent never supplies the diff"
        );
    }

    #[test]
    fn diagnosis_verdict_enforces_conditional_fields() {
        let signature = FailureSignature { kind: SignatureKind::ExitCode, value: "1".into(), matcher: Matcher::Exact };
        let base = SubmitDiagnosisInput {
            verdict: DiagnosisVerdict::Reproduced,
            summary: "s".into(),
            reason: None,
            script: Some("repro.sh".into()),
            signature: Some(signature),
            suggested_fix: None,
            fix_sites: vec![],
        };
        assert_eq!(base.verdict(), Ok(Verdict::Reproduced));
        let not = SubmitDiagnosisInput { verdict: DiagnosisVerdict::NotReproduced, ..base.clone() };
        assert_eq!(not.verdict(), Ok(Verdict::NotReproduced));
        let no_script = SubmitDiagnosisInput { script: None, ..base.clone() };
        assert!(no_script.verdict().is_err());
        let no_signature = SubmitDiagnosisInput { signature: None, verdict: DiagnosisVerdict::NotReproduced, ..base.clone() };
        assert!(no_signature.verdict().is_err());

        let inconclusive = SubmitDiagnosisInput { verdict: DiagnosisVerdict::Inconclusive, script: None, signature: None, ..base };
        assert!(inconclusive.verdict().is_err(), "reason missing");
        let blank = SubmitDiagnosisInput { reason: Some("  ".into()), ..inconclusive.clone() };
        assert!(blank.verdict().is_err(), "reason blank");
        let reasoned = SubmitDiagnosisInput { reason: Some(" no seed data ".into()), ..inconclusive };
        assert_eq!(reasoned.verdict(), Ok(Verdict::Inconclusive { reason: "no seed data".into() }));
    }

    #[test]
    fn tool_outputs_have_the_documented_fields() {
        let out = SubmitDiagnosisOutput { submitted: true, gate: Some(GateKind::Repro), script_artifact: None, message: "m".into() };
        assert_eq!(
            serde_json::to_value(&out).unwrap_or_default(),
            json!({ "submitted": true, "gate": "repro", "script_artifact": null, "message": "m" })
        );
        let fix: SubmitFixOutput = serde_json::from_value(json!({
            "submitted": true, "patch_artifact": "art_01j9z3k4m5n6p7q8r9s0t1v3b8",
            "files": [{ "path": "src/a.ts", "change": "modify" }], "excluded": ["package-lock.json"], "message": "m"
        }))
        .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(fix.files[0].change, PatchChange::Modify);
        let recipe: SubmitRecipeOutput = serde_json::from_value(json!({
            "submitted": true, "recipe_hash": "9c1e5a7b3d2f4e6a8b0c1d3e5f7a9b2c4d6e8f0a1b3c5d7e9f2a4b6c8d0e1f3a",
            "warnings": [{ "path": "services.api", "code": "version", "message": "m" }], "message": "m"
        }))
        .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(recipe.warnings.len(), 1);
    }
}
