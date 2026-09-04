//! The registry end to end through its public API, against an in-memory DNS.
// Helpers outside `#[test]` fns are test code too.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::net::{IpAddr, Ipv4Addr};

use cloud_domains::{BindingState, DomainError, DomainMode, Record, Registry, Serving, Transition, token, txt_name, txt_value};
use common::{FakeDns, edge};

const SECRET: [u8; 32] = [42; 32];
const GRACE: u64 = 3_600;

fn registry() -> Registry {
    let mut r = Registry::new(SECRET, GRACE);
    r.set_edge(edge());
    r
}

fn edge_v4() -> IpAddr {
    IpAddr::V4(edge().ipv4[0])
}

/// Publishes a binding's TXT and CNAME, as a customer following the instructions would.
fn point_cname(dns: &FakeDns, host: &str, token: &str) {
    dns.txt(&txt_name(host), &[&txt_value(token)]).cname(host, "edge.aho.cloud");
}

fn rec(kind: &str, name: &str, value: &str) -> Record {
    Record { kind: kind.into(), name: name.into(), value: value.into() }
}

#[test]
fn records_per_mode() {
    let mut r = registry();
    let e = edge();
    let c = r.add("p", "shop.acme.com", DomainMode::Cname, 0).unwrap();
    let a = r.add("p", "acme.com", DomainMode::Apex, 0).unwrap();
    let n = r.add("p", "eu.acme.com", DomainMode::NsDelegated, 0).unwrap();
    let txt = |host: &str, t: &str| rec("TXT", &format!("_aho-cloud.{host}"), &format!("aho-cloud-verify={t}"));

    assert_eq!(r.records(&c, &e), vec![txt("shop.acme.com", &c.token), rec("CNAME", "shop.acme.com", "edge.aho.cloud")]);
    assert_eq!(
        r.records(&a, &e),
        vec![txt("acme.com", &a.token), rec("A", "acme.com", "203.0.113.10"), rec("AAAA", "acme.com", "2001:db8::a")]
    );
    assert_eq!(
        r.records(&n, &e),
        vec![txt("eu.acme.com", &n.token), rec("NS", "eu.acme.com", "ns1.aho.cloud"), rec("NS", "eu.acme.com", "ns2.aho.cloud")]
    );
    let view = r.view(&c, &e);
    assert_eq!((view.state, view.records.len()), (BindingState::Pending, 2));
}

#[test]
fn tokens_are_hmacs_of_the_binding_with_a_fresh_nonce_per_binding() {
    let mut r = registry();
    let b = r.add("p", "shop.acme.com", DomainMode::Cname, 0).unwrap();
    assert_eq!(b.token, token(&SECRET, "p", "shop.acme.com", &b.nonce));
    r.detach("p", "shop.acme.com", 1).unwrap();
    let again = r.add("p", "shop.acme.com", DomainMode::Cname, 2).unwrap();
    assert_ne!(again.nonce, b.nonce);
    assert_ne!(again.token, b.token, "a new binding must need a new TXT, even for the same project");
}

#[test]
fn add_normalizes_validates_and_is_idempotent() {
    let mut r = registry();
    let b = r.add("p", "Shop.Acme.COM.", DomainMode::Cname, 0).unwrap();
    assert_eq!(b.hostname, "shop.acme.com");
    assert_eq!(r.add("p", "shop.acme.com", DomainMode::Cname, 5).unwrap(), b);
    assert!(matches!(r.add("p", "shop.acme.com", DomainMode::Apex, 5), Err(DomainError::ModeMismatch { existing: DomainMode::Cname, .. })));
    for bad in ["localhost", "*.acme.com", "https://acme.com", "a..com", ""] {
        assert!(matches!(r.add("p", bad, DomainMode::Cname, 0), Err(DomainError::InvalidHostname(_))), "{bad}");
    }
}

#[tokio::test]
async fn check_without_edge_target_is_an_error() {
    let mut r = Registry::new(SECRET, GRACE);
    r.add("p", "shop.acme.com", DomainMode::Cname, 0).unwrap();
    assert!(matches!(r.check("shop.acme.com", &FakeDns::new(), 1).await, Err(DomainError::NoEdgeTarget)));
    assert!(matches!(registry().check("nope.acme.com", &FakeDns::new(), 1).await, Err(DomainError::NotFound(_))));
}

