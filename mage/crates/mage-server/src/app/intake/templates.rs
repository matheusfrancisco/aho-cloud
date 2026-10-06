//! Notice templates (intake.md "Templates", fix.md "Storage and exposure" / "Media"): pure
//! rendering of one notice into the Linear comment, the Slack reply and the Linear attachment
//! subtitle.
//!
//! Security rules: model-produced text is untrusted. In Linear markdown it is escaped
//! (`\ * _ [ ] ( ) ! # < > |` and backticks), newlines folded, at most 200 chars per field; in
//! Slack it stays plain text (the adapter escapes `& < >`) and is folded and capped the same way.
//! Only Mage-built links appear: GitHub blob permalinks from citations, Mage UI links, the PR URL
//! `PullRequests` returned and Linear asset URLs Mage uploaded.

use std::fmt::Write as _;

use mage_core::domain::fix::BriefInLinear;
use mage_core::domain::{ArtifactId, CauseId, Citation, CommitSha, FixStats, RunId, TargetOrigin, TicketId};
use time::OffsetDateTime;
use url::Url;

use crate::domain::intake::{NotifyConfig, SlackDetail};

/// Per-field cap for untrusted text.
pub const FIELD_MAX_CHARS: usize = 200;
/// Linear comment budget for an inline fix brief (`brief_in_linear = full`) [unverified limit].
pub const LINEAR_COMMENT_MAX_CHARS: usize = 50_000;
/// Causes listed in a triage comment and citations per cause.
const TOP_CAUSES: usize = 3;
const CITATIONS_PER_CAUSE: usize = 2;

/// Fold newlines and whitespace runs, cap at [`FIELD_MAX_CHARS`] (with `…`).
pub fn fold(text: &str) -> String {
    let folded = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if folded.chars().count() <= FIELD_MAX_CHARS {
        return folded;
    }
    let mut out: String = folded.chars().take(FIELD_MAX_CHARS - 1).collect();
    out.push('…');
    out
}

/// Untrusted text for Linear markdown: folded, capped, metacharacters escaped.
pub fn md_text(text: &str) -> String {
    let folded = fold(text);
    let mut out = String::with_capacity(folded.len() + 8);
    for c in folded.chars() {
        if matches!(c, '\\' | '*' | '_' | '[' | ']' | '(' | ')' | '!' | '#' | '<' | '>' | '|' | '`') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Untrusted text for a Slack reply (plain; the adapter escapes `& < >`).
pub fn chat_text(text: &str) -> String {
    fold(text)
}

pub fn short_sha(sha: &CommitSha) -> String {
    sha.to_hex()[..7].to_owned()
}

/// `https://github.com/{owner}/{name}/blob/{sha}/{path}#L{start}-L{end}`, segments percent-encoded.
pub fn permalink(owner: &str, name: &str, citation: &Citation) -> Option<Url> {
    let mut url = Url::parse("https://github.com/").ok()?;
    {
        let mut segs = url.path_segments_mut().ok()?;
        segs.push(owner).push(name).push("blob").push(&citation.sha.to_hex());
        for s in citation.path.segments() {
            segs.push(s);
        }
    }
    url.set_fragment(Some(&format!("L{}-L{}", citation.lines.start(), citation.lines.end())));
    // Parentheses would end a markdown link destination.
    Url::parse(&url.as_str().replace('(', "%28").replace(')', "%29")).ok()
}

fn utc_minute(at: OffsetDateTime) -> String {
    let at = at.to_offset(time::UtcOffset::UTC);
    format!("{:04}-{:02}-{:02} {:02}:{:02} UTC", at.year(), u8::from(at.month()), at.day(), at.hour(), at.minute())
}

/// Mage UI links (short links resolved by the SPA, server.md).
pub struct Links<'a> {
    pub public_url: &'a Url,
}

impl Links<'_> {
    fn join(&self, path: &str) -> Url {
        let mut base = self.public_url.clone();
        if !base.path().ends_with('/') {
            base.set_path(&format!("{}/", base.path()));
        }
        base.join(path).unwrap_or_else(|_| self.public_url.clone())
    }
    pub fn ticket(&self, id: TicketId) -> Url {
        self.join(&format!("tickets/{id}"))
    }
    pub fn run(&self, id: RunId) -> Url {
        self.join(&format!("runs/{id}"))
    }
    pub fn brief(&self, id: CauseId) -> Url {
        self.join(&format!("causes/{id}/brief"))
    }
    pub fn artifact(&self, id: ArtifactId) -> Url {
        self.join(&format!("artifacts/{id}"))
    }
}

