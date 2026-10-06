//! Golden files for `fix::brief::render` (fix.md Testing): one per `ReproMethod`, missing
//! suggestion, assumed sha, large script (link only), mention/HTML neutralisation.

use mage_core::domain::{LineRange, RepoId, RepoPath};
use uuid::Uuid;

use super::*;
use crate::app::fix::golden::assert_golden;

const RECIPE: &str = r#"version = 1
ports = ["api", "web"]

[env]
DATABASE_URL = "${db.url}"
STRIPE_SECRET_KEY = "sk_test_mage_fake_000000"

[[build]]
name = "deps"
run = "npm ci"

[[build]]
name = "compile"
run = "npm run build"

[database]
provider = "runner"
migrate = "npm run db:migrate"
seed = "npm run db:seed"

[services.api]
run = "npm run start:api"
ports = ["api"]
[services.api.health]
http = "http://127.0.0.1:${port.api}/healthz"

[services.web]
run = "npm run start:web"
ports = ["web"]
depends_on = ["api"]

[test]
run = "npm test"

[browser]
base_url = "http://127.0.0.1:${port.web}"
[browser.login]
url = "/login"
username = "support@example.test"
password = "mage-fake-password"

[fix]
lint = "npm run lint"
generated = ["src/generated/**"]
"#;

const SCRIPT: &str = "import { test, expect } from '@playwright/test';\n\ntest('invoice total', async ({ page }) => {\n  await page.goto(process.env.MAGE_BASE_URL + '/invoices/42');\n  await expect(page.getByTestId('total')).toHaveText('$120.00');\n});\n";

struct Fixture {
    ui: Url,
    web: Url,
    ticket_url: Url,
    recipe: RecipeSpec,
    evidence: Vec<Citation>,
    sites: Vec<Citation>,
    signature: FailureSignature,
}

fn id(n: u128) -> Uuid {
    Uuid::from_u128(0x0190_0000_0000_7000_8000_0000_0000_0000 | n)
}

fn sha() -> CommitSha {
    "3f9a2c1e8b7d6a5f4e3d2c1b0a9f8e7d6c5b4a39".parse().unwrap()
}

fn cite(path: &str, a: u32, b: u32, note: Option<&str>) -> Citation {
    Citation {
        repo_id: RepoId::from_uuid(id(1)),
        sha: sha(),
        path: RepoPath::new(path).unwrap(),
        lines: LineRange::new(a, b).unwrap(),
        snippet_hash: Some([7; 32]),
        note: note.map(str::to_owned),
    }
}

fn fixture() -> Fixture {
    Fixture {
        ui: Url::parse("https://mage.example.com/").unwrap(),
        web: Url::parse("https://github.com/").unwrap(),
        ticket_url: Url::parse("https://linear.app/acme/issue/ENG-412/invoice-total-wrong").unwrap(),
        recipe: RecipeSpec::parse(RECIPE).unwrap(),
        evidence: vec![
            cite("src/billing/total.ts", 40, 52, Some("sums line items but skips the discount row")),
            cite("src/billing/discount.ts", 10, 14, None),
        ],
        sites: vec![cite("src/billing/total.ts", 44, 47, None)],
        signature: FailureSignature { kind: SignatureKind::AssertionText, value: "Expected: \"$120.00\" Received: \"$100.00\"".into(), matcher: Matcher::Contains },
    }
}

