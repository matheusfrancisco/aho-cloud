use std::time::Duration;

use mage_core::domain::{HarnessKind, RunEventKind, Usage};

use super::*;
use crate::ports::executor::{HarnessPlacement, IsolationMode};
use crate::testing::executor::FakeEnv;
use crate::testing::harness::sample_spec;

fn info() -> SessionInfo {
    SessionInfo {
        harness: HarnessKind::Omp,
        version: "18.3.1".into(),
        native_session_id: "s".into(),
        model: "claude-sonnet-4-5".into(),
        tools: vec![
            ToolRef::Builtin("read".into()),
            ToolRef::Mcp { server: McpServerName::run(), tool: "exec".into() },
            ToolRef::Mcp { server: McpServerName::knowledge(), tool: "search_code".into() },
            ToolRef::Mcp { server: McpServerName::run(), tool: "submit_fix".into() },
        ],
        pid: 1,
        pgid: 1,
    }
}

fn usage(input: u64, cache: u64, output: u64, cost: Option<u64>, calls: u32) -> HarnessEvent {
    HarnessEvent::Usage(UsageDelta {
        input_tokens: input,
        output_tokens: output,
        cache_read_tokens: cache,
        cache_write_tokens: 0,
        cost_micros: cost,
        model_calls: calls,
    })
}

#[test]
fn event_mapping_table() {
    let run = ToolRef::Mcp { server: McpServerName::run(), tool: "exec".into() };
    let mut m = RunEventMapper::new(CredentialModeKind::LocalLogin);
    let cases: Vec<(HarnessEvent, Option<RunEventKind>)> = vec![
        (
            HarnessEvent::Ready(info()),
            Some(RunEventKind::Notice("harness ready: omp 18.3.1, model claude-sonnet-4-5, tools 4, mcp [mage_knowledge, mage_run]".into())),
        ),
        (HarnessEvent::TurnStarted { turn: 1 }, None),
        (HarnessEvent::AssistantText { turn: 1, text: "hi".into() }, Some(RunEventKind::AssistantText("hi".into()))),
        (
            HarnessEvent::ToolCall { turn: 1, call_id: "c".into(), tool: run.clone(), args_preview: "{}".into() },
            Some(RunEventKind::ToolCall { name: "run.exec".into(), args_preview: "{}".into() }),
        ),
        (
            HarnessEvent::ToolResult { turn: 1, call_id: "c".into(), tool: ToolRef::Builtin("bash".into()), ok: false, preview: "x".into() },
            Some(RunEventKind::ToolResult { name: "bash".into(), ok: false, preview: "x".into() }),
        ),
        (HarnessEvent::Notice { level: NoticeLevel::Info, text: "a".into() }, Some(RunEventKind::Notice("a".into()))),
        (HarnessEvent::Notice { level: NoticeLevel::Warning, text: "b".into() }, Some(RunEventKind::Notice("warning: b".into()))),
        (HarnessEvent::Notice { level: NoticeLevel::Error, text: "c".into() }, Some(RunEventKind::Notice("error: c".into()))),
        (HarnessEvent::TurnEnded { turn: 1, outcome: TurnOutcome::Completed }, None),
        (HarnessEvent::TurnEnded { turn: 1, outcome: TurnOutcome::Aborted }, Some(RunEventKind::Notice("turn aborted".into()))),
        (
            HarnessEvent::TurnEnded {
                turn: 1,
                outcome: TurnOutcome::Failed { message: "401 bad token".into(), retryable: false, http_status: Some(401) },
            },
            Some(RunEventKind::Error("401 bad token".into())),
        ),
        (HarnessEvent::Idle, None),
    ];
    for (event, want) in cases {
        assert_eq!(m.map(&event), want, "{event:?}");
    }
}

