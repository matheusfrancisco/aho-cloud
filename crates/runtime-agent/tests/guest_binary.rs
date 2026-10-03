//! The `cloud-guest` binary outside a VM (not PID 1): config drive in, process running,
//! control protocol on a Unix socket, clean stop on SIGINT; and its `exec`, `files` and `file`
//! subcommands as the host runs them inside guests.
// Helpers outside `#[test]` fns are test code too.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use cloud_api::exec::{ExecRequest, Frame};
use cloud_api::{FileContent, FileTree};
use cloud_runtime::guest::control::{Client, ProcessState};
use cloud_runtime::guest::exec::{read_frame, write_frame};
use cloud_runtime::guest::{EXIT_BAD_PATH, EXIT_NOT_FOUND, GuestConfig, ImageConfig};
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

const GUEST: &str = env!("CARGO_BIN_EXE_cloud-guest");

#[tokio::test]
async fn exec_serves_one_session_on_stdin_and_stdout_with_its_own_environment() {
    let mut guest = tokio::process::Command::new(GUEST)
        .arg("exec")
        .env("GUEST_ONLY", "from-the-guest")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut to_guest = guest.stdin.take().unwrap();
    let mut from_guest = guest.stdout.take().unwrap();
    let request = ExecRequest {
        argv: vec!["sh".into(), "-c".into(), "echo \"$GUEST_ONLY $ADDED\"; cat; exit 5".into()],
        pty: None,
        env: BTreeMap::from([("ADDED".into(), "by-the-request".into())]),
        cwd: None,
    };
    write_frame(&mut to_guest, &Frame::Request(request)).await.unwrap();
    write_frame(&mut to_guest, &Frame::Stdin(b"piped in\n".to_vec())).await.unwrap();
    write_frame(&mut to_guest, &Frame::CloseStdin).await.unwrap();
    let mut stdout = Vec::new();
    let code = loop {
        match read_frame(&mut from_guest).await.unwrap().unwrap() {
            Frame::Stdout(b) => stdout.extend(b),
            Frame::Exit(code) => break code,
            other => panic!("{other:?}"),
        }
    };
    assert_eq!(String::from_utf8(stdout).unwrap(), "from-the-guest by-the-request\npiped in\n");
    assert_eq!(code, 5);
    assert_eq!(read_frame(&mut from_guest).await.unwrap(), None, "the session is over");
    let status = tokio::time::timeout(Duration::from_secs(10), guest.wait()).await.unwrap().unwrap();
    assert!(status.success(), "{status}");
}

fn workspace() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::create_dir_all(dir.path().join(".git")).unwrap();
    std::fs::write(dir.path().join("src/main.rs"), "fn main() {}").unwrap();
    std::fs::write(dir.path().join(".git/HEAD"), "ref: x").unwrap();
    dir
}

async fn guest(args: &[&str]) -> (i32, String, String) {
    let out = tokio::process::Command::new(GUEST).args(args).output().await.unwrap();
    (out.status.code().unwrap(), String::from_utf8(out.stdout).unwrap(), String::from_utf8(out.stderr).unwrap())
}

#[tokio::test]
async fn files_and_file_print_the_workspace_as_json_and_classify_refusals() {
    let ws = workspace();
    let root = ws.path().to_str().unwrap();
    let (code, out, _) = guest(&["files", "--root", root]).await;
    assert_eq!(code, 0);
    let tree: FileTree = serde_json::from_str(&out).unwrap();
    let paths: Vec<&str> = tree.files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(paths, ["src", "src/main.rs"], ".git is never listed");

    let (code, out, _) = guest(&["file", "--root", root, "--path=src/main.rs"]).await;
    assert_eq!(code, 0);
    let file: FileContent = serde_json::from_str(&out).unwrap();
    assert_eq!(file.text.as_deref(), Some("fn main() {}"));

    for (path, want) in [
        ("--path=../etc/passwd", EXIT_BAD_PATH),
        ("--path=.git/HEAD", EXIT_BAD_PATH),
        ("--path=nope.rs", EXIT_NOT_FOUND),
        ("--path=--help", EXIT_NOT_FOUND),
    ] {
        let (code, out, err) = guest(&["file", "--root", root, path]).await;
        assert_eq!(code, want, "{path}: {err}");
        assert!(out.is_empty() && !err.trim().is_empty(), "{path}: errors go to stderr only");
    }
}