fn input<'a>(f: &'a Fixture, method: ReproMethod) -> BriefInput<'a> {
    BriefInput {
        ui_url: &f.ui,
        github_web: &f.web,
        ticket_identifier: "ENG-412",
        ticket_url: &f.ticket_url,
        ticket_title: "Invoice total ignores discount",
        ticket_body: "Customer Acme reports the invoice total on /invoices/42 shows $100 instead of $120 after applying the loyalty discount.",
        cause_title: "Discount row excluded from invoice total",
        cause_explanation: "`computeTotal` filters line items by `kind === 'item'`, so the discount adjustment row never reaches the sum.",
        method,
        owner: "acme",
        name: "shop",
        sha: sha(),
        assumed: false,
        diagnose_run: RunId::from_uuid(id(2)),
        diagnose_summary: "Reproduced on a fresh fork: the total renders $100.00.",
        suggested_fix: Some("Include `kind === 'adjustment'` rows in the reduce, or sum before filtering."),
        evidence: &f.evidence,
        fix_sites: &f.sites,
        video: Some(ArtifactId::from_uuid(id(3))),
        trace: Some(ArtifactId::from_uuid(id(4))),
        signature: &f.signature,
        script: BriefScript {
            artifact: ArtifactId::from_uuid(id(5)),
            name: "repro.spec.ts",
            kind: ScriptKind::Playwright,
            path: ".mage/repro/cse_01/repro.spec.ts",
            body: Some(SCRIPT),
            bytes: SCRIPT.len() as u64,
        },
        recipe: &f.recipe,
        allow_workflow_edits: false,
        lockfiles: LockfilePolicy::WithManifest,
    }
}

#[test]
fn golden_ui_method() {
    let f = fixture();
    assert_golden("brief/ui.md", &render(&input(&f, ReproMethod::Ui)));
}

#[test]
fn golden_api_method_shell_script() {
    let f = fixture();
    let mut i = input(&f, ReproMethod::Api);
    let script = "#!/usr/bin/env bash\nset -euo pipefail\ncurl -sf \"$MAGE_BASE_URL/api/invoices/42\" | jq -e '.total == 120'\n";
    i.script = BriefScript { name: "repro.sh", kind: ScriptKind::Shell, path: ".mage/repro/cse_01/repro.sh", body: Some(script), bytes: script.len() as u64, ..i.script };
    let sig = FailureSignature { kind: SignatureKind::ExitCode, value: "1".into(), matcher: Matcher::Exact };
    i.signature = &sig;
    i.video = None;
    i.trace = None;
    assert_golden("brief/api.md", &render(&i));
}

#[test]
fn golden_job_method_without_suggestion_or_sites() {
    let f = fixture();
    let mut i = input(&f, ReproMethod::Job);
    i.suggested_fix = None;
    i.fix_sites = &[];
    i.lockfiles = LockfilePolicy::Exclude;
    i.allow_workflow_edits = true;
    assert_golden("brief/job_no_suggestion.md", &render(&i));
}

#[test]
fn golden_unknown_method_assumed_sha() {
    let f = fixture();
    let mut i = input(&f, ReproMethod::Unknown);
    i.assumed = true;
    assert_golden("brief/unknown_assumed.md", &render(&i));
}

#[test]
fn golden_large_script_is_linked_not_inlined() {
    let f = fixture();
    let mut i = input(&f, ReproMethod::Ui);
    i.script.body = None;
    i.script.bytes = 70_000;
    let md = render(&i);
    assert!(!md.contains("```ts"), "large scripts are not inlined");
    assert!(md.contains(&format!("https://mage.example.com/artifacts/{}", i.script.artifact)));
    assert_golden("brief/large_script.md", &md);
}

#[test]
fn golden_untrusted_text_is_neutralised() {
    let f = fixture();
    let mut i = input(&f, ReproMethod::Ui);
    i.ticket_body = "@alice @bob please look <img src=x onerror=alert(1)> <script>steal()</script> asap";
    i.diagnose_summary = "Pinging @security-team: see <b>this</b>.\n\nSecond paragraph.";
    i.suggested_fix = Some("Ask @carol; wrap in <details> then fix ```` fences");
    i.cause_explanation = "Explanation mentioning @dave and <a href='javascript:x'>a link</a>.";
    let md = render(&i);
    for raw in ["@alice", "@bob", "@security-team", "@carol", "@dave"] {
        assert!(!md.contains(raw), "{raw} must be neutralised");
        assert!(md.contains(&raw.replacen('@', "@\u{200B}", 1)));
    }
    for tag in ["<img", "<script", "<b>", "<details>", "<a href"] {
        assert!(!md.contains(tag), "{tag} must be stripped");
    }
    assert_golden("brief/untrusted.md", &md);
}

#[test]
fn render_is_deterministic() {
    let f = fixture();
    assert_eq!(render(&input(&f, ReproMethod::Ui)), render(&input(&f, ReproMethod::Ui)));
}
