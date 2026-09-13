//! aho session log → deploy-manifest draft. Every drafted field names the log entries it came
//! from (`aho-log:<session>#<entry-hash>`) and how strong that evidence is, so the confirm screen
//! can show a human what the agent actually did before anything becomes a manifest.
//!
//! Derivations:
//! - **build**: the last `bash` call that ran a build (`docker build`, `npm|pnpm|yarn build`,
//!   `cargo build`, `go build`, `pip install`, `mvn`/`gradle`) and whose op completed with code 0
//!   → [`Strength::Chained`] (the base image of a command build is [`Strength::Inferred`]);
//! - **processes**: long-running server commands; [`Strength::Chained`] when a `Child/Spawned`
//!   receipt from `system:ops` ties a live child to the call, else [`Strength::Inferred`];
//! - **ports**: `guest_port` of the port maps in the latest operator-appended `Net/Configured`
//!   → [`Strength::Chained`]; ports a server command names → [`Strength::Inferred`];
//! - **egress**: hosts of URLs in tool arguments and written files, dev-only hosts removed
//!   → [`Strength::Inferred`]; allowed `Net/Flow` destinations → [`Strength::Observed`] (IP only);
//! - **secret names**: environment reads in written code → [`Strength::Inferred`].
//!
//! Values that look like credentials are never copied into the draft, not even into notes.

#![forbid(unsafe_code)]

mod classify;
mod scan;
mod shell;

use std::collections::HashMap;
use std::net::IpAddr;

use aho_log::{Entry, EntryKind, LogError, LogStore, MemoryLog, OpStatus, ToolOutcome};
use aho_types::{OpId, Principal};
use cloud_contract::draft::{Draft, Evidence, Strength};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::classify::BuildStep;
use crate::shell::Segment;

/// Log namespace of `aho-net` entries.
const NET_NS: &str = "Net";
/// The only principal whose `Net/Flow` entries count; a tool's `value.events` cannot forge one.
const FLOW_BY: &str = "net-userspace";

/// Why a session log was not drafted.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Error {
    #[error("the session log has no entries")]
    Empty,
    /// An entry does not hash to its recorded hash, or its parent is missing or hashes differently.
    #[error("the session log's hash chain does not verify at entry {0}")]
    Tampered(u64),
    #[error("the session log does not decode: {0}")]
    Decode(String),
}

impl From<LogError> for Error {
    fn from(e: LogError) -> Self {
        match e {
            LogError::ChainBroken { at } => Error::Tampered(at.0),
            LogError::UnknownParent(id) => Error::Tampered(id.0),
            other => Error::Decode(other.to_string()),
        }
    }
}

/// The verified session a draft was made from; the same facts the control plane returns for an
/// uploaded log (`cloud_api::SessionUploaded`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSummary {
    pub session_id: String,
    /// Lowercase hex of the head entry's hash.
    pub head_hash: String,
    /// Entries on the verified head branch.
    pub entries: u64,
}

impl SessionSummary {
    /// Summary of a verified branch (oldest first); `None` when it is empty.
    pub fn of(session_id: &str, branch: &[Entry]) -> Option<Self> {
        let head = branch.last()?;
        Some(Self { session_id: session_id.to_owned(), head_hash: head.hash.to_hex(), entries: branch.len() as u64 })
    }
}

/// Decodes a JSONL session log and verifies the hash chain of its head branch; returns that
/// branch, oldest first.
pub fn verify_jsonl(text: &str) -> Result<Vec<Entry>, Error> {
    let log = MemoryLog::from_jsonl(text)?;
    let head = log.head().ok_or(Error::Empty)?;
    let branch = log.read_branch(head)?;
    if branch.is_empty() {
        return Err(Error::Empty);
    }
    Ok(branch)
}

