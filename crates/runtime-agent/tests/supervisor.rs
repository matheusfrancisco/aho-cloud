//! Supervisor behavior against an in-process `Vmm`: reconcile, scale, sleep, maintenance,
//! restarts, rollouts, crashes and health; and the sandbox lifecycle.
// Helpers outside `#[test]` fns are test code too.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::net::IpAddr;
use std::time::Duration;

use cloud_api::SandboxState;
use cloud_api::internal::{RunState, SANDBOX_WORKSPACE};
use cloud_contract::ledger::Stage;
use cloud_runtime::egress::EgressPolicies;
use cloud_runtime::net::Allocator;
use cloud_runtime::vmm::DiskId;
use cloud_runtime::{Supervisor, SupervisorConfig, SupervisorError};
use common::{DIGEST_1, DIGEST_2, TestVmm, deployment, index, sandbox};

const P: &str = "acme-shop";
const S: Stage = Stage::Live;

fn config(grace: Duration) -> SupervisorConfig {
    SupervisorConfig {
        proxy_port: 3128,
        max_replicas: 4,
        health_timeout: Duration::from_millis(500),
        unhealthy_threshold: 1,
        startup_grace: grace,
        sandbox_failures: 2,
    }
}

fn supervisor(grace: Duration) -> (Supervisor<TestVmm>, TestVmm, EgressPolicies) {
    let vmm = TestVmm::default();
    let egress = EgressPolicies::default();
    let allocator = Allocator::new("10.77.0.0/24".parse().unwrap()).unwrap();
    (Supervisor::new(vmm.clone(), allocator, egress.clone(), config(grace)), vmm, egress)
}

fn process<'a>(s: &'a cloud_api::internal::DeploymentStatus, name: &str) -> &'a cloud_api::internal::ProcessStatus {
    s.processes.iter().find(|p| p.name == name).unwrap()
}

#[tokio::test]
async fn apply_records_intent_and_reconcile_boots_one_healthy_replica_per_process() {
    let (sup, vmm, egress) = supervisor(Duration::ZERO);
    let status = sup.apply(deployment("r1", DIGEST_1)).unwrap();
    assert_eq!(status.state, RunState::Starting);
    assert!(status.processes.iter().all(|p| p.replicas == 0));
    assert!(vmm.log().is_empty(), "apply must not boot synchronously");

    sup.reconcile().await;
    let status = sup.status(P, S).unwrap();
    assert_eq!(status.state, RunState::Running);
    assert_eq!(status.release, "r1");
    let web = process(&status, "web");
    assert_eq!((web.replicas, web.healthy), (1, 1));
    assert_eq!(web.addresses.len(), 1);
    let worker = process(&status, "worker");
    assert_eq!((worker.replicas, worker.healthy), (1, 1));
    assert!(worker.addresses.is_empty(), "no port, no address");

    let web_spec = vmm.spec(&vmm.alive_ids("web")[0]);
    let worker_spec = vmm.spec(&vmm.alive_ids("worker")[0]);
    assert_ne!(web_spec.lease.ip, worker_spec.lease.ip);
    assert_eq!(web_spec.image_digest, DIGEST_1);
    assert_eq!(web_spec.guest.env["STRIPE_KEY"], "sk_live_1");
    assert_eq!(web_spec.guest.env["PORT"], "8080");
    assert!(web_spec.volume.is_none());
    assert_eq!(worker_spec.volume.as_ref().unwrap().size_mb, 1024);
    assert_eq!(worker_spec.guest.command.as_deref().unwrap()[0], "python");

    let policy = egress.get(IpAddr::V4(web_spec.lease.ip)).unwrap();
    assert!(policy.allows("api.stripe.com") && policy.allows("o1.ingest.sentry.io"));
    assert!(!policy.allows("example.com"));

    // A second pass with nothing to do changes nothing.
    let before = vmm.log().len();
    sup.reconcile().await;
    assert_eq!(vmm.log().len(), before);
}

