//! The `cloud-guest` binary outside a VM (not PID 1): config drive in, process running,
//! control protocol on a Unix socket, clean stop on SIGINT.
// Helpers outside `#[test]` fns are test code too.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use cloud_contract::manifest::Health;
use cloud_runtime::guest::control::{Client, ProcessState};
use cloud_runtime::guest::{GuestConfig, ImageConfig};
use nix::errno::Errno;
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use tokio::net::UnixStream;

async fn eventually<T>(what: &str, mut f: impl AsyncFnMut() -> Option<T>) -> T {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(v) = f().await {
            return v;
        }
        assert!(tokio::time::Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

fn lines(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path).unwrap_or_default().lines().map(str::to_owned).collect()
}

#[tokio::test]
async fn runs_the_configured_process_and_serves_control() {
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("srv");
    std::fs::create_dir(&work).unwrap();
    let runs = work.join("runs");
    let config = GuestConfig {
        process: "web".into(),
        command: None,
        image: ImageConfig {
            entrypoint: vec!["sh".into(), "-c".into()],
            cmd: vec!["echo \"$GREETING\" >> runs; exec sleep 30".into()],
            env: vec!["PATH=/usr/bin:/bin".into(), "GREETING=from-image".into()],
            working_dir: Some(work.display().to_string()),
            user: None,
        },
        env: BTreeMap::from([("GREETING".into(), "hello".into()), ("SECRET".into(), "s3".into())]),
        port: None,
        health: Health::Command { argv: vec!["true".into()] },
        volume: None,
        dns: vec![],
    };
    let drive = dir.path().join("config.drive");
    std::fs::write(&drive, config.to_drive().unwrap()).unwrap();
    let sock = dir.path().join("ctl.sock");

    let mut guest = tokio::process::Command::new(env!("CARGO_BIN_EXE_cloud-guest"))
        .arg("--config")
        .arg(&drive)
        .arg("--control-socket")
        .arg(&sock)
        .args(["--stop-grace-secs", "2"])
        .kill_on_drop(true)
        .spawn()
        .unwrap();

    let stream = eventually("control socket", async || UnixStream::connect(&sock).await.ok()).await;
    let mut client = Client::new(stream);
    let first: ProcessState = eventually("first start", async || {
        let s = client.status().await.unwrap();
        (s.pid.is_some() && lines(&runs).len() == 1).then_some(s)
    })
    .await;
    assert_eq!(first.starts, 1);
    assert_eq!(lines(&runs), ["hello"], "platform env overrides the image's");

    let sh = |s: &str| vec!["sh".to_owned(), "-c".to_owned(), s.to_owned()];
    let t = Duration::from_secs(5);
    assert_eq!(client.health(&sh("test \"$SECRET\" = s3 && test -e runs"), t).await.unwrap(), 0);
    assert_eq!(client.health(&sh("exit 4"), t).await.unwrap(), 4);

    client.restart().await.unwrap();
    let second = eventually("restart", async || {
        let s = client.status().await.unwrap();
        (s.starts == 2 && s.pid.is_some()).then_some(s)
    })
    .await;
    assert_ne!(second.pid, first.pid);
    assert_eq!(second.last_exit, Some(128 + 15));
    eventually("second run", async || (lines(&runs).len() == 2).then_some(())).await;

    let app = Pid::from_raw(second.pid.unwrap() as i32);
    kill(Pid::from_raw(guest.id().unwrap() as i32), Signal::SIGINT).unwrap();
    let status = tokio::time::timeout(Duration::from_secs(10), guest.wait()).await.unwrap().unwrap();
    assert!(status.success(), "{status}");
    assert_eq!(kill(app, None), Err(Errno::ESRCH), "the app outlived its init");
}
