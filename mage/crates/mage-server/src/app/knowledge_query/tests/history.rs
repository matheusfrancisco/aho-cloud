//! `changes_between`, `blame`, `similar_tickets`, `run_recipe`.

use std::time::Duration;

use mage_core::domain::{
    Budget, CommitSha, ContentHash, OrgId, Resolution, RunId, RunKind, RunSubject, Ticket, TicketData, TicketId, TicketSource, TicketStatus,
    TicketTarget, TargetOrigin, TreeOid, UserId,
};
use mage_core::ports::knowledge::{CommitRecord, KnowledgeStore, LinkSource, PullRequestRecord, TicketLink};
use mage_core::wire::indexer::{BlameHunk, ChangeKind, CommitInfo, FileChange, FileDiff, Hunk};
use serde_json::{Value, json};
use time::OffsetDateTime;
use time::macros::datetime;
use uuid::Uuid;

use super::{Fx, assert_envelope, cite_ref, path, sha};
use crate::app::knowledge_query::ToolErrorCode;
use mage_core::ports::repos::RepoStore;
use crate::ports::indexer_link::IndexerLinkError;
use crate::ports::runs::{NewRun, RunStore, RunWrite};
use crate::ports::ticket::TicketStore;
use crate::testing::runs::{sample_run, sample_spec};

fn commit(sha: CommitSha, parents: Vec<CommitSha>, subject: &str, at: OffsetDateTime, files: &[&str]) -> CommitInfo {
    CommitInfo {
        sha,
        tree: TreeOid::from_bytes([7; 20]),
        parents,
        author_name: "Ada".into(),
        author_time: at,
        committer_time: at,
        subject: subject.into(),
        body: String::new(),
        files: Some(
            files.iter().map(|p| FileChange { path: path(p), old_path: None, change: ChangeKind::Modified, adds: Some(1), dels: Some(1) }).collect(),
        ),
    }
}

/// `from` = 0x10 (indexed, with the old `svc/a.go`), fix commit 0x21 (PR #7, ENG-12), then the
/// scope sha.
async fn history_fixture(fx: &Fx) -> CommitSha {
    let from = sha(0x10);
    fx.file("svc/a.go", "package svc\n// new\n").await;
    fx.file("svc/b.go", "package svc\n").await;
    fx.other_version(from, &[("svc/a.go", "package svc\n// old\n")]).await;
    let fix = sha(0x21);
    let commits = vec![
        commit(from, vec![], "init", datetime!(2026-01-01 0:00 UTC), &["svc/a.go"]),
        commit(fix, vec![from], "fix: missing conn (#7)", datetime!(2026-01-02 0:00 UTC), &["svc/a.go"]),
        commit(fx.sha, vec![fix], "add b", datetime!(2026-01-03 0:00 UTC), &["svc/b.go"]),
    ];
    fx.indexer.put_commits(fx.org, fx.repo, commits.clone());
    let k = &fx.knowledge;
    k.put_commits(fx.org, fx.repo, &commits.into_iter().map(|info| CommitRecord { info }).collect::<Vec<_>>()).await.unwrap_or_else(|e| panic!("{e}"));
    let pr = PullRequestRecord {
        number: 7,
        title: "Fix missing connection".into(),
        body_head: String::new(),
        head_ref: "eng-12-fix".into(),
        base_ref: "main".into(),
        merge_commit: Some(fix),
        merged_at: Some(datetime!(2026-01-02 0:00 UTC)),
        updated_at: datetime!(2026-01-02 0:00 UTC),
        url: "https://github.com/acme/app/pull/7".into(),
    };
    k.put_pull_requests(fx.org, fx.repo, &[pr]).await.unwrap_or_else(|e| panic!("{e}"));
    let link = TicketLink { identifier: "ENG-12".into(), source: LinkSource::PrBranch, commit: None, pr: Some(7) };
    k.put_ticket_links(fx.org, fx.repo, &[link]).await.unwrap_or_else(|e| panic!("{e}"));
    from
}

