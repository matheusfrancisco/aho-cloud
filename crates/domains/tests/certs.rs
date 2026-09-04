//! Certificate store (rcgen-signed certs, real rustls handshakes) and ACME issuance through a
//! test CA that validates challenges the way a real one would: HTTP-01 through the edge's token
//! store, DNS-01 by querying our zone server over UDP.
// Helpers outside `#[test]` fns are test code too.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::net::SocketAddr;
use std::sync::Arc;

use cloud_domains::{
    Acme, AcmeError, Binding, BindingState, CertError, CertStore, ChallengeKind, DomainMode, HTTP01_PREFIX, HickoryResolver, Http01Store,
    IssueError, IssuedCert, Issuer, PendingOrder, Registry, Resolver, ZoneServer, Zones, acme_challenge_name, txt_name, txt_value,
};
use common::{FakeDns, edge};
use futures::future::BoxFuture;
use parking_lot::Mutex;
use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair};
use rustls::pki_types::{CertificateDer, ServerName};
use rustls::{ClientConfig, ClientConnection, RootCertStore, ServerConfig, ServerConnection};
use time::OffsetDateTime;

const DAY: u64 = 86_400;
const NOW: u64 = 1_900_000_000;

/// A private CA signing leaf certificates.
struct Ca {
    cert: rcgen::Certificate,
    issuer: rcgen::Issuer<'static, KeyPair>,
}

impl Ca {
    fn new() -> Self {
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let key = KeyPair::generate().unwrap();
        let cert = params.self_signed(&key).unwrap();
        Self { cert, issuer: rcgen::Issuer::new(params, key) }
    }

    /// `(chain_pem, key_pem, leaf_der)` valid until `not_after` (Unix seconds).
    fn leaf(&self, names: &[&str], not_after: u64) -> (String, String, Vec<u8>) {
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::new(names.iter().map(|n| n.to_string()).collect::<Vec<_>>()).unwrap();
        // Valid from 2023 so real TLS handshakes (checked against the wall clock) accept it too.
        params.not_before = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
        params.not_after = OffsetDateTime::from_unix_timestamp(not_after as i64).unwrap();
        let cert = params.signed_by(&key, &self.issuer).unwrap();
        (format!("{}{}", cert.pem(), self.cert.pem()), key.serialize_pem(), cert.der().to_vec())
    }

    fn root(&self) -> CertificateDer<'static> {
        self.cert.der().clone()
    }
}

/// Full TLS 1.3 handshake in memory; returns the leaf the server presented for `sni`.
fn handshake(store: Arc<CertStore>, root: CertificateDer<'static>, sni: &str) -> Result<Vec<u8>, rustls::Error> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let server_cfg = ServerConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_cert_resolver(store);
    let mut roots = RootCertStore::empty();
    roots.add(root)?;
    let client_cfg = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .with_root_certificates(roots)
        .with_no_client_auth();
    let name = ServerName::try_from(sni.to_owned()).unwrap();
    let mut client = ClientConnection::new(Arc::new(client_cfg), name)?;
    let mut server = ServerConnection::new(Arc::new(server_cfg))?;
    let mut buf = Vec::new();
    for _ in 0..10 {
        if !client.is_handshaking() && !server.is_handshaking() {
            break;
        }
        buf.clear();
        while client.wants_write() {
            client.write_tls(&mut buf).unwrap();
        }
        let mut rd = &buf[..];
        while !rd.is_empty() {
            server.read_tls(&mut rd).unwrap();
            server.process_new_packets()?;
        }
        buf.clear();
        while server.wants_write() {
            server.write_tls(&mut buf).unwrap();
        }
        let mut rd = &buf[..];
        while !rd.is_empty() {
            client.read_tls(&mut rd).unwrap();
            client.process_new_packets()?;
        }
    }
    Ok(client.peer_certificates().unwrap()[0].to_vec())
}

#[test]
fn sni_resolves_exact_names_then_single_label_wildcards() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(CertStore::open(dir.path()).unwrap());
    let ca = Ca::new();
    let (chain, key, shop_der) = ca.leaf(&["shop.acme.com"], NOW + 90 * DAY);
    store.insert(&chain, &key).unwrap();
    let (chain, key, zone_der) = ca.leaf(&["eu.acme.com", "*.eu.acme.com"], NOW + 90 * DAY);
    let info = store.insert(&chain, &key).unwrap();
    assert_eq!(info.names, vec!["eu.acme.com", "*.eu.acme.com"]);

    let leaf = |sni: &str| store.get(sni).map(|k| k.end_entity_cert().unwrap().to_vec());
    assert_eq!(leaf("shop.acme.com"), Some(shop_der.clone()));
    assert_eq!(leaf("SHOP.acme.com."), Some(shop_der.clone()));
    assert_eq!(leaf("eu.acme.com"), Some(zone_der.clone()));
    assert_eq!(leaf("api.eu.acme.com"), Some(zone_der.clone()));
    assert_eq!(leaf("a.b.eu.acme.com"), None, "wildcards cover exactly one label");
    assert_eq!(leaf("www.shop.acme.com"), None);
    assert_eq!(leaf("acme.com"), None);

    // The same answers through rustls, as the edge uses it.
    assert_eq!(handshake(store.clone(), ca.root(), "shop.acme.com").unwrap(), shop_der);
    assert_eq!(handshake(store.clone(), ca.root(), "api.eu.acme.com").unwrap(), zone_der);
    assert!(handshake(store.clone(), ca.root(), "unknown.acme.com").is_err(), "no cert → handshake refused");
}