#[tokio::test]
async fn pending_verifies_only_when_txt_and_routing_both_hold() {
    let mut r = registry();
    let dns = FakeDns::new();
    let host = "shop.acme.com";
    let b = r.add("p", host, DomainMode::Cname, 0).unwrap();

    dns.cname(host, "edge.aho.cloud");
    assert_eq!(r.check(host, &dns, 1).await.unwrap(), BindingState::Pending, "CNAME alone");
    dns.clear(host).txt(&txt_name(host), &[&txt_value(&b.token)]);
    assert_eq!(r.check(host, &dns, 2).await.unwrap(), BindingState::Pending, "TXT alone");
    dns.cname(host, "elsewhere.example.net");
    assert_eq!(r.check(host, &dns, 3).await.unwrap(), BindingState::Pending, "CNAME to someone else");
    dns.txt(&txt_name(host), &["aho-cloud-verify=forged"]).cname(host, "EDGE.aho.cloud.");
    assert_eq!(r.check(host, &dns, 4).await.unwrap(), BindingState::Pending, "wrong token");
    // Other TXT strings at the name are fine; target comparison is case/dot-insensitive.
    dns.txt(&txt_name(host), &["google-site-verification=x", &txt_value(&b.token)]);
    assert_eq!(r.check(host, &dns, 5).await.unwrap(), BindingState::Verified);
    let got = r.get(host).unwrap();
    assert_eq!((got.verified_at, got.changed_at), (Some(5), 5));
    assert!(matches!(r.serving(host), Serving::Project(b) if b.project == "p"));
}

#[tokio::test]
async fn cname_mode_accepts_flattened_records_that_resolve_only_to_the_edge() {
    let mut r = registry();
    let dns = FakeDns::new();
    let host = "shop.acme.com";
    let b = r.add("p", host, DomainMode::Cname, 0).unwrap();
    dns.txt(&txt_name(host), &[&txt_value(&b.token)]).addrs(host, &[edge_v4()]);
    assert_eq!(r.check(host, &dns, 1).await.unwrap(), BindingState::Verified);
}

#[tokio::test]
async fn apex_requires_every_address_to_be_the_edge() {
    let mut r = registry();
    let dns = FakeDns::new();
    let host = "acme.com";
    let b = r.add("p", host, DomainMode::Apex, 0).unwrap();
    dns.txt(&txt_name(host), &[&txt_value(&b.token)]).addrs(host, &[edge_v4(), IpAddr::V4(Ipv4Addr::new(198, 51, 100, 1))]);
    assert_eq!(r.check(host, &dns, 1).await.unwrap(), BindingState::Pending, "one foreign address");
    dns.addrs(host, &[]);
    assert_eq!(r.check(host, &dns, 2).await.unwrap(), BindingState::Pending, "no addresses");
    dns.addrs(host, &[edge_v4(), "2001:db8::a".parse().unwrap()]);
    assert_eq!(r.check(host, &dns, 3).await.unwrap(), BindingState::Verified);
}

#[tokio::test]
async fn verified_enters_grace_when_txt_or_routing_goes_and_is_restored() {
    let mut r = registry();
    let dns = FakeDns::new();
    let host = "shop.acme.com";
    let b = r.add("p", host, DomainMode::Cname, 0).unwrap();
    point_cname(&dns, host, &b.token);
    assert_eq!(r.check(host, &dns, 10).await.unwrap(), BindingState::Verified);

    // TXT disappears.
    dns.txt(&txt_name(host), &[]);
    assert_eq!(r.check(host, &dns, 20).await.unwrap(), BindingState::Grace);
    assert_eq!(r.get(host).unwrap().grace_since, Some(20));
    assert!(matches!(r.serving(host), Serving::Project(_)), "grace keeps serving");
    point_cname(&dns, host, &b.token);
    assert_eq!(r.check(host, &dns, 30).await.unwrap(), BindingState::Verified);
    assert_eq!(r.get(host).unwrap().grace_since, None);

    // Routing moves away.
    dns.cname(host, "other-host.example.net");
    assert_eq!(r.check(host, &dns, 40).await.unwrap(), BindingState::Grace);
    point_cname(&dns, host, &b.token);
    assert_eq!(r.check(host, &dns, 50).await.unwrap(), BindingState::Verified);
}

