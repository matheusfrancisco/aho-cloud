//! Authoritative DNS for `NsDelegated` subzones, on `hickory-server`.
//!
//! Each zone answers SOA/NS/A/AAAA at its apex, A/AAAA for any single label below it (the
//! `*.zone` wildcard the certificate covers), the verification TXT at `_aho-cloud.<zone>` and
//! DNS-01 challenges at `_acme-challenge.<zone>`. Names two or more labels deep are NXDOMAIN;
//! names outside every zone are REFUSED.

use std::collections::BTreeMap;
use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use hickory_server::Server;
use hickory_server::net::runtime::Time;
use hickory_server::proto::op::{Edns, Header, HeaderCounts, MessageType, Metadata, OpCode, ResponseCode};
use hickory_server::proto::rr::rdata::{A, AAAA, NS, SOA, TXT};
use hickory_server::proto::rr::{Name, RData, Record, RecordType};
use hickory_server::server::{Request, RequestHandler, ResponseHandler, ResponseInfo};
use hickory_server::zone_handler::MessageResponseBuilder;
use parking_lot::RwLock;
use tokio::net::{TcpListener, UdpSocket};

use crate::registry::{TXT_LABEL, txt_value};
use crate::resolver::normalize_name;

/// First label of DNS-01 challenge records.
pub const ACME_CHALLENGE_LABEL: &str = "_acme-challenge";

const TTL_NS: u32 = 3600;
const TTL_ADDR: u32 = 60;
const TTL_TXT: u32 = 30;
const TTL_SOA: u32 = 300;

/// The DNS-01 record name for an order identifier (`*.x.y` and `x.y` share `_acme-challenge.x.y`).
pub fn acme_challenge_name(identifier: &str) -> String {
    let base = normalize_name(identifier);
    let base = base.strip_prefix("*.").unwrap_or(&base);
    format!("{ACME_CHALLENGE_LABEL}.{base}")
}