#[test]
fn the_later_expiring_cert_wins_for_a_shared_name() {
    let dir = tempfile::tempdir().unwrap();
    let store = CertStore::open(dir.path()).unwrap();
    let ca = Ca::new();
    let (chain, key, _) = ca.leaf(&["shop.acme.com"], NOW + 10 * DAY);
    store.insert(&chain, &key).unwrap();
    let (chain, key, newer) = ca.leaf(&["www.shop.acme.com", "shop.acme.com"], NOW + 80 * DAY);
    store.insert(&chain, &key).unwrap();
    assert_eq!(store.get("shop.acme.com").unwrap().end_entity_cert().unwrap().to_vec(), newer);
}

#[test]
fn pairs_persist_and_reload_picks_up_other_writers() {
    let dir = tempfile::tempdir().unwrap();
    let ca = Ca::new();
    let writer = CertStore::open(dir.path()).unwrap();
    let reader = CertStore::open(dir.path()).unwrap();
    let (chain, key, der) = ca.leaf(&["*.eu.acme.com", "eu.acme.com"], NOW + 90 * DAY);
    let info = writer.insert(&chain, &key).unwrap();
    assert_eq!(info.id, "_wildcard.eu.acme.com");
    assert!(dir.path().join("_wildcard.eu.acme.com.pem").is_file());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(dir.path().join("_wildcard.eu.acme.com.key")).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0, "private key readable by others: {mode:o}");
    }

    assert!(reader.get("x.eu.acme.com").is_none());
    std::fs::write(dir.path().join("junk.pem"), "not a cert").unwrap();
    assert_eq!(reader.reload().unwrap(), 1, "junk is skipped, the real pair loads");
    assert_eq!(reader.get("x.eu.acme.com").unwrap().end_entity_cert().unwrap().to_vec(), der);
    assert_eq!(CertStore::open(dir.path()).unwrap().list(), vec![info]);

    assert!(writer.remove("_wildcard.eu.acme.com").unwrap());
    assert!(writer.get("x.eu.acme.com").is_none());
    assert!(!dir.path().join("_wildcard.eu.acme.com.key").exists());
    assert!(!writer.remove("_wildcard.eu.acme.com").unwrap());
}

#[test]
fn remove_ignores_ids_outside_the_store() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("certs");
    let store = CertStore::open(&dir).unwrap();
    std::fs::write(root.path().join("victim.pem"), "keep me").unwrap();
    assert!(!store.remove("../victim").unwrap());
    assert!(root.path().join("victim.pem").exists());
}

#[test]
fn reload_keeps_serving_the_old_pair_when_caught_mid_renewal() {
    let dir = tempfile::tempdir().unwrap();
    let ca = Ca::new();
    let writer = CertStore::open(dir.path()).unwrap();
    let (chain, key, old_der) = ca.leaf(&["shop.acme.com"], NOW + 10 * DAY);
    writer.insert(&chain, &key).unwrap();
    let reader = CertStore::open(dir.path()).unwrap();

    // A renewal has renamed the new key into place but not yet the new chain.
    let (_, new_key, _) = ca.leaf(&["shop.acme.com"], NOW + 90 * DAY);
    std::fs::write(dir.path().join("shop.acme.com.key"), new_key).unwrap();
    assert_eq!(reader.reload().unwrap(), 1);
    assert_eq!(reader.get("shop.acme.com").unwrap().end_entity_cert().unwrap().to_vec(), old_der);

    // A fresh process has nothing to fall back to and skips the inconsistent pair.
    assert!(CertStore::open(dir.path()).unwrap().get("shop.acme.com").is_none());
}

#[test]
fn mismatched_keys_and_garbage_are_rejected_without_touching_disk() {
    let dir = tempfile::tempdir().unwrap();
    let store = CertStore::open(dir.path()).unwrap();
    let ca = Ca::new();
    let (chain, _, _) = ca.leaf(&["shop.acme.com"], NOW + DAY);
    let (_, other_key, _) = ca.leaf(&["shop.acme.com"], NOW + DAY);
    assert!(matches!(store.insert(&chain, &other_key), Err(CertError::Key(_))));
    assert!(matches!(store.insert("", &other_key), Err(CertError::NoCertificate)));
    assert!(matches!(store.insert(&chain, "nope"), Err(CertError::Key(_))));
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    assert!(store.get("shop.acme.com").is_none());
}

