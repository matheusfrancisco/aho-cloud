//! Health probes: `GET path` against the replica, or a command run by `cloud-guest`.

use std::net::SocketAddr;
use std::time::Duration;

use http_body_util::Empty;
use hyper::body::Bytes;
use hyper_util::rt::TokioIo;
use tokio::net::TcpStream;

use crate::guest::control::Client;
use crate::vmm::GuestConn;

/// `GET path` on `addr` answers 2xx within `timeout`.
pub async fn http_ok(addr: SocketAddr, path: &str, timeout: Duration) -> bool {
    tokio::time::timeout(timeout, http_status(addr, path)).await.is_ok_and(|s| s.is_some_and(|s| s.is_success()))
}

async fn http_status(addr: SocketAddr, path: &str) -> Option<hyper::StatusCode> {
    let stream = TcpStream::connect(addr).await.ok()?;
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream)).await.ok()?;
    let driver = tokio::spawn(conn);
    let path = if path.starts_with('/') { path.to_owned() } else { format!("/{path}") };
    let request = hyper::Request::get(path)
        .header(hyper::header::HOST, addr.to_string())
        .header(hyper::header::USER_AGENT, "aho-cloud-health")
        .body(Empty::<Bytes>::new())
        .ok()?;
    let status = sender.send_request(request).await.ok().map(|r| r.status());
    driver.abort();
    status
}

/// `argv` run in the guest exits 0 within `timeout`.
pub async fn command_ok(conn: GuestConn, argv: &[String], timeout: Duration) -> bool {
    let probe = async move { Client::new(conn).health(argv, timeout).await };
    // The guest enforces `timeout` on the command; allow a little for the round trip.
    let limit = timeout + Duration::from_millis(500);
    matches!(tokio::time::timeout(limit, probe).await, Ok(Ok(0)))
}

#[cfg(test)]
mod tests {
    use axum::Router;
    use axum::http::StatusCode;
    use axum::routing::get;

    use super::*;

    async fn server() -> SocketAddr {
        let app = Router::new()
            .route("/healthz", get(|| async { "ok" }))
            .route("/sick", get(|| async { StatusCode::SERVICE_UNAVAILABLE }))
            .route(
                "/slow",
                get(|| async {
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    "late"
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await });
        addr
    }

    #[tokio::test]
    async fn http_probe_needs_2xx_in_time() {
        let addr = server().await;
        let t = Duration::from_secs(2);
        assert!(http_ok(addr, "/healthz", t).await);
        assert!(http_ok(addr, "healthz", t).await);
        assert!(!http_ok(addr, "/sick", t).await);
        assert!(!http_ok(addr, "/missing", t).await);
        assert!(!http_ok(addr, "/slow", Duration::from_millis(200)).await);
        let closed = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap();
        assert!(!http_ok(closed, "/healthz", t).await);
    }
}
