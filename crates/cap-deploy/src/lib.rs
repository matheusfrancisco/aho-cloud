//! aho capabilities for aho cloud.
//!
//! - [`DeployDraft`] (`cloud_draft`, ReadOnly): drafts a deploy manifest from this session's own
//!   log with `cloud-draft`, every field carrying its evidence and strength, and records the
//!   draft as a `Deploy/Drafted` entry.
//! - [`DeployPropose`] (`cloud_propose`, NetworkEgress): validates a manifest locally, uploads the
//!   session log as provenance and files the release proposal with the control plane, recorded
//!   as `Deploy/Proposed`. It can only propose; approving a release is a signed human action the
//!   tool has no way to perform.
//!
//! Both fold into the `Deploy` namespace ([`fold_deploy`]).

#![forbid(unsafe_code)]

mod propose;
#[cfg(test)]
mod tests;

use std::sync::Arc;

use aho_cap::{
    BoundWorld, BuildError, Capability, CapabilityId, Deps, ExecOutcome, ExecRequest, HandleKind, Kind, Manifest, MissingDep, Present,
    Registrar, Resolver, ToolDecl, ToolExec, declare_capability,
};
use aho_log::LogStore;
use aho_types::{Card, Consequence, ContentBlock, RootSchema, SessionId, Value};
use cloud_contract::draft::{Draft, Strength};
use serde::{Deserialize, Serialize};
use serde_json::json;

pub use crate::propose::{API_URL_DEFAULT, CREDENTIAL, DeployPropose, ProposeConfig, ProposeDeps};

/// Log namespace of this crate's entries.
pub const NS: &str = "Deploy";
pub const DRAFTED: &str = "Drafted";
pub const PROPOSED: &str = "Proposed";

// ───────────────────────────── entries and fold ─────────────────────────────

/// Body of `Deploy/Drafted`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Drafted {
    /// aho session id, lowercase hex (the log's file stem).
    pub session_id: String,
    /// Hash of the head entry the draft was made from.
    pub head_hash: String,
    pub draft: Draft,
    /// Pointers a human must confirm before the draft is a manifest.
    pub unconfirmed: Vec<String>,
}

/// Body of `Deploy/Proposed`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Proposed {
    pub project: String,
    /// Release id the control plane assigned.
    pub release: String,
    /// `blake3:<hex>` of the proposed manifest.
    pub manifest_hash: String,
}

/// The `Deploy` projection: the latest draft and every release this session proposed.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeployState {
    pub drafts: u64,
    pub last_draft: Option<LastDraft>,
    pub proposed: Vec<Proposed>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LastDraft {
    pub head_hash: String,
    pub fields: usize,
    pub unconfirmed: Vec<String>,
}

/// Custom fold for [`NS`]. Rejects bodies that do not decode, drafts whose recorded
/// `unconfirmed` list disagrees with their fields, malformed hashes, and a release proposed twice.
pub fn fold_deploy(prev: Option<&Value>, name: &str, body: &Value) -> Result<Value, String> {
    let mut st: DeployState = match prev {
        Some(v) => serde_json::from_value(v.clone()).map_err(|e| format!("Deploy state does not decode: {e}"))?,
        None => DeployState::default(),
    };
    match name {
        DRAFTED => {
            let d: Drafted = serde_json::from_value(body.clone()).map_err(|e| format!("Deploy/Drafted body: {e}"))?;
            if !is_hex64(&d.head_hash) {
                return Err(format!("Deploy/Drafted head_hash `{}` is not 64 lowercase hex", d.head_hash));
            }
            if d.unconfirmed != d.draft.unconfirmed() {
                return Err("Deploy/Drafted lists unconfirmed fields that disagree with the draft".into());
            }
            st.drafts += 1;
            st.last_draft = Some(LastDraft { head_hash: d.head_hash, fields: d.draft.fields.len(), unconfirmed: d.unconfirmed });
        }
        PROPOSED => {
            let p: Proposed = serde_json::from_value(body.clone()).map_err(|e| format!("Deploy/Proposed body: {e}"))?;
            if p.project.is_empty() || !p.project.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') {
                return Err(format!("Deploy/Proposed project `{}` is not a slug", p.project));
            }
            if p.release.trim().is_empty() {
                return Err("Deploy/Proposed has an empty release id".into());
            }
            if !p.manifest_hash.strip_prefix("blake3:").is_some_and(is_hex64) {
                return Err(format!("Deploy/Proposed manifest_hash `{}` is not blake3:<64 hex>", p.manifest_hash));
            }
            if st.proposed.iter().any(|q| q.project == p.project && q.release == p.release) {
                return Err(format!("release `{}` of `{}` was already proposed", p.release, p.project));
            }
            st.proposed.push(p);
        }
        other => return Err(format!("unknown Deploy entry `{other}`")),
    }
    serde_json::to_value(st).map_err(|e| e.to_string())
}

