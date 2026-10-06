//! Stderr capture (harness.md "Stderr"): a 64 KiB ring for `HarnessExit.stderr_tail` and a
//! runner-local log file capped at 8 MiB, both redacted. Never parsed for control flow.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::task::JoinHandle;

use super::redact::{Redactor, StreamRedactor};

pub const RING_BYTES: usize = 64 * 1024;
pub const LOG_CAP_BYTES: u64 = 8 * 1024 * 1024;
const TRUNCATED_MARKER: &[u8] = b"\n[mage: stderr log truncated at 8 MiB]\n";

pub struct StderrCapture {
    ring: Arc<Mutex<VecDeque<u8>>>,
    log: Option<PathBuf>,
    task: Mutex<Option<JoinHandle<()>>>,
}

impl StderrCapture {
    /// Starts reading `stream`. `log` is created (truncated) up front; when that fails the
    /// capture keeps the ring only.
    pub async fn start(stream: Pin<Box<dyn AsyncRead + Send>>, redactor: Redactor, log: Option<PathBuf>) -> Self {
        let file = match &log {
            Some(path) => match tokio::fs::File::create(path).await {
                Ok(f) => Some(f),
                Err(e) => {
                    tracing::warn!(path = %path.display(), error = %e, "cannot create harness stderr log");
                    None
                }
            },
            None => None,
        };
        let log = file.as_ref().and(log);
        let ring = Arc::new(Mutex::new(VecDeque::with_capacity(RING_BYTES)));
        let task = tokio::spawn(read_loop(stream, StreamRedactor::new(redactor), ring.clone(), file));
        Self { ring, log, task: Mutex::new(Some(task)) }
    }

    /// Waits up to `max` for stderr EOF (a grandchild may keep the pipe open), then returns the
    /// redacted tail and the log path.
    pub async fn finish(&self, max: Duration) -> (String, Option<PathBuf>) {
        let task = self.task.lock().take();
        if let Some(mut task) = task {
            if tokio::time::timeout(max, &mut task).await.is_err() {
                task.abort();
            }
        }
        (self.tail(), self.log.clone())
    }

    /// Current redacted tail (lossy UTF-8).
    pub fn tail(&self) -> String {
        let ring = self.ring.lock();
        let (a, b) = ring.as_slices();
        let mut bytes = Vec::with_capacity(a.len() + b.len());
        bytes.extend_from_slice(a);
        bytes.extend_from_slice(b);
        // Drop a partial leading UTF-8 sequence cut by the ring.
        let start = bytes.iter().position(|b| (*b & 0xC0) != 0x80).unwrap_or(bytes.len());
        String::from_utf8_lossy(&bytes[start..]).into_owned()
    }
}

async fn read_loop(
    mut stream: Pin<Box<dyn AsyncRead + Send>>,
    mut redact: StreamRedactor,
    ring: Arc<Mutex<VecDeque<u8>>>,
    mut file: Option<tokio::fs::File>,
) {
    let mut buf = vec![0u8; 8192];
    let mut written: u64 = 0;
    loop {
        let n = match stream.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        let out = redact.push(&buf[..n]);
        sink(&out, &ring, &mut file, &mut written).await;
    }
    let rest = redact.finish();
    sink(&rest, &ring, &mut file, &mut written).await;
    if let Some(f) = file.as_mut() {
        let _ = f.flush().await;
    }
}

async fn sink(bytes: &[u8], ring: &Mutex<VecDeque<u8>>, file: &mut Option<tokio::fs::File>, written: &mut u64) {
    if bytes.is_empty() {
        return;
    }
    {
        let mut ring = ring.lock();
        ring.extend(bytes);
        let excess = ring.len().saturating_sub(RING_BYTES);
        ring.drain(..excess);
    }
    let Some(f) = file.as_mut() else { return };
    let room = LOG_CAP_BYTES.saturating_sub(*written) as usize;
    let take = bytes.len().min(room);
    let ok = f.write_all(&bytes[..take]).await.is_ok();
    *written += take as u64;
    if !ok || take < bytes.len() {
        if ok {
            let _ = f.write_all(TRUNCATED_MARKER).await;
        }
        let _ = f.flush().await;
        *file = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn ring_and_log_are_redacted_and_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("r.stderr.log");
        let secret = "tok_0123456789abcdef";
        let mut data = format!("start {secret}\n").into_bytes();
        data.extend(std::iter::repeat_n(b'z', 100_000));
        data.extend(format!("\nend {secret}\n").into_bytes());
        let (mut w, r) = tokio::io::duplex(1024);
        let cap = StderrCapture::start(Box::pin(r), Redactor::new([secret]), Some(log.clone())).await;
        // Write in odd-sized chunks so the secret straddles reads.
        for chunk in data.chunks(7) {
            w.write_all(chunk).await.unwrap();
        }
        drop(w);
        let (tail, path) = cap.finish(Duration::from_secs(5)).await;
        assert_eq!(path.as_deref(), Some(log.as_path()));
        assert!(tail.len() <= RING_BYTES);
        assert!(tail.ends_with("end [redacted]\n"), "{}", &tail[tail.len() - 40..]);
        let logged = std::fs::read_to_string(&log).unwrap();
        assert!(logged.starts_with("start [redacted]\n"));
        assert!(!logged.contains(secret));
    }
}
