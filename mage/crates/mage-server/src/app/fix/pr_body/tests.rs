//! PR body golden files incl. the media fallback (fix.md Testing), title / commit message and
//! body-limit shortening.

use mage_core::domain::{LineRange, RepoId};
use uuid::Uuid;

use super::*;
use crate::app::fix::golden::assert_golden;
use crate::ports::pull_requests::parse_trailers;

fn id(n: u128) -> Uuid {
    Uuid::from_u128(0x0190_0000_0000_7000_8000_0000_0000_0000 | n)
}

fn sha(c: char) -> CommitSha {
    c.to_string().repeat(40).parse().unwrap()
}

struct Fixture {
    ui: Url,
    web: Url,
    ticket_url: Url,
    evidence: Vec<Citation>,
    excluded: Vec<RepoPath>,
    regression: RepoPath,
}

fn fixture() -> Fixture {
    let cite = |path: &str, a, b| Citation {
        repo_id: RepoId::from_uuid(id(1)),
        sha: sha('a'),
        path: RepoPath::new(path).unwrap(),
        lines: LineRange::new(a, b).unwrap(),
        snippet_hash: None,
        note: None,
    };
    Fixture {
        ui: Url::parse("https://mage.example.com").unwrap(),
        web: Url::parse("https://github.com/").unwrap(),
        ticket_url: Url::parse("https://linear.app/acme/issue/ENG-412").unwrap(),
        evidence: vec![cite("src/billing/total.ts", 40, 52), cite("src/billing/discount.ts", 10, 14)],
        excluded: vec![RepoPath::new("package-lock.json").unwrap(), RepoPath::new("dist/app.js").unwrap()],
        regression: RepoPath::new(".mage/regressions/eng-412-discount-row-excluded.spec.ts").unwrap(),
    }
}

fn input<'a>(f: &'a Fixture, media: &'a PrMediaLinks) -> PrBodyInput<'a> {
    PrBodyInput {
        ui_url: &f.ui,
        github_web: &f.web,
        ticket_identifier: "ENG-412",
        ticket_url: &f.ticket_url,
        ticket_title: "Invoice total ignores discount",
        requester: Some("Dana Support"),
        cause_explanation: "`computeTotal` filters line items by kind, dropping the discount row.",
        evidence: &f.evidence,
        owner: "acme",
        name: "shop",
        evidence_sha: sha('a'),
        summary: "Include adjustment rows in the total.\n\nAlso added a unit test. cc @lead",
        stats: FixStats { files: 2, added: 14, removed: 3 },
        excluded: &f.excluded,
        rebased: Some((sha('a'), sha('b'))),
        regression: Some((&f.regression, "npx playwright test .mage/regressions/eng-412-discount-row-excluded.spec.ts".into())),
        parent: sha('b'),
        repetitions: 2,
        signature_value: "Expected: \"$120.00\" Received: \"$100.00\"",
        test_command: Some("npm test"),
        lint: LintRow::Advisory { command: "npm run lint".into(), exit: Some(1) },
        media,
        trace: Some(ArtifactId::from_uuid(id(10))),
        test_log: Some(ArtifactId::from_uuid(id(11))),
        fix_run: RunId::from_uuid(id(20)),
        gate: GateId::from_uuid(id(21)),
        digest: ChangeSetDigest::from_bytes([0x3f; 32]),
    }
}

fn uploaded() -> PrMediaLinks {
    let asset = |n: u32| MediaLink::Uploaded(Url::parse(&format!("https://github.com/user-attachments/assets/0000000{n}-aaaa-4bbb-8ccc-dddddddddddd")).unwrap());
    PrMediaLinks { before_gif: asset(1), after_gif: asset(2), after_video: asset(3) }
}

#[test]
fn golden_body_with_uploaded_media_rebase_and_regression() {
    let f = fixture();
    let media = uploaded();
    let body = render(&input(&f, &media));
    assert!(!body.contains("@lead"), "mentions neutralised");
    assert_golden("pr/uploaded.md", &body);
}

#[test]
fn golden_body_with_media_fallback_links() {
    let f = fixture();
    let media = PrMediaLinks {
        before_gif: MediaLink::Fallback(ArtifactId::from_uuid(id(30))),
        after_gif: MediaLink::Fallback(ArtifactId::from_uuid(id(31))),
        after_video: MediaLink::Fallback(ArtifactId::from_uuid(id(32))),
    };
    let mut i = input(&f, &media);
    i.requester = None;
    i.rebased = None;
    i.regression = None;
    i.parent = sha('a');
    i.excluded = &[];
    i.lint = LintRow::NotConfigured;
    let body = render(&i);
    assert!(!body.contains("user-attachments"), "fallback never links GitHub assets");
    assert!(body.contains(&format!("https://mage.example.com/artifacts/{}", ArtifactId::from_uuid(id(30)))));
    assert_golden("pr/fallback.md", &body);
}

#[test]
fn golden_body_with_missing_media_and_required_lint() {
    let f = fixture();
    let media = PrMediaLinks { before_gif: MediaLink::Missing, after_gif: uploaded().after_gif, after_video: MediaLink::Missing };
    let mut i = input(&f, &media);
    i.lint = LintRow::Required { command: "npm run lint".into() };
    i.repetitions = 1;
    assert_golden("pr/missing_media.md", &render(&i));
}

#[test]
fn oversized_bodies_shorten_lists_first_and_keep_the_footer() {
    let mut f = fixture();
    let template = f.evidence[0].clone();
    f.evidence = (1..=400).map(|i| Citation { lines: LineRange::new(i, i + 1).unwrap(), ..template.clone() }).collect();
    let media = uploaded();
    let i = input(&f, &media);
    let full = render_within(&i, usize::MAX);
    assert!(full.contains("total.ts:400-401"));
    let limit = full.chars().count() - 1000;
    let short = render_within(&i, limit);
    assert!(short.chars().count() <= limit);
    assert!(short.contains("… and 395 more in the Mage UI"), "evidence list shortened first");
    assert!(short.contains("Include adjustment rows"), "prose kept while lists suffice");
    assert!(short.ends_with(&format!("<!-- mage:change-set={} run={} gate={} -->\n", "3f".repeat(32), i.fix_run, i.gate)));
    let tiny = render_within(&i, 900);
    assert!(tiny.chars().count() <= 900);
    assert!(tiny.contains("<!-- mage:change-set="), "footer survives a hard cut");
}

#[test]
fn title_and_commit_message_carry_trailers() {
    let url = Url::parse("https://linear.app/acme/issue/ENG-412").unwrap();
    let (run, gate, digest) = (RunId::from_uuid(id(20)), GateId::from_uuid(id(21)), ChangeSetDigest::from_bytes([0x3f; 32]));
    assert_eq!(title("Discount <b>row</b>\nexcluded @team", "ENG-412"), "fix: Discount row excluded @\u{200B}team (ENG-412)");
    let msg = commit_message("Discount row excluded", "ENG-412", "Include adjustment rows. cc @lead\n\nSecond paragraph.", &url, run, gate, &digest);
    assert!(msg.starts_with("fix: Discount row excluded (ENG-412)\n\nInclude adjustment rows. cc @\u{200B}lead\n\nLinear: "));
    assert!(!msg.contains("Second paragraph"), "first paragraph only");
    assert_eq!(
        parse_trailers(&msg),
        vec![
            ("Linear".to_owned(), url.to_string()),
            ("Mage-Run".to_owned(), run.to_string()),
            ("Mage-Gate".to_owned(), gate.to_string()),
            ("Mage-Change-Set".to_owned(), "3f".repeat(32)),
        ]
    );
}