fn is_hex64(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// `value.events` item the session appends as a `Custom` entry after the tool result.
fn event(name: &str, body: &impl Serialize) -> Value {
    json!({"ns": NS, "name": name, "body": body})
}

// ───────────────────────────── cloud_draft ─────────────────────────────

/// The session's own log and id, seeded by `aho-session` as services.
pub struct SessionDeps {
    pub log: Option<Arc<dyn LogStore>>,
    pub session: Option<SessionId>,
}

impl Deps for SessionDeps {
    const REQUIRES: &'static [HandleKind] = &[];
    fn resolve(r: &Resolver) -> Result<Self, MissingDep> {
        Ok(SessionDeps { log: r.service("aho.log"), session: r.service("aho.session_id") })
    }
}

/// The session id as aho prints it and names the log file: lowercase hex.
pub fn session_hex(id: SessionId) -> String {
    format!("{:x}", id.0)
}

fn empty_schema(_: &BoundWorld) -> RootSchema {
    RootSchema(json!({"type": "object", "properties": {}, "additionalProperties": false}))
}

fn draft_call_card(_: &Value) -> Card {
    Card::Generic { title: "Deploy draft".into(), body: "drafting a manifest from this session's log".into() }
}

fn draft_result_card(_: &Value, v: &Value) -> Card {
    let fields = v.get("draft").and_then(|d| d.get("fields")).and_then(Value::as_object).map_or(0, |m| m.len());
    let pending = v.get("unconfirmed").and_then(Value::as_array).map_or(0, Vec::len);
    Card::Generic { title: "Deploy draft".into(), body: format!("{fields} fields drafted, {pending} need a human's confirmation") }
}

pub static DRAFT_TOOLS: &[ToolDecl] = &[ToolDecl::new(
    "cloud_draft",
    "Draft an aho cloud deploy manifest from this session's own log: the build that last succeeded, the servers that kept \
     running, the ports the operator mapped, the hosts the code talks to, and the secret names the code reads. Every field \
     carries its evidence (the log entries) and strength; inferred and observed fields must be confirmed by the human before \
     proposing. Values that look like credentials are never copied.",
    Consequence::ReadOnly,
)
.safe()
.schema(empty_schema)
.present(Present { call: draft_call_card, result: draft_result_card })];

/// Top-level manifest fields a session log cannot tell; the human supplies them.
const HUMAN_FIELDS: &[&str] = &["api_version", "project", "source", "build", "processes", "auth", "data_class", "owner", "budget"];

/// `cloud_draft`.
#[derive(Clone)]
pub struct DeployDraft {
    log: Option<Arc<dyn LogStore>>,
    session: Option<SessionId>,
}

impl DeployDraft {
    fn run(&self) -> ExecOutcome {
        let (Some(log), Some(id)) = (&self.log, self.session) else {
            return ExecOutcome::err("NO_SESSION_LOG", "this host did not give the tool a session log, so there is nothing to draft from.");
        };
        let Some(head) = log.head() else {
            return ExecOutcome::err("EMPTY_LOG", "the session log is empty; nothing was drafted.");
        };
        let branch = match log.read_branch(head) {
            Ok(b) => b,
            Err(e) => return ExecOutcome::err("LOG_BROKEN", format!("the session log does not verify ({e}); nothing was drafted.")),
        };
        let session_id = session_hex(id);
        let draft = cloud_draft::draft_from_entries(&session_id, &branch);
        let head_hash = branch.last().map(|e| e.hash.to_hex()).unwrap_or_default();
        let unconfirmed = draft.unconfirmed();
        let text = render_draft(&draft, &unconfirmed);
        let body = Drafted { session_id: session_id.clone(), head_hash: head_hash.clone(), draft, unconfirmed };
        let events = json!([event(DRAFTED, &body)]);
        let value = json!({
            "session_id": session_id,
            "head_hash": head_hash,
            "entries": branch.len(),
            "draft": body.draft,
            "unconfirmed": body.unconfirmed,
            "events": events,
        });
        ExecOutcome::Done { value, content: vec![ContentBlock::text(text)], meta: None }
    }
}

/// The model-facing summary: each field with its strength and first note, then what is missing.
fn render_draft(draft: &Draft, unconfirmed: &[String]) -> String {
    if draft.fields.is_empty() {
        return "Nothing in this session's log shows a build, a server, a port, an outbound host or a secret name. Ask the human \
                for the whole manifest, or run the build and the server first."
            .into();
    }
    let mut out = format!("Drafted {} fields from this session's log.\n", draft.fields.len());
    for (pointer, f) in &draft.fields {
        let strength = match f.strength {
            Strength::Inferred => "inferred",
            Strength::Observed => "observed",
            Strength::Chained => "chained",
            Strength::Declared => "declared",
        };
        let confirm = if unconfirmed.contains(pointer) { ", needs confirmation" } else { "" };
        let why = f.evidence.first().map(|e| e.note.as_str()).unwrap_or("");
        out.push_str(&format!("- {pointer} = {} ({strength}{confirm}): {why}\n", f.value));
    }
    let missing: Vec<&str> = HUMAN_FIELDS
        .iter()
        .copied()
        .filter(|name| !draft.fields.keys().any(|p| p.strip_prefix('/').is_some_and(|r| r.split('/').next() == Some(name))))
        .collect();
    if !missing.is_empty() {
        out.push_str(&format!("Ask the human for: {} (api_version is `{}`).\n", missing.join(", "), cloud_contract::manifest::API_VERSION));
    }
    if !unconfirmed.is_empty() {
        out.push_str(&format!("Show the human the {} fields that need confirmation before proposing.\n", unconfirmed.len()));
    }
    out
}

impl Capability for DeployDraft {
    const ID: CapabilityId = Self::ID_;
    const MANIFEST: &'static Manifest = &Self::MANIFEST_;
    type Deps = SessionDeps;
    type Config = ();

    fn build(d: SessionDeps, _: ()) -> Result<Self, BuildError> {
        Ok(DeployDraft { log: d.log, session: d.session })
    }

    fn register(&self, r: &mut Registrar<'_>) {
        r.entry_kind(NS, fold_deploy);
        let me = self.clone();
        let exec: ToolExec = Arc::new(move |_: ExecRequest| {
            let out = me.run();
            Box::pin(async move { out })
        });
        r.tool(&DRAFT_TOOLS[0], exec);
    }
}

declare_capability!(
    DeployDraft,
    "cloud-deploy-draft",
    kind = Kind::Tool,
    provides = [],
    requires = [],
    tools = DRAFT_TOOLS,
    entry_kinds = ["Deploy/Drafted"],
    doc = "cloud_draft: an aho cloud deploy-manifest draft from this session's log, every field with its evidence and strength."
);