#[tokio::test]
async fn scale_adds_and_removes_the_highest_indices_within_bounds() {
    let (sup, vmm, egress) = supervisor(Duration::ZERO);
    sup.apply(deployment("r1", DIGEST_1)).unwrap();
    sup.reconcile().await;

    let status = sup.scale(P, S, "web", 3).unwrap();
    assert_eq!(status.state, RunState::Starting);
    sup.reconcile().await;
    let status = sup.status(P, S).unwrap();
    assert_eq!(status.state, RunState::Running);
    assert_eq!(process(&status, "web").replicas, 3);
    assert_eq!(process(&status, "web").addresses.len(), 3);
    let ids = vmm.alive_ids("web");
    let indices: Vec<u16> = ids.iter().map(|id| index(&vmm.spec(id))).collect();
    assert_eq!(indices, [0, 1, 2]);

    sup.scale(P, S, "web", 1).unwrap();
    sup.reconcile().await;
    assert_eq!(vmm.alive_ids("web"), [ids[0].clone()]);
    for gone in &ids[1..] {
        assert!(vmm.log().contains(&format!("stop {gone}")));
        assert!(egress.get(IpAddr::V4(vmm.spec(gone).lease.ip)).is_none(), "policy outlived the replica");
    }
    assert_eq!(sup.status(P, S).unwrap().state, RunState::Running);

    sup.scale(P, S, "worker", 0).unwrap();
    sup.reconcile().await;
    assert!(vmm.alive_ids("worker").is_empty());
    assert_eq!(sup.status(P, S).unwrap().state, RunState::Running);

    assert_eq!(sup.scale(P, S, "ghost", 1), Err(SupervisorError::UnknownProcess("ghost".into())));
    assert_eq!(sup.scale(P, S, "web", 5), Err(SupervisorError::TooManyReplicas { max: 4 }));
    assert!(matches!(sup.scale(P, Stage::Preview, "web", 1), Err(SupervisorError::NotFound(..))));
}

#[tokio::test]
async fn sleep_stops_everything_until_the_next_apply() {
    let (sup, vmm, egress) = supervisor(Duration::ZERO);
    sup.apply(deployment("r1", DIGEST_1)).unwrap();
    sup.scale(P, S, "web", 2).unwrap();
    sup.reconcile().await;
    let ips: Vec<_> = vmm.fake.lock().specs.values().map(|s| s.lease.ip).collect();

    assert_eq!(sup.sleep(P, S).unwrap().state, RunState::Sleeping);
    sup.reconcile().await;
    assert!(vmm.fake.lock().alive.is_empty());
    let status = sup.status(P, S).unwrap();
    assert_eq!(status.state, RunState::Sleeping);
    assert!(status.processes.iter().all(|p| p.replicas == 0 && p.addresses.is_empty()));
    assert!(ips.iter().all(|ip| egress.get(IpAddr::V4(*ip)).is_none()));
    sup.reconcile().await;
    assert!(vmm.fake.lock().alive.is_empty(), "stays asleep");

    // Re-applying (the owner raised the ceiling) wakes it with the same scale.
    assert_eq!(sup.apply(deployment("r1", DIGEST_1)).unwrap().state, RunState::Starting);
    sup.reconcile().await;
    let status = sup.status(P, S).unwrap();
    assert_eq!(status.state, RunState::Running);
    assert_eq!(process(&status, "web").replicas, 2);
}

#[tokio::test]
async fn maintenance_keeps_replicas_running() {
    let (sup, vmm, _) = supervisor(Duration::ZERO);
    sup.apply(deployment("r1", DIGEST_1)).unwrap();
    sup.reconcile().await;
    let before = vmm.log();

    assert_eq!(sup.maintenance(P, S, true).unwrap().state, RunState::Maintenance);
    sup.reconcile().await;
    assert_eq!(vmm.log(), before, "maintenance must not touch replicas");
    let status = sup.status(P, S).unwrap();
    assert_eq!(status.state, RunState::Maintenance);
    assert_eq!(process(&status, "web").healthy, 1);

    // Maintenance survives a new release; turning it off reports the real state again.
    sup.apply(deployment("r2", DIGEST_2)).unwrap();
    assert_eq!(sup.status(P, S).unwrap().state, RunState::Maintenance);
    sup.reconcile().await;
    assert_eq!(sup.maintenance(P, S, false).unwrap().state, RunState::Running);
}