#[tokio::test]
async fn grace_times_out_to_detached_which_is_terminal() {
    let mut r = registry();
    let dns = FakeDns::new();
    let host = "shop.acme.com";
    let b = r.add("p", host, DomainMode::Cname, 0).unwrap();
    point_cname(&dns, host, &b.token);
    r.check(host, &dns, 100).await.unwrap();
    dns.clear(host);
    assert_eq!(r.check(host, &dns, 200).await.unwrap(), BindingState::Grace);
    assert_eq!(r.check(host, &dns, 200 + GRACE - 1).await.unwrap(), BindingState::Grace, "one second early");
    assert_eq!(r.check(host, &dns, 200 + GRACE).await.unwrap(), BindingState::Detached);
    assert!(matches!(r.serving(host), Serving::Tombstone(b) if b.state == BindingState::Detached));

    // Records coming back do not revive a detached binding; the project must add it again.
    point_cname(&dns, host, &b.token);
    assert_eq!(r.check(host, &dns, 200 + GRACE + 1).await.unwrap(), BindingState::Detached);
    let again = r.add("p", host, DomainMode::Cname, 200 + GRACE + 2).unwrap();
    assert_eq!(again.state, BindingState::Pending);
    assert_eq!(r.check(host, &dns, 200 + GRACE + 3).await.unwrap(), BindingState::Pending, "old TXT is stale");
    point_cname(&dns, host, &again.token);
    assert_eq!(r.check(host, &dns, 200 + GRACE + 4).await.unwrap(), BindingState::Verified);
}

#[tokio::test]
async fn failed_lookups_never_change_state() {
    let mut r = registry();
    let dns = FakeDns::new();
    let host = "shop.acme.com";
    let b = r.add("p", host, DomainMode::Cname, 0).unwrap();
    point_cname(&dns, host, &b.token);
    r.check(host, &dns, 1).await.unwrap();
    dns.clear(host);
    r.check(host, &dns, 2).await.unwrap();
    dns.failing(true);
    // Past the grace deadline, but we cannot see DNS: stay in grace.
    assert!(matches!(r.check(host, &dns, 2 + GRACE * 10).await, Err(DomainError::Dns(_))));
    assert_eq!(r.get(host).unwrap().state, BindingState::Grace);
    assert!(r.recheck_all(&dns, 3 + GRACE * 10).await.is_empty());
    assert_eq!(r.get(host).unwrap().state, BindingState::Grace);
}

#[tokio::test]
async fn takeover_guard_blocks_live_bindings_and_needs_a_new_txt_after_tombstone() {
    let mut r = registry();
    let dns = FakeDns::new();
    let host = "shop.acme.com";

    let a = r.add("alpha", host, DomainMode::Cname, 0).unwrap();
    assert!(matches!(r.add("beta", host, DomainMode::Cname, 1), Err(DomainError::Taken { .. })), "pending blocks");
    point_cname(&dns, host, &a.token);
    r.check(host, &dns, 2).await.unwrap();
    assert!(matches!(r.add("beta", host, DomainMode::Apex, 3), Err(DomainError::Taken { .. })), "verified blocks");
    let msg = r.add("beta", host, DomainMode::Cname, 3).unwrap_err().to_string();
    assert!(!msg.contains("alpha"), "error must not leak the other tenant: {msg}");

    assert_eq!(r.remove_project("alpha", 4), vec![host.to_string()]);
    let b = r.add("beta", host, DomainMode::Cname, 5).unwrap();
    assert_ne!(b.token, a.token);

    // alpha's records are still published (dangling DNS): beta must not inherit them.
    assert_eq!(r.check(host, &dns, 6).await.unwrap(), BindingState::Pending);
    assert!(matches!(r.serving(host), Serving::Tombstone(t) if t.project == "alpha"));

    point_cname(&dns, host, &b.token);
    assert_eq!(r.check(host, &dns, 7).await.unwrap(), BindingState::Verified);
    assert!(matches!(r.serving(host), Serving::Project(p) if p.project == "beta"));
    assert!(r.for_project("alpha").is_empty(), "the tombstone no longer governs once beta verified");
}