#[tokio::test]
async fn changes_between_lists_files_from_manifests_and_enriched_commits() {
    let fx = Fx::new().await;
    let from = history_fixture(&fx).await;
    let got = fx.call("changes_between", json!({ "from": from.to_hex() })).await.unwrap_or_else(|e| panic!("{e}"));
    assert_envelope(&got, &fx, "History");
    assert_eq!(got["files"], json!([{ "path": "svc/a.go", "change": "modified" }, { "path": "svc/b.go", "change": "added" }]));
    let commits = got["commits"].as_array().cloned().unwrap_or_default();
    assert_eq!(commits.len(), 2, "{got}");
    let fix = commits.iter().find(|c| c["sha"] == sha(0x21).to_hex()).cloned().unwrap_or_default();
    assert_eq!(fix["pr"]["number"], 7);
    assert_eq!(fix["tickets"], json!(["ENG-12"]));
    assert!(got.get("hunks").is_none());

    let paged = fx.call("changes_between", json!({ "from": from.to_hex(), "limit": 1, "include": ["files"] })).await.unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(paged["files"], json!([{ "path": "svc/a.go", "change": "modified" }]));
    let rest = fx.call("changes_between", json!({ "from": from.to_hex(), "limit": 1, "include": ["files"], "cursor": paged["next_cursor"] })).await.unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(rest["files"], json!([{ "path": "svc/b.go", "change": "added" }]));

    fx.indexer.fail_with(IndexerLinkError::Unavailable("down".into()));
    let degraded = fx.call("changes_between", json!({ "from": from.to_hex() })).await.unwrap_or_else(|e| panic!("{e}"));
    assert_envelope(&degraded, &fx, "Files");
    assert_eq!(degraded["degraded"]["wanted"], "History");
    assert_eq!(degraded["files"].as_array().map(Vec::len), Some(2), "files still come from the manifests");
}

#[tokio::test]
async fn changes_between_uses_the_indexer_for_unindexed_commits_and_hunks() {
    let fx = Fx::new().await;
    history_fixture(&fx).await;
    let fix = sha(0x21);
    let hunk = Hunk { old_start: 2, old_lines: 1, new_start: 2, new_lines: 1, text: "-// old\n+// new\n".into() };
    let diff = FileDiff {
        change: FileChange { path: path("svc/a.go"), old_path: None, change: ChangeKind::Modified, adds: Some(1), dels: Some(1) },
        hunks: vec![hunk],
        binary: false,
        truncated: false,
    };
    fx.indexer.put_diff(fx.org, fx.repo, fix, fx.sha, vec![diff]);
    let got = fx.call("changes_between", json!({ "from": fix.to_hex(), "include": ["files", "hunks"] })).await.unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(got["files"], json!([{ "path": "svc/a.go", "change": "modified" }]));
    assert_eq!(got["hunks"][0]["hunks"][0]["text"], "-// old\n+// new\n");
    fx.indexer.fail_with(IndexerLinkError::Timeout);
    assert_eq!(fx.error("changes_between", json!({ "from": fix.to_hex() })).await, ToolErrorCode::Unavailable);
}

#[tokio::test]
async fn blame_enriches_hunks_and_needs_the_indexer() {
    let fx = Fx::new().await;
    history_fixture(&fx).await;
    let hunk = BlameHunk {
        start: 1,
        lines: 2,
        commit: sha(0x21),
        author_name: "Ada".into(),
        author_time: datetime!(2026-01-02 0:00 UTC),
        subject: "fix: missing conn (#7)".into(),
        orig_path: path("svc/a.go"),
        orig_start: 1,
    };
    fx.indexer.put_blame(fx.org, fx.key(), path("svc/a.go"), vec![hunk]);
    let got = fx.call("blame", json!({ "path": "svc/a.go", "start_line": 1, "end_line": 2 })).await.unwrap_or_else(|e| panic!("{e}"));
    assert_envelope(&got, &fx, "History");
    let item = &got["items"][0];
    assert_eq!(item["lines"], json!([1, 2]));
    assert_eq!(item["commit"]["author_name"], "Ada");
    assert!(item["commit"].get("email").is_none());
    assert_eq!(item["pr"]["number"], 7);
    assert_eq!(item["tickets"], json!(["ENG-12"]));
    assert_eq!(item["citation"]["ref"], cite_ref(&fx, "svc/a.go", 1, 2));
    assert_eq!(fx.error("blame", json!({ "path": "svc/a.go", "start_line": 1, "end_line": 201 })).await, ToolErrorCode::InvalidArgument);
    fx.indexer.fail_with(IndexerLinkError::Unavailable("down".into()));
    assert_eq!(fx.error("blame", json!({ "path": "svc/a.go", "start_line": 1, "end_line": 2 })).await, ToolErrorCode::Unavailable);
}

fn ticket(org: OrgId, repo: mage_core::domain::RepoId, n: u128, title: &str, at: OffsetDateTime) -> Ticket {
    let id = TicketId::from_uuid(Uuid::from_u128(0x7000 + n));
    Ticket::rehydrate(TicketData {
        id,
        org_id: org,
        source: TicketSource::Linear {
            issue_id: format!("issue-{n}"),
            identifier: format!("ENG-{n}"),
            url: url::Url::parse(&format!("https://linear.app/acme/issue/ENG-{n}")).unwrap_or_else(|e| panic!("{e}")),
        },
        title: title.into(),
        body: "Users see the login page again".into(),
        content_hash: ContentHash::from_bytes([u8::try_from(n).unwrap_or(0); 32]),
        slack_thread: None,
        customer: None,
        target: Some(TicketTarget { repo_id: repo, sha: sha(0xab), origin: TargetOrigin::DefaultHead }),
        error_strings: vec!["session expired".into()],
        status: TicketStatus::Triaged,
        triaged_hash: None,
        triage_run: None,
        created_at: at,
        updated_at: at,
    })
}

