//! PR title, body and commit message (fix.md F3 "Use case OpenPullRequest" step 5, "PR body
//! template"). Pure and golden-tested (`testdata/pr/*.md`). Media come from
//! `crate::app::fix::media`: uploaded user-attachments URLs render inline, anything else falls
//! back to links to the Mage artifact page.

use std::fmt::Write as _;

use mage_core::domain::fix::ChangeSetDigest;
use mage_core::domain::{ArtifactId, Citation, CommitSha, FixStats, GateId, RepoPath, RunId};
use url::Url;

use crate::app::fix::text::{cap_chars, cell, code, first_paragraph, permalink, quote, short, ui_link, untrusted};

/// GitHub's PR body limit [unverified: 65 536 chars].
pub const BODY_MAX_CHARS: usize = 65_536;
const TITLE_MAX_CHARS: usize = 200;
const SUMMARY_MAX_CHARS: usize = 8_000;

/// One recording slot of the before/after table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaLink {
    /// `https://github.com/user-attachments/assets/…` (renders inline).
    Uploaded(Url),
    /// Upload refused or impossible: link to the Mage artifact page.
    Fallback(ArtifactId),
    Missing,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrMediaLinks {
    pub before_gif: MediaLink,
    pub after_gif: MediaLink,
    pub after_video: MediaLink,
}

/// The lint row of the verification table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LintRow {
    NotConfigured,
    /// `lint_mode = required` (the gate passed, so it exited 0).
    Required { command: String },
    Advisory { command: String, exit: Option<i32> },
}

#[derive(Debug, Clone)]
pub struct PrBodyInput<'a> {
    pub ui_url: &'a Url,
    pub github_web: &'a Url,
    pub ticket_identifier: &'a str,
    pub ticket_url: &'a Url,
    pub ticket_title: &'a str,
    /// Display name of the member who asked; `None` = opened automatically by org setting.
    pub requester: Option<&'a str>,
    pub cause_explanation: &'a str,
    pub evidence: &'a [Citation],
    pub owner: &'a str,
    pub name: &'a str,
    /// The sha the evidence was checked at.
    pub evidence_sha: CommitSha,
    pub summary: &'a str,
    pub stats: FixStats,
    pub excluded: &'a [RepoPath],
    /// `(gated base, new parent)` when the change was rebased onto the moved default head.
    pub rebased: Option<(CommitSha, CommitSha)>,
    /// Regression file path and the command that runs it.
    pub regression: Option<(&'a RepoPath, String)>,
    pub parent: CommitSha,
    pub repetitions: u8,
    pub signature_value: &'a str,
    pub test_command: Option<&'a str>,
    pub lint: LintRow,
    pub media: &'a PrMediaLinks,
    pub trace: Option<ArtifactId>,
    pub test_log: Option<ArtifactId>,
    pub fix_run: RunId,
    pub gate: GateId,
    pub digest: ChangeSetDigest,
}

/// `fix: {cause.title} ({identifier})`, one line.
pub fn title(cause_title: &str, identifier: &str) -> String {
    let t = untrusted(cause_title, TITLE_MAX_CHARS).replace(['\r', '\n'], " ");
    format!("fix: {t} ({identifier})")
}

/// fix.md step 5 commit message; the trailers let a retry recognise Mage's own commit.
pub fn commit_message(cause_title: &str, identifier: &str, summary: &str, ticket_url: &Url, fix_run: RunId, gate: GateId, digest: &ChangeSetDigest) -> String {
    let mut msg = title(cause_title, identifier);
    let para = untrusted(first_paragraph(summary), 2_000);
    if !para.is_empty() {
        msg.push_str("\n\n");
        msg.push_str(&para);
    }
    let _ = write!(msg, "\n\nLinear: {ticket_url}\nMage-Run: {fix_run}\nMage-Gate: {gate}\nMage-Change-Set: {}\n", digest.to_hex());
    msg
}

/// How much detail a rendering keeps; later levels shorten lists first, then prose.
#[derive(Clone, Copy)]
struct Detail {
    evidence: usize,
    excluded: usize,
    prose: usize,
}

const LEVELS: [Detail; 4] = [
    Detail { evidence: usize::MAX, excluded: usize::MAX, prose: SUMMARY_MAX_CHARS },
    Detail { evidence: 5, excluded: 10, prose: SUMMARY_MAX_CHARS },
    Detail { evidence: 1, excluded: 3, prose: 2_000 },
    Detail { evidence: 0, excluded: 0, prose: 500 },
];

/// The PR body, shortened to fit [`BODY_MAX_CHARS`] (evidence and change lists first).
pub fn render(input: &PrBodyInput<'_>) -> String {
    render_within(input, BODY_MAX_CHARS)
}

pub fn render_within(input: &PrBodyInput<'_>, max_chars: usize) -> String {
    let mut last = String::new();
    for level in LEVELS {
        last = render_at(input, level);
        if last.chars().count() <= max_chars {
            return last;
        }
    }
    // Still too long (absurd inputs): keep the head and the footer comment.
    let footer = footer(input);
    // `cap_chars` may append one `…`; the separator is two newlines.
    let keep = max_chars.saturating_sub(footer.chars().count() + 3);
    let mut out = cap_chars(&last, keep);
    out.push_str("\n\n");
    out.push_str(&footer);
    out
}

fn footer(input: &PrBodyInput<'_>) -> String {
    format!("<!-- mage:change-set={} run={} gate={} -->\n", input.digest.to_hex(), input.fix_run, input.gate)
}

