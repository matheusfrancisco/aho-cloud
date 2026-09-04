//! The authoritative zone server over real UDP on 127.0.0.1:0, queried with the hickory client,
//! and the `HickoryResolver` adapter driven against it.
// Helpers outside `#[test]` fns are test code too.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::str::FromStr;

use cloud_domains::{
    BindingState, DomainMode, HickoryResolver, Registry, Resolver, Zone, ZoneServer, Zones, acme_challenge_name, txt_name, txt_value,
};
use common::{FakeDns, edge};
use hickory_net::client::{Client, ClientHandle};
use hickory_net::proto::op::ResponseCode;
use hickory_net::proto::rr::{DNSClass, Name, RData, RecordType};
use hickory_net::runtime::TokioRuntimeProvider;
use hickory_net::udp::UdpClientStream;

fn zone() -> Zone {
    Zone {
        apex: "eu.acme.com".into(),
        ipv4: vec![Ipv4Addr::new(203, 0, 113, 10)],
        ipv6: vec![Ipv6Addr::from_str("2001:db8::a").unwrap()],
        nameservers: vec!["ns1.aho.cloud".into(), "ns2.aho.cloud".into()],
        verify_token: Some("tok123".into()),
    }
}

async fn serve(zones: &Zones) -> ZoneServer {
    ZoneServer::bind(SocketAddr::from(([127, 0, 0, 1], 0)), zones.clone()).await.unwrap()
}

async fn client(addr: SocketAddr) -> Client<TokioRuntimeProvider> {
    let stream = UdpClientStream::builder(addr, TokioRuntimeProvider::default()).build();
    let (client, bg) = Client::<TokioRuntimeProvider>::from_sender(stream);
    tokio::spawn(bg);
    client
}

struct Reply {
    code: ResponseCode,
    aa: bool,
    answers: Vec<RData>,
    soa_in_authority: bool,
}

async fn ask(c: &mut Client<TokioRuntimeProvider>, name: &str, t: RecordType) -> Reply {
    let r = c.query(Name::from_ascii(name).unwrap(), DNSClass::IN, t).await.unwrap();
    Reply {
        code: r.metadata.response_code,
        aa: r.metadata.authoritative,
        answers: r.answers.iter().map(|a| a.data.clone()).collect(),
        soa_in_authority: r.authorities.iter().any(|a| a.record_type() == RecordType::SOA),
    }
}

