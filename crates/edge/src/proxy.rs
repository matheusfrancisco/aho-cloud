//! Forwarding to a site's upstreams: round-robin, passive health (a replica that refused a
//! connection is tried last for a while), retry on another replica when the request can be
//! replayed, and WebSocket upgrade passthrough.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use cloud_api::internal::Site;
use http::{Request, Response, StatusCode, Uri, Version, request};
use http_body_util::BodyExt;
use hyper::body::{Bytes, Incoming};
use hyper::upgrade::OnUpgrade;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::{TokioExecutor, TokioIo};
use parking_lot::Mutex;

use crate::headers;
use crate::pages::{self, Body};

/// How long a replica that refused a connection goes to the back of the line.
const DOWN_FOR: Duration = Duration::from_secs(10);

/// The request body on its way upstream.
pub(crate) enum Outbound {
    Empty,
    Buffered(Bytes),
    Stream(Incoming),
}

pub(crate) struct Upstreams {
    client: Client<HttpConnector, Body>,
    next: Mutex<HashMap<String, usize>>,
    down: Mutex<HashMap<String, Instant>>,
}

impl Upstreams {
    pub fn new(connect_timeout: Duration) -> Self {
        let mut http = HttpConnector::new();
        http.set_connect_timeout(Some(connect_timeout));
        http.set_nodelay(true);
        let client = Client::builder(TokioExecutor::new()).pool_idle_timeout(Duration::from_secs(30)).build(http);
        Self { client, next: Mutex::new(HashMap::new()), down: Mutex::new(HashMap::new()) }
    }

    /// Upstreams in the order to try: round-robin start, recently failed ones last.
    fn order<'s>(&self, site: &'s Site) -> Vec<&'s str> {
        let n = site.upstreams.len();
        if n == 0 {
            return Vec::new();
        }
        let start = {
            let mut next = self.next.lock();
            let slot = next.entry(site.hostname.clone()).or_insert(0);
            let s = *slot % n;
            *slot = slot.wrapping_add(1);
            s
        };
        let now = Instant::now();
        let down = self.down.lock();
        let (mut up, dn): (Vec<&str>, Vec<&str>) =
            (0..n).map(|i| site.upstreams[(start + i) % n].as_str()).partition(|a| down.get(*a).is_none_or(|until| *until <= now));
        up.extend(dn);
        up
    }

    fn mark(&self, addr: &str, failed: bool) {
        let mut down = self.down.lock();
        if failed {
            down.insert(addr.to_owned(), Instant::now() + DOWN_FOR);
        } else {
            down.remove(addr);
        }
    }

    /// Sends the request to the first upstream that takes it. `upgrade` is the client side of
    /// an HTTP/1.1 upgrade; on `101` both sides are spliced together.
    pub async fn forward(&self, site: &Site, parts: request::Parts, body: Outbound, mut upgrade: Option<OnUpgrade>) -> Response<Body> {
        let order = self.order(site);
        if order.is_empty() {
            return pages::bad_gateway();
        }
        let path = parts.uri.path_and_query().map_or("/", |p| p.as_str());
        let (replay, mut stream) = match body {
            Outbound::Empty => (Some(Bytes::new()), None),
            Outbound::Buffered(b) => (Some(b), None),
            Outbound::Stream(s) => (None, Some(s)),
        };
        for addr in order {
            let Ok(uri) = Uri::try_from(format!("http://{addr}{path}")) else {
                tracing::warn!(site = %site.hostname, upstream = addr, "unusable upstream address");
                continue;
            };
            let body = match (&replay, stream.take()) {
                (Some(b), _) if b.is_empty() => pages::empty(),
                (Some(b), _) => pages::full(b.clone()),
                (None, Some(s)) => s.boxed(),
                (None, None) => break,
            };
            let mut req = Request::new(body);
            *req.method_mut() = parts.method.clone();
            *req.uri_mut() = uri;
            *req.version_mut() = Version::HTTP_11;
            *req.headers_mut() = parts.headers.clone();
            match self.client.request(req).await {
                Ok(mut resp) => {
                    self.mark(addr, false);
                    if resp.status() == StatusCode::SWITCHING_PROTOCOLS {
                        let Some(client_side) = upgrade.take() else {
                            tracing::warn!(site = %site.hostname, "upstream switched protocols unasked");
                            return pages::bad_gateway();
                        };
                        let upstream_side = hyper::upgrade::on(&mut resp);
                        tokio::spawn(tunnel(client_side, upstream_side));
                    } else {
                        headers::strip_response(resp.headers_mut());
                    }
                    return resp.map(BodyExt::boxed);
                }
                Err(e) if e.is_connect() => {
                    tracing::warn!(site = %site.hostname, upstream = addr, error = %e, "upstream refused");
                    self.mark(addr, true);
                    if replay.is_none() {
                        break;
                    }
                }
                Err(e) => {
                    tracing::warn!(site = %site.hostname, upstream = addr, error = %e, "upstream failed");
                    return pages::bad_gateway();
                }
            }
        }
        pages::bad_gateway()
    }
}

async fn tunnel(client: OnUpgrade, upstream: OnUpgrade) {
    let (client, upstream) = tokio::join!(client, upstream);
    let (Ok(client), Ok(upstream)) = (client, upstream) else {
        tracing::debug!("upgrade did not complete on both sides");
        return;
    };
    let mut client = TokioIo::new(client);
    let mut upstream = TokioIo::new(upstream);
    if let Err(e) = tokio::io::copy_bidirectional(&mut client, &mut upstream).await {
        tracing::debug!(error = %e, "upgraded connection closed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cloud_api::internal::SiteState;
    use cloud_contract::edge::EdgeConfig;
    use cloud_contract::ledger::Stage;
    use cloud_contract::manifest::Auth;

    fn site(upstreams: &[&str]) -> Site {
        Site {
            hostname: "a.test".into(),
            project: "a".into(),
            stage: Stage::Live,
            state: SiteState::Active,
            upstreams: upstreams.iter().map(|s| s.to_string()).collect(),
            edge: EdgeConfig::default(),
            auth: Auth::None,
            password_hash: None,
            oidc_client_secret: None,
            verifier_secrets: Default::default(),
            service_token_hashes: vec![],
            allowed_emails: vec![],
        }
    }

    #[tokio::test]
    async fn round_robin_and_failed_replicas_go_last() {
        let u = Upstreams::new(Duration::from_secs(1));
        let s = site(&["a:1", "b:1", "c:1"]);
        let firsts: Vec<&str> = (0..3).map(|_| u.order(&s)[0]).collect();
        assert_eq!(firsts, vec!["a:1", "b:1", "c:1"]);
        u.mark("a:1", true);
        assert_eq!(u.order(&s), vec!["b:1", "c:1", "a:1"]);
        assert_eq!(u.order(&s), vec!["b:1", "c:1", "a:1"]);
        u.mark("a:1", false);
        assert_eq!(u.order(&s), vec!["c:1", "a:1", "b:1"]);
        assert!(u.order(&site(&[])).is_empty());
    }
}
