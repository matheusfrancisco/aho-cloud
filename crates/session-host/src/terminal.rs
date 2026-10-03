//! Sandbox terminals: single-use grants and the browser ↔ runtime agent frame relay.
//!
//! The control plane (service token) asks for a terminal with a [`cloud_api::internal::SandboxTerminal`];
//! the request is kept under a fresh nonce and a token naming that nonce, the sandbox, the user and
//! the request's digest is returned. The browser opens `GET /terminal/{sandbox}?token=` within
//! the token's lifetime; the first valid presentation consumes the grant. The relay then opens
//! the runtime agent's exec socket with the granted request and forwards binary
//! [`cloud_api::exec::Frame`]s both ways:
//!
//! * client → runtime: only `Stdin`, `Resize` and `CloseStdin`. A `Request` (the request is
//!   fixed by the grant) or any server-kind frame closes the socket with 1008, a frame over
//!   [`MAX_FRAME`] with 1009, a text message with 1003.
//! * runtime → client: `Stdout`/`Stderr` as they come; `Exit` or `Error` is forwarded and the
//!   socket closed with 1000. A runtime socket that ends without either sends the client an
//!   `Error` frame and closes with 1011.
//! * The client closing (or vanishing) closes the runtime side, which ends the process. Idle
//!   connections close with 1001 after the host's idle timeout.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, close_code};
use cloud_api::exec::{ExecRequest, Frame, FrameError, MAX_FRAME};
use futures::{SinkExt, StreamExt};
use tokio::time::{Instant, sleep_until};
use tokio_tungstenite::tungstenite::Message as RuntimeMessage;

use crate::bridge::{Farewell, say_farewell};
use crate::limits::{Denied, Limits, Permit};
use crate::runtime::RuntimeClient;
use crate::token::TerminalClaims;

/// Longest lifetime of a terminal token; [`crate::server::SandboxConfig::terminal_token_ttl`]
/// may only shorten it.
pub const TERMINAL_TOKEN_TTL: Duration = Duration::from_secs(60);
/// Most grants waiting to be redeemed; beyond it new terminals are refused until some expire.
const MAX_PENDING: usize = 4096;

/// One terminal the control plane asked for, waiting for its socket.
#[derive(Clone, Debug)]
pub(crate) struct Grant {
    pub sandbox: String,
    pub user: String,
    pub exec: ExecRequest,
    /// Unix seconds, as in the token.
    pub exp: u64,
}

/// Hex BLAKE3 of the request's JSON (`env` is a `BTreeMap`, so the encoding is canonical).
pub(crate) fn digest(exec: &ExecRequest) -> String {
    blake3::hash(&serde_json::to_vec(exec).unwrap_or_default()).to_hex().to_string()
}

/// Pending grants by nonce.
#[derive(Debug, Default)]
pub(crate) struct Grants {
    pending: Mutex<HashMap<String, Grant>>,
}

impl Grants {
    /// Stores `grant` under a fresh nonce, dropping expired ones first; `None` when full.
    pub(crate) fn issue(&self, grant: Grant, now: u64) -> Option<String> {
        let mut pending = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
        pending.retain(|_, g| g.exp > now);
        if pending.len() >= MAX_PENDING {
            return None;
        }
        let nonce = uuid::Uuid::new_v4().simple().to_string();
        pending.insert(nonce.clone(), grant);
        Some(nonce)
    }

    /// Consumes the grant `claims` names (verified claims only). `None` if it was already used
    /// or does not match the claims.
    pub(crate) fn redeem(&self, claims: &TerminalClaims) -> Option<Grant> {
        let grant = self.pending.lock().unwrap_or_else(PoisonError::into_inner).remove(&claims.nonce)?;
        (grant.sandbox == claims.sandbox && grant.user == claims.user && digest(&grant.exec) == claims.request).then_some(grant)
    }
}

/// The rule for one binary message from the browser: `Err((close code, reason))` ends the socket.
pub(crate) fn check_client_frame(bytes: &[u8]) -> Result<(), (u16, String)> {
    match Frame::decode(bytes) {
        Ok(Frame::Stdin(_) | Frame::Resize(_) | Frame::CloseStdin) => Ok(()),
        Ok(Frame::Request(_)) => Err((close_code::POLICY, "the terminal's command is fixed; request frames are refused".into())),
        Ok(_) => Err((close_code::POLICY, "only stdin, resize and close-stdin frames are accepted".into())),
        Err(e @ FrameError::TooLarge(_)) => Err((close_code::SIZE, e.to_string())),
        Err(e) => Err((close_code::POLICY, e.to_string())),
    }
}

