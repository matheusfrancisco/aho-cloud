use std::collections::{BTreeMap, BTreeSet};
use std::str::FromStr;
use std::time::Duration;

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use time::macros::datetime;

use super::*;
use crate::domain::{AbortReason, BudgetLimit, GateFeedback, GateOutcome, Matcher, SignatureKind, UserId};

/// Parses every sample into `T`, serializes it back and requires the identical JSON: field names,
/// tags, and encodings stay compatible across a runner and a server one version apart.
fn round_trip<T: Serialize + DeserializeOwned>(name: &str, samples: &Value) {
    let Value::Array(items) = samples else { panic!("{name}: samples must be an array") };
    assert!(!items.is_empty(), "{name}: no samples");
    for (i, sample) in items.iter().enumerate() {
        let parsed: T = serde_json::from_value(sample.clone()).unwrap_or_else(|e| panic!("{name}[{i}]: {e}"));
        let back = serde_json::to_value(&parsed).unwrap_or_else(|e| panic!("{name}[{i}]: {e}"));
        assert_eq!(&back, sample, "{name}[{i}] did not round trip");
    }
}

macro_rules! checks {
    ($($t:ident),* $(,)?) => {
        vec![$((stringify!($t), round_trip::<$t> as fn(&str, &Value))),*]
    };
}

#[test]
fn every_runner_dto_round_trips_its_checked_in_samples() {
    let samples: BTreeMap<String, Value> =
        serde_json::from_str(include_str!("../testdata/runner_samples.json")).unwrap_or_else(|e| panic!("runner_samples.json: {e}"));
    let checks = checks![
        Arch,
        Os,
        ExecutorKind,
        CredentialModeKind,
        HarnessAvailability,
        Capabilities,
        SlotClass,
        SlotCounts,
        PreparedBaseKey,
        RegisterGrant,
        Registration,
        HeldRun,
        LeaseTerms,
        RegistrationAck,
        ReleasedRun,
        LeaseRequest,
        LeaseResponse,
        RunAssignment,
        RepoRef,
        RecipeRef,
        KnowledgeAccess,
        GatewayAccess,
        HarnessAssignment,
        FixSpec,
        DownloadRef,
        Download,
        RunnerPhase,
        HeartbeatRequest,
        HeartbeatAck,
        OutboundEvent,
        OutboundEventKind,
        EventsRequest,
        EventsAck,
        CommandsQuery,
        CommandEnvelope,
        CommandBatch,
        ArtifactSpec,
        UploadRequest,
        UploadGrant,
        CompleteUploadRequest,
        ArtifactState,
        CompleteUploadAck,
        RefreshDownloadsRequest,
        RefreshDownloadsResponse,
        WireRunResult,
        TriageSubmission,
        CauseDraftDto,
        CitationDraftDto,
        SubmitResultRequest,
        SubmitAck,
        GateTicket,
        Rejection,
        RejectionItem,
        FinishReport,
        FinishOutcome,
        FinishRequest,
        FinishAck,
        WireErrorCode,
        WireError,
    ];
    let names: BTreeSet<&str> = checks.iter().map(|(name, _)| *name).collect();
    let keys: BTreeSet<&str> = samples.keys().map(String::as_str).collect();
    assert_eq!(names, keys, "every DTO has samples, and every sample names a DTO");
    for (name, check) in checks {
        check(name, &samples[name]);
    }
}

fn id<T: FromStr>(s: &str) -> T
where
    T::Err: std::fmt::Debug,
{
    s.parse().unwrap_or_else(|e| panic!("{s}: {e:?}"))
}

fn golden(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or_else(|e| panic!("golden: {e}"))
}

