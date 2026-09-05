//! Running one build tool: argv in, bounded output out, the whole process group killed at the
//! deadline (or as soon as the tool exits, so stragglers cannot hold its pipes open).

use std::ffi::OsString;
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use parking_lot::Mutex;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::time::Instant;

use crate::{BuildError, SERVICE_TOKEN_ENV};

/// Output lines longer than this are split, so one runaway line cannot grow memory.
const MAX_LINE: usize = 16 * 1024;
/// Per-stream tail kept for parsing (`git rev-parse`, pack's digest line).
const SCAN_LIMIT: usize = 64 * 1024;
/// Between `SIGTERM` and `SIGKILL` when a step runs out of time, so tools can cancel remote work.
const KILL_GRACE: Duration = Duration::from_secs(2);
/// Stands in for "no limit" when `now + limit` would overflow.
const FAR_FUTURE: Duration = Duration::from_secs(100 * 365 * 24 * 3600);

/// When the current build must be done by, and the limit it came from (for the error).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Deadline {
    at: Instant,
    limit: Duration,
}

impl Deadline {
    pub(crate) fn after(limit: Duration) -> Self {
        let now = Instant::now();
        Self { at: now.checked_add(limit).unwrap_or(now + FAR_FUTURE), limit }
    }

    pub(crate) fn exceeded(self) -> BuildError {
        BuildError::TimedOut(self.limit)
    }

    /// Runs `work` until the deadline; dropping it there.
    pub(crate) async fn within<F: Future>(self, work: F) -> Result<F::Output, BuildError> {
        tokio::time::timeout_at(self.at, work).await.map_err(|_| self.exceeded())
    }
}

/// The last `limit` bytes pushed, plus how many were dropped in front of them.
pub(crate) struct Tail {
    limit: usize,
    buf: Vec<u8>,
    dropped: u64,
}

impl Tail {
    pub(crate) fn new(limit: usize) -> Self {
        Self { limit, buf: Vec::new(), dropped: 0 }
    }

    fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
        // Trim in batches: the buffer may reach twice the limit, never more.
        if self.buf.len() > self.limit.saturating_mul(2) {
            self.trim();
        }
    }

    fn trim(&mut self) {
        if let Some(cut) = self.buf.len().checked_sub(self.limit).filter(|&c| c > 0) {
            self.buf.drain(..cut);
            self.dropped += cut as u64;
        }
    }

    fn into_string(mut self) -> String {
        self.trim();
        let body = String::from_utf8_lossy(&self.buf);
        if self.dropped == 0 { body.into_owned() } else { format!("[… {} bytes of earlier output dropped …]\n{body}", self.dropped) }
    }
}

/// One build's log: our own notes and every tool's output lines, bounded to the tail.
pub(crate) struct Log(Mutex<Tail>);

impl Log {
    pub(crate) fn new(limit: usize) -> Self {
        Self(Mutex::new(Tail::new(limit)))
    }

    pub(crate) fn note(&self, message: impl AsRef<str>) {
        self.push(format!("==> {}\n", message.as_ref()).as_bytes());
    }

    fn push(&self, bytes: &[u8]) {
        self.0.lock().push(bytes);
    }

    pub(crate) fn finish(self) -> String {
        self.0.into_inner().into_string()
    }
}

/// What a successful step printed (each stream's tail, for parsing; everything also went to the log).
pub(crate) struct Output {
    pub(crate) stdout: String,
    pub(crate) stderr: String,
}

/// Runs `program args` in `cwd` in its own process group, stdin closed, output into `log`.
/// The service token is removed from the tool's environment.
pub(crate) async fn run(program: &Path, args: &[OsString], cwd: &Path, deadline: Deadline, log: &Log) -> Result<Output, BuildError> {
    let name = program.display().to_string();
    let shown: Vec<_> = args.iter().map(|a| a.to_string_lossy()).collect();
    log.note(format!("$ {name} {}", shown.join(" ")));
    let mut child = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .env_remove(SERVICE_TOKEN_ENV)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true)
        .spawn()
        .map_err(|source| BuildError::Spawn { program: name.clone(), source })?;
    let group = Group::of(child.id());
    let (stdout, stderr) = (child.stdout.take(), child.stderr.take());
    let (mut out, mut err) = (Tail::new(SCAN_LIMIT), Tail::new(SCAN_LIMIT));

    let work = async {
        let wait = async {
            let status = child.wait().await;
            group.reap();
            status
        };
        let (status, (), ()) = tokio::join!(wait, pump(stdout, log, &mut out), pump(stderr, log, &mut err));
        status
    };
    let finished = tokio::time::timeout_at(deadline.at, work).await;
    let status = match finished {
        Ok(status) => status?,
        Err(_) => {
            group.signal(Signal::SIGTERM);
            if tokio::time::timeout(KILL_GRACE, child.wait()).await.is_err() {
                group.signal(Signal::SIGKILL);
                // Reaps the leader; SIGKILL cannot be ignored, so this returns promptly.
                let _ = child.wait().await;
            }
            group.reap();
            log.note(format!("{name} killed: the build ran out of time"));
            return Err(deadline.exceeded());
        }
    };
    if !status.success() {
        log.note(format!("{name} exited with {status}"));
        return Err(BuildError::StepFailed { program: name, status: status.to_string() });
    }
    Ok(Output { stdout: out.into_string(), stderr: err.into_string() })
}