fn media_cell(label: &str, link: &MediaLink, ui: &Url) -> String {
    match link {
        MediaLink::Uploaded(url) => format!("![{label}]({url})"),
        MediaLink::Fallback(id) => format!("[{label} recording (Mage)]({})", ui_link(ui, &format!("artifacts/{id}"))),
        MediaLink::Missing => "—".to_owned(),
    }
}

fn render_at(input: &PrBodyInput<'_>, d: Detail) -> String {
    let mut md = String::with_capacity(4 * 1024);
    let ui = |path: &str| ui_link(input.ui_url, path);

    let _ = writeln!(md, "Fixes [{}]({}) — {}", cell(input.ticket_identifier), input.ticket_url, untrusted(input.ticket_title, 300));
    md.push('\n');
    let who = input.requester.map(|r| untrusted(r, 100)).unwrap_or_else(|| "auto (org setting)".to_owned());
    let _ = writeln!(md, "> Opened by **Mage** at the request of {who}.");
    md.push_str("> Human review required. Mage does not merge.\n\n");

    md.push_str("## Root cause\n");
    let _ = writeln!(md, "{}", untrusted(input.cause_explanation, d.prose));
    md.push('\n');
    if d.evidence > 0 && !input.evidence.is_empty() {
        let _ = writeln!(md, "Evidence (at `{}`):", short(&input.evidence_sha));
        for c in input.evidence.iter().take(d.evidence) {
            let label = format!("{}:{}-{}", c.path, c.lines.start(), c.lines.end());
            let _ = writeln!(md, "- [{}]({})", code(&label), permalink(input.github_web, input.owner, input.name, &c.sha, &c.path, &c.lines));
        }
        if input.evidence.len() > d.evidence {
            let _ = writeln!(md, "- … and {} more in the Mage UI", input.evidence.len() - d.evidence);
        }
        md.push('\n');
    }

    md.push_str("## Change\n");
    let _ = writeln!(md, "{}", quote(&untrusted(input.summary, d.prose)));
    md.push('\n');
    let s = input.stats;
    let _ = write!(md, "{} files changed, +{} −{}.", s.files, s.added, s.removed);
    if !input.excluded.is_empty() {
        let shown: Vec<String> = input.excluded.iter().take(d.excluded.max(1)).map(|p| code(p.as_str())).collect();
        let more = input.excluded.len().saturating_sub(d.excluded.max(1));
        let more = if more > 0 { format!(" and {more} more") } else { String::new() };
        let _ = write!(md, " Discarded by policy: {}{more}.", shown.join(", "));
    }
    if let Some((gated, head)) = input.rebased {
        let _ = write!(md, "\nRebased from `{}` onto `{}` and re-verified.", short(&gated), short(&head));
    }
    md.push_str("\n\n");

    if let Some((path, command)) = &input.regression {
        md.push_str("## Regression test\n");
        let _ = writeln!(
            md,
            "{} — the script that reproduced the bug; it failed before this change and passes\nafter. Run: {} (see `.mage/env.toml` for setup).",
            code(path.as_str()),
            code(command)
        );
        md.push('\n');
    }

    md.push_str("## Verification (fix gate, clean environment)\n| Check | Result |\n|---|---|\n");
    let _ = writeln!(md, "| Patch applies at `{}` | pass |", short(&input.parent));
    let _ = writeln!(
        md,
        "| Repro script ({} fresh fork{}) | pass — signature {} absent |",
        input.repetitions,
        if input.repetitions == 1 { "" } else { "s" },
        code(&cell(&untrusted(input.signature_value, 200)))
    );
    if let Some(test) = input.test_command {
        let _ = writeln!(md, "| {} | exit 0 |", code(&cell(test)));
    }
    match &input.lint {
        LintRow::NotConfigured => md.push_str("| Lint | n/a |\n"),
        LintRow::Required { command } => {
            let _ = writeln!(md, "| {} | pass |", code(&cell(command)));
        }
        LintRow::Advisory { command, exit } => {
            let result = match exit {
                Some(0) => "advisory (pass)".to_owned(),
                Some(code) => format!("advisory (exit {code})"),
                None => "advisory (not run)".to_owned(),
            };
            let _ = writeln!(md, "| {} | {result} |", code(&cell(command)));
        }
    }
    md.push('\n');

    md.push_str("## Before / after\n\n| Before (repro) | After (fix) |\n|---|---|\n");
    let _ = writeln!(md, "| {} | {} |", media_cell("before", &input.media.before_gif, input.ui_url), media_cell("after", &input.media.after_gif, input.ui_url));
    match &input.media.after_video {
        MediaLink::Uploaded(url) => {
            let _ = write!(md, "\n{url}\n");
        }
        MediaLink::Fallback(id) => {
            let _ = write!(md, "\n[After (fix) video (Mage)]({})\n", ui(&format!("artifacts/{id}")));
        }
        MediaLink::Missing => {}
    }
    md.push('\n');
    let mut links = Vec::new();
    if let Some(t) = input.trace {
        links.push(format!("[trace]({})", ui(&format!("artifacts/{t}"))));
    }
    if let Some(l) = input.test_log {
        links.push(format!("[test log]({})", ui(&format!("artifacts/{l}"))));
    }
    links.push(format!("[Mage run]({})", ui(&format!("runs/{}", input.fix_run))));
    let _ = writeln!(md, "{}", links.join(" · "));
    md.push('\n');
    md.push_str(&footer(input));
    md
}

#[cfg(test)]
mod tests;