/// One delegated subzone, as derived by [`crate::Registry::zones`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Zone {
    pub apex: String,
    pub ipv4: Vec<Ipv4Addr>,
    pub ipv6: Vec<Ipv6Addr>,
    /// Our nameservers; the first is the SOA MNAME.
    pub nameservers: Vec<String>,
    /// Published as `_aho-cloud.<apex> TXT aho-cloud-verify=<token>`.
    pub verify_token: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum AuthorityError {
    #[error("`{0}` is not a valid DNS name")]
    Name(String),
    #[error("zone server I/O: {0}")]
    Io(#[from] io::Error),
    #[error("zone server: {0}")]
    Server(String),
}

#[derive(Debug)]
struct Compiled {
    zone: Zone,
    apex: Name,
    mname: Name,
    rname: Name,
    nameservers: Vec<Name>,
}

#[derive(Debug, Default)]
struct Inner {
    zones: BTreeMap<String, Compiled>,
    /// `_acme-challenge.<zone>` → TXT values.
    challenges: BTreeMap<String, Vec<String>>,
    serial: u32,
}

/// Shared, live-updatable zone data. Clones share state: hand one to [`ZoneServer::bind`] and
/// keep another for [`Zones::sync`] and the ACME issuer.
#[derive(Clone, Debug, Default)]
pub struct Zones {
    inner: Arc<RwLock<Inner>>,
}

/// Result of a lookup, before it is put on the wire.
#[derive(Debug)]
pub(crate) struct Found {
    pub(crate) code: ResponseCode,
    pub(crate) authoritative: bool,
    pub(crate) answers: Vec<Record>,
    pub(crate) soa: Option<Record>,
}

fn dns_name(name: &str) -> Result<Name, AuthorityError> {
    Name::from_ascii(format!("{}.", normalize_name(name))).map_err(|_| AuthorityError::Name(name.to_owned()))
}

impl Zones {
    /// No zones, no challenges.
    pub fn new() -> Self {
        Self::default()
    }

    /// Replaces every zone (challenges are kept). Validates all names first; on error nothing
    /// changes.
    pub fn sync(&self, zones: Vec<Zone>) -> Result<(), AuthorityError> {
        let mut compiled = BTreeMap::new();
        for mut zone in zones {
            zone.apex = normalize_name(&zone.apex);
            let apex = dns_name(&zone.apex)?;
            let nameservers = zone.nameservers.iter().map(|n| dns_name(n)).collect::<Result<Vec<_>, _>>()?;
            let mname = match nameservers.first() {
                Some(first) => first.clone(),
                None => dns_name(&format!("ns.{}", zone.apex))?,
            };
            let rname =
                Name::from_ascii("hostmaster").and_then(|h| h.append_domain(&apex)).map_err(|_| AuthorityError::Name(zone.apex.clone()))?;
            compiled.insert(zone.apex.clone(), Compiled { zone, apex, mname, rname, nameservers });
        }
        let mut inner = self.inner.write();
        inner.zones = compiled;
        inner.serial = inner.serial.wrapping_add(1);
        Ok(())
    }

    /// True when `name` is inside one of our zones.
    pub fn serves(&self, name: &str) -> bool {
        let inner = self.inner.read();
        zone_for(&inner.zones, &normalize_name(name)).is_some()
    }

    /// Publishes a DNS-01 value at `name` (see [`acme_challenge_name`]).
    pub fn add_challenge(&self, name: &str, value: &str) {
        let mut inner = self.inner.write();
        let values = inner.challenges.entry(normalize_name(name)).or_default();
        if !values.iter().any(|v| v == value) {
            values.push(value.to_owned());
        }
        inner.serial = inner.serial.wrapping_add(1);
    }

    /// Withdraws a DNS-01 value.
    pub fn remove_challenge(&self, name: &str, value: &str) {
        let mut inner = self.inner.write();
        let key = normalize_name(name);
        if let Some(values) = inner.challenges.get_mut(&key) {
            values.retain(|v| v != value);
            if values.is_empty() {
                inner.challenges.remove(&key);
            }
        }
        inner.serial = inner.serial.wrapping_add(1);
    }

    /// TXT values currently published at `name` by [`Zones::add_challenge`].
    pub fn challenges(&self, name: &str) -> Vec<String> {
        let inner = self.inner.read();
        inner.challenges.get(&normalize_name(name)).cloned().unwrap_or_default()
    }

    pub(crate) fn lookup(&self, qname: &Name, qtype: RecordType) -> Found {
        let name = normalize_name(&qname.to_ascii());
        let inner = self.inner.read();
        let Some(z) = zone_for(&inner.zones, &name) else {
            return Found { code: ResponseCode::Refused, authoritative: false, answers: Vec::new(), soa: None };
        };
        let soa = Record::from_rdata(
            z.apex.clone(),
            TTL_SOA,
            RData::SOA(SOA::new(z.mname.clone(), z.rname.clone(), inner.serial, 3600, 600, 86400, TTL_TXT)),
        );
        let owner = qname.clone();
        let addrs = |out: &mut Vec<Record>| {
            out.extend(z.zone.ipv4.iter().map(|ip| Record::from_rdata(owner.clone(), TTL_ADDR, RData::A(A(*ip)))));
            out.extend(z.zone.ipv6.iter().map(|ip| Record::from_rdata(owner.clone(), TTL_ADDR, RData::AAAA(AAAA(*ip)))));
        };
        let txts = |values: &[String]| -> Vec<Record> {
            values.iter().map(|v| Record::from_rdata(owner.clone(), TTL_TXT, RData::TXT(TXT::new(vec![v.clone()])))).collect()
        };

        let mut node = Vec::new();
        if name == z.zone.apex {
            node.push(soa.clone());
            node.extend(z.nameservers.iter().map(|ns| Record::from_rdata(owner.clone(), TTL_NS, RData::NS(NS(ns.clone())))));
            addrs(&mut node);
        } else {
            let label = name.strip_suffix(&format!(".{}", z.zone.apex)).unwrap_or(&name);
            if label.contains('.') {
                return Found { code: ResponseCode::NXDomain, authoritative: true, answers: Vec::new(), soa: Some(soa) };
            }
            // An explicit TXT node shadows the wildcard; otherwise the wildcard synthesizes A/AAAA.
            let explicit = match label {
                TXT_LABEL => z.zone.verify_token.as_deref().map(|t| txts(&[txt_value(t)])).unwrap_or_default(),
                ACME_CHALLENGE_LABEL => inner.challenges.get(&name).map(|v| txts(v)).unwrap_or_default(),
                _ => Vec::new(),
            };
            if explicit.is_empty() {
                addrs(&mut node);
            } else {
                node = explicit;
            }
            if node.is_empty() {
                return Found { code: ResponseCode::NXDomain, authoritative: true, answers: Vec::new(), soa: Some(soa) };
            }
        }
        let answers: Vec<Record> = node.into_iter().filter(|r| qtype == RecordType::ANY || r.record_type() == qtype).collect();
        let soa = answers.is_empty().then_some(soa);
        Found { code: ResponseCode::NoError, authoritative: true, answers, soa }
    }
}

/// Longest zone apex that `name` is equal to or below.
fn zone_for<'a>(zones: &'a BTreeMap<String, Compiled>, name: &str) -> Option<&'a Compiled> {
    let mut candidate = name;
    loop {
        if let Some(z) = zones.get(candidate) {
            return Some(z);
        }
        candidate = candidate.split_once('.')?.1;
    }
}

struct Handler {
    zones: Zones,
}

