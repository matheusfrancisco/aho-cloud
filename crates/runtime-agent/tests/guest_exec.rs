//! The guest exec server over an in-memory stream with real processes: PTY sessions (echo,
//! resize, close-stdin), pipe sessions (separate stdout/stderr, env, cwd), exit codes and
//! signals, refusals, frame limits, and what happens to the process when the host goes away.
// Helpers outside `#[test]` fns are test code too.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, Instant};

use cloud_api::exec::{ExecRequest, Frame, MAX_FRAME, PtySize};
use cloud_runtime::guest::exec::{ExecContext, SessionError, collect, read_frame, serve, write_frame};
use cloud_runtime::guest::process::{ReapMode, Reaper};
use nix::sys::signal::kill;
use nix::unistd::Pid;
use tokio::io::{AsyncWriteExt, DuplexStream};
use tokio::task::JoinHandle;

const WAIT: Duration = Duration::from_secs(10);

fn context(grace: Duration) -> ExecContext {
    let mut ctx = ExecContext::inherit(grace);
    ctx.env.insert("BASE".into(), "from-the-guest".into());
    ctx
}

/// The host end of a session served on a duplex stream.
fn session(ctx: ExecContext) -> (DuplexStream, JoinHandle<Result<(), SessionError>>) {
    let (host, guest) = tokio::io::duplex(1 << 21);
    let server = tokio::spawn(async move {
        let reaper = Reaper::start(ReapMode::Own).unwrap();
        serve(guest, &reaper, &ctx).await
    });
    (host, server)
}

fn request(argv: &[&str], pty: Option<(u16, u16)>) -> ExecRequest {
    ExecRequest {
        argv: argv.iter().map(|a| (*a).to_owned()).collect(),
        pty: pty.map(|(cols, rows)| PtySize { cols, rows }),
        env: BTreeMap::new(),
        cwd: None,
    }
}

fn sh(script: &str) -> Vec<&str> {
    vec!["sh", "-c", script]
}