fn close(code: u16, reason: impl Into<String>) -> Farewell {
    Farewell::Close { error: None, code, reason: reason.into() }
}

/// Closes a connection the limits refused: an `Error` frame, then 1013.
pub(crate) async fn refuse(mut socket: WebSocket, denied: Denied) {
    let message = denied.to_string();
    let _ = socket.send(Message::Binary(Frame::Error(message.clone()).encode().into())).await;
    let (sink, stream) = socket.split();
    say_farewell(sink, Some(stream), close(close_code::AGAIN, message)).await;
}

/// How a relay ended.
#[derive(Debug)]
enum End {
    /// The runtime sent `Exit` or `Error` (already forwarded).
    Finished,
    RuntimeGone,
    ClientClosed,
    ClientGone,
    Violation(u16, String),
    Idle,
}

/// Runs one terminal to completion. `permit` is held until both sides are closed.
pub(crate) async fn relay(socket: WebSocket, runtime: Arc<RuntimeClient>, grant: Grant, limits: Limits, permit: Permit) {
    let (mut client_sink, mut client) = socket.split();
    let upstream = match runtime.exec(&grant.sandbox, &grant.exec).await {
        Ok(upstream) => upstream,
        Err(e) => {
            tracing::warn!(sandbox = %grant.sandbox, user = %grant.user, "terminal: {e}");
            let message = "could not open a session in the sandbox";
            let _ = client_sink.send(Message::Binary(Frame::Error(message.into()).encode().into())).await;
            drop(permit);
            say_farewell(client_sink, Some(client), close(close_code::ERROR, message)).await;
            return;
        }
    };
    tracing::info!(sandbox = %grant.sandbox, user = %grant.user, argv = ?grant.exec.argv, "terminal opened");
    let (mut runtime_sink, mut runtime_stream) = upstream.split();
    let mut last = Instant::now();

    let end = loop {
        tokio::select! {
            msg = client.next() => {
                last = Instant::now();
                match msg {
                    None | Some(Err(_)) => break End::ClientGone,
                    Some(Ok(Message::Binary(bytes))) => {
                        if let Err((code, reason)) = check_client_frame(&bytes) {
                            break End::Violation(code, reason);
                        }
                        if runtime_sink.send(RuntimeMessage::Binary(bytes)).await.is_err() {
                            break End::RuntimeGone;
                        }
                    }
                    Some(Ok(Message::Text(_))) => {
                        break End::Violation(close_code::UNSUPPORTED, "text messages are not accepted; send binary exec frames".into());
                    }
                    Some(Ok(Message::Close(_))) => break End::ClientClosed,
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                }
            }
            msg = runtime_stream.next() => {
                last = Instant::now();
                match msg {
                    Some(Ok(RuntimeMessage::Binary(bytes))) => {
                        let last_frame = match Frame::decode(&bytes) {
                            Ok(Frame::Stdout(_) | Frame::Stderr(_)) => false,
                            Ok(Frame::Exit(_) | Frame::Error(_)) => true,
                            Ok(other) => {
                                tracing::warn!(sandbox = %grant.sandbox, "terminal: runtime sent a client frame {other:?}");
                                break End::RuntimeGone;
                            }
                            Err(e) => {
                                tracing::warn!(sandbox = %grant.sandbox, "terminal: runtime sent a bad frame: {e}");
                                break End::RuntimeGone;
                            }
                        };
                        if client_sink.send(Message::Binary(bytes)).await.is_err() {
                            break End::ClientGone;
                        }
                        if last_frame {
                            break End::Finished;
                        }
                    }
                    Some(Ok(RuntimeMessage::Close(_))) | None | Some(Err(_)) => break End::RuntimeGone,
                    Some(Ok(_)) => {}
                }
            }
            () = sleep_until(last + limits.idle_timeout) => break End::Idle,
        }
    };

    let _ = runtime_sink.send(RuntimeMessage::Close(None)).await;
    drop((runtime_sink, runtime_stream));
    drop(permit);
    tracing::info!(sandbox = %grant.sandbox, user = %grant.user, ?end, "terminal closed");
    let farewell = match end {
        End::Finished => close(close_code::NORMAL, "session ended"),
        End::RuntimeGone => {
            let message = "the sandbox session ended unexpectedly";
            let _ = client_sink.send(Message::Binary(Frame::Error(message.into()).encode().into())).await;
            close(close_code::ERROR, message)
        }
        End::ClientClosed => Farewell::Echo,
        End::ClientGone => Farewell::Silent,
        End::Violation(code, reason) => close(code, reason),
        End::Idle => close(close_code::AWAY, "idle timeout"),
    };
    say_farewell(client_sink, Some(client), farewell).await;
}

