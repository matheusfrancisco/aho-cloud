//! 3.1 Normalization and 3.7 content hash (intake.md): the ticket body triage reads, rebuilt
//! deterministically from the Linear issue and the stored Slack thread snapshot.

use mage_core::domain::{ContentHash, CustomerId, TicketTarget};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;

/// Body cap (bytes).
pub const BODY_MAX_BYTES: usize = 64 * 1024;

/// A kept (human, not Mage's) Linear comment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BodyComment<'a> {
    pub author_id: &'a str,
    pub at: OffsetDateTime,
    pub body: &'a str,
}

/// A kept (non-bot) Slack thread message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BodyMessage<'a> {
    pub author_id: &'a str,
    pub at: OffsetDateTime,
    pub text: &'a str,
    pub file_names: &'a [String],
}

/// CRLF → LF, trailing whitespace stripped per line, runs of more than 2 blank lines collapsed to
/// 2, leading/trailing blank lines removed.
pub fn clean_text(text: &str) -> String {
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut out = String::with_capacity(text.len());
    let mut blanks = 0usize;
    for line in text.split('\n') {
        let line = line.trim_end();
        if line.is_empty() {
            blanks += 1;
            continue;
        }
        if !out.is_empty() {
            out.push('\n');
            for _ in 0..blanks.min(2) {
                out.push('\n');
            }
        }
        blanks = 0;
        out.push_str(line);
    }
    out
}

/// `2026-10-01T12:00Z` (UTC, minute precision).
pub fn stamp(at: OffsetDateTime) -> String {
    let at = at.to_offset(time::UtcOffset::UTC);
    format!("{:04}-{:02}-{:02}T{:02}:{:02}Z", at.year(), u8::from(at.month()), at.day(), at.hour(), at.minute())
}

enum Entry<'a> {
    Comment(&'a BodyComment<'a>),
    Message(&'a BodyMessage<'a>),
}

impl Entry<'_> {
    fn at(&self) -> OffsetDateTime {
        match self {
            Self::Comment(c) => c.at,
            Self::Message(m) => m.at,
        }
    }
}

fn render_comment(c: &BodyComment<'_>) -> String {
    format!("**{} · {}**\n{}", c.author_id, stamp(c.at), clean_text(c.body))
}

fn render_message(m: &BodyMessage<'_>) -> String {
    let mut s = format!("**{} · {}**\n{}", m.author_id, stamp(m.at), clean_text(m.text));
    if !m.file_names.is_empty() {
        if !s.ends_with('\n') {
            s.push(' ');
        }
        s.push_str(&format!("[files: {}]", m.file_names.join(", ")));
    }
    s
}

fn assemble(description: &str, comments: &[String], messages: &[String], truncated: usize) -> String {
    let mut out = String::from("## Description\n");
    out.push_str(description);
    if truncated > 0 {
        out.push_str(&format!("\n\n[truncated {truncated} messages]"));
    }
    if !comments.is_empty() {
        out.push_str("\n\n## Comments\n");
        out.push_str(&comments.join("\n\n"));
    }
    if !messages.is_empty() {
        out.push_str("\n\n## Slack thread\n");
        out.push_str(&messages.join("\n\n"));
    }
    out
}

/// The fixed body layout. Comments and messages are rendered oldest first; over
/// [`BODY_MAX_BYTES`] the oldest comments and thread messages are dropped first (marked
/// `[truncated N messages]`), the description is truncated last.
pub fn build_body(description: &str, comments: &[BodyComment<'_>], messages: &[BodyMessage<'_>]) -> String {
    let description = clean_text(description);
    let mut comments: Vec<&BodyComment<'_>> = comments.iter().collect();
    comments.sort_by_key(|c| c.at);
    let mut messages: Vec<&BodyMessage<'_>> = messages.iter().collect();
    messages.sort_by_key(|m| m.at);

    // Every entry, oldest first, so the oldest go first when the body is over the cap.
    let mut entries: Vec<Entry<'_>> = comments.iter().map(|c| Entry::Comment(c)).chain(messages.iter().map(|m| Entry::Message(m))).collect();
    entries.sort_by_key(Entry::at);
    let mut drop = 0usize;
    loop {
        let kept = &entries[drop..];
        let mut rc: Vec<(OffsetDateTime, String)> = Vec::new();
        let mut rm: Vec<(OffsetDateTime, String)> = Vec::new();
        for e in kept {
            match e {
                Entry::Comment(c) => rc.push((c.at, render_comment(c))),
                Entry::Message(m) => rm.push((m.at, render_message(m))),
            }
        }
        let rc: Vec<String> = rc.into_iter().map(|(_, s)| s).collect();
        let rm: Vec<String> = rm.into_iter().map(|(_, s)| s).collect();
        let body = assemble(&description, &rc, &rm, drop);
        if body.len() <= BODY_MAX_BYTES {
            return body;
        }
        if drop < entries.len() {
            drop += 1;
            continue;
        }
        // Only the description is left: cut it on a char boundary to fit.
        let overhead = body.len() - description.len();
        let budget = BODY_MAX_BYTES.saturating_sub(overhead);
        let mut cut = budget.min(description.len());
        while !description.is_char_boundary(cut) {
            cut -= 1;
        }
        return assemble(&description[..cut], &[], &[], drop);
    }
}

fn put(hasher: &mut Sha256, bytes: &[u8]) {
    let len = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
    hasher.update(len.to_be_bytes());
    hasher.update(bytes);
}