/// Drafts from a JSONL session log after verifying its chain. The text does not carry aho's
/// session id (that is the file name), so the summary's `session_id` is the hex hash of the root
/// entry; callers that know the id use [`verify_jsonl`] and [`draft_from_entries`].
pub fn draft_from_jsonl(text: &str) -> Result<(Draft, SessionSummary), Error> {
    let branch = verify_jsonl(text)?;
    let session_id = branch.first().map(|e| e.hash.to_hex()).ok_or(Error::Empty)?;
    let summary = SessionSummary::of(&session_id, &branch).ok_or(Error::Empty)?;
    Ok((draft_from_entries(&session_id, &branch), summary))
}

/// Drafts from the entries of one branch, oldest first. Does not verify the chain.
pub fn draft_from_entries(session_id: &str, entries: &[Entry]) -> Draft {
    let s = Session::index(session_id, entries);
    let mut d = Draft::default();
    s.build(&mut d);
    s.processes(&mut d);
    s.egress(&mut d);
    s.secrets(&mut d);
    d
}

// ───────────────────────────── the indexed session ─────────────────────────────

struct Call<'a> {
    entry: &'a Entry,
    tool: &'a str,
    args: &'a Value,
    /// `OpDone` entry and status.
    done: Option<(&'a Entry, &'a OpStatus)>,
    result: Option<(&'a ToolOutcome, &'a Value)>,
    /// `Child/Spawned` receipt from `system:ops` for this call's op.
    receipt: Option<&'a Entry>,
}

impl Call<'_> {
    fn command(&self) -> Option<&str> {
        self.args.get("command").and_then(Value::as_str)
    }

    /// The op completed with code 0 and, for a shell, the command exited 0 on its own (a
    /// `bash` op completes even when its command fails or is killed).
    fn completed_ok(&self) -> bool {
        matches!(self.done, Some((_, OpStatus::Completed { code: 0 })))
            && self.result.is_none_or(|(outcome, value)| {
                matches!(outcome, ToolOutcome::Ok)
                    && value.get("timed_out").and_then(Value::as_bool) != Some(true)
                    && value.get("exit").is_none_or(|exit| exit.as_i64() == Some(0))
            })
    }

    /// Still running when the log ends, stopped by a cancel, killed by the time limit, or of
    /// unknown fate: the command did not simply finish.
    fn kept_running(&self) -> bool {
        let timed_out = self.result.is_some_and(|(_, v)| v.get("timed_out").and_then(Value::as_bool) == Some(true));
        timed_out || matches!(self.done, None | Some((_, OpStatus::Cancelled { .. } | OpStatus::Uncertain { .. })))
    }

    /// Text a file-writing call put into the workspace, with its path, if the write succeeded.
    fn written(&self) -> Option<(&str, &str)> {
        let path = self.args.get("path").and_then(Value::as_str)?;
        let text = WRITE_PAYLOAD.iter().find_map(|k| self.args.get(*k).and_then(Value::as_str))?;
        let ok = self.result.is_some_and(|(o, _)| matches!(o, ToolOutcome::Ok));
        ok.then_some((path, text))
    }
}