/// The ticket's target as the comment states it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetLine {
    pub sha: CommitSha,
    pub origin: TargetOrigin,
    /// When the deployment was observed (Deployment origin).
    pub deployed_at: Option<OffsetDateTime>,
}

/// One cause of a triage comment.
#[derive(Debug, Clone, PartialEq)]
pub struct CauseLine {
    pub title: String,
    pub confidence: f64,
    pub citations: Vec<Citation>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiagnosisOutcome {
    Reproduced,
    NotReproduced,
    Inconclusive { reason: String },
    /// Repro gate failed with no attempts left.
    GateFailed { attempts: u8 },
    TimedOut,
}

/// Notice-specific data loaded by `notify`.
#[derive(Debug, Clone, PartialEq)]
pub enum NoticeDetail {
    TriageReady { causes: Vec<CauseLine>, total: usize },
    DiagnosisVerdict {
        run_id: RunId,
        cause_rank: u16,
        cause_title: String,
        outcome: DiagnosisOutcome,
        gate_attempt: Option<u8>,
        script: Option<ArtifactId>,
        video: Option<ArtifactId>,
        trace: Option<ArtifactId>,
        /// Linear asset URL of the uploaded mp4.
        video_asset: Option<Url>,
    },
    FixBriefReady { cause_id: CauseId, cause_title: String, version: u32, mode: BriefInLinear, brief_markdown: String },
    FixReady { run_id: RunId, cause_rank: u16, stats: FixStats, summary: String, video_asset: Option<Url> },
    FixFailed { run_id: RunId, cause_rank: u16, reason: String, video_asset: Option<Url> },
    PrOpened { url: Url, number: Option<u64>, gate_summary: String },
}

#[derive(Debug, Clone, PartialEq)]
pub struct NoticeContext {
    pub ticket_id: TicketId,
    /// Linear identifier, e.g. `ENG-412`.
    pub identifier: String,
    pub target: Option<TargetLine>,
    /// `(owner, name)` of the target repo, for permalinks.
    pub repo: Option<(String, String)>,
    pub detail: NoticeDetail,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rendered {
    /// `None` = no Linear comment for this notice (e.g. `brief_in_linear = off`).
    pub linear_markdown: Option<String>,
    pub chat_text: String,
    pub chat_link: Option<(Url, String)>,
    pub attachment_subtitle: String,
}

fn target_line(target: Option<&TargetLine>) -> String {
    let Some(t) = target else { return "no target yet (choose a repo in Mage)".into() };
    let sha = short_sha(&t.sha);
    match &t.origin {
        TargetOrigin::Deployment { environment } => match t.deployed_at {
            Some(at) => format!("target `{sha}` (deployed to `{}` as of {})", md_text(environment), utc_minute(at)),
            None => format!("target `{sha}` (deployed to `{}`)", md_text(environment)),
        },
        TargetOrigin::DefaultHead => format!("target `{sha}` (assumed: default branch head; set the customer or a deployment for a precise sha)"),
        TargetOrigin::Directive => format!("target `{sha}` (from the issue's `mage-sha`/`mage-ref` line)"),
        TargetOrigin::Override => format!("target `{sha}` (set in Mage)"),
    }
}

fn citation_md(repo: Option<&(String, String)>, c: &Citation) -> Option<String> {
    let (owner, name) = repo?;
    let url = permalink(owner, name, c)?;
    let label = format!("{}:{}@{}", c.path.as_str(), c.lines, short_sha(&c.sha));
    let label = if label.contains('`') { md_text(&label) } else { format!("`{label}`") };
    Some(format!("[{label}]({url})"))
}

fn video_md(asset: Option<&Url>, name: &str) -> String {
    // Linear renders an uploaded asset embedded with image syntax [unverified for video].
    asset.map(|u| format!("\n\n![{name}]({u})")).unwrap_or_default()
}

pub fn render(ctx: &NoticeContext, cfg: &NotifyConfig, links: &Links<'_>) -> Rendered {
    let ticket_link = links.ticket(ctx.ticket_id);
    let ident = chat_text(&ctx.identifier);
    let full = cfg.slack_detail == SlackDetail::Full;
    match &ctx.detail {
        NoticeDetail::TriageReady { causes, total } => {
            let mut md = format!("**Mage triage** · {}\n", target_line(ctx.target.as_ref()));
            for (i, cause) in causes.iter().take(TOP_CAUSES).enumerate() {
                let _ = write!(md, "\n{}. **{}** · confidence {:.2}", i + 1, md_text(&cause.title), cause.confidence);
                for c in cause.citations.iter().take(CITATIONS_PER_CAUSE) {
                    if let Some(link) = citation_md(ctx.repo.as_ref(), c) {
                        let _ = write!(md, "\n   {link}");
                    }
                }
            }
            if *total > TOP_CAUSES {
                let _ = write!(md, "\n\n{} more in Mage.", total - TOP_CAUSES);
            }
            let _ = write!(md, "\n\n[Open in Mage]({ticket_link}) to run a diagnostic.");
            let mut text = format!("Mage found {total} likely causes for {ident}.");
            if full {
                if let Some(top) = causes.first() {
                    let _ = write!(text, " Top: {} ({:.2}).", chat_text(&top.title), top.confidence);
                }
            }
            Rendered {
                linear_markdown: Some(md),
                chat_text: text,
                chat_link: Some((ticket_link, "Open in Mage".into())),
                attachment_subtitle: format!("Triaged · {total} causes"),
            }
        }
        NoticeDetail::DiagnosisVerdict { run_id, cause_rank, cause_title, outcome, gate_attempt, script, video, trace, video_asset } => {
            let run_link = links.run(*run_id);
            let verdict = match outcome {
                DiagnosisOutcome::Reproduced => "reproduced".to_owned(),
                DiagnosisOutcome::NotReproduced => "not reproduced".to_owned(),
                DiagnosisOutcome::Inconclusive { .. } => "inconclusive".to_owned(),
                DiagnosisOutcome::GateFailed { attempts } => format!("not confirmed (repro gate failed after {attempts} attempts)"),
                DiagnosisOutcome::TimedOut => "timed out".to_owned(),
            };
            let mut md = format!("**Mage diagnostic** · cause #{cause_rank} **{}** · {verdict}\n", md_text(cause_title));
            let _ = write!(md, "\n{}", target_line(ctx.target.as_ref()));
            match (outcome, gate_attempt) {
                (DiagnosisOutcome::Reproduced, Some(a)) => {
                    let _ = write!(md, "\nRepro gate passed (attempt {a}).");
                }
                (DiagnosisOutcome::GateFailed { .. }, Some(a)) => {
                    let _ = write!(md, "\nRepro gate failed (attempt {a}).");
                }
                (DiagnosisOutcome::Inconclusive { reason }, _) => {
                    let _ = write!(md, "\nReason: {}", md_text(reason));
                }
                _ => {}
            }
            let mut extra = vec![format!("[run]({run_link})")];
            for (label, id) in [("repro script", script), ("video", video), ("trace", trace)] {
                if let Some(id) = id {
                    extra.push(format!("[{label}]({})", links.artifact(*id)));
                }
            }
            let _ = write!(md, "\n\n{}", extra.join(" · "));
            md.push_str(&video_md(video_asset.as_ref(), "repro.mp4"));
            let subtitle = match outcome {
                DiagnosisOutcome::Reproduced => format!("Reproduced · cause #{cause_rank}"),
                DiagnosisOutcome::NotReproduced | DiagnosisOutcome::GateFailed { .. } => "Not reproduced".to_owned(),
                DiagnosisOutcome::Inconclusive { .. } | DiagnosisOutcome::TimedOut => "Inconclusive".to_owned(),
            };
            let mut text = format!("Diagnostic for cause #{cause_rank}: {verdict}.");
            if full {
                let _ = write!(text, " Cause: {}.", chat_text(cause_title));
            }
            Rendered { linear_markdown: Some(md), chat_text: text, chat_link: Some((run_link, "Details".into())), attachment_subtitle: subtitle }
        }
        NoticeDetail::FixBriefReady { cause_id, cause_title, version, mode, brief_markdown } => {
            let brief_link = links.brief(*cause_id);
            let linear_markdown = match mode {
                BriefInLinear::Off => None,
                BriefInLinear::Full if brief_markdown.chars().count() <= LINEAR_COMMENT_MAX_CHARS => {
                    Some(format!("**Mage fix brief v{version}** · [open in Mage]({brief_link})\n\n{brief_markdown}"))
                }
                BriefInLinear::Summary | BriefInLinear::Full => Some(format!(
                    "**Mage fix brief v{version}** · root cause: **{}**\n\nAcceptance: the repro script passes and the project tests pass.\n\n[Open the brief in Mage]({brief_link})",
                    md_text(cause_title)
                )),
            };
            Rendered {
                linear_markdown,
                chat_text: format!("Fix brief ready for {ident}."),
                chat_link: Some((brief_link, "Fix brief".into())),
                attachment_subtitle: "Fix brief ready".into(),
            }
        }
        NoticeDetail::FixReady { run_id, cause_rank, stats, summary, video_asset } => {
            let run_link = links.run(*run_id);
            let mut md = format!(
                "**Mage fix** · cause #{cause_rank} · fix gate passed\n\n{} files changed, +{} −{}\n\n{}\n\n[Review the patch in Mage]({run_link})",
                stats.files,
                stats.added,
                stats.removed,
                md_text(summary)
            );
            md.push_str(&video_md(video_asset.as_ref(), "fix.mp4"));
            Rendered {
                linear_markdown: Some(md),
                chat_text: format!("Mage has a patch ready for review for {ident}."),
                chat_link: Some((run_link, "Review".into())),
                attachment_subtitle: "Patch ready for review".into(),
            }
        }
        NoticeDetail::FixFailed { run_id, cause_rank, reason, video_asset } => {
            let run_link = links.run(*run_id);
            let mut md = format!("**Mage fix attempt failed** · cause #{cause_rank}\n\nReason: {}\n\n[Details in Mage]({run_link})", md_text(reason));
            md.push_str(&video_md(video_asset.as_ref(), "fix.mp4"));
            Rendered {
                linear_markdown: Some(md),
                chat_text: format!("Mage's fix attempt for {ident} failed."),
                chat_link: Some((run_link, "Details".into())),
                attachment_subtitle: "Fix attempt failed".into(),
            }
        }
        NoticeDetail::PrOpened { url, number, gate_summary } => {
            let label = number.map_or_else(|| "pull request".to_owned(), |n| format!("PR #{n}"));
            let md = format!("**Mage opened a pull request** · [{label}]({url})\n\nHuman review required. {}", md_text(gate_summary));
            Rendered {
                linear_markdown: Some(md),
                chat_text: format!("Mage opened a PR for {ident}."),
                chat_link: Some((url.clone(), label)),
                attachment_subtitle: "PR open".into(),
            }
        }
    }
}

#[cfg(test)]
mod tests;
