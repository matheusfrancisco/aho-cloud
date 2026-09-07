//! Host networking for guests: one /24 per host, one routed tap per replica, and an nftables
//! ruleset under which a guest can reach only the host's egress proxy and DNS resolver.
//!
//! Each tap carries the gateway address as a `/32` and a `/32` route to its guest, so every
//! guest packet is routed (never bridged) through the host and passes the ruleset.

use std::collections::BTreeSet;
use std::fmt;
use std::fmt::Write as _;
use std::net::Ipv4Addr;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Prefix of every guest tap device; the ruleset matches `iifname "ahotap*"`.
pub const TAP_PREFIX: &str = "ahotap";
/// Dummy interface that holds the gateway address so the egress proxy can bind before any tap exists.
pub const GATEWAY_DEVICE: &str = "aho0";
/// Name of the nftables table the agent owns (replaced atomically on every change).
pub const NFT_TABLE: &str = "aho_cloud";

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum NetError {
    #[error("invalid CIDR `{0}`")]
    Cidr(String),
    #[error("guest subnet must be a /24 network, got `{0}`")]
    Subnet(String),
    #[error("no free guest address in {0}/24")]
    Exhausted(Ipv4Addr),
}

/// An IPv4 network in CIDR notation, e.g. `10.0.0.0/8`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Ipv4Cidr {
    pub network: Ipv4Addr,
    pub prefix_len: u8,
}

impl Ipv4Cidr {
    fn mask(self) -> u32 {
        match self.prefix_len {
            0 => 0,
            n => u32::MAX << (32 - u32::from(n)),
        }
    }

    pub fn contains(self, ip: Ipv4Addr) -> bool {
        u32::from(ip) & self.mask() == u32::from(self.network)
    }
}

impl FromStr for Ipv4Cidr {
    type Err = NetError;

    /// Parses `a.b.c.d/n`; host bits must be zero.
    fn from_str(s: &str) -> Result<Self, NetError> {
        let bad = || NetError::Cidr(s.to_owned());
        let (addr, len) = s.split_once('/').ok_or_else(bad)?;
        let network: Ipv4Addr = addr.parse().map_err(|_| bad())?;
        let prefix_len: u8 = len.parse().map_err(|_| bad())?;
        if prefix_len > 32 {
            return Err(bad());
        }
        let cidr = Ipv4Cidr { network, prefix_len };
        if u32::from(network) & !cidr.mask() != 0 {
            return Err(bad());
        }
        Ok(cidr)
    }
}

impl fmt::Display for Ipv4Cidr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.network, self.prefix_len)
    }
}

/// One replica's network identity on this host.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lease {
    pub ip: Ipv4Addr,
    pub gateway: Ipv4Addr,
    /// The guest's view of the subnet (always 24).
    pub prefix_len: u8,
    /// Locally administered unicast MAC derived from `ip`.
    pub mac: String,
    /// Host tap device, at most 15 bytes (IFNAMSIZ).
    pub tap: String,
}

impl Lease {
    /// Dotted netmask for the kernel `ip=` boot parameter.
    pub fn netmask(&self) -> Ipv4Addr {
        let bits = match self.prefix_len {
            0 => 0,
            n => u32::MAX << (32 - u32::from(n.min(32))),
        };
        Ipv4Addr::from(bits)
    }
}

/// `02:fc:` + the four address octets: locally administered, unicast, unique per address.
pub fn mac_for(ip: Ipv4Addr) -> String {
    let [a, b, c, d] = ip.octets();
    format!("02:fc:{a:02x}:{b:02x}:{c:02x}:{d:02x}")
}

/// Tap device for the guest at `ip` (host octet as suffix, so names are reused with addresses).
pub fn tap_for(ip: Ipv4Addr) -> String {
    format!("{TAP_PREFIX}{}", ip.octets()[3])
}

/// Hands out guest addresses from one /24: `.1` is the host gateway, `.2`–`.254` are guests.
/// The lowest free address is always returned, so released addresses are reused first.
#[derive(Clone, Debug)]
pub struct Allocator {
    network: Ipv4Addr,
    used: BTreeSet<u8>,
}

