//! Env log files: append with rotation at 64 MiB (two files kept, `<name>` and `<name>.1`) and
//! a logical byte offset that survives rotation (`LogCursor`); `logs` queries with tail, grep,
//! and since (executor.md "Output capture", Run MCP `logs`).

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;

use crate::ports::executor::{ExecError, LogCursor, LogQuery, LogSlice};

/// Rotation threshold.
pub(crate) const ROTATE_BYTES: u64 = 64 * 1024 * 1024;
/// `logs` scans at most this many trailing bytes.
pub(crate) const SCAN_BYTES: u64 = 16 * 1024 * 1024;

fn sidecar(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".base");
    PathBuf::from(s)
}

fn rotated(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".1");
    PathBuf::from(s)
}

/// Logical offset of the first byte of the current file.
fn read_base(path: &Path) -> u64 {
    std::fs::read_to_string(sidecar(path)).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0)
}

struct Inner {
    path: PathBuf,
    file: Option<File>,
    size: u64,
    base: u64,
    rotate_at: u64,
}

/// Shared appender for one log file (stdout and stderr tasks write through the same handle, so
/// chunks keep their arrival order).
#[derive(Clone)]
pub(crate) struct LogWriter(Arc<Mutex<Inner>>);

impl LogWriter {
    pub(crate) fn open(path: &Path) -> Result<Self, ExecError> {
        Self::open_with(path, ROTATE_BYTES)
    }

    fn open_with(path: &Path, rotate_at: u64) -> Result<Self, ExecError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(path).map_err(|e| ExecError::Io(format!("open log {}: {e}", path.display())))?;
        let size = file.metadata().map(|m| m.len()).unwrap_or(0);
        Ok(Self(Arc::new(Mutex::new(Inner { path: path.to_owned(), file: Some(file), size, base: read_base(path), rotate_at }))))
    }

    /// Appends; I/O errors are logged and dropped (log output must never fail a process).
    pub(crate) fn write(&self, data: &[u8]) {
        let mut inner = self.0.lock();
        if inner.size > 0 && inner.size + data.len() as u64 > inner.rotate_at {
            let new_base = inner.base + inner.size;
            inner.file = None;
            let rotated_path = rotated(&inner.path);
            if let Err(e) = std::fs::rename(&inner.path, &rotated_path) {
                tracing::warn!(error = %e, "log rotation failed");
            }
            let _ = std::fs::write(sidecar(&inner.path), new_base.to_string());
            inner.base = new_base;
            inner.size = 0;
            inner.file = OpenOptions::new().create(true).append(true).open(&inner.path).ok();
        }
        if let Some(file) = inner.file.as_mut() {
            if let Err(e) = file.write_all(data) {
                tracing::warn!(error = %e, "log write failed");
                return;
            }
        }
        inner.size += data.len() as u64;
    }

    /// Writes `line` plus a newline.
    pub(crate) fn line(&self, line: &str) {
        let mut buf = Vec::with_capacity(line.len() + 1);
        buf.extend_from_slice(line.as_bytes());
        buf.push(b'\n');
        self.write(&buf);
    }
}

/// Reads `[from, end)` logical bytes of a log (current file plus `.1`); returns
/// `(bytes, first logical offset returned, end)`.
fn read_window(path: &Path, from: u64) -> Result<(Vec<u8>, u64, u64), ExecError> {
    let base = read_base(path);
    let cur_len = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let end = base + cur_len;
    let rot = rotated(path);
    let rot_len = std::fs::metadata(&rot).map(|m| m.len()).unwrap_or(0);
    let oldest = base.saturating_sub(rot_len);
    let start = from.max(oldest).min(end);
    let mut out = Vec::with_capacity((end - start) as usize);
    if start < base && rot_len > 0 {
        let mut f = File::open(&rot)?;
        f.seek(SeekFrom::Start(start - oldest))?;
        f.take(base - start).read_to_end(&mut out)?;
    }
    if cur_len > 0 {
        let mut f = File::open(path)?;
        f.seek(SeekFrom::Start(start.saturating_sub(base)))?;
        f.take(end - start.max(base)).read_to_end(&mut out)?;
    }
    Ok((out, start, end))
}