#[tokio::test]
async fn a_new_release_hands_over_and_volumes_are_never_shared() {
    let (sup, vmm, _) = supervisor(Duration::from_secs(30));
    sup.apply(deployment("r1", DIGEST_1)).unwrap();
    sup.reconcile().await;
    let old_web = vmm.alive_ids("web")[0].clone();
    let old_worker = vmm.alive_ids("worker")[0].clone();
    let old_addr = process(&sup.status(P, S).unwrap(), "web").addresses.clone();

    sup.apply(deployment("r2", DIGEST_2)).unwrap();
    sup.reconcile().await;
    let log = vmm.log();
    let pos = |needle: &str| log.iter().position(|l| l.starts_with(needle)).unwrap();
    let new_web = vmm.alive_ids("web").into_iter().find(|id| *id != old_web).unwrap();
    let new_worker = vmm.alive_ids("worker")[0].clone();
    assert_ne!(new_worker, old_worker);
    assert_eq!(vmm.spec(&new_web).image_digest, DIGEST_2);
    // The volume holder stops before its successor boots.
    assert!(pos(&format!("stop {old_worker}")) < pos(&format!("start worker 0 {new_worker}")));
    // The old web keeps serving next to the new one until the new one is proven healthy.
    assert_eq!(vmm.alive_ids("web").len(), 2);
    let status = sup.status(P, S).unwrap();
    assert_eq!(status.release, "r2");
    assert_eq!(status.state, RunState::Running);
    let web = process(&status, "web");
    assert_eq!((web.replicas, web.healthy), (1, 1));
    assert_eq!(web.addresses.len(), 2);
    assert!(web.addresses.contains(&old_addr[0]));

    sup.reconcile().await;
    assert_eq!(vmm.alive_ids("web"), [new_web]);
    let web = process(&sup.status(P, S).unwrap(), "web").clone();
    assert_eq!(web.addresses.len(), 1);
    assert!(!web.addresses.contains(&old_addr[0]));
}

#[tokio::test]
async fn restart_and_rotated_secrets_replace_every_replica() {
    let (sup, vmm, _) = supervisor(Duration::from_secs(30));
    sup.apply(deployment("r1", DIGEST_1)).unwrap();
    sup.reconcile().await;
    let first: Vec<String> = vmm.fake.lock().alive.iter().cloned().collect();

    assert_eq!(sup.restart(P, S).unwrap().state, RunState::Starting);
    sup.reconcile().await;
    sup.reconcile().await;
    let second: Vec<String> = vmm.fake.lock().alive.iter().cloned().collect();
    assert_eq!(second.len(), 2);
    assert!(second.iter().all(|id| !first.contains(id)), "{first:?} vs {second:?}");
    assert_eq!(sup.status(P, S).unwrap().state, RunState::Running);

    let mut rotated = deployment("r1", DIGEST_1);
    rotated.secrets.insert("STRIPE_KEY".into(), "sk_live_2".into());
    sup.apply(rotated).unwrap();
    sup.reconcile().await;
    sup.reconcile().await;
    let web = vmm.alive_ids("web");
    assert_eq!(web.len(), 1);
    assert!(!second.contains(&web[0]));
    assert_eq!(vmm.spec(&web[0]).guest.env["STRIPE_KEY"], "sk_live_2");
}

#[tokio::test]
async fn crashed_replicas_are_rebooted_in_their_slot() {
    let (sup, vmm, _) = supervisor(Duration::ZERO);
    sup.apply(deployment("r1", DIGEST_1)).unwrap();
    sup.reconcile().await;
    let dead = vmm.alive_ids("web")[0].clone();
    let dead_ip = vmm.spec(&dead).lease.ip;
    vmm.crash(&dead);

    sup.reconcile().await;
    let web = vmm.alive_ids("web");
    assert_eq!(web.len(), 1);
    assert_ne!(web[0], dead);
    assert_eq!(index(&vmm.spec(&web[0])), 0);
    assert!(vmm.log().contains(&format!("stop {dead}")), "dead VM is cleaned up");
    // Its address went back to the pool and was reused by the replacement.
    assert_eq!(vmm.spec(&web[0]).lease.ip, dead_ip);
    assert_eq!(sup.status(P, S).unwrap().state, RunState::Running);
}

#[tokio::test]
async fn failing_health_checks_degrade_then_recover() {
    let (sup, vmm, _) = supervisor(Duration::ZERO);
    sup.apply(deployment("r1", DIGEST_1)).unwrap();
    vmm.set_healthy("web", false);
    sup.reconcile().await;
    let status = sup.status(P, S).unwrap();
    assert_eq!(status.state, RunState::Degraded);
    assert!(process(&status, "web").addresses.is_empty());
    assert_eq!(process(&status, "worker").healthy, 1);
    let sick = vmm.alive_ids("web")[0].clone();

    // The unhealthy replica is replaced (its successor is sick too).
    sup.reconcile().await;
    assert!(vmm.log().contains(&format!("stop {sick}")));
    assert_eq!(sup.status(P, S).unwrap().state, RunState::Degraded);

    // Command health goes through the guest control protocol.
    vmm.set_healthy("worker", false);
    sup.reconcile().await;
    assert_eq!(sup.status(P, S).unwrap().state, RunState::Failed);

    vmm.set_healthy("web", true);
    vmm.set_healthy("worker", true);
    sup.reconcile().await;
    let status = sup.status(P, S).unwrap();
    assert_eq!(status.state, RunState::Running);
    assert_eq!(process(&status, "web").addresses.len(), 1);
}