/// First and last guest host octets.
const FIRST_GUEST: u8 = 2;
const LAST_GUEST: u8 = 254;

impl Allocator {
    /// `subnet` must be a `/24` network address, e.g. `10.77.0.0/24`.
    pub fn new(subnet: Ipv4Cidr) -> Result<Self, NetError> {
        if subnet.prefix_len != 24 {
            return Err(NetError::Subnet(subnet.to_string()));
        }
        Ok(Allocator { network: subnet.network, used: BTreeSet::new() })
    }

    pub fn subnet(&self) -> Ipv4Cidr {
        Ipv4Cidr { network: self.network, prefix_len: 24 }
    }

    pub fn gateway(&self) -> Ipv4Addr {
        self.host(1)
    }

    fn host(&self, octet: u8) -> Ipv4Addr {
        let [a, b, c, _] = self.network.octets();
        Ipv4Addr::new(a, b, c, octet)
    }

    /// Reserves the lowest free guest address.
    pub fn allocate(&mut self) -> Result<Lease, NetError> {
        let octet = (FIRST_GUEST..=LAST_GUEST).find(|o| !self.used.contains(o)).ok_or(NetError::Exhausted(self.network))?;
        self.used.insert(octet);
        let ip = self.host(octet);
        Ok(Lease { ip, gateway: self.gateway(), prefix_len: 24, mac: mac_for(ip), tap: tap_for(ip) })
    }

    /// Returns `ip` to the pool; `false` if it was not leased from this allocator.
    pub fn release(&mut self, ip: Ipv4Addr) -> bool {
        if !self.subnet().contains(ip) {
            return false;
        }
        self.used.remove(&ip.octets()[3])
    }

    pub fn in_use(&self) -> usize {
        self.used.len()
    }

    /// Number of guest addresses the subnet holds.
    pub fn capacity() -> usize {
        usize::from(LAST_GUEST - FIRST_GUEST) + 1
    }
}

/// What the host firewall must know to confine guests.
#[derive(Clone, Debug)]
pub struct FirewallPlan {
    pub gateway: Ipv4Addr,
    /// TCP port of the egress proxy on `gateway`.
    pub proxy_port: u16,
    /// UDP/TCP port of the DNS resolver on `gateway`.
    pub dns_port: u16,
    /// Networks allowed to open connections to guests (e.g. remote edge nodes). Traffic from
    /// the host itself (a local edge, health checks) is always allowed.
    pub inbound_sources: Vec<Ipv4Cidr>,
    /// `(tap, guest address)` of every running replica; anything else from a tap is spoofed.
    pub guests: Vec<(String, Ipv4Addr)>,
}

