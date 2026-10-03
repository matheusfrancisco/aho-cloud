//! The exec protocol: one command running inside a sandbox, with its stdio (or a PTY) carried as
//! frames. The same frames travel as WebSocket binary messages (browser ↔ session host ↔
//! runtime agent) and, length-prefixed, over byte streams (runtime agent ↔ guest).
//!
//! A session opens with one JSON [`ExecRequest`] (a WebSocket text message, or a length-prefixed
//! frame of kind [`KIND_REQUEST`] on a stream). Then:
//!
//! ```text
//! client → server   Stdin(bytes) · Resize{cols,rows} (PTY only) · CloseStdin
//! server → client   Stdout(bytes) · Stderr(bytes) (pipe only) · Exit(code) · Error(text)
//! ```
//!
//! Encoding: first byte is the kind, the rest the payload. `Resize` = cols u16 BE, rows u16 BE.
//! `Exit` = i32 BE (`128 + signal` when killed). `Error` = UTF-8 text; it ends the session.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use ts_rs::TS;

pub const KIND_STDIN: u8 = 0;
pub const KIND_STDOUT: u8 = 1;
pub const KIND_STDERR: u8 = 2;
pub const KIND_RESIZE: u8 = 3;
pub const KIND_CLOSE_STDIN: u8 = 4;
pub const KIND_EXIT: u8 = 5;
pub const KIND_ERROR: u8 = 6;
/// Only on byte streams: the JSON [`ExecRequest`] that opens the session.
pub const KIND_REQUEST: u8 = 7;

/// Largest frame (kind byte included) either side sends or accepts.
pub const MAX_FRAME: usize = 1 << 20;