#[test]
fn renewal_due_lists_certs_inside_the_window_including_expired_ones() {
    let dir = tempfile::tempdir().unwrap();
    let store = CertStore::open(dir.path()).unwrap();
    let ca = Ca::new();
    for (name, not_after) in [
        ("expired.acme.com", NOW - DAY),
        ("soon.acme.com", NOW + 29 * DAY),
        ("edge.acme.com", NOW + 30 * DAY),
        ("later.acme.com", NOW + 31 * DAY),
    ] {
        let (chain, key, _) = ca.leaf(&[name], not_after);
        store.insert(&chain, &key).unwrap();
    }
    let mut due: Vec<String> = store.renewal_due(NOW, 30).into_iter().map(|c| c.id).collect();
    due.sort();
    assert_eq!(due, vec!["edge.acme.com", "expired.acme.com", "soon.acme.com"]);
    assert_eq!(store.renewal_due(NOW, 0).len(), 1);
}

// ---- issuance -----------------------------------------------------------------------------

/// A CA that validates published challenges before signing, like a real one.
struct TestCa {
    ca: Ca,
    http01: Http01Store,
    dns: Option<HickoryResolver>,
    orders: Mutex<Vec<(Vec<String>, ChallengeKind)>>,
    /// Sign these names instead of the ordered ones.
    sign_names: Option<Vec<String>>,
}

impl TestCa {
    fn new(http01: Http01Store, dns: Option<HickoryResolver>) -> Self {
        Self { ca: Ca::new(), http01, dns, orders: Mutex::new(Vec::new()), sign_names: None }
    }
}

impl Acme for TestCa {
    fn order<'a>(&'a self, names: &'a [String], kind: ChallengeKind) -> BoxFuture<'a, Result<PendingOrder, AcmeError>> {
        self.orders.lock().push((names.to_vec(), kind));
        let challenges = names
            .iter()
            .enumerate()
            .map(|(i, n)| cloud_domains::Challenge {
                identifier: n.clone(),
                token: format!("tok{i}"),
                key_authorization: format!("tok{i}.thumbprint"),
                dns_value: format!("dns-value-{i}"),
            })
            .collect();
        let order = PendingOrder { url: "https://ca.test/order/1".into(), kind, names: names.to_vec(), challenges };
        Box::pin(async move { Ok(order) })
    }

    fn finish<'a>(&'a self, order: &'a PendingOrder) -> BoxFuture<'a, Result<IssuedCert, AcmeError>> {
        Box::pin(async move {
            for c in &order.challenges {
                let ok = match order.kind {
                    ChallengeKind::Http01 => {
                        self.http01.for_path(&format!("{HTTP01_PREFIX}{}", c.token)).as_deref() == Some(c.key_authorization.as_str())
                    }
                    ChallengeKind::Dns01 => {
                        let dns = self.dns.as_ref().ok_or(AcmeError::Order("no resolver".into()))?;
                        let txt = dns.txt(&acme_challenge_name(&c.identifier)).await.map_err(|e| AcmeError::Protocol(e.to_string()))?;
                        txt.contains(&c.dns_value)
                    }
                };
                if !ok {
                    return Err(AcmeError::Authorization { identifier: c.identifier.clone(), status: "invalid".into() });
                }
            }
            let names = self.sign_names.clone().unwrap_or_else(|| order.names.clone());
            let names: Vec<&str> = names.iter().map(String::as_str).collect();
            let (chain_pem, key_pem, _) = self.ca.leaf(&names, NOW + 90 * DAY);
            Ok(IssuedCert { chain_pem, key_pem })
        })
    }
}

async fn verified(mode: DomainMode, host: &str) -> (Registry, Binding) {
    let mut r = Registry::new([3; 32], 3600);
    r.set_edge(edge());
    let b = r.add("p", host, mode, 0).unwrap();
    let dns = FakeDns::new();
    dns.txt(&txt_name(host), &[&txt_value(&b.token)])
        .cname(host, "edge.aho.cloud")
        .addrs(host, &["203.0.113.10".parse().unwrap()])
        .ns(host, &["ns1.aho.cloud"]);
    assert_eq!(r.check(host, &dns, 1).await.unwrap(), BindingState::Verified);
    let b = r.get(host).unwrap().clone();
    (r, b)
}

struct Rig {
    _dir: tempfile::TempDir,
    certs: Arc<CertStore>,
    http01: Http01Store,
    zones: Zones,
    server: ZoneServer,
}