#[tokio::test]
async fn tombstones_never_serve_another_project_and_never_revive() {
    let mut r = registry();
    let dns = FakeDns::new();
    let host = "shop.acme.com";
    let a = r.add("alpha", host, DomainMode::Cname, 0).unwrap();
    point_cname(&dns, host, &a.token);
    r.check(host, &dns, 1).await.unwrap();
    r.remove_project("alpha", 2);

    assert_eq!(r.get(host).unwrap().state, BindingState::Tombstoned);
    assert_eq!(r.check(host, &dns, 3).await.unwrap(), BindingState::Tombstoned, "valid DNS does not revive it");
    assert!(r.recheck_all(&dns, 4).await.is_empty());

    // A pending claimant does not displace the tombstone, with or without the old records.
    r.add("beta", host, DomainMode::Cname, 5).unwrap();
    assert_eq!(r.check(host, &dns, 6).await.unwrap(), BindingState::Pending);
    assert!(matches!(r.serving(host), Serving::Tombstone(t) if t.project == "alpha"));
    // If the claimant gives up, the tombstone is still there.
    r.remove_project("beta", 7);
    assert!(matches!(r.serving(host), Serving::Tombstone(_)));
    assert!(matches!(r.serving("unknown.acme.com"), Serving::Unbound));
}

#[tokio::test]
async fn ns_delegated_needs_the_parent_zone_txt_before_delegation_and_covers_one_label() {
    let mut r = registry();
    let dns = FakeDns::new();
    let zone = "eu.acme.com";
    let b = r.add("p", zone, DomainMode::NsDelegated, 0).unwrap();
    assert!(r.zones(&edge())[0].verify_token.is_none(), "our server never vouches for a pending zone");

    // Delegated first, TXT never seen: a dangling delegation can't be claimed.
    dns.ns(zone, &["ns1.aho.cloud", "ns2.aho.cloud"]);
    assert_eq!(r.check(zone, &dns, 1).await.unwrap(), BindingState::Pending);

    // TXT seen in the parent zone while the delegation points elsewhere …
    dns.txt(&txt_name(zone), &[&txt_value(&b.token)]).ns(zone, &["ns1.registrar.example"]);
    assert_eq!(r.check(zone, &dns, 2).await.unwrap(), BindingState::Pending);
    assert_eq!(r.get(zone).unwrap().txt_proven_at, Some(2));
    // … then the delegation takes effect and occludes it: the proof sticks.
    dns.clear(&txt_name(zone)).ns(zone, &["NS1.aho.cloud.", "ns2.aho.cloud"]);
    assert_eq!(r.check(zone, &dns, 3).await.unwrap(), BindingState::Verified);

    assert_eq!(r.zones(&edge())[0].verify_token.as_deref(), Some(b.token.as_str()));
    assert!(matches!(r.serving("api.eu.acme.com"), Serving::Project(p) if p.hostname == zone));
    assert!(matches!(r.serving("a.b.eu.acme.com"), Serving::Unbound), "wildcard covers one label");

    // A mixed NS set is not ours alone.
    dns.txt(&txt_name(zone), &[&txt_value(&b.token)]).ns(zone, &["ns1.aho.cloud", "ns1.registrar.example"]);
    assert_eq!(r.check(zone, &dns, 4).await.unwrap(), BindingState::Grace);
}

#[tokio::test]
async fn exact_tombstone_beats_zone_coverage() {
    let mut r = registry();
    let dns = FakeDns::new();
    let zone = "eu.acme.com";
    let z = r.add("zone-owner", zone, DomainMode::NsDelegated, 0).unwrap();
    dns.txt(&txt_name(zone), &[&txt_value(&z.token)]).ns(zone, &["ns1.aho.cloud"]);
    r.check(zone, &dns, 1).await.unwrap();

    let host = "api.eu.acme.com";
    let h = r.add("old", host, DomainMode::Cname, 2).unwrap();
    point_cname(&dns, host, &h.token);
    r.check(host, &dns, 3).await.unwrap();
    r.remove_project("old", 4);
    assert!(matches!(r.serving(host), Serving::Tombstone(t) if t.project == "old"));
}