/// Copies `reader` into `log` and `tail` line by line (lines capped at [`MAX_LINE`]).
async fn pump<R: AsyncRead + Unpin>(reader: Option<R>, log: &Log, tail: &mut Tail) {
    let Some(mut reader) = reader else { return };
    let mut chunk = vec![0u8; 8192];
    let mut line = Vec::with_capacity(256);
    let mut emit = |line: &mut Vec<u8>| {
        if line.last() != Some(&b'\n') {
            line.push(b'\n');
        }
        log.push(line);
        tail.push(line);
        line.clear();
    };
    loop {
        let n = match reader.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => {
                log.note(format!("reading tool output: {e}"));
                break;
            }
        };
        let mut rest = &chunk[..n];
        while !rest.is_empty() {
            let room = MAX_LINE - line.len();
            let window = &rest[..rest.len().min(room)];
            let take = window.iter().position(|&b| b == b'\n').map_or(window.len(), |i| i + 1);
            line.extend_from_slice(&rest[..take]);
            rest = &rest[take..];
            if line.last() == Some(&b'\n') || line.len() >= MAX_LINE {
                emit(&mut line);
            }
        }
    }
    if !line.is_empty() {
        emit(&mut line);
    }
}

/// The step's process group. Signalled on timeout, killed once the leader exits (stragglers),
/// and killed on drop if the step is abandoned midway (e.g. the HTTP caller went away).
struct Group {
    pgid: Option<Pid>,
    live: AtomicBool,
}

impl Group {
    fn of(pid: Option<u32>) -> Self {
        let pgid = pid.and_then(|p| i32::try_from(p).ok()).map(Pid::from_raw);
        Self { pgid, live: AtomicBool::new(true) }
    }

    fn signal(&self, signal: Signal) {
        if self.live.load(Ordering::Acquire)
            && let Some(pgid) = self.pgid
        {
            // ESRCH (group already gone) is the expected outcome after a clean exit.
            let _ = killpg(pgid, signal);
        }
    }

    /// Kills whatever is left of the group and forgets it, so its id is never signalled again.
    fn reap(&self) {
        self.signal(Signal::SIGKILL);
        self.live.store(false, Ordering::Release);
    }
}

impl Drop for Group {
    fn drop(&mut self) {
        self.signal(Signal::SIGKILL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_keeps_the_end_and_counts_what_it_dropped() {
        let mut t = Tail::new(10);
        for i in 0..100 {
            t.push(format!("{i:02}\n").as_bytes());
        }
        assert_eq!(t.into_string(), "[… 290 bytes of earlier output dropped …]\n\n97\n98\n99\n");

        let mut t = Tail::new(10);
        t.push(b"short\n");
        assert_eq!(t.into_string(), "short\n");
    }

    #[tokio::test]
    async fn pump_splits_lines_and_caps_runaway_ones() {
        let log = Log::new(1 << 20);
        let mut tail = Tail::new(1 << 20);
        let mut input = b"one\ntwo\n".to_vec();
        input.extend(std::iter::repeat_n(b'x', MAX_LINE + 5));
        pump(Some(&input[..]), &log, &mut tail).await;
        let s = tail.into_string();
        let lines: Vec<&str> = s.lines().collect();
        assert_eq!(lines.len(), 4);
        assert_eq!(&lines[..2], ["one", "two"]);
        assert_eq!(lines[2].len(), MAX_LINE);
        assert_eq!(lines[3], "xxxxx");
        assert_eq!(log.finish(), s);
    }
}