/// Reads until stdout holds `needle`; everything read so far.
async fn read_until(host: &mut DuplexStream, needle: &str) -> String {
    let mut out = String::new();
    tokio::time::timeout(WAIT, async {
        while !out.contains(needle) {
            match read_frame(host).await.unwrap().unwrap() {
                Frame::Stdout(b) => out.push_str(&String::from_utf8_lossy(&b)),
                other => panic!("waiting for {needle:?} in {out:?}: {other:?}"),
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("no {needle:?} in {out:?}"));
    out
}

/// Reads to the end of the session: stdout, stderr, and the last frame.
async fn finish(host: &mut DuplexStream) -> (String, String, Frame) {
    let (mut out, mut err) = (String::new(), String::new());
    let last = tokio::time::timeout(WAIT, async {
        loop {
            match read_frame(host).await.unwrap().unwrap() {
                Frame::Stdout(b) => out.push_str(&String::from_utf8_lossy(&b)),
                Frame::Stderr(b) => err.push_str(&String::from_utf8_lossy(&b)),
                last => return last,
            }
        }
    })
    .await
    .unwrap();
    (out, err, last)
}

fn pid_in(path: &Path) -> Pid {
    Pid::from_raw(std::fs::read_to_string(path).unwrap().trim().parse().unwrap())
}

async fn gone(pid: Pid) -> bool {
    tokio::time::timeout(WAIT, async {
        while kill(pid, None).is_ok() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .is_ok()
}

#[tokio::test]
async fn a_terminal_session_echoes_through_cat_and_ends_on_close_stdin() {
    let (mut host, server) = session(context(Duration::from_secs(2)));
    write_frame(&mut host, &Frame::Request(request(&["cat"], Some((80, 24))))).await.unwrap();
    write_frame(&mut host, &Frame::Stdin(b"hello\n".to_vec())).await.unwrap();
    // The terminal echoes the line, then cat prints it.
    read_until(&mut host, "hello\r\nhello\r\n").await;
    write_frame(&mut host, &Frame::CloseStdin).await.unwrap();
    let (_, err, last) = finish(&mut host).await;
    assert_eq!((err.as_str(), last), ("", Frame::Exit(0)), "a terminal has no separate stderr");
    assert_eq!(read_frame(&mut host).await.unwrap(), None, "the session ends after Exit");
    server.await.unwrap().unwrap();
}

#[tokio::test]
async fn resizes_reach_the_terminal() {
    let (mut host, server) = session(context(Duration::from_secs(2)));
    let script = sh("[ -t 0 ] && echo tty; stty size; read line; stty size");
    write_frame(&mut host, &Frame::Request(request(&script, Some((80, 24))))).await.unwrap();
    read_until(&mut host, "tty\r\n24 80\r\n").await;
    write_frame(&mut host, &Frame::Resize(PtySize { cols: 132, rows: 50 })).await.unwrap();
    write_frame(&mut host, &Frame::Stdin(b"\n".to_vec())).await.unwrap();
    read_until(&mut host, "50 132\r\n").await;
    assert_eq!(finish(&mut host).await.2, Frame::Exit(0));
    server.await.unwrap().unwrap();
}

#[tokio::test]
async fn pipe_sessions_keep_stdout_and_stderr_apart_and_add_env_and_cwd() {
    let dir = tempfile::tempdir().unwrap();
    let (host, server) = session(context(Duration::from_secs(2)));
    let mut req = request(&sh("echo \"$BASE $ADDED\"; echo oops >&2; pwd"), None);
    req.env.insert("ADDED".into(), "by-the-request".into());
    req.cwd = Some(dir.path().display().to_string());
    let out = collect(host, req, 4096).await.unwrap();
    let cwd = dir.path().canonicalize().unwrap();
    assert_eq!(String::from_utf8(out.stdout).unwrap(), format!("from-the-guest by-the-request\n{}\n", cwd.display()));
    assert_eq!(String::from_utf8(out.stderr).unwrap(), "oops\n");
    assert_eq!(out.code, 0);
    server.await.unwrap().unwrap();
}

#[tokio::test]
async fn exit_codes_and_killing_signals_are_reported() {
    for (argv, pty, code) in [
        (sh("exit 7"), None, 7),
        (sh("kill -9 $$"), None, 128 + 9),
        (sh("exit 3"), Some((80, 24)), 3),
        (sh("kill -TERM $$"), Some((80, 24)), 128 + 15),
    ] {
        let (mut host, server) = session(context(Duration::from_secs(2)));
        write_frame(&mut host, &Frame::Request(request(&argv, pty))).await.unwrap();
        assert_eq!(finish(&mut host).await.2, Frame::Exit(code), "{argv:?} pty={pty:?}");
        server.await.unwrap().unwrap();
    }
}

#[tokio::test]
async fn what_cannot_run_is_answered_with_an_error() {
    let missing = tempfile::tempdir().unwrap().path().join("gone");
    let mut in_missing_dir = request(&["true"], None);
    in_missing_dir.cwd = Some(missing.display().to_string());
    for (req, starts) in [
        (request(&["/nonexistent/prog"], None), "/nonexistent/prog: "),
        (request(&["/nonexistent/prog"], Some((80, 24))), "/nonexistent/prog: "),
        (request(&[], None), "empty argv"),
        (in_missing_dir, "true: "),
    ] {
        let (mut host, server) = session(context(Duration::from_secs(2)));
        write_frame(&mut host, &Frame::Request(req)).await.unwrap();
        match read_frame(&mut host).await.unwrap() {
            Some(Frame::Error(message)) => assert!(message.starts_with(starts), "{message}"),
            other => panic!("{other:?}"),
        }
        assert_eq!(read_frame(&mut host).await.unwrap(), None);
        server.await.unwrap().unwrap();
    }

    // A session must open with its request.
    let (mut host, server) = session(context(Duration::from_secs(2)));
    write_frame(&mut host, &Frame::Stdin(b"ls\n".to_vec())).await.unwrap();
    assert!(matches!(read_frame(&mut host).await.unwrap(), Some(Frame::Error(m)) if m.contains("request")));
    assert!(matches!(server.await.unwrap(), Err(SessionError::Protocol(_))));
}

#[tokio::test]
async fn oversized_frames_are_refused_and_end_the_session() {
    // Before anything runs: only the length prefix is read.
    let (mut host, server) = session(context(Duration::from_secs(2)));
    host.write_all(&u32::try_from(MAX_FRAME + 1).unwrap().to_be_bytes()).await.unwrap();
    assert!(matches!(read_frame(&mut host).await.unwrap(), Some(Frame::Error(m)) if m.contains("limit")));
    assert!(server.await.unwrap().is_err());

    // Mid-session: the process goes too.
    let dir = tempfile::tempdir().unwrap();
    let pid_file = dir.path().join("pid");
    let (mut host, server) = session(context(Duration::from_millis(300)));
    let script = format!("echo $$ > '{}'; echo ready; exec sleep 30", pid_file.display());
    write_frame(&mut host, &Frame::Request(request(&sh(&script), None))).await.unwrap();
    read_until(&mut host, "ready").await;
    host.write_all(&u32::try_from(MAX_FRAME + 1).unwrap().to_be_bytes()).await.unwrap();
    assert!(matches!(read_frame(&mut host).await.unwrap(), Some(Frame::Error(m)) if m.contains("limit")));
    assert!(server.await.unwrap().is_err());
    assert!(gone(pid_in(&pid_file)).await);
}

#[tokio::test]
async fn the_host_going_away_hangs_up_the_whole_process_group() {
    let dir = tempfile::tempdir().unwrap();
    let (leader, child) = (dir.path().join("leader"), dir.path().join("child"));
    let (mut host, server) = session(context(Duration::from_secs(5)));
    let script = format!("echo $$ > '{}'; sleep 30 & echo $! > '{}'; echo ready; wait", leader.display(), child.display());
    write_frame(&mut host, &Frame::Request(request(&sh(&script), None))).await.unwrap();
    read_until(&mut host, "ready").await;
    let started = Instant::now();
    drop(host);
    server.await.unwrap().unwrap();
    assert!(started.elapsed() < Duration::from_secs(5), "SIGHUP alone ends it, no SIGKILL grace needed");
    assert!(gone(pid_in(&leader)).await, "the leader survived");
    assert!(gone(pid_in(&child)).await, "its background child survived");
}

#[tokio::test]
async fn a_process_ignoring_sighup_is_killed_after_the_grace() {
    let dir = tempfile::tempdir().unwrap();
    let pid_file = dir.path().join("pid");
    for pty in [None, Some((80, 24))] {
        let grace = Duration::from_millis(400);
        let (mut host, server) = session(context(grace));
        let script = format!("trap '' HUP; echo $$ > '{}'; echo ready; while :; do sleep 1; done", pid_file.display());
        write_frame(&mut host, &Frame::Request(request(&sh(&script), pty))).await.unwrap();
        read_until(&mut host, "ready").await;
        let started = Instant::now();
        drop(host);
        server.await.unwrap().unwrap();
        assert!(started.elapsed() >= grace, "SIGKILL came before the grace ran out (pty={pty:?})");
        assert!(gone(pid_in(&pid_file)).await, "pty={pty:?}");
    }
}

#[tokio::test]
async fn a_process_that_never_reads_its_stdin_cannot_stall_its_output() {
    let (host, server) = session(context(Duration::from_secs(2)));
    let (mut rx, mut tx) = tokio::io::split(host);
    write_frame(&mut tx, &Frame::Request(request(&sh("sleep 0.2; head -c 3000000 /dev/zero"), None))).await.unwrap();
    // Far more input than any pipe holds, sent while output is pending.
    let flood = tokio::spawn(async move {
        for _ in 0..64 {
            if write_frame(&mut tx, &Frame::Stdin(vec![b'x'; 64 * 1024])).await.is_err() {
                return;
            }
        }
    });
    let mut bytes = 0;
    let last = tokio::time::timeout(WAIT, async {
        loop {
            match read_frame(&mut rx).await.unwrap().unwrap() {
                Frame::Stdout(b) => bytes += b.len(),
                last => return last,
            }
        }
    })
    .await
    .expect("output stalled behind unread input");
    assert_eq!((bytes, last), (3_000_000, Frame::Exit(0)));
    server.await.unwrap().unwrap();
    flood.abort();
}

#[tokio::test]
async fn collect_bounds_what_it_keeps() {
    let (host, _server) = session(context(Duration::from_millis(200)));
    let err = collect(host, request(&sh("head -c 10000 /dev/zero"), None), 4096).await.unwrap_err();
    assert!(matches!(&err, SessionError::Protocol(m) if m.contains("4096")), "{err}");
}