async fn resolved(fx: &Fx, n: u128, title: &str, at: OffsetDateTime) -> TicketId {
    let t = ticket(fx.org, fx.repo, n, title, at);
    fx.tickets.save_ingested(fx.org, &t, None, &[]).await.unwrap_or_else(|e| panic!("{e}"));
    let resolution = Resolution {
        ticket_id: t.id,
        org_id: fx.org,
        confirmed_cause: None,
        root_cause: "the session cookie was dropped on redirect".into(),
        fix_pr_url: None,
        repro_script: None,
        resolved_by: UserId::from_uuid(Uuid::from_u128(1)),
        at,
    };
    fx.tickets.resolve(fx.org, resolution, TicketStatus::Triaged, &[]).await.unwrap_or_else(|e| panic!("{e}"));
    t.id
}

#[tokio::test]
async fn similar_tickets_applies_the_memory_cutoff_and_defaults_to_the_run_ticket() {
    let fx = Fx::new().await;
    resolved(&fx, 1, "Login fails after session expired", datetime!(2026-01-10 0:00 UTC)).await;
    let late = resolved(&fx, 2, "Login fails on Safari", datetime!(2026-02-10 0:00 UTC)).await;
    let ids = |v: &Value| -> Vec<String> { v["items"].as_array().into_iter().flatten().filter_map(|i| i["ticket"]["identifier"].as_str().map(str::to_owned)).collect() };
    let all = fx.call("similar_tickets", json!({ "text": "login fails" })).await.unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(all["layer"], "Memory");
    let mut got = ids(&all);
    got.sort();
    assert_eq!(got, vec!["ENG-1", "ENG-2"]);

    let mut scope = fx.scope();
    scope.memory.exclude_tickets = vec![late];
    let excluded = fx.call_as(&scope, "similar_tickets", json!({ "text": "login fails" })).await.unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(ids(&excluded), vec!["ENG-1"]);
    scope.memory = mage_core::wire::token::MemoryCutoff { exclude_tickets: vec![], as_of: Some(datetime!(2026-01-10 0:00 UTC)) };
    let before = fx.call_as(&scope, "similar_tickets", json!({ "text": "login fails" })).await.unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(ids(&before), Vec::<String>::new(), "resolved_at must be before as_of");

    // Default text: the run's ticket.
    let current = ticket(fx.org, fx.repo, 3, "Login fails", datetime!(2026-03-01 0:00 UTC));
    fx.tickets.save_ingested(fx.org, &current, None, &[]).await.unwrap_or_else(|e| panic!("{e}"));
    let run_id = RunId::from_uuid(Uuid::from_u128(0x0c02));
    let budget = Budget { max_wall: Duration::from_secs(600), max_turns: 10, max_cost_micros: 1, max_gate_attempts: 1 };
    let run = sample_run(fx.org, run_id, RunKind::Triage, RunSubject::Ticket(current.id), budget, datetime!(2026-03-01 0:00 UTC));
    let insert = NewRun { run, spec: sample_spec(RunKind::Triage), create_key: None, source_ready: true, affinity: None };
    fx.runs.commit(fx.org, RunWrite { inserts: vec![insert], ..Default::default() }).await.unwrap_or_else(|e| panic!("{e}"));
    let mut scope = fx.scope();
    scope.run = run_id;
    scope.memory.exclude_tickets = vec![current.id];
    let from_run = fx.call_as(&scope, "similar_tickets", json!({})).await.unwrap_or_else(|e| panic!("{e}"));
    let mut got = ids(&from_run);
    got.sort();
    assert_eq!(got, vec!["ENG-1", "ENG-2"]);
    assert_eq!(from_run["items"][0]["root_cause"], "the session cookie was dropped on redirect");
}

#[tokio::test]
async fn run_recipe_reports_none_or_the_latest_recipe() {
    let fx = Fx::new().await;
    let none = fx.call("run_recipe", json!({})).await.unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(none["status"], "none");
    let body = "version = 1\n";
    let recipe = mage_core::domain::Recipe::rehydrate(mage_core::domain::RecipeData {
        org_id: fx.org,
        repo_id: fx.repo,
        hash: mage_core::domain::RecipeHash::from_bytes([1; 32]),
        body: body.into(),
        validated_at_sha: None,
        status: mage_core::domain::RecipeStatus::Draft,
    });
    fx.repos.put_recipe(fx.org, &recipe).await.unwrap_or_else(|e| panic!("{e}"));
    let draft = fx.call("run_recipe", json!({})).await.unwrap_or_else(|e| panic!("{e}"));
    assert_envelope(&draft, &fx, "Recipe");
    assert_eq!(draft["status"], "draft");
    assert_eq!(draft["toml"], body);
}