/// The transport limit for terminal sockets: frames up to twice [`MAX_FRAME`] still reach
/// [`check_client_frame`] and get a clean 1009.
pub(crate) const TRANSPORT_MAX: usize = MAX_FRAME * 2;

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use cloud_api::exec::PtySize;

    use super::*;

    fn exec(argv: &[&str]) -> ExecRequest {
        ExecRequest { argv: argv.iter().map(|s| (*s).to_owned()).collect(), pty: None, env: BTreeMap::new(), cwd: None }
    }

    fn claims_for(grants: &Grants, sandbox: &str, user: &str, exec: &ExecRequest) -> TerminalClaims {
        let grant = Grant { sandbox: sandbox.into(), user: user.into(), exec: exec.clone(), exp: 100 };
        let nonce = grants.issue(grant, 1).unwrap();
        TerminalClaims { sandbox: sandbox.into(), user: user.into(), nonce, request: digest(exec), exp: 100 }
    }

    #[test]
    fn a_grant_is_redeemed_once_and_only_with_matching_claims() {
        let grants = Grants::default();
        let bash = exec(&["bash", "-l"]);
        let claims = claims_for(&grants, "box", "alice", &bash);
        assert_eq!(grants.redeem(&claims).map(|g| g.exec), Some(bash.clone()));
        assert!(grants.redeem(&claims).is_none(), "second use");

        // Claims naming a pending nonce but another request: refused, and the grant is burnt.
        let claims = claims_for(&grants, "box", "alice", &bash);
        let other = TerminalClaims { request: digest(&exec(&["sh"])), ..claims.clone() };
        assert!(grants.redeem(&other).is_none());
        assert!(grants.redeem(&claims).is_none());
    }

    #[test]
    fn expired_grants_are_dropped_and_the_store_is_bounded() {
        let grants = Grants::default();
        let g = |exp| Grant { sandbox: "box".into(), user: "u".into(), exec: exec(&["sh"]), exp };
        for _ in 0..MAX_PENDING {
            grants.issue(g(10), 1).unwrap();
        }
        assert!(grants.issue(g(10), 1).is_none(), "full");
        assert!(grants.issue(g(20), 10).is_some(), "expired ones made room");
    }

    #[test]
    fn digests_bind_every_field_of_the_request() {
        let base = ExecRequest { argv: vec!["bash".into()], pty: Some(PtySize { cols: 80, rows: 24 }), env: BTreeMap::new(), cwd: None };
        let variants = [
            ExecRequest { argv: vec!["sh".into()], ..base.clone() },
            ExecRequest { pty: Some(PtySize { cols: 81, rows: 24 }), ..base.clone() },
            ExecRequest { env: BTreeMap::from([("A".into(), "1".into())]), ..base.clone() },
            ExecRequest { cwd: Some("/".into()), ..base.clone() },
        ];
        for v in variants {
            assert_ne!(digest(&v), digest(&base), "{v:?}");
        }
    }

    #[test]
    fn only_input_frames_pass_from_the_browser() {
        for ok in [Frame::Stdin(b"ls\r".to_vec()), Frame::Resize(PtySize { cols: 1, rows: 1 }), Frame::CloseStdin] {
            assert_eq!(check_client_frame(&ok.encode()), Ok(()), "{ok:?}");
        }
        for bad in [Frame::Request(exec(&["sh"])), Frame::Stdout(vec![1]), Frame::Stderr(vec![]), Frame::Exit(0), Frame::Error("x".into())]
        {
            assert_eq!(check_client_frame(&bad.encode()).unwrap_err().0, close_code::POLICY, "{bad:?}");
        }
        assert_eq!(check_client_frame(&[3, 0]).unwrap_err().0, close_code::POLICY, "malformed resize");
        assert_eq!(check_client_frame(&[]).unwrap_err().0, close_code::POLICY, "empty");
        let mut huge = vec![0u8; MAX_FRAME + 1];
        huge[0] = cloud_api::exec::KIND_STDIN;
        assert_eq!(check_client_frame(&huge).unwrap_err().0, close_code::SIZE);
    }
}