/// Run MCP `logs`: lines appended at or after `since` (default: the last 16 MiB), filtered by
/// `grep`, keeping the last `tail` matches. A missing file is an empty log.
pub(crate) fn query(path: &Path, q: &LogQuery) -> Result<LogSlice, ExecError> {
    let re = q
        .grep
        .as_deref()
        .map(|g| regex::RegexBuilder::new(g).size_limit(1 << 20).build())
        .transpose()
        .map_err(|e| ExecError::Invalid(format!("grep: {e}")))?;
    let base = read_base(path);
    let end = base + std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let floor = end.saturating_sub(SCAN_BYTES);
    let from = q.since.map_or(floor, |c| c.0.max(floor));
    let (bytes, start, end) = read_window(path, from)?;
    let mut text: &[u8] = &bytes;
    let mut window_cut = start > q.since.map_or(0, |c| c.0);
    // Starting mid-line (window cut): drop the partial first line.
    if window_cut && start > 0 {
        if let Some(i) = text.iter().position(|&b| b == b'\n') {
            text = &text[i + 1..];
        } else {
            text = &[];
        }
    }
    let mut matches: Vec<String> = Vec::new();
    for raw in text.split(|&b| b == b'\n') {
        let line = String::from_utf8_lossy(raw);
        let line = line.strip_suffix('\r').unwrap_or(&line);
        if raw.is_empty() {
            continue;
        }
        if re.as_ref().is_none_or(|re| re.is_match(line)) {
            matches.push(line.to_owned());
        }
    }
    let matched = u32::try_from(matches.len()).unwrap_or(u32::MAX);
    let keep = (q.tail as usize).min(matches.len());
    let dropped = matches.len() - keep;
    window_cut |= dropped > 0;
    let lines = matches.split_off(dropped);
    Ok(LogSlice { lines, cursor: LogCursor(end), truncated: window_cut, matched })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ports::executor::LogSource;

    fn q(tail: u32, grep: Option<&str>, since: Option<u64>) -> LogQuery {
        LogQuery { source: LogSource::Prepare, tail, grep: grep.map(str::to_owned), since: since.map(LogCursor) }
    }

    #[test]
    fn tail_grep_and_cursor() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("svc/api.log");
        let w = LogWriter::open(&path).unwrap();
        for i in 0..10 {
            w.line(&format!("line {i} {}", if i % 2 == 0 { "even" } else { "odd" }));
        }
        let all = query(&path, &q(3, None, None)).unwrap();
        assert_eq!(all.lines, vec!["line 7 odd", "line 8 even", "line 9 odd"]);
        assert_eq!((all.matched, all.truncated), (10, true));
        let evens = query(&path, &q(100, Some("even$"), None)).unwrap();
        assert_eq!(evens.matched, 5);
        assert!(!evens.truncated);
        let cursor = all.cursor;
        w.line("after cursor");
        let since = query(&path, &q(100, None, Some(cursor.0))).unwrap();
        assert_eq!(since.lines, vec!["after cursor"]);
        assert!(query(&path, &q(1, Some("("), None)).is_err());
        assert_eq!(query(&dir.path().join("missing.log"), &q(5, None, None)).unwrap().lines.len(), 0);
    }

    #[test]
    fn cursor_survives_rotation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("exec/1.log");
        let w = LogWriter::open_with(&path, 32).unwrap();
        w.line("aaaaaaaaaaaaaaa");
        w.line("bbbbbbbbbbbbbbb");
        let before = query(&path, &q(10, None, None)).unwrap();
        assert_eq!(before.cursor, LogCursor(32));
        w.line("ccccccccccccccc");
        assert!(rotated(&path).exists());
        let after = query(&path, &q(10, None, Some(before.cursor.0))).unwrap();
        assert_eq!(after.lines, vec!["ccccccccccccccc"]);
        assert_eq!(after.cursor, LogCursor(48));
        // Both files are read for a full query.
        let full = query(&path, &q(10, None, None)).unwrap();
        assert_eq!(full.lines.len(), 3);
        // A reopened writer continues the logical offsets.
        drop(w);
        let w2 = LogWriter::open_with(&path, 32).unwrap();
        w2.line("ddd");
        assert_eq!(query(&path, &q(1, None, None)).unwrap().cursor, LogCursor(52));
    }
}