/// 3.7: SHA-256 over a length-prefixed canonical encoding (`u32` big-endian length, then bytes)
/// of `"mage.ticket.v1"`, title, body, error strings (sorted), repo id, sha, customer id (each
/// empty when absent). Status, labels, assignee, priority and timestamps are excluded on purpose.
pub fn content_hash(title: &str, body: &str, error_strings: &[String], target: Option<&TicketTarget>, customer: Option<CustomerId>) -> ContentHash {
    let mut hasher = Sha256::new();
    put(&mut hasher, b"mage.ticket.v1");
    put(&mut hasher, title.as_bytes());
    put(&mut hasher, body.as_bytes());
    let mut sorted: Vec<&String> = error_strings.iter().collect();
    sorted.sort();
    put(&mut hasher, &u32::try_from(sorted.len()).unwrap_or(u32::MAX).to_be_bytes());
    for s in sorted {
        put(&mut hasher, s.as_bytes());
    }
    match target {
        Some(t) => {
            put(&mut hasher, t.repo_id.as_uuid().as_bytes());
            put(&mut hasher, t.sha.as_bytes());
        }
        None => {
            put(&mut hasher, &[]);
            put(&mut hasher, &[]);
        }
    }
    match customer {
        Some(c) => put(&mut hasher, c.as_uuid().as_bytes()),
        None => put(&mut hasher, &[]),
    }
    ContentHash::from_bytes(hasher.finalize().into())
}

#[cfg(test)]
mod tests {
    use mage_core::domain::{CommitSha, RepoId, TargetOrigin};
    use time::macros::datetime;
    use uuid::Uuid;

    use super::*;

    #[test]
    fn clean_text_rules() {
        assert_eq!(clean_text("a  \r\nb\t\r\n\r\n\r\n\r\n\r\nc\n\n"), "a\nb\n\n\nc");
        assert_eq!(clean_text("\n\n  x\n"), "  x");
        assert_eq!(clean_text(""), "");
    }

    #[test]
    fn body_layout_is_fixed() {
        let files = vec!["trace.har".to_string()];
        let body = build_body(
            "Export fails  \r\n",
            &[BodyComment { author_id: "u1", at: datetime!(2026-10-01 12:00:31 UTC), body: "Also on Firefox" }],
            &[BodyMessage { author_id: "U9", at: datetime!(2026-10-01 11:58 UTC), text: "see attached", file_names: &files }],
        );
        assert_eq!(
            body,
            "## Description\nExport fails\n\n## Comments\n**u1 · 2026-10-01T12:00Z**\nAlso on Firefox\n\n## Slack thread\n**U9 · 2026-10-01T11:58Z**\nsee attached [files: trace.har]"
        );
        assert_eq!(build_body("only", &[], &[]), "## Description\nonly");
    }

    #[test]
    fn over_the_cap_drops_oldest_messages_then_cuts_description() {
        let big = "x".repeat(20 * 1024);
        let comments: Vec<BodyComment<'_>> = (0..5)
            .map(|i| BodyComment { author_id: "u", at: datetime!(2026-10-01 00:00 UTC) + time::Duration::minutes(i), body: &big })
            .collect();
        let body = build_body("desc", &comments, &[]);
        assert!(body.len() <= BODY_MAX_BYTES);
        // 5 × 20 KiB is over 64 KiB; the three newest (≈ 60 KiB) fit, the two oldest are dropped.
        assert!(body.contains("[truncated 2 messages]"), "{}", &body[..80]);
        assert!(["00:02Z", "00:03Z", "00:04Z"].iter().all(|m| body.contains(&format!("2026-10-01T{m}"))));
        assert!(!body.contains("2026-10-01T00:01Z") && !body.contains("2026-10-01T00:00Z"));

        let huge = "é".repeat(40 * 1024);
        let body = build_body(&huge, &comments, &[]);
        assert!(body.len() <= BODY_MAX_BYTES);
        assert!(body.contains("[truncated 5 messages]") && !body.contains("## Comments"));
    }

    fn target(sha: u8) -> TicketTarget {
        TicketTarget { repo_id: RepoId::from_uuid(Uuid::from_u128(1)), sha: CommitSha::from_bytes([sha; 20]), origin: TargetOrigin::DefaultHead }
    }

    #[test]
    fn content_hash_includes_and_excludes_the_right_fields() {
        let errs = vec!["b".to_string(), "a".to_string()];
        let base = content_hash("T", "B", &errs, Some(&target(1)), None);
        // Stable and order-insensitive in error strings.
        assert_eq!(base, content_hash("T", "B", &["a".to_string(), "b".to_string()], Some(&target(1)), None));
        // Origin (assumed vs deployment) is not part of the hash; only repo and sha are.
        let mut deployed = target(1);
        deployed.origin = TargetOrigin::Deployment { environment: "production".into() };
        assert_eq!(base, content_hash("T", "B", &errs, Some(&deployed), None));
        // Each included field changes it.
        assert_ne!(base, content_hash("T2", "B", &errs, Some(&target(1)), None));
        assert_ne!(base, content_hash("T", "B2", &errs, Some(&target(1)), None));
        assert_ne!(base, content_hash("T", "B", &["a".to_string()], Some(&target(1)), None));
        assert_ne!(base, content_hash("T", "B", &errs, Some(&target(2)), None));
        assert_ne!(base, content_hash("T", "B", &errs, None, None));
        assert_ne!(base, content_hash("T", "B", &errs, Some(&target(1)), Some(CustomerId::from_uuid(Uuid::from_u128(3)))));
        // Length prefixes keep field boundaries: moving bytes between title and body changes it.
        assert_ne!(content_hash("ab", "c", &[], None, None), content_hash("a", "bc", &[], None, None));
    }
}