#[tokio::test]
async fn boot_failures_report_failed_and_retry() {
    let (sup, vmm, _) = supervisor(Duration::ZERO);
    vmm.fake.lock().fail_start = true;
    sup.apply(deployment("r1", DIGEST_1)).unwrap();
    sup.reconcile().await;
    assert_eq!(sup.status(P, S).unwrap().state, RunState::Failed);
    // Nothing leaked: the next successful boots get the first addresses.
    vmm.fake.lock().fail_start = false;
    sup.reconcile().await;
    assert_eq!(sup.status(P, S).unwrap().state, RunState::Running);
    let mut ips: Vec<_> = vmm.fake.lock().specs.values().map(|s| s.lease.ip.octets()[3]).collect();
    ips.sort();
    assert_eq!(ips, [2, 3]);
}

#[tokio::test]
async fn remove_stops_and_forgets() {
    let (sup, vmm, _) = supervisor(Duration::ZERO);
    sup.apply(deployment("r1", DIGEST_1)).unwrap();
    sup.reconcile().await;
    sup.remove(P, S).unwrap();
    assert!(matches!(sup.status(P, S), Err(SupervisorError::NotFound(..))));
    assert!(matches!(sup.remove(P, S), Err(SupervisorError::NotFound(..))));
    sup.reconcile().await;
    assert!(vmm.fake.lock().alive.is_empty());

    // Deploying again starts from scratch (scale back to one replica each).
    sup.apply(deployment("r2", DIGEST_2)).unwrap();
    sup.reconcile().await;
    assert_eq!(vmm.fake.lock().alive.len(), 2);
}

#[tokio::test]
async fn project_mismatch_is_refused() {
    let (sup, _, _) = supervisor(Duration::ZERO);
    let mut d = deployment("r1", DIGEST_1);
    d.project = "other".into();
    assert!(matches!(sup.apply(d), Err(SupervisorError::Invalid(_))));
}

const SB: &str = "dev-box";

#[tokio::test]
async fn a_sandbox_boots_with_its_own_resources_disk_env_and_egress() {
    let (sup, vmm, egress) = supervisor(Duration::ZERO);
    sup.apply(deployment("r1", DIGEST_1)).unwrap();
    let status = sup.put_sandbox(sandbox(SB, DIGEST_1)).unwrap();
    assert_eq!((status.state, status.address.as_deref()), (SandboxState::Starting, None));
    assert!(vmm.log().is_empty(), "put must not boot synchronously");

    sup.reconcile().await;
    let status = sup.sandbox_status(SB).unwrap();
    assert_eq!(status.state, SandboxState::Running);
    let [id] = vmm.sandbox_ids(SB).try_into().unwrap();
    let spec = vmm.spec(&id);
    assert_eq!((spec.repo.as_str(), spec.image_digest.as_str(), spec.hostname.as_str()), ("aho.sandbox", DIGEST_1, SB));
    assert_eq!((spec.cpus, spec.mem_mib), (Some(2), Some(2048)), "the sandbox's resources, not the backend defaults");
    let volume = spec.volume.as_ref().unwrap();
    assert_eq!((&volume.disk, volume.size_mb), (&DiskId::Sandbox(SB.into()), 10240));
    assert_eq!(spec.guest.volume.as_ref().unwrap().mount, SANDBOX_WORKSPACE);
    assert_eq!(spec.guest.command, None, "the image's idle command runs");
    assert_eq!(spec.guest.env["ANTHROPIC_API_KEY"], "sk-ant-1");
    assert_eq!(spec.guest.env["PORT"], "3000");
    assert_eq!(spec.guest.env["HTTPS_PROXY"], format!("http://{}:3128", spec.lease.gateway));
    assert_eq!(status.address, Some(vmm.endpoint_of(&id).to_string()));
    assert_eq!(sup.sandbox_address(SB).unwrap(), Some(vmm.endpoint_of(&id)));

    // One address pool for deployments and sandboxes; the sandbox's own allowlist.
    let mut ips: Vec<_> = vmm.fake.lock().specs.values().map(|s| s.lease.ip).collect();
    ips.sort();
    ips.dedup();
    assert_eq!(ips.len(), 3);
    let policy = egress.get(IpAddr::V4(spec.lease.ip)).unwrap();
    assert!(policy.allows("api.anthropic.com") && policy.allows("api.github.com"));
    assert!(!policy.allows("api.stripe.com") && !policy.allows("example.com"));

    let before = vmm.log().len();
    sup.reconcile().await;
    assert_eq!(vmm.log().len(), before, "nothing to do");
}

