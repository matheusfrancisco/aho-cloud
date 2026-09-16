//! The cloud composition plans with its capabilities Active, and `ahoc` serves a real session over
//! rpc in which the scripted model builds and then calls `cloud_draft`.
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration-test helpers may panic on setup failure

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::Duration;

use aho_compose::{Inputs, load};
use aho_log::{CapState, EntryKind, Transition};
use aho_sdk::{Client, Event, Spawn};
use serde_json::{Value, json};

const CLOUD_CAPS: [&str; 3] = ["cloud-deploy-draft", "cloud-deploy-propose", "cloud-edge-contract"];

/// Operator inputs whose PATH holds only a stand-in `git`, the one binary the host world requires.
fn inputs(dir: &Path, sets: &[(&str, &str)]) -> Inputs {
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let git = bin.join("git");
    std::fs::write(&git, "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(&git, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut i = Inputs::new(dir, dir.join("home"));
    i.env.insert("PATH".into(), bin.display().to_string());
    i.sets = sets.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    i
}

#[test]
fn cloud_plans_its_caps_active_once_the_credential_is_set() {
    let dir = tempfile::tempdir().unwrap();
    let with = load(&bundle_cloud::cloud(), &inputs(dir.path(), &[("credentials.ahocloud", "ahc_tok")])).unwrap();
    for id in CLOUD_CAPS {
        let row = with.plan.row(id).unwrap();
        assert_eq!(row.state, CapState::Active, "{id}: {:?} {:?}", row.state, row.missing);
    }
    assert!(with.plan.row("tool-bash").is_some(), "the lab product is included");

    let without = load(&bundle_cloud::cloud(), &inputs(dir.path(), &[])).unwrap();
    let propose = without.plan.row("cloud-deploy-propose").unwrap();
    assert!(matches!(propose.state, CapState::Locked { .. }), "{:?}", propose.state);
    assert!(propose.missing.iter().any(|m| m.fix.as_deref() == Some("aho credential set ahocloud")), "{:?}", propose.missing);
    for id in ["cloud-deploy-draft", "cloud-edge-contract"] {
        assert_eq!(without.plan.row(id).map(|r| r.state.clone()), Some(CapState::Active), "{id} needs no credential");
    }
}

async fn within<T>(f: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(60), f).await.expect("finished within 60 s")
}

#[tokio::test]
async fn ahoc_serves_a_session_whose_agent_drafts_from_its_own_build() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("project");
    let home = dir.path().join("home");
    std::fs::create_dir_all(project.join("tools")).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    // A stand-in `npm` so the build really runs and exits 0.
    let npm = project.join("tools/npm");
    std::fs::write(&npm, "#!/bin/sh\necho built\nexit 0\n").unwrap();
    std::fs::set_permissions(&npm, std::fs::Permissions::from_mode(0o755)).unwrap();
    let script = dir.path().join("script.json");
    let steps = json!({"steps": [
        {"tool_calls": [{"name": "bash", "args": {"command": "./tools/npm run build"}}]},
        {"expect_tool_result_contains": "built", "tool_calls": [{"name": "cloud_draft", "args": {}}]},
        {"expect_tool_result_contains": "/build/command", "text": "Drafted."}
    ]});
    std::fs::write(&script, steps.to_string()).unwrap();

    let mut spec = Spawn::new(env!("CARGO_BIN_EXE_ahoc"))
        .set("model.provider", "scripted")
        .set("provider.scripted.script", &script.display().to_string())
        .set("credentials.ahocloud", "ahc_tok");
    spec.project = Some(project.clone());
    spec.env.push(("HOME".into(), home.into_os_string()));
    let c = within(Client::spawn(&spec, aho_sdk::hello("ahoc-test"))).await.unwrap();
    let mut events = c.events().unwrap();
    c.subscribe(None).await.unwrap();
    assert_eq!(within(c.prompt("deploy this")).await.unwrap(), Transition::Completed);

    let entries: Vec<EntryKind> =
        std::iter::from_fn(|| events.try_next()).filter_map(|e| if let Event::Entry(e) = e { Some(e.kind) } else { None }).collect();
    let drafted: &Value = entries
        .iter()
        .find_map(|k| match k {
            EntryKind::Custom { ns, name, body } if ns == "Deploy" && name == "Drafted" => Some(body),
            _ => None,
        })
        .expect("cloud_draft recorded Deploy/Drafted");
    let command = &drafted["draft"]["fields"]["/build/command"];
    assert_eq!(command["value"], json!(["./tools/npm", "run", "build"]));
    assert_eq!(command["strength"], "chained");
    assert!(command["evidence"][0]["uri"].as_str().unwrap().starts_with(&format!("aho-log:{}#", drafted["session_id"].as_str().unwrap())));
    assert_eq!(drafted["session_id"].as_str(), Some(c.hello_ok().session.as_str()), "evidence is keyed by the session id aho prints");

    let status = within(c.close()).await.unwrap().expect("a spawned server has an exit status");
    assert!(status.success(), "{status:?}");
}
