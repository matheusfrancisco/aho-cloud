//! DNS lookups behind a small trait, with the real adapter on `hickory-resolver`.
//!
//! Contract for implementations: a name that does not exist or has no records of the asked type
//! is `Ok(empty)` / `Ok(None)`; `Err` is reserved for lookups that could not be answered
//! (timeouts, SERVFAIL, refused). The registry never changes a binding's state on `Err`.

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use futures::future::BoxFuture;
use hickory_resolver::TokioResolver;
use hickory_resolver::config::{ConnectionConfig, LookupIpStrategy, NameServerConfig, ResolverConfig, ResolverOpts};
use hickory_resolver::net::NetError;
use hickory_resolver::net::runtime::TokioRuntimeProvider;
use hickory_resolver::proto::rr::{RData, RecordType};

/// A lookup that could not be answered.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("DNS lookup of {name} failed: {reason}")]
pub struct DnsError {
    pub name: String,
    pub reason: String,
}

impl DnsError {
    pub fn new(name: impl Into<String>, reason: impl Into<String>) -> Self {
        Self { name: name.into(), reason: reason.into() }
    }
}

/// The four lookups domain verification needs. Names are passed without a trailing dot;
/// returned names are lowercase without a trailing dot.
pub trait Resolver: Send + Sync {
    /// TXT strings at `name` (the character-strings of each record concatenated).
    fn txt(&self, name: &str) -> BoxFuture<'_, Result<Vec<String>, DnsError>>;
    /// The CNAME target at `name`, if `name` is an alias.
    fn cname(&self, name: &str) -> BoxFuture<'_, Result<Option<String>, DnsError>>;
    /// A and AAAA addresses of `name`, following aliases.
    fn addrs(&self, name: &str) -> BoxFuture<'_, Result<Vec<IpAddr>, DnsError>>;
    /// NS targets at `name`.
    fn ns(&self, name: &str) -> BoxFuture<'_, Result<Vec<String>, DnsError>>;
}

/// Lowercase, trimmed, no trailing dot: the canonical form for comparing DNS names.
pub fn normalize_name(name: &str) -> String {
    name.trim().trim_end_matches('.').to_ascii_lowercase()
}

/// [`Resolver`] backed by a hickory stub resolver. Caching is off: verification must see the
/// customer's latest records, not what we looked up last time.
#[derive(Clone)]
pub struct HickoryResolver {
    inner: TokioResolver,
}

impl std::fmt::Debug for HickoryResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HickoryResolver").finish_non_exhaustive()
    }
}

impl HickoryResolver {
    /// Uses the host's resolver configuration (`/etc/resolv.conf` on Unix).
    pub fn system() -> Result<Self, DnsError> {
        let mut builder = TokioResolver::builder_tokio().map_err(|e| DnsError::new("<system config>", e.to_string()))?;
        tune(builder.options_mut());
        let inner = builder.build().map_err(|e| DnsError::new("<system config>", e.to_string()))?;
        Ok(Self { inner })
    }

    /// Sends every query to `servers` (UDP, falling back to TCP), e.g. a known recursive
    /// resolver or, in tests, an in-process server.
    pub fn with_servers(servers: &[SocketAddr]) -> Result<Self, DnsError> {
        let name_servers = servers
            .iter()
            .map(|addr| {
                let mut udp = ConnectionConfig::udp();
                udp.port = addr.port();
                let mut tcp = ConnectionConfig::tcp();
                tcp.port = addr.port();
                let mut ns = NameServerConfig::udp_and_tcp(addr.ip());
                ns.connections = vec![udp, tcp];
                ns
            })
            .collect();
        let config = ResolverConfig::from_name_servers(name_servers);
        let mut builder = TokioResolver::builder_with_config(config, TokioRuntimeProvider::default());
        tune(builder.options_mut());
        let inner = builder.build().map_err(|e| DnsError::new("<servers>", e.to_string()))?;
        Ok(Self { inner })
    }
}

fn tune(opts: &mut ResolverOpts) {
    opts.cache_size = 0;
    opts.ip_strategy = LookupIpStrategy::Ipv4AndIpv6;
    opts.timeout = Duration::from_secs(5);
    opts.attempts = 2;
}

/// Absolute form so search domains never apply.
fn fqdn(name: &str) -> String {
    format!("{}.", normalize_name(name))
}

/// `Ok(())` when the error only means "nothing there" (NXDOMAIN or no records of that type).
fn absent(name: &str, e: NetError) -> Result<(), DnsError> {
    if e.is_no_records_found() { Ok(()) } else { Err(DnsError::new(name, e.to_string())) }
}

impl Resolver for HickoryResolver {
    fn txt(&self, name: &str) -> BoxFuture<'_, Result<Vec<String>, DnsError>> {
        let name = name.to_owned();
        Box::pin(async move {
            match self.inner.lookup(fqdn(&name), RecordType::TXT).await {
                Ok(lookup) => Ok(lookup
                    .answers()
                    .iter()
                    .filter_map(|r| match &r.data {
                        RData::TXT(txt) => Some(txt.txt_data.iter().map(|part| String::from_utf8_lossy(part)).collect::<String>()),
                        _ => None,
                    })
                    .collect()),
                Err(e) => absent(&name, e).map(|()| Vec::new()),
            }
        })
    }

    fn cname(&self, name: &str) -> BoxFuture<'_, Result<Option<String>, DnsError>> {
        let name = name.to_owned();
        Box::pin(async move {
            match self.inner.lookup(fqdn(&name), RecordType::CNAME).await {
                Ok(lookup) => Ok(lookup.answers().iter().find_map(|r| match &r.data {
                    RData::CNAME(target) => Some(normalize_name(&target.0.to_ascii())),
                    _ => None,
                })),
                Err(e) => absent(&name, e).map(|()| None),
            }
        })
    }

    fn addrs(&self, name: &str) -> BoxFuture<'_, Result<Vec<IpAddr>, DnsError>> {
        let name = name.to_owned();
        Box::pin(async move {
            match self.inner.lookup_ip(fqdn(&name)).await {
                Ok(lookup) => Ok(lookup.iter().collect()),
                Err(e) => absent(&name, e).map(|()| Vec::new()),
            }
        })
    }

    fn ns(&self, name: &str) -> BoxFuture<'_, Result<Vec<String>, DnsError>> {
        let name = name.to_owned();
        Box::pin(async move {
            match self.inner.lookup(fqdn(&name), RecordType::NS).await {
                Ok(lookup) => Ok(lookup
                    .answers()
                    .iter()
                    .filter_map(|r| match &r.data {
                        RData::NS(ns) => Some(normalize_name(&ns.0.to_ascii())),
                        _ => None,
                    })
                    .collect()),
                Err(e) => absent(&name, e).map(|()| Vec::new()),
            }
        })
    }
}