struct Session<'a> {
    id: &'a str,
    calls: Vec<Call<'a>>,
    /// Latest `Net/Configured` appended by the operator.
    net_config: Option<(&'a Entry, aho_net::NetworkConfig)>,
    flows: Vec<(&'a Entry, aho_net::FlowEvent)>,
}

const SHELL_TOOLS: &[&str] = &["bash", "bash_persistent"];

impl<'a> Session<'a> {
    fn index(id: &'a str, entries: &'a [Entry]) -> Self {
        let mut calls: Vec<Call<'a>> = Vec::new();
        let mut by_call = HashMap::new();
        let mut by_op: HashMap<OpId, usize> = HashMap::new();
        let mut open: Vec<OpId> = Vec::new();
        let mut net_config = None;
        let mut flows = Vec::new();
        for e in entries {
            match &e.kind {
                EntryKind::ToolCall { call, tool, args, .. } => {
                    by_call.insert(call.clone(), calls.len());
                    calls.push(Call { entry: e, tool, args, done: None, result: None, receipt: None });
                }
                EntryKind::OpStarted { op, call, .. } => {
                    if let Some(&i) = by_call.get(call) {
                        by_op.insert(*op, i);
                        open.push(*op);
                    }
                }
                EntryKind::OpDone { op, status } => {
                    if let Some(&i) = by_op.get(op) {
                        calls[i].done = Some((e, status));
                        open.retain(|o| o != op);
                    }
                }
                EntryKind::ToolResult { call, outcome, value, .. } => {
                    if let Some(&i) = by_call.get(call) {
                        calls[i].result = Some((outcome, value));
                    }
                }
                EntryKind::Custom { ns, name, body } => {
                    if let Some(receipt) = aho_proc::child_receipt(e) {
                        // The receipt names the tool; it belongs to the newest open op of that tool.
                        let owner = open.iter().rev().filter_map(|op| by_op.get(op)).find(|&&i| calls[i].tool == receipt.label);
                        if let Some(&i) = owner
                            && calls[i].receipt.is_none()
                        {
                            calls[i].receipt = Some(e);
                        }
                    } else if ns == NET_NS && name == "Configured" && e.by == Principal::Operator {
                        if let Some(cfg) = body.get("config").and_then(|c| serde_json::from_value(c.clone()).ok()) {
                            net_config = Some((e, cfg));
                        }
                    } else if ns == NET_NS
                        && name == "Flow"
                        && matches!(&e.by, Principal::System { name } if name == FLOW_BY)
                        && let Ok(flow) = serde_json::from_value::<aho_net::FlowEvent>(body.clone())
                    {
                        flows.push((e, flow));
                    }
                }
                _ => {}
            }
        }
        Session { id, calls, net_config, flows }
    }

    fn evidence(&self, e: &Entry, note: impl Into<String>) -> Evidence {
        Evidence { uri: format!("aho-log:{}#{}", self.id, e.hash.to_hex()), note: note.into() }
    }

    // ── build ──

    fn build(&self, d: &mut Draft) {
        let last = self.calls.iter().rev().filter(|c| SHELL_TOOLS.contains(&c.tool) && c.completed_ok()).find_map(|c| {
            let segs = shell::segments(c.command()?);
            segs.into_iter().rev().find_map(|s| classify::build_step(&s).map(|b| (c, s, b)))
        });
        let Some((call, seg, step)) = last else { return };
        let shown = scan::render_safe(&seg.argv);
        let done = call.done.map(|(e, _)| format!(" (op done aho-log:{}#{})", self.id, e.hash.to_hex())).unwrap_or_default();
        let ev = |what: &str| self.evidence(call.entry, format!("{what}: `{shown}` completed with code 0{done}"));
        match step {
            BuildStep::Dockerfile { path, context } => {
                let strength = if classify::inside_source(&path) && classify::inside_source(&context) {
                    Strength::Chained
                } else {
                    Strength::Inferred
                };
                d.offer("/build/kind", json!("dockerfile"), Strength::Chained, ev("last successful build"));
                d.offer("/build/path", json!(path), strength, ev("Dockerfile of the last successful build"));
                d.offer("/build/context", json!(context), strength, ev("build context of the last successful build"));
            }
            BuildStep::Command { argv, image, toolchain } => {
                d.offer("/build/kind", json!("command"), Strength::Chained, ev("last successful build"));
                if !argv.iter().any(|w| scan::looks_secret(w)) {
                    d.offer("/build/command", json!(argv), Strength::Chained, ev("command of the last successful build"));
                }
                d.offer(
                    "/build/base_image",
                    json!(image),
                    Strength::Inferred,
                    ev(&format!("usual base image for a {toolchain} build; confirm the version")),
                );
            }
        }
    }

    // ── processes and ports ──

    fn processes(&self, d: &mut Draft) {
        let mut procs: Vec<Proc<'_>> = Vec::new();
        for call in &self.calls {
            if !(SHELL_TOOLS.contains(&call.tool) || call.tool == "terminal_open") {
                continue;
            }
            let Some(command) = call.command() else { continue };
            let long_running = call.tool == "terminal_open" || call.kept_running();
            for seg in shell::segments(command) {
                if !classify::is_server(&seg) || !(long_running || seg.background) {
                    continue;
                }
                let strength = if call.receipt.is_some() { Strength::Chained } else { Strength::Inferred };
                match procs.iter_mut().find(|p| p.seg.argv == seg.argv) {
                    Some(p) => p.seen.push((call, strength)),
                    None => procs.push(Proc { port: classify::port(&seg), seg, seen: vec![(call, strength)], mapped: None }),
                }
            }
        }

        // Operator port maps: to the server that names the port, else the first without one,
        // else a process of their own.
        if let Some((entry, cfg)) = &self.net_config {
            let mut ports: Vec<u16> = Vec::new();
            for m in &cfg.port_maps {
                if !ports.contains(&m.guest_port) {
                    ports.push(m.guest_port);
                }
            }
            for port in ports {
                let slot = procs
                    .iter()
                    .position(|p| p.port == Some(port) && p.mapped.is_none())
                    .or_else(|| procs.iter().position(|p| p.mapped.is_none() && p.port.is_none()));
                match slot {
                    Some(i) => procs[i].mapped = Some((port, *entry)),
                    None => procs.push(Proc { seg: Segment::default(), seen: Vec::new(), port: None, mapped: Some((port, *entry)) }),
                }
            }
        }

        let mut web = 0;
        let mut worker = 0;
        for (i, p) in procs.iter().enumerate() {
            let at = |f: &str| format!("/processes/{i}/{f}");
            let port = p.mapped.map(|(port, _)| port).or(p.port);
            let (base, n) = if port.is_some() {
                web += 1;
                ("web", web)
            } else {
                worker += 1;
                ("worker", worker)
            };
            let name = if n == 1 { base.to_owned() } else { format!("{base}-{n}") };
            let first = p.seen.first().map(|(c, _)| c.entry).or(p.mapped.map(|(_, e)| e));
            if let Some(first) = first {
                d.offer(&at("name"), json!(name), Strength::Inferred, self.evidence(first, "name picked by the drafter"));
            }
            if !p.seg.argv.is_empty() {
                let shown = scan::render_safe(&p.seg.argv);
                let clean = !p.seg.argv.iter().any(|w| scan::looks_secret(w));
                for (call, strength) in &p.seen {
                    let how = match call.receipt {
                        Some(r) => format!("server `{shown}` ran with a live child (receipt aho-log:{}#{})", self.id, r.hash.to_hex()),
                        None => format!("server `{shown}` kept running in `{}`", call.tool),
                    };
                    if clean {
                        d.offer(&at("command"), json!(p.seg.argv), *strength, self.evidence(call.entry, how));
                    }
                }
            }
            match (p.mapped, p.port) {
                (Some((port, entry)), named) => {
                    let also = if named == Some(port) { "; the server command names it too" } else { "" };
                    let note = format!("operator mapped guest port {port} to the host in Net/Configured{also}");
                    d.offer(&at("port"), json!(port), Strength::Chained, self.evidence(entry, note));
                    d.offer(
                        &at("expose"),
                        json!("public"),
                        Strength::Inferred,
                        self.evidence(entry, "a host-mapped port is meant to be reached"),
                    );
                }
                (None, Some(port)) => {
                    if let Some((call, _)) = p.seen.first() {
                        d.offer(
                            &at("port"),
                            json!(port),
                            Strength::Inferred,
                            self.evidence(call.entry, format!("the command names port {port}")),
                        );
                    }
                }
                (None, None) => {}
            }
            if let (Some(port), Some(first)) = (port, first) {
                let ev = || self.evidence(first, format!("HTTP check on port {port}; confirm the path"));
                d.offer(&at("health/kind"), json!("http"), Strength::Inferred, ev());
                d.offer(&at("health/path"), json!("/"), Strength::Inferred, ev());
            }
        }
    }

    // ── egress ──

    fn egress(&self, d: &mut Draft) {
        let mut hosts: Vec<(String, Vec<Evidence>)> = Vec::new();
        for call in &self.calls {
            let sources = arg_strings(call.args).into_iter().map(|t| (t, None)).chain(call.written().map(|(path, t)| (t, Some(path))));
            for (text, written_to) in sources {
                for host in scan::url_hosts(text) {
                    if scan::dev_only(&host) {
                        continue;
                    }
                    let note = match written_to {
                        Some(path) => format!("`{host}` appears in code written to {path}"),
                        None => format!("`{host}` appears in `{}` arguments", call.tool),
                    };
                    let ev = self.evidence(call.entry, note);
                    match hosts.iter_mut().find(|(h, _)| *h == host) {
                        Some((_, evs)) => evs.push(ev),
                        None => hosts.push((host, vec![ev])),
                    }
                }
            }
        }
        let mut ips: Vec<(IpAddr, Vec<Evidence>)> = Vec::new();
        for (entry, flow) in &self.flows {
            let ip = flow.destination.ip();
            if !flow.allowed || flow.destination.port() == 53 || !public_ip(ip) {
                continue;
            }
            let ev = Evidence {
                uri: format!("net:{}#{}", self.id, entry.hash.to_hex()),
                note: format!("allowed flow to {ip}; flows carry no hostname, confirm or replace with the host"),
            };
            match ips.iter_mut().find(|(i, _)| *i == ip) {
                Some((_, evs)) => evs.push(ev),
                None => ips.push((ip, vec![ev])),
            }
        }
        let offers = hosts
            .into_iter()
            .map(|(h, e)| (h, e, Strength::Inferred))
            .chain(ips.into_iter().map(|(ip, e)| (ip.to_string(), e, Strength::Observed)));
        for (i, (value, evs, strength)) in offers.enumerate() {
            for ev in evs {
                d.offer(&format!("/egress/{i}"), json!(value), strength, ev);
            }
        }
    }

    // ── secret names ──

    fn secrets(&self, d: &mut Draft) {
        let mut names: Vec<String> = Vec::new();
        for call in &self.calls {
            let Some((path, text)) = call.written() else { continue };
            for read in scan::env_reads(text) {
                let i = match names.iter().position(|n| *n == read.name) {
                    Some(i) => i,
                    None => {
                        names.push(read.name.clone());
                        names.len() - 1
                    }
                };
                let note = format!("`{}` in code written to {path} (line {} of the write)", read.expr, read.line);
                d.offer(&format!("/secrets/{i}/name"), json!(read.name), Strength::Inferred, self.evidence(call.entry, note));
            }
        }
    }
}

struct Proc<'a> {
    seg: Segment,
    seen: Vec<(&'a Call<'a>, Strength)>,
    /// Port the command names.
    port: Option<u16>,
    /// Port the operator mapped, with the `Net/Configured` entry.
    mapped: Option<(u16, &'a Entry)>,
}

/// Keys whose string is file content when the arguments also carry a `path`.
const WRITE_PAYLOAD: &[&str] = &["content", "new_string", "file_text", "new_str"];

/// Every string in a call's arguments except file content being written (scanned only once the
/// write succeeded, through [`Call::written`]).
fn arg_strings(args: &Value) -> Vec<&str> {
    fn walk<'v>(v: &'v Value, out: &mut Vec<&'v str>) {
        match v {
            Value::String(s) => out.push(s),
            Value::Array(items) => items.iter().for_each(|i| walk(i, out)),
            Value::Object(map) => map.values().for_each(|i| walk(i, out)),
            _ => {}
        }
    }
    let mut out = Vec::new();
    match args {
        Value::Object(map) if map.contains_key("path") => {
            map.iter().filter(|(k, _)| !WRITE_PAYLOAD.contains(&k.as_str())).for_each(|(_, v)| walk(v, &mut out));
        }
        other => walk(other, &mut out),
    }
    out
}

/// Globally routable enough to be egress: not loopback, private, link-local or unspecified.
fn public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            !(v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.octets()[0] == 100 && (v4.octets()[1] & 0xc0) == 64)
        }
        IpAddr::V6(v6) => {
            !(v6.is_loopback() || v6.is_unspecified() || (v6.segments()[0] & 0xfe00) == 0xfc00 || (v6.segments()[0] & 0xffc0) == 0xfe80)
        }
    }
}