#[tokio::test]
async fn stopping_keeps_the_disk_and_starting_again_reuses_it() {
    let (sup, vmm, egress) = supervisor(Duration::ZERO);
    sup.put_sandbox(sandbox(SB, DIGEST_1)).unwrap();
    sup.reconcile().await;
    let [first] = vmm.sandbox_ids(SB).try_into().unwrap();
    let ip = vmm.spec(&first).lease.ip;

    let status = sup.stop_sandbox(SB).unwrap();
    assert_eq!((status.state, status.address), (SandboxState::Stopped, None));
    assert!(matches!(sup.sandbox_exec(SB).await, Err(SupervisorError::NotRunning(_))));
    sup.reconcile().await;
    assert!(vmm.sandbox_ids(SB).is_empty());
    assert!(vmm.log().contains(&format!("stop {first}")));
    assert!(!vmm.log().iter().any(|l| l.starts_with("destroy")), "a stop keeps the disk");
    assert!(egress.get(IpAddr::V4(ip)).is_none(), "policy outlived the VM");
    sup.reconcile().await;
    assert_eq!(sup.sandbox_status(SB).unwrap().state, SandboxState::Stopped, "stays stopped");
    assert!(vmm.sandbox_ids(SB).is_empty());

    sup.put_sandbox(sandbox(SB, DIGEST_1)).unwrap();
    sup.reconcile().await;
    let [second] = vmm.sandbox_ids(SB).try_into().unwrap();
    assert_ne!(second, first);
    assert_eq!(vmm.spec(&second).volume.unwrap().disk, DiskId::Sandbox(SB.into()));
    assert_eq!(sup.sandbox_status(SB).unwrap().state, SandboxState::Running);
}

#[tokio::test]
async fn removing_stops_then_destroys_the_disk_and_forgets() {
    let (sup, vmm, _) = supervisor(Duration::ZERO);
    sup.put_sandbox(sandbox(SB, DIGEST_1)).unwrap();
    sup.reconcile().await;
    let [id] = vmm.sandbox_ids(SB).try_into().unwrap();

    sup.remove_sandbox(SB).unwrap();
    assert!(matches!(sup.sandbox_status(SB), Err(SupervisorError::NoSandbox(_))));
    assert!(matches!(sup.stop_sandbox(SB), Err(SupervisorError::NoSandbox(_))));
    sup.reconcile().await;
    let log = vmm.log();
    let pos = |needle: &str| log.iter().position(|l| l == needle).unwrap();
    assert!(pos(&format!("stop {id}")) < pos(&format!("destroy sandbox/{SB}")), "{log:?}");
    sup.reconcile().await;
    assert_eq!(vmm.log(), log, "done once");

    // A slug this agent never saw (it restarted) still loses its disk; bad slugs are refused.
    sup.remove_sandbox("old-box").unwrap();
    sup.reconcile().await;
    assert_eq!(vmm.log().last().unwrap(), "destroy sandbox/old-box");
    assert!(matches!(sup.remove_sandbox("../x"), Err(SupervisorError::Invalid(_))));
    assert!(matches!(sup.put_sandbox(sandbox("Bad.Slug", DIGEST_1)), Err(SupervisorError::Invalid(_))));
}

#[tokio::test]
async fn a_sandbox_recreated_before_its_removal_ran_never_boots_from_the_old_disk() {
    let (sup, vmm, _) = supervisor(Duration::ZERO);
    sup.put_sandbox(sandbox(SB, DIGEST_1)).unwrap();
    sup.reconcile().await;
    let [old] = vmm.sandbox_ids(SB).try_into().unwrap();
    sup.remove_sandbox(SB).unwrap();
    assert_eq!(sup.put_sandbox(sandbox(SB, DIGEST_2)).unwrap().state, SandboxState::Starting);
    sup.reconcile().await;
    let log = vmm.log();
    let [new] = vmm.sandbox_ids(SB).try_into().unwrap();
    let pos = |needle: &str| log.iter().position(|l| l == needle).unwrap();
    assert!(pos(&format!("stop {old}")) < pos(&format!("destroy sandbox/{SB}")));
    assert!(pos(&format!("destroy sandbox/{SB}")) < pos(&format!("start sandbox {SB} {new}")), "{log:?}");
    assert_eq!(sup.sandbox_status(SB).unwrap().state, SandboxState::Running);
}