#[async_trait::async_trait]
impl RequestHandler for Handler {
    async fn handle_request<R: ResponseHandler, T: Time>(&self, request: &Request, mut response_handle: R) -> ResponseInfo {
        let edns = request.edns.as_ref().map(|req| {
            let mut e = Edns::new();
            e.set_max_payload(req.max_payload().max(512));
            e
        });
        let mut metadata = Metadata::response_from_request(&request.metadata);
        let builder = MessageResponseBuilder::new(&request.queries, edns.as_ref());
        let query = (request.metadata.message_type == MessageType::Query && request.metadata.op_code == OpCode::Query)
            .then(|| request.request_info().ok())
            .flatten();
        let found = match query {
            Some(info) => self.zones.lookup(info.query.original().name(), info.query.query_type()),
            None => {
                let code = if request.metadata.op_code == OpCode::Query { ResponseCode::FormErr } else { ResponseCode::NotImp };
                Found { code, authoritative: false, answers: Vec::new(), soa: None }
            }
        };
        metadata.response_code = found.code;
        metadata.authoritative = found.authoritative;
        let response = builder.build(metadata, found.answers.iter(), std::iter::empty(), found.soa.iter(), std::iter::empty());
        match response_handle.send_response(response).await {
            Ok(info) => info,
            Err(error) => {
                tracing::warn!(%error, "zone server failed to send a response");
                metadata.response_code = ResponseCode::ServFail;
                ResponseInfo::from(Header { metadata, counts: HeaderCounts::default() })
            }
        }
    }
}

/// A running authoritative server (UDP and TCP on the same port).
pub struct ZoneServer {
    server: Server<Handler>,
    addr: SocketAddr,
}

impl std::fmt::Debug for ZoneServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ZoneServer").field("addr", &self.addr).finish_non_exhaustive()
    }
}

impl ZoneServer {
    /// Binds UDP on `addr`, then TCP on the same port (port 0 picks a free UDP port first), and
    /// starts answering from `zones`.
    pub async fn bind(addr: SocketAddr, zones: Zones) -> Result<Self, AuthorityError> {
        let udp = UdpSocket::bind(addr).await?;
        let addr = udp.local_addr()?;
        let tcp = TcpListener::bind(addr).await?;
        let mut server = Server::new(Handler { zones });
        server.register_socket(udp);
        server.register_listener(tcp, Duration::from_secs(10), 32);
        Ok(Self { server, addr })
    }

    /// The bound address (UDP and TCP).
    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// Stops accepting queries and waits for in-flight ones.
    pub async fn shutdown(mut self) -> Result<(), AuthorityError> {
        self.server.shutdown_gracefully().await.map_err(|e| AuthorityError::Server(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zones() -> Zones {
        let z = Zones::new();
        z.sync(vec![Zone {
            apex: "shop.acme.com".into(),
            ipv4: vec![Ipv4Addr::new(203, 0, 113, 7)],
            ipv6: vec![],
            nameservers: vec!["ns1.aho.cloud".into(), "ns2.aho.cloud".into()],
            verify_token: Some("tok".into()),
        }])
        .unwrap();
        z
    }

    fn q(z: &Zones, name: &str, t: RecordType) -> Found {
        z.lookup(&Name::from_ascii(name).unwrap(), t)
    }

    #[test]
    fn wildcard_covers_one_label_only() {
        let z = zones();
        assert_eq!(q(&z, "api.shop.acme.com.", RecordType::A).answers.len(), 1);
        let deep = q(&z, "a.b.shop.acme.com.", RecordType::A);
        assert_eq!(deep.code, ResponseCode::NXDomain);
        assert!(deep.soa.is_some());
    }

    #[test]
    fn missing_type_is_nodata_with_soa_and_foreign_names_are_refused() {
        let z = zones();
        let nodata = q(&z, "api.shop.acme.com.", RecordType::TXT);
        assert_eq!((nodata.code, nodata.answers.len(), nodata.soa.is_some()), (ResponseCode::NoError, 0, true));
        let foreign = q(&z, "other.example.", RecordType::A);
        assert_eq!((foreign.code, foreign.authoritative), (ResponseCode::Refused, false));
        // A sibling that merely shares a suffix string is not inside the zone.
        assert_eq!(q(&z, "xshop.acme.com.", RecordType::A).code, ResponseCode::Refused);
    }

    #[test]
    fn challenges_come_and_go_and_sync_keeps_them() {
        let z = zones();
        let name = acme_challenge_name("*.shop.acme.com");
        assert_eq!(name, "_acme-challenge.shop.acme.com");
        z.add_challenge(&name, "v1");
        z.add_challenge(&name, "v2");
        z.sync(vec![Zone {
            apex: "shop.acme.com".into(),
            ipv4: vec![],
            ipv6: vec![],
            nameservers: vec!["ns1.aho.cloud".into()],
            verify_token: None,
        }])
        .unwrap();
        assert_eq!(q(&z, "_acme-challenge.shop.acme.com.", RecordType::TXT).answers.len(), 2);
        z.remove_challenge(&name, "v1");
        z.remove_challenge(&name, "v2");
        assert!(z.challenges(&name).is_empty());
        // No challenge, no verification token, no addresses: the name does not exist.
        assert_eq!(q(&z, "_acme-challenge.shop.acme.com.", RecordType::TXT).code, ResponseCode::NXDomain);
        assert_eq!(q(&z, "_aho-cloud.shop.acme.com.", RecordType::TXT).code, ResponseCode::NXDomain);
    }

    #[test]
    fn sync_rejects_bad_names_without_changing_state() {
        let z = zones();
        let bad = Zone { apex: "bad..name".into(), ipv4: vec![], ipv6: vec![], nameservers: vec![], verify_token: None };
        assert!(z.sync(vec![bad]).is_err());
        assert!(z.serves("api.shop.acme.com"));
    }
}