#[test]
fn run_assignment_matches_golden_json() {
    let repo_id: RepoId = id("repo_01j9z3k4m5n6p7q8r9s0t1v2z6");
    let sha: CommitSha = id("3f2a1c9e5b7d4f6a8c0e2b4d6f8a0c2e4b6d8f0a");
    let built = RunAssignment {
        run_id: id("run_01j9z3k4m5n6p7q8r9s0t1v2x4"),
        org_id: id("org_01j9z3k4m5n6p7q8r9s0t1v2w3"),
        attempt: 2,
        kind: RunKind::Diagnose,
        lease: LeaseTerms { ttl: Duration::from_secs(60), heartbeat_every: Duration::from_secs(15) },
        repo: Some(RepoRef { repo_id, owner: "hoophq".into(), name: "billing".into() }),
        sha: Some(sha),
        source: Some(Download {
            what: DownloadRef::SourceBundle { repo_id, sha },
            name: None,
            url: "https://storage.googleapis.com/mage-artifacts/bundles/0a1b.tar.zst?X-Goog-Signature=00ff".into(),
            bytes: 1_048_576,
            sha256: *id::<crate::domain::RecipeHash>("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855").as_bytes(),
            expires_at: datetime!(2026-10-05 12:30:00 UTC),
        }),
        recipe: Some(RecipeRef {
            hash: id("9c1e5a7b3d2f4e6a8b0c1d3e5f7a9b2c4d6e8f0a1b3c5d7e9f2a4b6c8d0e1f3a"),
            toml: "version = 1\n\n[services.api]\ncommand = \"npm start\"\n".into(),
        }),
        harness: Some(HarnessAssignment {
            kind: HarnessKind::ClaudeCode,
            model: "claude-sonnet-4-5".into(),
            credential_mode: CredentialModeKind::Gateway,
            prompt: "Reproduce cause 1 \"Null customer on invoice export\".".into(),
            system_append: "You are Mage's diagnostic agent.".into(),
        }),
        verify: None,
        downloads: vec![],
        knowledge: Some(KnowledgeAccess {
            url: "https://mage.example.com/mcp/knowledge".into(),
            knowledge_token: "mgk_eyJhbGciOiJFZERTQSJ9.e30.c2ln".into(),
        }),
        gateway: Some(GatewayAccess {
            base_url: "https://mage.example.com/llm/anthropic".into(),
            gateway_token: "mgg_eyJhbGciOiJFZERTQSJ9.e30.c2ln".into(),
        }),
        budget: Budget { max_wall: Duration::from_secs(45 * 60), max_turns: 150, max_cost_micros: 15_000_000, max_gate_attempts: 2 },
        prior_feedback: vec!["Repro gate attempt 1 failed: the script did not fail the same way in a clean fork.".into()],
        fix: None,
        trace: Some("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01".into()),
    };
    let want = golden(include_str!("../testdata/run_assignment.json"));
    assert_eq!(serde_json::to_value(&built).unwrap_or_else(|e| panic!("{e}")), want);
    let parsed: RunAssignment = serde_json::from_value(want).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(parsed, built);
}

#[test]
fn command_batch_matches_golden_json() {
    let built = CommandBatch {
        commands: vec![
            CommandEnvelope {
                seq: 4,
                attempt: 2,
                issued_at: datetime!(2026-10-05 12:01:00 UTC),
                command: RunCommand::Steer {
                    text: "Check the export path first.".into(),
                    by: id::<UserId>("usr_01j9z3k4m5n6p7q8r9s0t1v3e1"),
                },
            },
            CommandEnvelope {
                seq: 5,
                attempt: 2,
                issued_at: datetime!(2026-10-05 12:07:30 UTC),
                command: RunCommand::GateFeedback(GateFeedback {
                    gate_id: id("gat_01j9z3k4m5n6p7q8r9s0t1v3d0"),
                    kind: GateKind::Repro,
                    attempt: 1,
                    outcome: GateOutcome::Failed {
                        reason: "signature mismatch".into(),
                        details: "expected error_string contains \"Cannot read properties of null\"".into(),
                        attempts_left: 1,
                    },
                }),
            },
            CommandEnvelope {
                seq: 6,
                attempt: 2,
                issued_at: datetime!(2026-10-05 12:40:00 UTC),
                command: RunCommand::Abort { reason: AbortReason::Budget { limit: BudgetLimit::Wall } },
            },
        ],
        status: RunStatus::TimedOut,
    };
    let want = golden(include_str!("../testdata/command_batch.json"));
    assert_eq!(serde_json::to_value(&built).unwrap_or_else(|e| panic!("{e}")), want);
    let parsed: CommandBatch = serde_json::from_value(want).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(parsed, built);
}