/// What to run. `argv` is never interpreted by a shell unless it names one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ExecRequest {
    pub argv: Vec<String>,
    /// Allocate a terminal of this size; without it, stdout and stderr are separate pipes.
    #[serde(default)]
    pub pty: Option<PtySize>,
    /// Added to the sandbox's own environment.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub cwd: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct PtySize {
    pub cols: u16,
    pub rows: u16,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Frame {
    Stdin(Vec<u8>),
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    Resize(PtySize),
    CloseStdin,
    Exit(i32),
    Error(String),
    /// Byte streams only.
    Request(ExecRequest),
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum FrameError {
    #[error("empty frame")]
    Empty,
    #[error("frame is {0} bytes; the limit is {MAX_FRAME}")]
    TooLarge(usize),
    #[error("unknown frame kind {0}")]
    UnknownKind(u8),
    #[error("malformed {0} frame")]
    Malformed(&'static str),
}

impl Frame {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            Frame::Stdin(b) => push(&mut out, KIND_STDIN, b),
            Frame::Stdout(b) => push(&mut out, KIND_STDOUT, b),
            Frame::Stderr(b) => push(&mut out, KIND_STDERR, b),
            Frame::Resize(s) => {
                out.push(KIND_RESIZE);
                out.extend_from_slice(&s.cols.to_be_bytes());
                out.extend_from_slice(&s.rows.to_be_bytes());
            }
            Frame::CloseStdin => out.push(KIND_CLOSE_STDIN),
            Frame::Exit(code) => {
                out.push(KIND_EXIT);
                out.extend_from_slice(&code.to_be_bytes());
            }
            Frame::Error(text) => push(&mut out, KIND_ERROR, text.as_bytes()),
            Frame::Request(r) => push(&mut out, KIND_REQUEST, &serde_json::to_vec(r).unwrap_or_default()),
        }
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Frame, FrameError> {
        if bytes.len() > MAX_FRAME {
            return Err(FrameError::TooLarge(bytes.len()));
        }
        let (&kind, rest) = bytes.split_first().ok_or(FrameError::Empty)?;
        Ok(match kind {
            KIND_STDIN => Frame::Stdin(rest.to_vec()),
            KIND_STDOUT => Frame::Stdout(rest.to_vec()),
            KIND_STDERR => Frame::Stderr(rest.to_vec()),
            KIND_RESIZE => match rest {
                [c0, c1, r0, r1] => Frame::Resize(PtySize { cols: u16::from_be_bytes([*c0, *c1]), rows: u16::from_be_bytes([*r0, *r1]) }),
                _ => return Err(FrameError::Malformed("resize")),
            },
            KIND_CLOSE_STDIN if rest.is_empty() => Frame::CloseStdin,
            KIND_CLOSE_STDIN => return Err(FrameError::Malformed("close-stdin")),
            KIND_EXIT => match rest {
                [a, b, c, d] => Frame::Exit(i32::from_be_bytes([*a, *b, *c, *d])),
                _ => return Err(FrameError::Malformed("exit")),
            },
            KIND_ERROR => Frame::Error(String::from_utf8(rest.to_vec()).map_err(|_| FrameError::Malformed("error"))?),
            KIND_REQUEST => Frame::Request(serde_json::from_slice(rest).map_err(|_| FrameError::Malformed("request"))?),
            other => return Err(FrameError::UnknownKind(other)),
        })
    }

    /// `u32` big-endian length, then [`Self::encode`]: the form used on byte streams.
    pub fn encode_prefixed(&self) -> Vec<u8> {
        let body = self.encode();
        let mut out = Vec::with_capacity(body.len() + 4);
        out.extend_from_slice(&u32::try_from(body.len()).unwrap_or(u32::MAX).to_be_bytes());
        out.extend_from_slice(&body);
        out
    }

    /// The length a 4-byte prefix announces, refusing anything above [`MAX_FRAME`].
    pub fn prefixed_len(prefix: [u8; 4]) -> Result<usize, FrameError> {
        let len = u32::from_be_bytes(prefix) as usize;
        match len {
            0 => Err(FrameError::Empty),
            n if n > MAX_FRAME => Err(FrameError::TooLarge(n)),
            n => Ok(n),
        }
    }
}

fn push(out: &mut Vec<u8>, kind: u8, payload: &[u8]) {
    out.reserve(payload.len() + 1);
    out.push(kind);
    out.extend_from_slice(payload);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_frame_round_trips() {
        let frames = [
            Frame::Stdin(b"ls -la\r".to_vec()),
            Frame::Stdout(vec![0, 1, 2, 255]),
            Frame::Stderr(b"oops".to_vec()),
            Frame::Resize(PtySize { cols: 120, rows: 40 }),
            Frame::CloseStdin,
            Frame::Exit(-1),
            Frame::Error("no such sandbox".into()),
            Frame::Request(ExecRequest {
                argv: vec!["bash".into(), "-l".into()],
                pty: Some(PtySize { cols: 80, rows: 24 }),
                env: BTreeMap::from([("TERM".into(), "xterm-256color".into())]),
                cwd: Some("/workspace".into()),
            }),
        ];
        for f in frames {
            assert_eq!(Frame::decode(&f.encode()), Ok(f.clone()));
            let p = f.encode_prefixed();
            let len = Frame::prefixed_len([p[0], p[1], p[2], p[3]]).unwrap();
            assert_eq!(Frame::decode(&p[4..4 + len]), Ok(f));
        }
    }

    #[test]
    fn wire_bytes_are_fixed() {
        // Mirrored by the console (web/src/lib/exec.ts); changing these breaks the browser.
        assert_eq!(Frame::Resize(PtySize { cols: 0x0102, rows: 0x0304 }).encode(), [3, 1, 2, 3, 4]);
        assert_eq!(Frame::Exit(130).encode(), [5, 0, 0, 0, 130]);
        assert_eq!(Frame::Stdin(b"a".to_vec()).encode(), [0, b'a']);
    }

    #[test]
    fn malformed_and_oversized_frames_are_refused() {
        assert_eq!(Frame::decode(&[]), Err(FrameError::Empty));
        assert_eq!(Frame::decode(&[3, 0, 80]), Err(FrameError::Malformed("resize")));
        assert_eq!(Frame::decode(&[5, 0]), Err(FrameError::Malformed("exit")));
        assert_eq!(Frame::decode(&[4, 1]), Err(FrameError::Malformed("close-stdin")));
        assert_eq!(Frame::decode(&[6, 0xff]), Err(FrameError::Malformed("error")));
        assert_eq!(Frame::decode(&[9]), Err(FrameError::UnknownKind(9)));
        assert_eq!(Frame::decode(&vec![0; MAX_FRAME + 1]), Err(FrameError::TooLarge(MAX_FRAME + 1)));
        assert_eq!(Frame::prefixed_len(((MAX_FRAME + 1) as u32).to_be_bytes()), Err(FrameError::TooLarge(MAX_FRAME + 1)));
        assert_eq!(Frame::prefixed_len([0; 4]), Err(FrameError::Empty));
    }
}