#[tokio::test]
async fn a_changed_spec_replaces_the_vm_and_keeps_the_disk() {
    let (sup, vmm, _) = supervisor(Duration::ZERO);
    sup.put_sandbox(sandbox(SB, DIGEST_1)).unwrap();
    sup.reconcile().await;
    let [old] = vmm.sandbox_ids(SB).try_into().unwrap();

    // The same spec again changes nothing.
    assert_eq!(sup.put_sandbox(sandbox(SB, DIGEST_1)).unwrap().state, SandboxState::Running);
    let before = vmm.log();
    sup.reconcile().await;
    assert_eq!(vmm.log(), before);

    let mut changed = sandbox(SB, DIGEST_1);
    changed.env.insert("GITHUB_TOKEN".into(), "ghp_1".into());
    assert_eq!(sup.put_sandbox(changed).unwrap().state, SandboxState::Starting);
    sup.reconcile().await;
    let [new] = vmm.sandbox_ids(SB).try_into().unwrap();
    assert_ne!(new, old);
    let log = vmm.log();
    let pos = |needle: &str| log.iter().position(|l| l == needle).unwrap();
    assert!(pos(&format!("stop {old}")) < pos(&format!("start sandbox {SB} {new}")), "one VM on the disk at a time");
    assert!(!log.iter().any(|l| l.starts_with("destroy")));
    assert_eq!(vmm.spec(&new).guest.env["GITHUB_TOKEN"], "ghp_1");
    assert_eq!(sup.sandbox_status(SB).unwrap().state, SandboxState::Running);
}

#[tokio::test]
async fn a_crashed_sandbox_vm_is_rebooted() {
    let (sup, vmm, _) = supervisor(Duration::ZERO);
    sup.put_sandbox(sandbox(SB, DIGEST_1)).unwrap();
    sup.reconcile().await;
    let [dead] = vmm.sandbox_ids(SB).try_into().unwrap();
    vmm.crash(&dead);
    sup.reconcile().await;
    let [alive] = vmm.sandbox_ids(SB).try_into().unwrap();
    assert_ne!(alive, dead);
    assert!(vmm.log().contains(&format!("stop {dead}")), "the dead VM is cleaned up");
    assert_eq!(sup.sandbox_status(SB).unwrap().state, SandboxState::Running);
}

#[tokio::test]
async fn repeated_boot_failures_report_failed_until_a_boot_succeeds() {
    let (sup, vmm, _) = supervisor(Duration::ZERO);
    vmm.fake.lock().fail_start = true;
    sup.put_sandbox(sandbox(SB, DIGEST_1)).unwrap();
    sup.reconcile().await;
    let status = sup.sandbox_status(SB).unwrap();
    assert_eq!((status.state, status.error), (SandboxState::Starting, None), "one failure is not yet failed");
    sup.reconcile().await;
    let status = sup.sandbox_status(SB).unwrap();
    assert_eq!(status.state, SandboxState::Failed);
    assert!(status.error.unwrap().contains("image pull failed"));

    // Still retried; nothing leaked, so the first boot that works gets the first address.
    vmm.fake.lock().fail_start = false;
    sup.reconcile().await;
    let status = sup.sandbox_status(SB).unwrap();
    assert_eq!((status.state, status.error), (SandboxState::Running, None));
    let [id] = vmm.sandbox_ids(SB).try_into().unwrap();
    assert_eq!(vmm.spec(&id).lease.ip.octets()[3], 2);

    // A VM that keeps dying right after boot fails the same way.
    vmm.crash(&id);
    vmm.fake.lock().fail_start = true;
    sup.reconcile().await;
    sup.reconcile().await;
    assert_eq!(sup.sandbox_status(SB).unwrap().state, SandboxState::Failed);
    // A put (the owner starting it again) clears the failure count.
    assert_eq!(sup.put_sandbox(sandbox(SB, DIGEST_1)).unwrap().state, SandboxState::Starting);
}
