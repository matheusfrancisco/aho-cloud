//! JSONL reader for harness stdout with the physical-line and total-output limits
//! (harness.md "Event pump": line ≤ 16 MiB, ≤ 512 MiB per session).

use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};

pub const DEFAULT_MAX_LINE: usize = 16 * 1024 * 1024;
pub const MAX_TOTAL_OUTPUT: u64 = 512 * 1024 * 1024;

#[derive(Debug, PartialEq, Eq)]
pub enum LineError {
    /// A physical line exceeded the current limit.
    Oversized { limit: usize },
    /// The session wrote more than [`MAX_TOTAL_OUTPUT`].
    OutputLimit,
    Io(String),
}

pub struct LineReader {
    inner: BufReader<Pin<Box<dyn AsyncRead + Send>>>,
    /// Shared so the protocol can tighten it (omp: `maxFrameBytes` + 1 KiB from `ready`).
    max_line: Arc<AtomicUsize>,
    max_total: u64,
    total: u64,
    buf: Vec<u8>,
}

impl LineReader {
    pub fn new(stream: Pin<Box<dyn AsyncRead + Send>>, max_line: Arc<AtomicUsize>, max_total: u64) -> Self {
        Self { inner: BufReader::with_capacity(64 * 1024, stream), max_line, max_total, total: 0, buf: Vec::new() }
    }

    /// Next non-empty line without the trailing `\n` (and `\r`). `Ok(None)` at EOF; a final line
    /// without newline is returned.
    pub async fn next_line(&mut self) -> Result<Option<Vec<u8>>, LineError> {
        loop {
            self.buf.clear();
            let limit = self.max_line.load(Ordering::Relaxed);
            loop {
                let available = self.inner.fill_buf().await.map_err(|e| LineError::Io(e.to_string()))?;
                if available.is_empty() {
                    if self.buf.is_empty() {
                        return Ok(None);
                    }
                    break;
                }
                let (take, done) = match available.iter().position(|b| *b == b'\n') {
                    Some(i) => (i + 1, true),
                    None => (available.len(), false),
                };
                self.total += take as u64;
                if self.total > self.max_total {
                    return Err(LineError::OutputLimit);
                }
                if self.buf.len() + take > limit + 1 {
                    return Err(LineError::Oversized { limit });
                }
                self.buf.extend_from_slice(&available[..take]);
                self.inner.consume(take);
                if done {
                    break;
                }
            }
            while matches!(self.buf.last(), Some(b'\n' | b'\r')) {
                self.buf.pop();
            }
            if !self.buf.iter().all(u8::is_ascii_whitespace) {
                return Ok(Some(std::mem::take(&mut self.buf)));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reader(data: &'static [u8], max_line: usize, max_total: u64) -> LineReader {
        LineReader::new(Box::pin(data), Arc::new(AtomicUsize::new(max_line)), max_total)
    }

    #[tokio::test]
    async fn splits_lines_and_skips_blank() {
        let mut r = reader(b"{\"a\":1}\n\n  \r\n{\"b\":2}\r\n{\"c\":3}", 100, 1000);
        assert_eq!(r.next_line().await.unwrap().unwrap(), b"{\"a\":1}");
        assert_eq!(r.next_line().await.unwrap().unwrap(), b"{\"b\":2}");
        assert_eq!(r.next_line().await.unwrap().unwrap(), b"{\"c\":3}");
        assert_eq!(r.next_line().await.unwrap(), None);
    }

    #[tokio::test]
    async fn oversized_physical_line_is_an_error() {
        let mut r = reader(b"0123456789\nabcdefghijklmnop\n", 10, 1000);
        assert_eq!(r.next_line().await.unwrap().unwrap(), b"0123456789");
        assert_eq!(r.next_line().await, Err(LineError::Oversized { limit: 10 }));
    }

    #[tokio::test]
    async fn total_output_cap() {
        let mut r = reader(b"aaaa\nbbbb\ncccc\n", 100, 12);
        assert!(r.next_line().await.unwrap().is_some());
        assert!(r.next_line().await.unwrap().is_some());
        assert_eq!(r.next_line().await, Err(LineError::OutputLimit));
    }
}