#[test]
fn triage_submission_matches_golden_json() {
    let built = TriageSubmission {
        summary: "Invoice export crashes for customers without a billing address.".into(),
        causes: vec![CauseDraftDto {
            title: "Null customer on invoice export".into(),
            explanation: "exportInvoice dereferences customer.billing without a null check.".into(),
            evidence: vec![
                CitationDraftDto {
                    path: "src/billing/export.ts".into(),
                    start_line: 120,
                    end_line: 140,
                    quote: Some("const addr = customer.billing.address;".into()),
                    note: Some("dereferences billing without a null check".into()),
                },
                CitationDraftDto { path: "src/billing/totals.ts".into(), start_line: 88, end_line: 96, quote: None, note: None },
            ],
            repro_steps: vec!["Create a customer without a billing address".into(), "Export one of their invoices".into()],
            confirm_if: "The export fails with \"Cannot read properties of null\".".into(),
            rule_out_if: "Customers without a billing address export fine.".into(),
            confidence: 0.7,
            method: ReproMethod::Ui,
        }],
        missing_info: vec!["Which customer account was affected?".into()],
    };
    let want = golden(include_str!("../testdata/triage_submission.json"));
    assert_eq!(serde_json::to_value(&built).unwrap_or_else(|e| panic!("{e}")), want);
    let parsed: TriageSubmission = serde_json::from_value(want.clone()).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(parsed, built);

    // The same document is what `result` carries, tagged with the run kind.
    let mut tagged = want;
    if let Some(o) = tagged.as_object_mut() {
        o.insert("kind".into(), json!("triage"));
    }
    let result: WireRunResult = serde_json::from_value(tagged).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(result, WireRunResult::Triage(built));
    assert_eq!(result.kind(), RunKind::Triage);
}

#[test]
fn triage_submission_rejects_unknown_fields_even_inside_the_result_tag() {
    let base = golden(include_str!("../testdata/triage_submission.json"));

    let mut top = base.clone();
    if let Some(o) = top.as_object_mut() {
        o.insert("score".into(), json!(1));
    }
    assert!(serde_json::from_value::<TriageSubmission>(top.clone()).is_err());
    if let Some(o) = top.as_object_mut() {
        o.insert("kind".into(), json!("triage"));
    }
    assert!(serde_json::from_value::<WireRunResult>(top).is_err());

    let mut citation = base;
    citation["causes"][0]["evidence"][0]["repo_id"] = json!("repo_01j9z3k4m5n6p7q8r9s0t1v2z6");
    assert!(serde_json::from_value::<TriageSubmission>(citation).is_err(), "the agent never names the repo");
}

#[test]
fn result_kind_must_match_its_payload() {
    // A Learn proposal tagged as a fix result does not parse as a `FixResult`.
    let mismatched = json!({ "kind": "fix", "toml": "version = 1\n", "notes": "" });
    assert!(serde_json::from_value::<WireRunResult>(mismatched).is_err());
}

#[test]
fn outbound_events_have_the_json_shape_of_their_run_event_kind() {
    let usage = Usage { input_tokens: 1, output_tokens: 2, cost_micros: 3, turns: 1, wall: Duration::ZERO };
    let kinds = [
        OutboundEventKind::AssistantText("hi".into()),
        OutboundEventKind::ToolCall { name: "exec".into(), args_preview: "ls".into() },
        OutboundEventKind::ToolResult { name: "exec".into(), ok: false, preview: "exit 2".into() },
        OutboundEventKind::Usage(usage),
        OutboundEventKind::Notice("n".into()),
        OutboundEventKind::Error("e".into()),
    ];
    for kind in kinds {
        let outbound = serde_json::to_value(&kind).unwrap_or_else(|e| panic!("{e}"));
        let event = serde_json::to_value(RunEventKind::from(kind)).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(outbound, event);
    }
    // Server-only kinds are not accepted from a runner.
    let status = serde_json::to_value(RunEventKind::Status(RunStatus::Succeeded)).unwrap_or_else(|e| panic!("{e}"));
    assert!(serde_json::from_value::<OutboundEventKind>(status).is_err());
}

#[test]
fn rejection_travels_inside_a_rejected_wire_error() {
    let rejection = Rejection {
        gate: Some(GateTicket { gate_id: id("gat_01j9z3k4m5n6p7q8r9s0t1v3d0"), kind: GateKind::Evidence, attempt: 1 }),
        message: "REJECTED (attempt 1 of 2).".into(),
        items: vec![RejectionItem { path: "causes[0].evidence[1]".into(), reason: "path not found".into() }],
    };
    let error = WireError::rejected(rejection.clone());
    assert_eq!(error.code, WireErrorCode::Rejected);
    assert_eq!(error.code.http_status(), 422);
    let over_the_wire: WireError =
        serde_json::from_value(serde_json::to_value(&error).unwrap_or_else(|e| panic!("{e}"))).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(over_the_wire.rejection(), Some(rejection));

    let lost = WireError::new(WireErrorCode::LeaseLost, "fence mismatch");
    assert_eq!(lost.rejection(), None);
    assert_eq!(lost.code.http_status(), 409);
    assert!(!lost.code.retryable());
}