#[test]
fn usage_is_cumulative_outside_gateway_and_suppressed_in_gateway() {
    let mut local = RunEventMapper::new(CredentialModeKind::LocalLogin);
    let first = local.map(&usage(10, 5, 3, Some(100), 1));
    assert_eq!(first, Some(RunEventKind::Usage(Usage { input_tokens: 15, output_tokens: 3, cost_micros: 100, turns: 1, wall: Duration::ZERO })));
    let second = local.map(&usage(1, 0, 2, None, 1));
    assert_eq!(second, Some(RunEventKind::Usage(Usage { input_tokens: 16, output_tokens: 5, cost_micros: 100, turns: 2, wall: Duration::ZERO })));
    assert_eq!(local.usage().turns, 2);

    let mut gateway = RunEventMapper::new(CredentialModeKind::Gateway);
    assert_eq!(gateway.map(&usage(10, 5, 3, Some(100), 1)), None);
    assert_eq!(gateway.usage(), Usage::default());
    let mut vertex = RunEventMapper::new(CredentialModeKind::Vertex);
    assert!(vertex.map(&usage(1, 0, 1, None, 1)).is_some());
}

#[test]
fn previews_are_bounded_by_the_mapper() {
    let mut m = RunEventMapper::new(CredentialModeKind::Gateway);
    let event = HarnessEvent::ToolResult { turn: 1, call_id: "c".into(), tool: ToolRef::Builtin("read".into()), ok: true, preview: "y".repeat(9000) };
    match m.map(&event) {
        Some(RunEventKind::ToolResult { preview, .. }) => assert_eq!(preview.len(), mage_core::domain::event::PREVIEW_MAX_BYTES),
        other => panic!("{other:?}"),
    }
}

#[test]
fn tool_event_names() {
    assert_eq!(ToolRef::Mcp { server: McpServerName::knowledge(), tool: "search_code".into() }.event_name(), "knowledge.search_code");
    assert_eq!(ToolRef::Mcp { server: McpServerName::new("other_srv").unwrap(), tool: "t".into() }.event_name(), "other_srv.t");
    assert_eq!(ToolRef::Builtin("Read".into()).event_name(), "Read");
}

#[test]
fn newtype_validation() {
    assert!(ModelId::new("claude-sonnet-4-5").is_ok());
    assert!(ModelId::new("anthropic/claude@2025:v1.0_x").is_ok());
    assert!(ModelId::new("").is_err());
    assert!(ModelId::new("a b").is_err());
    assert!(ModelId::new("x".repeat(129)).is_err());
    assert!(McpServerName::new("mage_run").is_ok());
    assert!(McpServerName::new("a".repeat(32)).is_ok());
    for bad in ["", "Mage", "1x", "a-b", "_a"] {
        assert!(McpServerName::new(bad).is_err(), "{bad:?}");
    }
    assert!(McpServerName::new("a".repeat(33)).is_err());
}

#[test]
fn secret_debug_is_redacted() {
    let spec = sample_spec();
    let debug = format!("{spec:?}");
    for secret in spec.secrets() {
        assert!(!debug.contains(secret.expose()), "{debug}");
    }
    assert_eq!(format!("{:?}", Secret::new("x")), "Secret([redacted])");
}

#[test]
fn static_validation_accepts_the_sample_and_boundaries() {
    let mut spec = sample_spec();
    spec.validate().unwrap();
    spec.prompt = "x".repeat(PROMPT_MAX_BYTES);
    spec.system_append = "y".repeat(SYSTEM_APPEND_MAX_BYTES);
    spec.validate().unwrap();
    spec.mcp_servers[0].url = "http://localhost:9/mcp".into();
    spec.validate().unwrap();
    spec.mcp_servers[0].url = "http://[::1]:9/mcp".into();
    spec.validate().unwrap();
    spec.env_allowlist = vec!["NODE_EXTRA_CA_CERTS".into(), "_X".into()];
    spec.validate().unwrap();
    spec.session_dir = RelPath::new("launch/x").unwrap();
    assert!(matches!(spec.validate(), Err(HarnessError::Invalid(_))));
    spec.session_dir = RelPath::root();
    assert!(matches!(spec.validate(), Err(HarnessError::Invalid(_))));
}