#[tokio::test]
async fn recheck_all_returns_exactly_the_transitions() {
    let mut r = registry();
    let dns = FakeDns::new();
    let a = r.add("p", "a.acme.com", DomainMode::Cname, 0).unwrap();
    let b = r.add("q", "b.acme.com", DomainMode::Cname, 0).unwrap();
    r.add("q", "c.acme.com", DomainMode::Cname, 0).unwrap();
    point_cname(&dns, "b.acme.com", &b.token);
    r.check("b.acme.com", &dns, 1).await.unwrap();

    point_cname(&dns, "a.acme.com", &a.token);
    dns.clear("b.acme.com");
    let got = r.recheck_all(&dns, 10).await;
    let t = |host: &str, project: &str, from, to| Transition { hostname: host.into(), project: project.into(), from, to, at: 10 };
    assert_eq!(
        got,
        vec![
            t("a.acme.com", "p", BindingState::Pending, BindingState::Verified),
            t("b.acme.com", "q", BindingState::Verified, BindingState::Grace),
        ]
    );
    let got = r.recheck_all(&dns, 10 + GRACE).await;
    assert_eq!(got, vec![Transition { at: 10 + GRACE, ..t("b.acme.com", "q", BindingState::Grace, BindingState::Detached) }]);
}

#[tokio::test]
async fn snapshot_round_trips_and_keeps_the_secret() {
    let mut r = registry();
    let dns = FakeDns::new();
    let v = r.add("p", "v.acme.com", DomainMode::Cname, 0).unwrap();
    r.add("p", "pending.acme.com", DomainMode::Apex, 0).unwrap();
    r.add("gone", "t.acme.com", DomainMode::NsDelegated, 0).unwrap();
    point_cname(&dns, "v.acme.com", &v.token);
    r.check("v.acme.com", &dns, 1).await.unwrap();
    r.remove_project("gone", 2);

    let json = serde_json::to_string(&r).unwrap();
    assert!(!json.contains("edge.aho.cloud"), "edge target is runtime config, not state");
    let mut back: Registry = serde_json::from_str(&json).unwrap();
    assert!(back.edge().is_none());
    for host in ["v.acme.com", "pending.acme.com", "t.acme.com"] {
        assert_eq!(back.get(host), r.get(host), "{host}");
    }
    assert!(matches!(back.check("v.acme.com", &dns, 3).await, Err(DomainError::NoEdgeTarget)));
    back.set_edge(edge());
    assert_eq!(back, r);
    let fresh = back.add("p", "new.acme.com", DomainMode::Cname, 4).unwrap();
    assert_eq!(fresh.token, token(&SECRET, "p", "new.acme.com", &fresh.nonce));

    let bad = json.replacen(&hex::encode(SECRET), "abcd", 1);
    assert!(serde_json::from_str::<Registry>(&bad).is_err(), "short secret");
}

#[test]
fn for_project_lists_live_and_retired_bindings() {
    let mut r = registry();
    r.add("p", "a.acme.com", DomainMode::Cname, 0).unwrap();
    r.add("p", "b.acme.com", DomainMode::Cname, 0).unwrap();
    r.detach("p", "b.acme.com", 1).unwrap();
    r.add("p", "b.acme.com", DomainMode::Apex, 2).unwrap();
    r.add("p", "c.acme.com", DomainMode::Cname, 0).unwrap();
    r.detach("p", "c.acme.com", 3).unwrap();
    let mut got: Vec<_> = r.for_project("p").into_iter().map(|b| (b.hostname.as_str(), b.state, b.mode)).collect();
    got.sort_by_key(|(host, ..)| *host);
    assert_eq!(
        got,
        vec![
            ("a.acme.com", BindingState::Pending, DomainMode::Cname),
            ("b.acme.com", BindingState::Pending, DomainMode::Apex),
            ("c.acme.com", BindingState::Detached, DomainMode::Cname),
        ]
    );
    assert!(matches!(r.detach("other", "a.acme.com", 4), Err(DomainError::NotFound(_))), "only the owner detaches");
}