#[test]
fn wire_error_without_gate_field_still_parses() {
    // Bodies from a server that predates `gate` (the field is additive).
    let body = json!({ "code": "unavailable", "message": "db down", "status": null, "details": [], "retry_after_ms": 2000 });
    let error: WireError = serde_json::from_value(body).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(error.gate, None);
    assert!(error.code.retryable());
}

#[test]
fn runner_phases_are_ordered_like_the_run_lifecycle() {
    let phases = [RunnerPhase::Preparing, RunnerPhase::Running, RunnerPhase::AwaitingGate, RunnerPhase::Finishing];
    assert!(phases.windows(2).all(|w| w[0] < w[1]));
}

#[test]
fn artifact_names_cannot_escape_their_prefix() {
    assert!(ArtifactSpec::name_valid("repetition-1.mp4"));
    assert!(ArtifactSpec::name_valid(&"a".repeat(128)));
    assert!(!ArtifactSpec::name_valid(&"a".repeat(129)));
    assert!(!ArtifactSpec::name_valid(""));
    for bad in [".", "..", "../x", "a/b", "a b", "a\\b", "ä", "..patch", "Video.mp4", "-x"] {
        assert!(!ArtifactSpec::name_valid(bad), "{bad:?}");
    }
    assert!(ArtifactSpec::name_valid("explore-3.video.webm"), "dots inside a name are fine");
}

#[test]
fn only_runner_artifact_kinds_are_uploadable() {
    for runner_kind in [ArtifactKind::ScipIndex, ArtifactKind::Gif] {
        assert!(ArtifactSpec::kind_allowed(runner_kind), "{runner_kind:?}");
    }
    for server_only in [ArtifactKind::SourceBundle, ArtifactKind::FixBrief] {
        assert!(!ArtifactSpec::kind_allowed(server_only), "{server_only:?}");
    }
}

#[test]
fn lease_matching_needs_kind_and_credential_mode() {
    let caps = Capabilities {
        arch: Arch::Aarch64,
        os: Os::MacOs,
        executor: ExecutorKind::Host,
        harnesses: vec![HarnessAvailability {
            kind: HarnessKind::Omp,
            version: "18.3.1".into(),
            credential_modes: vec![CredentialModeKind::LocalLogin],
        }],
        browser: false,
        slots: SlotCounts { light: 1, work: 1, verify: 1 },
    };
    assert!(caps.offers_harness(HarnessKind::Omp, CredentialModeKind::LocalLogin));
    assert!(!caps.offers_harness(HarnessKind::Omp, CredentialModeKind::Gateway));
    assert!(!caps.offers_harness(HarnessKind::ClaudeCode, CredentialModeKind::LocalLogin));
}

#[test]
fn fix_spec_signature_is_a_domain_signature() {
    let spec: FixSpec = serde_json::from_value(json!({
        "brief": "art_01j9z3k4m5n6p7q8r9s0t1v3c9",
        "script": "art_01j9z3k4m5n6p7q8r9s0t1v3a7",
        "script_sha256": "2c26b46b68ffc68ff99b453c1d30413413422d706483bfa0f98a5e886266e7ae",
        "signature": { "kind": "http_status", "value": "500", "matcher": "exact" },
        "policy": { "allow_workflow_edits": false, "lockfiles": "include" },
        "regression": { "type": "off" }
    }))
    .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(spec.signature, FailureSignature { kind: SignatureKind::HttpStatus, value: "500".into(), matcher: Matcher::Exact });
    // Policy defaults apply when the server omits them.
    assert!(spec.policy.generated.is_empty());
    assert_eq!(spec.policy.limits, crate::domain::fix::PatchLimits::default());
    // Hex must be exactly 32 bytes.
    let short = json!({
        "brief": "art_01j9z3k4m5n6p7q8r9s0t1v3c9", "script": "art_01j9z3k4m5n6p7q8r9s0t1v3a7", "script_sha256": "2c26",
        "signature": { "kind": "exit_code", "value": "1", "matcher": "exact" },
        "policy": { "allow_workflow_edits": false, "lockfiles": "include" }, "regression": { "type": "off" }
    });
    assert!(serde_json::from_value::<FixSpec>(short).is_err());
}