fn txt_strings(answers: &[RData]) -> Vec<String> {
    answers
        .iter()
        .filter_map(|d| match d {
            RData::TXT(t) => Some(t.txt_data.iter().map(|p| String::from_utf8_lossy(p).into_owned()).collect()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn zone_server_answers_authoritatively_over_udp() {
    let zones = Zones::new();
    zones.sync(vec![zone()]).unwrap();
    zones.add_challenge(&acme_challenge_name("*.eu.acme.com"), "dns01-value");
    let server = serve(&zones).await;
    let mut c = client(server.local_addr()).await;

    let r = ask(&mut c, "_aho-cloud.eu.acme.com.", RecordType::TXT).await;
    assert_eq!((r.code, r.aa), (ResponseCode::NoError, true));
    assert_eq!(txt_strings(&r.answers), vec!["aho-cloud-verify=tok123"]);

    let r = ask(&mut c, "_acme-challenge.eu.acme.com.", RecordType::TXT).await;
    assert_eq!(txt_strings(&r.answers), vec!["dns01-value"]);

    let r = ask(&mut c, "eu.acme.com.", RecordType::A).await;
    assert_eq!(r.answers, vec![RData::A(Ipv4Addr::new(203, 0, 113, 10).into())]);
    let r = ask(&mut c, "Api.EU.acme.com.", RecordType::AAAA).await;
    assert_eq!(r.answers, vec![RData::AAAA(Ipv6Addr::from_str("2001:db8::a").unwrap().into())], "wildcard, case-insensitive");

    let r = ask(&mut c, "eu.acme.com.", RecordType::NS).await;
    let ns: Vec<String> = r
        .answers
        .iter()
        .filter_map(|d| match d {
            RData::NS(n) => Some(n.0.to_ascii()),
            _ => None,
        })
        .collect();
    assert_eq!(ns, vec!["ns1.aho.cloud.", "ns2.aho.cloud."]);
    let r = ask(&mut c, "eu.acme.com.", RecordType::SOA).await;
    assert!(matches!(&r.answers[..], [RData::SOA(soa)] if soa.mname.to_ascii() == "ns1.aho.cloud."));

    let r = ask(&mut c, "a.b.eu.acme.com.", RecordType::A).await;
    assert_eq!((r.code, r.aa, r.answers.len(), r.soa_in_authority), (ResponseCode::NXDomain, true, 0, true));
    let r = ask(&mut c, "api.eu.acme.com.", RecordType::MX).await;
    assert_eq!((r.code, r.answers.len(), r.soa_in_authority), (ResponseCode::NoError, 0, true), "NODATA");
    let r = ask(&mut c, "example.org.", RecordType::A).await;
    assert_eq!((r.code, r.aa), (ResponseCode::Refused, false), "not our zone");

    // Updates are live: withdrawing the challenge and re-syncing without a token.
    zones.remove_challenge(&acme_challenge_name("eu.acme.com"), "dns01-value");
    zones.sync(vec![Zone { verify_token: None, ..zone() }]).unwrap();
    let r = ask(&mut c, "_acme-challenge.eu.acme.com.", RecordType::TXT).await;
    assert!(txt_strings(&r.answers).is_empty());
    let r = ask(&mut c, "_aho-cloud.eu.acme.com.", RecordType::TXT).await;
    assert!(txt_strings(&r.answers).is_empty());

    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn hickory_resolver_follows_the_resolver_contract() {
    let zones = Zones::new();
    zones.sync(vec![zone()]).unwrap();
    let server = serve(&zones).await;
    let dns = HickoryResolver::with_servers(&[server.local_addr()]).unwrap();

    assert_eq!(dns.txt("_aho-cloud.eu.acme.com").await.unwrap(), vec!["aho-cloud-verify=tok123"]);
    assert_eq!(dns.ns("EU.acme.com.").await.unwrap(), vec!["ns1.aho.cloud", "ns2.aho.cloud"]);
    let mut addrs = dns.addrs("www.eu.acme.com").await.unwrap();
    addrs.sort();
    assert_eq!(addrs, vec![IpAddr::from([203, 0, 113, 10]), IpAddr::from_str("2001:db8::a").unwrap()]);
    // Absence is not an error: NODATA and NXDOMAIN are empty answers.
    assert_eq!(dns.cname("www.eu.acme.com").await.unwrap(), None);
    assert!(dns.txt("www.eu.acme.com").await.unwrap().is_empty());
    assert!(dns.addrs("a.b.eu.acme.com").await.unwrap().is_empty());
    // A server that refuses to answer is an error, which the registry treats as "no change".
    assert!(dns.txt("example.org").await.is_err());

    server.shutdown().await.unwrap();
}

/// A delegated zone verified through the customer's parent zone, then served by us: rechecks
/// through our own server keep it verified, and after the project is deleted a new claimant
/// cannot verify the still-dangling delegation through our answers.
#[tokio::test]
async fn delegated_zone_lifecycle_through_our_own_server() {
    let mut registry = Registry::new([9; 32], 3600);
    registry.set_edge(edge());
    let host = "eu.acme.com";
    let a = registry.add("alpha", host, DomainMode::NsDelegated, 0).unwrap();

    // Customer's parent zone during setup: TXT visible, then the NS delegation lands.
    let parent = FakeDns::new();
    parent.txt(&txt_name(host), &[&txt_value(&a.token)]);
    registry.check(host, &parent, 1).await.unwrap();
    parent.clear(&txt_name(host)).ns(host, &["ns1.aho.cloud", "ns2.aho.cloud"]);
    assert_eq!(registry.check(host, &parent, 2).await.unwrap(), BindingState::Verified);

    // From now on the internet asks us.
    let zones = Zones::new();
    zones.sync(registry.zones(&edge())).unwrap();
    let server = serve(&zones).await;
    let ours = HickoryResolver::with_servers(&[server.local_addr()]).unwrap();
    assert!(registry.recheck_all(&ours, 3).await.is_empty(), "stays verified on our own answers");

    registry.remove_project("alpha", 4);
    let b = registry.add("beta", host, DomainMode::NsDelegated, 5).unwrap();
    zones.sync(registry.zones(&edge())).unwrap();
    assert!(ours.txt(&txt_name(host)).await.unwrap().is_empty(), "tombstoned zone publishes no token");
    assert_eq!(registry.check(host, &ours, 6).await.unwrap(), BindingState::Pending);
    assert_ne!(b.token, a.token);
    // Still answering for the zone (the edge shows the tombstone page), just not vouching.
    assert_eq!(ours.ns(host).await.unwrap(), vec!["ns1.aho.cloud", "ns2.aho.cloud"]);

    server.shutdown().await.unwrap();
}
