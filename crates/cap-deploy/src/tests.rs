use aho_cap::{CancelToken, Composition, Env, ExecView, Os, PolicyInput, PolicyView, Registry, plan};
use aho_config::{ResolveInputs, Schema, resolve};
use aho_log::{CapState, CustomFold, CustomFolds, EntryKind, LogError, MemoryLog, OpKind, OpStatus, ToolOutcome, fold};
use aho_types::{AgentId, CallId, ConsequenceSet, Epoch, OpId, Principal, TurnId};

use super::*;

fn drafted(unconfirmed: Vec<String>) -> Value {
    let mut draft = Draft::default();
    draft.offer(
        "/build/base_image",
        json!("node:22"),
        Strength::Inferred,
        cloud_contract::draft::Evidence { uri: "aho-log:1#00".into(), note: "n".into() },
    );
    serde_json::to_value(Drafted { session_id: "1".into(), head_hash: "a".repeat(64), draft, unconfirmed }).unwrap()
}

fn proposed(release: &str, hash: &str) -> Value {
    serde_json::to_value(Proposed { project: "shop".into(), release: release.into(), manifest_hash: hash.into() }).unwrap()
}

#[test]
fn the_fold_keeps_the_latest_draft_and_every_proposal_and_rejects_forgeries() {
    let hash = format!("blake3:{}", "b".repeat(64));
    let s1 = fold_deploy(None, DRAFTED, &drafted(vec!["/build/base_image".into()])).unwrap();
    let s2 = fold_deploy(Some(&s1), PROPOSED, &proposed("r1", &hash)).unwrap();
    let s3 = fold_deploy(Some(&s2), PROPOSED, &proposed("r2", &hash)).unwrap();
    let st: DeployState = serde_json::from_value(s3.clone()).unwrap();
    assert_eq!(st.drafts, 1);
    assert_eq!(st.last_draft.as_ref().map(|d| d.unconfirmed.clone()), Some(vec!["/build/base_image".to_string()]));
    assert_eq!(st.proposed.iter().map(|p| p.release.as_str()).collect::<Vec<_>>(), ["r1", "r2"]);

    for (name, body, why) in [
        (DRAFTED, drafted(vec![]), "a draft that hides a field needing confirmation"),
        (PROPOSED, proposed("r1", &hash), "the same release twice"),
        (PROPOSED, proposed("r3", "sha256:abc"), "a manifest hash that is not blake3"),
        (PROPOSED, proposed(" ", &hash), "an empty release id"),
        (PROPOSED, json!({"project": "shop"}), "a body missing fields"),
        ("Approved", json!({}), "an entry kind this namespace does not have: approvals are never session entries"),
    ] {
        assert!(fold_deploy(Some(&s3), name, &body).is_err(), "{why} must fail the fold");
    }
}

fn loadout(comp: &Composition, sets: &[(&str, &str)]) -> aho_config::Loadout {
    let mut schema = Schema::default();
    for k in comp.config_keys() {
        schema.add(k);
    }
    let mut i = ResolveInputs::default();
    i.sets = sets.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    resolve(&i, &schema, false).unwrap()
}

#[test]
fn the_caps_plan_without_exceeding_their_ceilings_and_propose_locks_without_the_credential() {
    let comp = Composition::new("t").consume::<DeployDraft>().consume::<DeployPropose>();
    let l = loadout(&comp, &[]);
    let policy = PolicyInput { preset: "read-only".into(), allowed: ConsequenceSet::of(&[Consequence::ReadOnly]) };
    let p = plan(&comp.manifests(), &l, &Env::empty(Os::current()), &policy, vec![]).unwrap();
    assert!(matches!(p.row("cloud-deploy-draft").unwrap().state, CapState::Active));
    let propose = p.row("cloud-deploy-propose").unwrap();
    let CapState::Locked { missing } = &propose.state else { panic!("{:?}", propose.state) };
    assert!(missing.iter().any(|m| m == "Credential(ahocloud)"), "{missing:?}");
    assert!(propose.missing.iter().any(|m| m.fix.as_deref() == Some("aho credential set ahocloud")));
    assert!(propose.ceiling.contains(Consequence::NetworkEgress), "the egress handle admits NetworkEgress");
    assert!(propose.over_preset.contains(Consequence::NetworkEgress), "read-only flags proposing as needing a wider preset");

    let (reg, broken) = comp.start(&p, &l);
    assert!(broken.is_empty(), "{broken:?}");
    assert!(reg.tool("cloud_draft").is_some() && reg.tool("cloud_propose").is_none(), "a locked capability registers nothing");
    assert!(reg.entry_kinds.contains_key(NS));
}