async fn rig() -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let zones = Zones::new();
    let server = ZoneServer::bind(SocketAddr::from(([127, 0, 0, 1], 0)), zones.clone()).await.unwrap();
    Rig { certs: Arc::new(CertStore::open(dir.path()).unwrap()), _dir: dir, http01: Http01Store::new(), zones, server }
}

#[tokio::test]
async fn cname_and_apex_bindings_use_http01_for_the_hostname() {
    let rig = rig().await;
    let ca = Arc::new(TestCa::new(rig.http01.clone(), None));
    let issuer = Issuer::new(ca.clone(), rig.http01.clone(), rig.zones.clone(), rig.certs.clone());
    for (mode, host) in [(DomainMode::Cname, "shop.acme.com"), (DomainMode::Apex, "acme.com")] {
        let (_, binding) = verified(mode, host).await;
        let info = issuer.issue(&binding).await.unwrap();
        assert_eq!(info.names, vec![host]);
        assert!(rig.certs.get(host).is_some());
        assert_eq!(ca.orders.lock().last().unwrap(), &(vec![host.to_string()], ChallengeKind::Http01));
        assert_eq!(rig.http01.get("tok0"), None, "challenge withdrawn after issuance");
    }
}

#[tokio::test]
async fn delegated_zones_use_dns01_for_the_zone_and_its_wildcard() {
    let rig = rig().await;
    let host = "eu.acme.com";
    let (registry, binding) = verified(DomainMode::NsDelegated, host).await;
    rig.zones.sync(registry.zones(&edge())).unwrap();
    let dns = HickoryResolver::with_servers(&[rig.server.local_addr()]).unwrap();
    let ca = Arc::new(TestCa::new(rig.http01.clone(), Some(dns)));
    let issuer = Issuer::new(ca.clone(), rig.http01.clone(), rig.zones.clone(), rig.certs.clone());

    let info = issuer.issue(&binding).await.unwrap();
    assert_eq!(info.names, vec!["eu.acme.com", "*.eu.acme.com"]);
    assert_eq!(ca.orders.lock()[0], (vec![host.to_string(), format!("*.{host}")], ChallengeKind::Dns01));
    assert!(rig.certs.get("api.eu.acme.com").is_some());
    assert!(rig.zones.challenges(&acme_challenge_name(host)).is_empty(), "challenge withdrawn");
}

#[tokio::test]
async fn only_verified_bindings_get_certificates() {
    let rig = rig().await;
    let ca = Arc::new(TestCa::new(rig.http01.clone(), None));
    let issuer = Issuer::new(ca.clone(), rig.http01.clone(), rig.zones.clone(), rig.certs.clone());
    let mut r = Registry::new([3; 32], 3600);
    let pending = r.add("p", "shop.acme.com", DomainMode::Cname, 0).unwrap();
    assert!(matches!(issuer.issue(&pending).await, Err(IssueError::NotVerified { state: BindingState::Pending, .. })));
    let (_, mut grace) = verified(DomainMode::Cname, "shop.acme.com").await;
    grace.state = BindingState::Grace;
    assert!(matches!(issuer.issue(&grace).await, Err(IssueError::NotVerified { .. })));
    assert!(ca.orders.lock().is_empty(), "no order placed");

    // DNS-01 needs our zone server to answer for the zone.
    let (_, delegated) = verified(DomainMode::NsDelegated, "eu.acme.com").await;
    assert!(matches!(issuer.issue(&delegated).await, Err(IssueError::ZoneNotServed(_))));
}

#[tokio::test]
async fn failed_or_incomplete_issuance_stores_nothing_and_withdraws_challenges() {
    let rig = rig().await;
    let host = "eu.acme.com";
    let (registry, binding) = verified(DomainMode::NsDelegated, host).await;
    rig.zones.sync(registry.zones(&edge())).unwrap();
    let dns = HickoryResolver::with_servers(&[rig.server.local_addr()]).unwrap();

    let mut short = TestCa::new(rig.http01.clone(), Some(dns.clone()));
    short.sign_names = Some(vec![host.to_string()]);
    let issuer = Issuer::new(Arc::new(short), rig.http01.clone(), rig.zones.clone(), rig.certs.clone());
    assert!(matches!(issuer.issue(&binding).await, Err(IssueError::Incomplete { missing }) if missing == vec![format!("*.{host}")]));

    // The CA fails validation (it has no way to see our zone).
    let blind = TestCa::new(rig.http01.clone(), None);
    let issuer = Issuer::new(Arc::new(blind), rig.http01.clone(), rig.zones.clone(), rig.certs.clone());
    assert!(matches!(issuer.issue(&binding).await, Err(IssueError::Acme(_))));

    assert!(rig.certs.list().is_empty());
    assert!(rig.zones.challenges(&acme_challenge_name(host)).is_empty());
}
