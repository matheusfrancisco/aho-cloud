//! Text renderings: the approval card (what a release widens and narrows, and what approving it
//! takes), release lists, refusals, drafted fields, sandboxes.

use std::fmt::Write;

use cloud_api::{CheckResult, DomainView, Release, Sandbox};
use cloud_contract::diff::{Direction, SurfaceDiff};
use cloud_contract::ledger::Provenance;
use cloud_contract::policy::Requirement;

pub fn diff(d: &SurfaceDiff) -> String {
    if d.is_empty() {
        return "  (no changes)\n".into();
    }
    let mut out = String::new();
    // Widenings first: they are what the approval is for.
    for dir in [Direction::Widen, Direction::Neutral, Direction::Narrow] {
        for c in d.changes.iter().filter(|c| c.direction == dir) {
            let label = match dir {
                Direction::Widen => "widen  ",
                Direction::Neutral => "neutral",
                Direction::Narrow => "narrow ",
            };
            let _ = writeln!(out, "  {label}  {}", c.detail);
        }
    }
    out
}

pub fn requirement(r: &Requirement) -> String {
    let n = r.approvals;
    let plural = if n == 1 { "" } else { "s" };
    if r.owner_required { format!("needs {n} approval{plural}, including an org owner") } else { format!("needs {n} approval{plural}") }
}

fn provenance(p: &Provenance) -> String {
    match p {
        Provenance::Attested { session_id, head_hash } => format!("attested (aho session {session_id}, head {})", short(head_hash)),
        Provenance::Unattested { commit, ci_run } => {
            let mut s = "unattested".to_owned();
            if let Some(c) = commit {
                let _ = write!(s, ", commit {}", short(c));
            }
            if let Some(r) = ci_run {
                let _ = write!(s, ", CI run {r}");
            }
            s
        }
    }
}

fn short(h: &str) -> &str {
    let body = h.split_once(':').map_or(h, |(_, b)| b);
    body.get(..12).unwrap_or(body)
}

fn status(r: &Release) -> String {
    let s = serde_json::to_value(r.status).ok().and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_default();
    match r.stage {
        Some(stage) => format!("{s}, active at {}", stage.as_str()),
        None => s,
    }
}

/// The approval card.
pub fn release(r: &Release) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "release {} · project {} · {}", r.id, r.project, status(r));
    let _ = writeln!(out, "manifest {}", r.manifest_hash);
    let _ = writeln!(out, "provenance {}", provenance(&r.provenance));
    if let Some(d) = &r.image_digest {
        let _ = writeln!(out, "image {d}");
    }
    let _ = writeln!(out, "changes against live:");
    out.push_str(&diff(&r.diff));
    let _ = writeln!(out, "{}", requirement(&r.requirement));
    for a in &r.approvals {
        let owner = if a.owner { ", owner" } else { "" };
        let _ = writeln!(out, "approved for {} by {}{owner}", a.stage.as_str(), a.user.email);
    }
    for t in &r.taint {
        let _ = writeln!(out, "TAINTED: a value of secret {} was found at {}; rotate it", t.secret, t.location);
    }
    if !r.undeclared_egress.is_empty() {
        let _ = writeln!(out, "undeclared egress seen in staging: {}", r.undeclared_egress.join(", "));
    }
    out
}

pub fn release_row(r: &Release) -> String {
    let widen = r.diff.widenings().count();
    let kind = match r.provenance {
        Provenance::Attested { .. } => "attested",
        Provenance::Unattested { .. } => "unattested",
    };
    format!("{}  {}  {} widening{}  {kind}", r.id, status(r), widen, if widen == 1 { "" } else { "s" })
}

/// Prints every refusal with its fix; empty when accepted.
pub fn check(result: &CheckResult) -> String {
    let mut out = String::new();
    for r in &result.refusals {
        let code = serde_json::to_value(r.code).ok().and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_default();
        let _ = writeln!(out, "refused  {}  {} ({code}); fix: {}", r.field, r.message, r.fix);
    }
    for e in &result.edge_errors {
        let _ = writeln!(out, "refused  edge.toml  {e}");
    }
    out
}

pub fn domain(d: &DomainView) -> String {
    let state = serde_json::to_value(d.state).ok().and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_default();
    let mode = serde_json::to_value(d.mode).ok().and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_default();
    let mut out = format!("{}  {state}  ({mode})\n", d.hostname);
    for r in &d.records {
        let _ = writeln!(out, "  {:<5} {}  {}", r.kind, r.name, r.value);
    }
    out
}

fn sandbox_state(s: &Sandbox) -> String {
    serde_json::to_value(s.state).ok().and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_default()
}

pub fn sandbox_row(s: &Sandbox) -> String {
    format!("{}  {}  {}  {}", s.slug, sandbox_state(s), s.org, s.url.as_deref().unwrap_or("-"))
}

/// One sandbox, `now` in Unix seconds (for when it stops).
pub fn sandbox(s: &Sandbox, now: u64) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "sandbox {} · org {} · {}", s.slug, s.org, sandbox_state(s));
    let _ = writeln!(out, "image {} · {} cpus · {} MiB memory · {} MiB disk", s.image, s.cpus, s.mem_mib, s.disk_mib);
    let _ = writeln!(out, "egress {}", if s.settings.egress.is_empty() { "(none)".into() } else { s.settings.egress.join(", ") });
    match (s.settings.port, &s.url) {
        (Some(port), Some(url)) => _ = writeln!(out, "port {port} at {url} (org members only)"),
        (Some(port), None) => _ = writeln!(out, "port {port}"),
        (None, _) => _ = writeln!(out, "no public port"),
    }
    let _ = write!(out, "auto-stop {}h after each start", s.settings.auto_stop_hours);
    match s.stops_at {
        Some(at) => {
            let left = at.saturating_sub(now);
            let _ = writeln!(out, "; stops in {}h {:02}m", left / 3600, left % 3600 / 60);
        }
        None => out.push('\n'),
    }
    if let Some(e) = &s.error {
        let _ = writeln!(out, "failed: {e}");
    }
    out
}

#[cfg(test)]
mod tests {
    use cloud_contract::diff::{Category, Change};

    use super::*;

    #[test]
    fn widenings_lead_the_card() {
        let d = SurfaceDiff {
            changes: vec![
                Change { category: Category::Egress, direction: Direction::Narrow, detail: "egress − a.com".into() },
                Change { category: Category::Code, direction: Direction::Neutral, detail: "source or build changed".into() },
                Change { category: Category::Egress, direction: Direction::Widen, detail: "egress + b.com".into() },
            ],
        };
        let lines: Vec<String> = diff(&d).lines().map(|l| l.split_whitespace().next().unwrap().to_owned()).collect();
        assert_eq!(lines, ["widen", "neutral", "narrow"]);
        assert_eq!(requirement(&Requirement { approvals: 2, owner_required: true }), "needs 2 approvals, including an org owner");
        assert_eq!(requirement(&Requirement { approvals: 1, owner_required: false }), "needs 1 approval");
    }
}