fn view(tool: &str, args: Value) -> ExecView {
    ExecView {
        call: CallId("c9".into()),
        tool: tool.into(),
        class: Consequence::ReadOnly,
        args,
        policy: PolicyView {
            epoch: Epoch(1),
            allowed: ConsequenceSet::ALL,
            world: BoundWorld::host(),
            workspace: String::new(),
            home: String::new(),
        },
        by: Principal::Agent { id: AgentId(1), turn: TurnId(1) },
    }
}

#[tokio::test]
async fn cloud_draft_drafts_this_sessions_log_and_its_event_folds() {
    let log = MemoryLog::new();
    let agent = Principal::Agent { id: AgentId(1), turn: TurnId(1) };
    let call = CallId("c1".into());
    let build = log
        .append(
            None,
            agent.clone(),
            EntryKind::ToolCall {
                call: call.clone(),
                tool: "bash".into(),
                args: json!({"command": "docker build ."}),
                class: Consequence::SpawnsProcess,
            },
        )
        .unwrap();
    log.append(
        None,
        Principal::system("ops"),
        EntryKind::OpStarted { op: OpId(1), call: call.clone(), kind: OpKind::Process, rerunnable: false, external_effect: true },
    )
    .unwrap();
    log.append(
        None,
        Principal::system("tool"),
        EntryKind::ToolResult {
            call,
            outcome: ToolOutcome::Ok,
            value: json!({"exit": 0}),
            content: vec![],
            spill_ref: None,
            meta: None,
            late: false,
        },
    )
    .unwrap();
    log.append(None, Principal::system("ops"), EntryKind::OpDone { op: OpId(1), status: OpStatus::Completed { code: 0 } }).unwrap();

    let comp = Composition::new("t").consume::<DeployDraft>();
    let l = loadout(&comp, &[]);
    let policy = PolicyInput { preset: "read-only".into(), allowed: ConsequenceSet::of(&[Consequence::ReadOnly]) };
    let p = plan(&comp.manifests(), &l, &Env::empty(Os::current()), &policy, vec![]).unwrap();
    let mut seed = Registry::default();
    let shared: Arc<dyn LogStore> = Arc::new(log.clone());
    seed.services.insert("aho.log".into(), Arc::new(shared));
    seed.services.insert("aho.session_id".into(), Arc::new(SessionId(0xbeef)));
    let (reg, broken) = comp.start_seeded(&p, &l, seed);
    assert!(broken.is_empty(), "{broken:?}");
    let (_, exec) = reg.tool("cloud_draft").unwrap();
    let ExecOutcome::Done { value, content, .. } =
        exec(ExecRequest { view: view("cloud_draft", json!({})), cancel: CancelToken::new(), progress: None }).await
    else {
        panic!("cloud_draft failed")
    };

    assert_eq!(value["session_id"], "beef");
    assert_eq!(value["draft"]["fields"]["/build/kind"]["value"], "dockerfile");
    assert_eq!(value["draft"]["fields"]["/build/kind"]["evidence"][0]["uri"], format!("aho-log:beef#{}", build.hash.to_hex()));
    assert_eq!(value["unconfirmed"], json!([]));
    let text = content[0].as_text().unwrap();
    assert!(text.contains("/build/kind") && text.contains("Ask the human for: api_version, project, source, processes"), "{text}");

    // what the session does with `value.events`
    let e = &value["events"][0];
    assert_eq!((e["ns"].as_str(), e["name"].as_str()), (Some(NS), Some(DRAFTED)));
    log.append(None, Principal::system("cloud_draft"), EntryKind::Custom { ns: NS.into(), name: DRAFTED.into(), body: e["body"].clone() })
        .unwrap();
    let mut folds = CustomFolds::new();
    folds.insert(NS.into(), fold_deploy as CustomFold);
    let st = fold(&log.read_branch(log.head().unwrap()).unwrap(), &folds).unwrap();
    let deploy: DeployState = serde_json::from_value(st.custom[NS].clone()).unwrap();
    assert_eq!(deploy.last_draft.map(|d| d.fields), Some(3));

    // a hand-written Drafted entry that disagrees with its own draft fails replay loudly
    log.append(None, Principal::system("cloud_draft"), EntryKind::Custom { ns: NS.into(), name: DRAFTED.into(), body: drafted(vec![]) })
        .unwrap();
    assert!(matches!(fold(&log.read_branch(log.head().unwrap()).unwrap(), &folds), Err(LogError::Invariant { rule: "custom.fold", .. })));
}

#[test]
fn cloud_draft_without_a_session_log_says_so() {
    let d = DeployDraft { log: None, session: None };
    assert!(matches!(d.run(), ExecOutcome::Err { code, .. } if code == "NO_SESSION_LOG"));
}
