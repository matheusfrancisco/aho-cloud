use serde_json::json;

use super::*;
use crate::domain::IntegrationKind;

#[test]
fn error_body_shape() {
    let body = ApiErrorBody { error: ApiError { code: ApiErrorCode::NotFound, message: "not found".into(), request_id: "req_1".into() } };
    assert_eq!(
        serde_json::to_value(&body).unwrap_or_default(),
        json!({ "error": { "code": "not_found", "message": "not found", "request_id": "req_1" } })
    );
    assert_eq!(ApiErrorCode::Invalid.http_status(), 422, "Invalid maps to 422 on /api (server.md error formats)");
}

#[test]
fn org_settings_wire_format() {
    let budget = json!({ "max_wall": 600000, "max_turns": 40, "max_cost_micros": 3000000, "max_gate_attempts": 3 });
    let stored = json!({
        "general": { "harness": "omp", "budgets": { "learn": budget, "diagnose": budget, "fix": budget } },
        "triage": {
            "auto_triage": true, "harness": "claude_code", "model": null, "budget": budget, "max_causes": 5,
            "layer_wait_ms": 120000, "auto_retriage_per_day": 3, "ticket_cost_cap_micros": 10000000
        },
        "fix": {
            "brief": true, "brief_in_linear": "summary", "run": false, "pull_requests": false, "open_pr": "manual",
            "pr": { "branch_from": "default_head", "draft": true, "labels": [], "reviewers": { "users": [], "teams": [] } },
            "lockfiles": "with_manifest", "allow_workflow_edits": false,
            "limits": { "max_patch_bytes": 1048576, "max_files": 50, "max_changed_lines": 5000, "max_file_bytes": 262144 },
            "repetitions": 2, "regression": "mage_dir"
        }
    });
    let parsed: OrgSettings = serde_json::from_value(stored.clone()).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(parsed.triage.layer_wait, std::time::Duration::from_secs(120));
    assert_eq!(serde_json::to_value(&parsed).unwrap_or_default(), stored);
}

#[test]
fn requests_reject_unknown_fields_and_default_optional_ones() {
    let minimal: ResolveTicketRequest =
        serde_json::from_value(json!({ "root_cause": "null billing address" })).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(minimal.confirmed_cause, None);
    assert!(
        serde_json::from_value::<ResolveTicketRequest>(json!({ "root_cause": "x", "resolved_by": "usr_x" })).is_err(),
        "server sets the resolver"
    );
    assert_eq!(serde_json::from_value::<StartHarnessRunRequest>(json!({})).ok(), Some(StartHarnessRunRequest::default()));
    assert!(serde_json::from_value::<StartHarnessRunRequest>(json!({ "harness": "gpt" })).is_err());
    let query: TicketListQuery = serde_json::from_value(json!({ "status": "triaged" })).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(query.status, Some(crate::domain::TicketStatus::Triaged));
    assert_eq!(query.cursor, None);
}

#[test]
fn model_key_never_shows_in_debug() {
    let request = SetModelKeyRequest { api_key: "sk-ant-api03-secret".into() };
    assert!(!format!("{request:?}").contains("secret"));
    assert_eq!(serde_json::to_value(&request).unwrap_or_default(), json!({ "api_key": "sk-ant-api03-secret" }));
}

#[test]
fn integration_slugs_round_trip_and_differ_from_serde_names() {
    for kind in [IntegrationKind::GitHub, IntegrationKind::GitHubFix, IntegrationKind::Linear, IntegrationKind::Slack] {
        assert_eq!(integration_from_slug(integration_slug(kind)), Some(kind));
    }
    assert_eq!(integration_slug(IntegrationKind::GitHubFix), "github-fix");
    assert_eq!(serde_json::to_value(IntegrationKind::GitHubFix).unwrap_or_default(), json!("github_fix"));
    assert_eq!(integration_from_slug("github_fix"), None);
}

#[test]
fn feed_event_and_actor_shapes() {
    let event = FeedEvent { kind: FeedKind::Run, subject: "run_01j9z3k4m5n6p7q8r9s0t1v2x4".into() };
    assert_eq!(serde_json::to_value(&event).unwrap_or_default(), json!({ "kind": "run", "subject": "run_01j9z3k4m5n6p7q8r9s0t1v2x4" }));
    let actor: AuditActor = serde_json::from_value(json!({ "type": "operator", "os_user": "chico" })).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(actor, AuditActor::Operator { os_user: "chico".into() });
}