#[test]
fn credential_fields_are_validated() {
    let mut spec = sample_spec();
    spec.credentials = CredentialsMode::Gateway { base_url: "http://gateway.example.com/llm".into(), token: Secret::new("mgg_0123456789abcdef") };
    assert!(matches!(spec.validate(), Err(HarnessError::Invalid(_))));
    spec.credentials = CredentialsMode::Vertex { project_id: "Proj!".into(), region: "us-east5".into() };
    assert!(matches!(spec.validate(), Err(HarnessError::Invalid(_))));
    spec.credentials = CredentialsMode::Vertex { project_id: "proj".into(), region: "us-east5".into() };
    spec.validate().unwrap();
}

#[tokio::test]
async fn env_dependent_rows() {
    let env = FakeEnv::empty();
    let spec = sample_spec();
    let gateway_only = [CredentialModeKind::Gateway];
    spec.validate_in(env.as_ref(), &gateway_only, false).await.unwrap();

    // Credentials: unsupported by the adapter, LocalLogin outside dev mode, Vertex without opt-in.
    let mut local = spec.clone();
    local.credentials = CredentialsMode::LocalLogin;
    assert!(matches!(local.validate_in(env.as_ref(), &gateway_only, false).await, Err(HarnessError::Unsupported(_))));
    let both = [CredentialModeKind::Gateway, CredentialModeKind::LocalLogin, CredentialModeKind::Vertex];
    local.validate_in(env.as_ref(), &both, false).await.unwrap();
    env.set_isolation(IsolationMode::Enforced);
    assert!(matches!(local.validate_in(env.as_ref(), &both, false).await, Err(HarnessError::Unsupported(_))));
    env.set_isolation(IsolationMode::Dev);
    let mut vertex = spec.clone();
    vertex.credentials = CredentialsMode::Vertex { project_id: "proj".into(), region: "us-east5".into() };
    assert!(matches!(vertex.validate_in(env.as_ref(), &both, false).await, Err(HarnessError::Unsupported(_))));
    vertex.validate_in(env.as_ref(), &both, true).await.unwrap();

    // Placement: a harness outside the env gets no built-ins.
    env.set_harness_placement(HarnessPlacement::Host);
    assert!(matches!(spec.validate_in(env.as_ref(), &gateway_only, false).await, Err(HarnessError::Unsupported(_))));
    let mut mcp_only = spec.clone();
    mcp_only.builtin_tools = BuiltinToolPolicy::None;
    mcp_only.cwd = None;
    mcp_only.validate_in(env.as_ref(), &gateway_only, false).await.unwrap();
    env.set_harness_placement(HarnessPlacement::InEnv);

    // cwd must be a directory; session_dir must be fresh.
    let file = EnvPath::ws("README.md").unwrap();
    env.set_file(&file, b"x");
    let mut on_file = spec.clone();
    on_file.cwd = Some(RelPath::new("README.md").unwrap());
    assert!(matches!(on_file.validate_in(env.as_ref(), &gateway_only, false).await, Err(HarnessError::Invalid(_))));
    env.make_dir(&EnvPath::scratch(spec.session_dir.as_str()).unwrap(), crate::ports::executor::FileAccess::AgentReadOnly).await.unwrap();
    assert!(matches!(spec.validate_in(env.as_ref(), &gateway_only, false).await, Err(HarnessError::Invalid(_))));
}

#[test]
fn error_classification() {
    assert!(HarnessError::Unavailable("x".into()).is_infrastructure());
    assert!(HarnessError::Protocol("x".into()).is_infrastructure());
    assert!(HarnessError::Closed.is_infrastructure());
    assert!(HarnessError::Env(ExecError::Destroyed).is_infrastructure());
    for e in [
        HarnessError::Invalid("x".into()),
        HarnessError::Unsupported("x".into()),
        HarnessError::IsolationViolation("x".into()),
        HarnessError::NotFound("x".into()),
        HarnessError::Conflict("x".into()),
    ] {
        assert!(!e.is_infrastructure(), "{e:?}");
    }
    assert!(matches!(AppError::from(HarnessError::IsolationViolation("x".into())), AppError::Invalid(_)));
    assert!(matches!(AppError::from(HarnessError::Protocol("x".into())), AppError::Unavailable(_)));
}
