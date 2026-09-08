//! PID 1 reaping mode. Its own test binary: `ReapMode::All` collects every child of the
//! process, which would steal children from unrelated tests running in the same binary.
// Helpers outside `#[test]` fns are test code too.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::process::Command;
use std::time::Duration;

use cloud_runtime::guest::process::{ReapMode, Reaper};
use nix::errno::Errno;
use nix::sys::signal::kill;
use nix::unistd::Pid;

#[tokio::test]
async fn pid1_mode_collects_children_nobody_waits_for() {
    let reaper = Reaper::start(ReapMode::All).unwrap();

    // Not spawned through the reaper: as PID 1 this is an orphan reparented to us.
    let orphan = Command::new("sh").args(["-c", "exit 7"]).spawn().unwrap();
    let pid = Pid::from_raw(orphan.id() as i32);
    drop(orphan);

    // Registered children still get their exit code.
    let (_, exited) = reaper.spawn(Command::new("sh").args(["-c", "exit 3"])).unwrap();
    assert_eq!(tokio::time::timeout(Duration::from_secs(5), exited).await.unwrap().unwrap(), 3);

    // A zombie still answers signal 0; once reaped the pid is gone.
    let reaped = tokio::time::timeout(Duration::from_secs(5), async {
        while kill(pid, None) != Err(Errno::ESRCH) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(reaped.is_ok(), "orphan left as a zombie");
}
