//! Shared test fixtures: an in-memory DNS that tests edit between checks.

#![allow(dead_code)]

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use cloud_domains::{DnsError, EdgeTarget, Resolver};
use futures::future::BoxFuture;
use parking_lot::Mutex;

pub fn edge() -> EdgeTarget {
    EdgeTarget {
        cname: "edge.aho.cloud".into(),
        ipv4: vec![Ipv4Addr::new(203, 0, 113, 10)],
        ipv6: vec![Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 10)],
        nameservers: vec!["ns1.aho.cloud".into(), "ns2.aho.cloud".into()],
    }
}

#[derive(Default)]
struct Data {
    txt: HashMap<String, Vec<String>>,
    cname: HashMap<String, String>,
    addrs: HashMap<String, Vec<IpAddr>>,
    ns: HashMap<String, Vec<String>>,
    failing: bool,
}

/// The customer's DNS as the internet currently sees it.
#[derive(Default)]
pub struct FakeDns {
    data: Mutex<Data>,
}

impl FakeDns {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn txt(&self, name: &str, values: &[&str]) -> &Self {
        self.data.lock().txt.insert(name.into(), values.iter().map(|v| v.to_string()).collect());
        self
    }

    pub fn cname(&self, name: &str, target: &str) -> &Self {
        self.data.lock().cname.insert(name.into(), target.into());
        self
    }

    pub fn addrs(&self, name: &str, addrs: &[IpAddr]) -> &Self {
        self.data.lock().addrs.insert(name.into(), addrs.to_vec());
        self
    }

    pub fn ns(&self, name: &str, ns: &[&str]) -> &Self {
        self.data.lock().ns.insert(name.into(), ns.iter().map(|v| v.to_string()).collect());
        self
    }

    /// Removes every record at `name`.
    pub fn clear(&self, name: &str) -> &Self {
        let mut d = self.data.lock();
        d.txt.remove(name);
        d.cname.remove(name);
        d.addrs.remove(name);
        d.ns.remove(name);
        self
    }

    /// Every lookup fails (SERVFAIL-like) while set.
    pub fn failing(&self, on: bool) -> &Self {
        self.data.lock().failing = on;
        self
    }

    fn answer<T: Clone + Default>(&self, name: &str, pick: impl Fn(&Data) -> Option<T>) -> Result<T, DnsError> {
        let d = self.data.lock();
        if d.failing {
            return Err(DnsError::new(name, "SERVFAIL"));
        }
        Ok(pick(&d).unwrap_or_default())
    }
}

impl Resolver for FakeDns {
    fn txt(&self, name: &str) -> BoxFuture<'_, Result<Vec<String>, DnsError>> {
        let r = self.answer(name, |d| d.txt.get(name).cloned());
        Box::pin(async move { r })
    }

    fn cname(&self, name: &str) -> BoxFuture<'_, Result<Option<String>, DnsError>> {
        let r = self.answer(name, |d| Some(d.cname.get(name).cloned()));
        Box::pin(async move { r })
    }

    fn addrs(&self, name: &str) -> BoxFuture<'_, Result<Vec<IpAddr>, DnsError>> {
        let r = self.answer(name, |d| d.addrs.get(name).cloned());
        Box::pin(async move { r })
    }

    fn ns(&self, name: &str) -> BoxFuture<'_, Result<Vec<String>, DnsError>> {
        let r = self.answer(name, |d| d.ns.get(name).cloned());
        Box::pin(async move { r })
    }
}