/// The complete `inet aho_cloud` table, written so `nft -f` replaces the previous one atomically.
///
/// Guests may send only: replies on connections the host or an inbound source opened, and new
/// connections to the proxy and DNS on the gateway. Guests never reach each other, the
/// internet, or any other host address; IPv6 from guests is dropped.
pub fn nft_ruleset(plan: &FirewallPlan) -> String {
    let taps = format!("\"{TAP_PREFIX}*\"");
    let gw = plan.gateway;
    let mut out = String::new();
    // `table …{}` then `delete` makes the load idempotent whether or not the table exists.
    let _ = writeln!(out, "table inet {NFT_TABLE} {{}}");
    let _ = writeln!(out, "delete table inet {NFT_TABLE}");
    let _ = writeln!(out, "table inet {NFT_TABLE} {{");

    let _ = writeln!(out, "\tchain prerouting {{");
    let _ = writeln!(out, "\t\ttype filter hook prerouting priority raw; policy accept;");
    let _ = writeln!(out, "\t\tiifname {taps} meta nfproto ipv6 drop");
    for (tap, ip) in &plan.guests {
        let _ = writeln!(out, "\t\tiifname \"{tap}\" ip saddr != {ip} drop");
    }
    // A tap with no running guest must not carry traffic at all.
    if plan.guests.is_empty() {
        let _ = writeln!(out, "\t\tiifname {taps} drop");
    } else {
        let known: Vec<String> = plan.guests.iter().map(|(t, _)| format!("\"{t}\"")).collect();
        let _ = writeln!(out, "\t\tiifname {taps} iifname != {{ {} }} drop", known.join(", "));
    }
    let _ = writeln!(out, "\t}}");

    let _ = writeln!(out, "\tchain input {{");
    let _ = writeln!(out, "\t\ttype filter hook input priority filter; policy accept;");
    let _ = writeln!(out, "\t\tiifname {taps} ct state established,related accept");
    let _ = writeln!(out, "\t\tiifname {taps} ip daddr {gw} tcp dport {} accept", plan.proxy_port);
    let _ = writeln!(out, "\t\tiifname {taps} ip daddr {gw} udp dport {} accept", plan.dns_port);
    let _ = writeln!(out, "\t\tiifname {taps} ip daddr {gw} tcp dport {} accept", plan.dns_port);
    let _ = writeln!(out, "\t\tiifname {taps} drop");
    let _ = writeln!(out, "\t}}");

    let _ = writeln!(out, "\tchain forward {{");
    let _ = writeln!(out, "\t\ttype filter hook forward priority filter; policy accept;");
    let _ = writeln!(out, "\t\tiifname {taps} oifname != {taps} ct state established,related accept");
    let _ = writeln!(out, "\t\tiifname {taps} drop");
    let _ = writeln!(out, "\t\toifname {taps} ct state established,related accept");
    if !plan.inbound_sources.is_empty() {
        let sources: Vec<String> = plan.inbound_sources.iter().map(ToString::to_string).collect();
        let _ = writeln!(out, "\t\toifname {taps} ip saddr {{ {} }} ct state new accept", sources.join(", "));
    }
    let _ = writeln!(out, "\t\toifname {taps} drop");
    let _ = writeln!(out, "\t}}");

    let _ = writeln!(out, "}}");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alloc() -> Allocator {
        Allocator::new("10.77.3.0/24".parse().unwrap()).unwrap()
    }

    #[test]
    fn cidr_parse_rejects_host_bits_and_bad_lengths() {
        assert_eq!("10.0.0.0/8".parse::<Ipv4Cidr>().unwrap().to_string(), "10.0.0.0/8");
        assert!("10.0.0.1/8".parse::<Ipv4Cidr>().is_err());
        assert!("10.0.0.0/33".parse::<Ipv4Cidr>().is_err());
        assert!("10.0.0.0".parse::<Ipv4Cidr>().is_err());
        let all: Ipv4Cidr = "0.0.0.0/0".parse().unwrap();
        assert!(all.contains(Ipv4Addr::new(8, 8, 8, 8)));
        let net: Ipv4Cidr = "192.168.4.0/22".parse().unwrap();
        assert!(net.contains(Ipv4Addr::new(192, 168, 7, 255)));
        assert!(!net.contains(Ipv4Addr::new(192, 168, 8, 0)));
    }

    #[test]
    fn allocator_requires_a_slash_24() {
        assert_eq!(Allocator::new("10.77.0.0/16".parse().unwrap()).unwrap_err(), NetError::Subnet("10.77.0.0/16".into()));
    }

    #[test]
    fn leases_skip_network_gateway_and_broadcast_and_carry_identity() {
        let mut a = alloc();
        let first = a.allocate().unwrap();
        assert_eq!(first.ip, Ipv4Addr::new(10, 77, 3, 2));
        assert_eq!(first.gateway, Ipv4Addr::new(10, 77, 3, 1));
        assert_eq!(first.mac, "02:fc:0a:4d:03:02");
        assert_eq!(first.tap, "ahotap2");
        assert_eq!(first.netmask(), Ipv4Addr::new(255, 255, 255, 0));
        let mut all = vec![first];
        while let Ok(l) = a.allocate() {
            all.push(l);
        }
        assert_eq!(all.len(), Allocator::capacity());
        assert_eq!(all.last().unwrap().ip, Ipv4Addr::new(10, 77, 3, 254));
        assert!(all.iter().all(|l| l.tap.len() <= 15));
        let macs: BTreeSet<_> = all.iter().map(|l| l.mac.clone()).collect();
        assert_eq!(macs.len(), all.len());
    }

    #[test]
    fn exhaustion_then_reuse_of_the_lowest_released_address() {
        let mut a = alloc();
        for _ in 0..Allocator::capacity() {
            a.allocate().unwrap();
        }
        assert_eq!(a.allocate().unwrap_err(), NetError::Exhausted(Ipv4Addr::new(10, 77, 3, 0)));
        assert!(a.release(Ipv4Addr::new(10, 77, 3, 90)));
        assert!(a.release(Ipv4Addr::new(10, 77, 3, 17)));
        assert!(!a.release(Ipv4Addr::new(10, 77, 3, 17)), "double release");
        assert!(!a.release(Ipv4Addr::new(10, 77, 4, 17)), "foreign subnet");
        assert_eq!(a.allocate().unwrap().ip, Ipv4Addr::new(10, 77, 3, 17));
        assert_eq!(a.allocate().unwrap().ip, Ipv4Addr::new(10, 77, 3, 90));
        assert!(a.allocate().is_err());
        assert_eq!(a.in_use(), Allocator::capacity());
    }

    #[test]
    fn ruleset_only_opens_proxy_and_dns_and_pins_each_tap_to_its_address() {
        let plan = FirewallPlan {
            gateway: Ipv4Addr::new(10, 77, 0, 1),
            proxy_port: 3128,
            dns_port: 53,
            inbound_sources: vec!["172.20.0.0/16".parse().unwrap()],
            guests: vec![("ahotap2".into(), Ipv4Addr::new(10, 77, 0, 2)), ("ahotap3".into(), Ipv4Addr::new(10, 77, 0, 3))],
        };
        let rules = nft_ruleset(&plan);
        let lines: Vec<&str> = rules.lines().map(str::trim).collect();
        assert_eq!(lines[0], "table inet aho_cloud {}");
        assert_eq!(lines[1], "delete table inet aho_cloud");
        for expected in [
            "iifname \"ahotap2\" ip saddr != 10.77.0.2 drop",
            "iifname \"ahotap3\" ip saddr != 10.77.0.3 drop",
            "iifname \"ahotap*\" iifname != { \"ahotap2\", \"ahotap3\" } drop",
            "iifname \"ahotap*\" meta nfproto ipv6 drop",
            "iifname \"ahotap*\" ip daddr 10.77.0.1 tcp dport 3128 accept",
            "iifname \"ahotap*\" ip daddr 10.77.0.1 udp dport 53 accept",
            "iifname \"ahotap*\" ip daddr 10.77.0.1 tcp dport 53 accept",
            "oifname \"ahotap*\" ip saddr { 172.20.0.0/16 } ct state new accept",
        ] {
            assert!(lines.contains(&expected), "missing `{expected}` in\n{rules}");
        }
        // Every accept for guest-originated traffic is either a reply or proxy/DNS on the gateway.
        for line in lines.iter().filter(|l| l.starts_with("iifname \"ahotap*\"") && l.ends_with("accept")) {
            assert!(
                line.contains("ct state established,related") || line.contains("ip daddr 10.77.0.1"),
                "unexpected guest accept: {line}"
            );
        }
        // Input and forward both end in a drop for guest traffic, after their accepts.
        let input_drop = lines.iter().position(|l| *l == "iifname \"ahotap*\" drop").unwrap();
        let proxy = lines.iter().position(|l| l.contains("dport 3128")).unwrap();
        assert!(proxy < input_drop);
        assert_eq!(lines.iter().filter(|l| **l == "iifname \"ahotap*\" drop").count(), 2);
        assert!(lines.contains(&"oifname \"ahotap*\" drop"));
    }

    #[test]
    fn ruleset_without_guests_or_sources_drops_all_tap_traffic() {
        let plan =
            FirewallPlan { gateway: Ipv4Addr::new(10, 77, 0, 1), proxy_port: 3128, dns_port: 53, inbound_sources: vec![], guests: vec![] };
        let rules = nft_ruleset(&plan);
        assert!(rules.lines().any(|l| l.trim() == "iifname \"ahotap*\" drop"));
        assert!(!rules.contains("ct state new accept"));
        assert!(!rules.contains("saddr !="));
    }
}
