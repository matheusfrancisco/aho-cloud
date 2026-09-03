//! Domain bindings: who may serve a hostname, how the customer proves it, and what happens when
//! the proof or the routing goes away.
//!
//! One hostname has at most one *live* binding (`Pending`, `Verified`, `Grace`) and remembers the
//! last *retired* one (`Detached`, `Tombstoned`). A retired binding keeps governing the hostname —
//! the edge serves its tombstone page — until a new live binding verifies with its own TXT token.

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use serde::{Deserialize, Serialize};

use crate::authority::Zone;
use crate::resolver::{DnsError, Resolver, normalize_name};
use crate::token::{nonce, token};

pub use cloud_api::{DnsRecord as Record, DomainState as BindingState};
pub use cloud_contract::manifest::DomainMode;

/// First label of the verification TXT record: `_aho-cloud.<hostname>`.
pub const TXT_LABEL: &str = "_aho-cloud";
/// Prefix of the verification TXT value: `aho-cloud-verify=<token>`.
pub const TXT_VALUE_PREFIX: &str = "aho-cloud-verify=";

/// Name of the verification TXT record for `hostname`.
pub fn txt_name(hostname: &str) -> String {
    format!("{TXT_LABEL}.{hostname}")
}

/// Value of the verification TXT record for `token`.
pub fn txt_value(token: &str) -> String {
    format!("{TXT_VALUE_PREFIX}{token}")
}

/// Where customer DNS must point: the edge's CNAME target, its stable addresses, and the
/// nameservers of the authoritative zone server.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EdgeTarget {
    /// `Cname` mode target, e.g. `edge.aho.cloud`.
    pub cname: String,
    /// `Apex` mode A records; also accepted for flattened `Cname` records.
    pub ipv4: Vec<Ipv4Addr>,
    /// `Apex` mode AAAA records.
    pub ipv6: Vec<Ipv6Addr>,
    /// `NsDelegated` mode NS targets: our zone servers.
    pub nameservers: Vec<String>,
}

impl EdgeTarget {
    fn has_addr(&self, addr: &IpAddr) -> bool {
        match addr {
            IpAddr::V4(v4) => self.ipv4.contains(v4),
            IpAddr::V6(v6) => self.ipv6.contains(v6),
        }
    }
}

/// What the last successful check saw.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Observed {
    pub at: u64,
    /// `_aho-cloud.<hostname>` carried this binding's token.
    pub txt: bool,
    /// The mode's routing record pointed at the edge.
    pub routing: bool,
}

/// One project's claim on one hostname.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Binding {
    pub hostname: String,
    pub project: String,
    pub mode: DomainMode,
    pub state: BindingState,
    /// Expected in `_aho-cloud.<hostname>` as `aho-cloud-verify=<token>`.
    pub token: String,
    /// Per-binding randomness mixed into `token`.
    pub nonce: String,
    pub created_at: u64,
    /// When `state` last changed.
    pub changed_at: u64,
    /// When the binding last entered `Verified`.
    pub verified_at: Option<u64>,
    /// When the binding entered `Grace`; cleared on restore.
    pub grace_since: Option<u64>,
    /// `NsDelegated` only: when the TXT was first seen. After delegation the zone is answered by
    /// our own server, which never publishes a pending binding's token, so the proof has to come
    /// from the customer's parent zone before the NS records take effect — and it sticks.
    pub txt_proven_at: Option<u64>,
    pub observed: Option<Observed>,
}

impl Binding {
    /// `Pending`, `Verified` or `Grace`: still subject to checks.
    pub fn is_live(&self) -> bool {
        matches!(self.state, BindingState::Pending | BindingState::Verified | BindingState::Grace)
    }

    /// `Verified` or `Grace`: the edge routes this hostname to the project.
    pub fn is_serving(&self) -> bool {
        matches!(self.state, BindingState::Verified | BindingState::Grace)
    }
}

/// A state change made by [`Registry::check`] / [`Registry::recheck_all`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Transition {
    pub hostname: String,
    pub project: String,
    pub from: BindingState,
    pub to: BindingState,
    pub at: u64,
}

/// What the edge does with a hostname.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Serving<'a> {
    /// Route to this binding's project (`Verified` or `Grace`).
    Project(&'a Binding),
    /// Serve the tombstone page for this retired binding; never another tenant.
    Tombstone(&'a Binding),
    /// Nothing governs the hostname (unknown, or only a pending claim).
    Unbound,
}

/// Why a registry operation was refused.
#[derive(Debug, thiserror::Error)]
pub enum DomainError {
    #[error("`{0}` is not a hostname (lowercase labels, at least two)")]
    InvalidHostname(String),
    /// Deliberately does not name the other project.
    #[error("{hostname} is bound to another project")]
    Taken { hostname: String },
    #[error("{hostname} is already bound to this project in {existing:?} mode; detach it before changing modes")]
    ModeMismatch { hostname: String, existing: DomainMode },
    #[error("{0} is not bound")]
    NotFound(String),
    #[error("no edge target configured; call Registry::set_edge")]
    NoEdgeTarget,
    #[error(transparent)]
    Dns(#[from] DnsError),
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Slot {
    live: Option<Binding>,
    retired: Option<Binding>,
}

impl Slot {
    fn governing(&self) -> Option<Serving<'_>> {
        match (&self.live, &self.retired) {
            (Some(live), _) if live.is_serving() => Some(Serving::Project(live)),
            (_, Some(retired)) => Some(Serving::Tombstone(retired)),
            _ => None,
        }
    }
}

/// All domain bindings. Serializes to a snapshot that includes the token secret (store it like
/// other control-plane secrets). The edge target is runtime configuration and is not persisted:
/// call [`Registry::set_edge`] after [`Registry::new`] or deserializing.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Registry {
    #[serde(with = "hex_secret")]
    secret: [u8; 32],
    grace_secs: u64,
    hosts: BTreeMap<String, Slot>,
    #[serde(skip)]
    edge: Option<EdgeTarget>,
}

impl std::fmt::Debug for Registry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Registry")
            .field("grace_secs", &self.grace_secs)
            .field("hosts", &self.hosts.len())
            .field("edge", &self.edge)
            .finish_non_exhaustive()
    }
}

impl Registry {
    /// `secret` keys the verification tokens; `grace_secs` is how long a binding keeps serving
    /// after its TXT or routing disappears.
    pub fn new(secret: [u8; 32], grace_secs: u64) -> Self {
        Self { secret, grace_secs, hosts: BTreeMap::new(), edge: None }
    }

    /// Sets where routing records must point for [`Registry::check`].
    pub fn set_edge(&mut self, edge: EdgeTarget) {
        self.edge = Some(edge);
    }

    /// The edge target set by [`Registry::set_edge`].
    pub fn edge(&self) -> Option<&EdgeTarget> {
        self.edge.as_ref()
    }

    /// Claims `hostname` for `project`. Idempotent for the same project and mode. Fails when
    /// another project holds a live binding; a retired (tombstoned/detached) binding does not
    /// block, but it keeps governing the hostname until the new binding verifies with its own,
    /// freshly-nonced token.
    pub fn add(&mut self, project: &str, hostname: &str, mode: DomainMode, now: u64) -> Result<Binding, DomainError> {
        let hostname = canonical_host(hostname)?;
        let slot = self.hosts.entry(hostname.clone()).or_default();
        if let Some(live) = &slot.live {
            if live.project != project {
                return Err(DomainError::Taken { hostname });
            }
            if live.mode != mode {
                return Err(DomainError::ModeMismatch { hostname, existing: live.mode });
            }
            return Ok(live.clone());
        }
        let nonce = nonce();
        let binding = Binding {
            token: token(&self.secret, project, &hostname, &nonce),
            nonce,
            hostname,
            project: project.to_owned(),
            mode,
            state: BindingState::Pending,
            created_at: now,
            changed_at: now,
            verified_at: None,
            grace_since: None,
            txt_proven_at: None,
            observed: None,
        };
        slot.live = Some(binding.clone());
        Ok(binding)
    }

    /// The project stops serving `hostname` (e.g. removed from its manifest): the live binding
    /// becomes `Detached` and governs the hostname as a tombstone.
    pub fn detach(&mut self, project: &str, hostname: &str, now: u64) -> Result<Binding, DomainError> {
        let hostname = canonical_host(hostname)?;
        let slot = self.hosts.get_mut(&hostname).ok_or_else(|| DomainError::NotFound(hostname.clone()))?;
        match &slot.live {
            Some(live) if live.project == project => {}
            _ => return Err(DomainError::NotFound(hostname)),
        }
        let binding = retire(slot, BindingState::Detached, now);
        binding.ok_or(DomainError::NotFound(hostname))
    }

    /// Tombstones every live binding of `project`; returns their hostnames.
    pub fn remove_project(&mut self, project: &str, now: u64) -> Vec<String> {
        let mut out = Vec::new();
        for (hostname, slot) in &mut self.hosts {
            if slot.live.as_ref().is_some_and(|b| b.project == project) {
                retire(slot, BindingState::Tombstoned, now);
                out.push(hostname.clone());
            }
        }
        out
    }

    /// The binding a hostname's status refers to: the live one, else the last retired one.
    pub fn get(&self, hostname: &str) -> Option<&Binding> {
        let slot = self.hosts.get(&normalize_name(hostname))?;
        slot.live.as_ref().or(slot.retired.as_ref())
    }

    /// Every binding of `project` (live, or retired when it has no live one for that hostname).
    pub fn for_project(&self, project: &str) -> Vec<&Binding> {
        self.hosts
            .values()
            .filter_map(|slot| match (&slot.live, &slot.retired) {
                (Some(live), _) if live.project == project => Some(live),
                (_, Some(retired)) if retired.project == project => Some(retired),
                _ => None,
            })
            .collect()
    }

    /// What the edge serves for `hostname`: an exact binding first, then the `*.zone` coverage of
    /// an `NsDelegated` binding one label up. A retired exact binding wins over zone coverage.
    pub fn serving(&self, hostname: &str) -> Serving<'_> {
        let hostname = normalize_name(hostname);
        if let Some(serving) = self.hosts.get(&hostname).and_then(Slot::governing) {
            return serving;
        }
        let parent = hostname.split_once('.').map(|(_, parent)| parent);
        match parent.and_then(|p| self.hosts.get(p)).and_then(Slot::governing) {
            Some(Serving::Project(b)) if b.mode == DomainMode::NsDelegated => Serving::Project(b),
            Some(Serving::Tombstone(b)) if b.mode == DomainMode::NsDelegated => Serving::Tombstone(b),
            _ => Serving::Unbound,
        }
    }

    /// The records the customer must create for `binding`, TXT first.
    pub fn records(&self, binding: &Binding, target: &EdgeTarget) -> Vec<Record> {
        let rec = |kind: &str, value: String| Record { kind: kind.into(), name: binding.hostname.clone(), value };
        let mut out = vec![Record { kind: "TXT".into(), name: txt_name(&binding.hostname), value: txt_value(&binding.token) }];
        match binding.mode {
            DomainMode::Cname => out.push(rec("CNAME", target.cname.clone())),
            DomainMode::Apex => {
                out.extend(target.ipv4.iter().map(|ip| rec("A", ip.to_string())));
                out.extend(target.ipv6.iter().map(|ip| rec("AAAA", ip.to_string())));
            }
            DomainMode::NsDelegated => out.extend(target.nameservers.iter().map(|ns| rec("NS", ns.clone()))),
        }
        out
    }

    /// The API view of `binding` with its records.
    pub fn view(&self, binding: &Binding, target: &EdgeTarget) -> cloud_api::DomainView {
        cloud_api::DomainView {
            hostname: binding.hostname.clone(),
            mode: binding.mode,
            state: binding.state,
            records: self.records(binding, target),
        }
    }

    /// Zones the authoritative server must answer: every hostname whose live or retired binding
    /// is `NsDelegated`. The verification TXT is published only for a live `Verified`/`Grace`
    /// binding, so a dangling delegation can never verify a new claimant.
    pub fn zones(&self, target: &EdgeTarget) -> Vec<Zone> {
        self.hosts
            .iter()
            .filter_map(|(hostname, slot)| {
                let delegated = |b: &Option<Binding>| b.as_ref().is_some_and(|b| b.mode == DomainMode::NsDelegated);
                if !delegated(&slot.live) && !delegated(&slot.retired) {
                    return None;
                }
                let verify_token =
                    slot.live.as_ref().filter(|b| b.mode == DomainMode::NsDelegated && b.is_serving()).map(|b| b.token.clone());
                Some(Zone {
                    apex: hostname.clone(),
                    ipv4: target.ipv4.clone(),
                    ipv6: target.ipv6.clone(),
                    nameservers: target.nameservers.clone(),
                    verify_token,
                })
            })
            .collect()
    }

    /// Looks up the live binding of `hostname` and advances its state. A failed lookup returns
    /// `Err` and changes nothing. Retired bindings are terminal: their state is returned as is.
    pub async fn check(&mut self, hostname: &str, resolver: &dyn Resolver, now: u64) -> Result<BindingState, DomainError> {
        let hostname = normalize_name(hostname);
        self.check_one(&hostname, resolver, now).await.map(|(state, _)| state)
    }

    /// Checks every live binding; returns the transitions made. Hostnames whose lookups fail are
    /// skipped (logged) and keep their state.
    pub async fn recheck_all(&mut self, resolver: &dyn Resolver, now: u64) -> Vec<Transition> {
        let hosts: Vec<String> = self.hosts.iter().filter(|(_, s)| s.live.is_some()).map(|(h, _)| h.clone()).collect();
        let mut out = Vec::new();
        for hostname in hosts {
            match self.check_one(&hostname, resolver, now).await {
                Ok((_, Some(t))) => out.push(t),
                Ok((_, None)) => {}
                Err(error) => tracing::warn!(%hostname, %error, "domain recheck skipped"),
            }
        }
        out
    }

    async fn check_one(
        &mut self,
        hostname: &str,
        resolver: &dyn Resolver,
        now: u64,
    ) -> Result<(BindingState, Option<Transition>), DomainError> {
        let edge = self.edge.clone().ok_or(DomainError::NoEdgeTarget)?;
        let slot = self.hosts.get(hostname).ok_or_else(|| DomainError::NotFound(hostname.to_owned()))?;
        let (mode, token) = match (&slot.live, &slot.retired) {
            (Some(live), _) => (live.mode, live.token.clone()),
            (None, Some(retired)) => return Ok((retired.state, None)),
            (None, None) => return Err(DomainError::NotFound(hostname.to_owned())),
        };
        let seen = observe(resolver, hostname, mode, &token, &edge, now).await?;
        let grace_secs = self.grace_secs;
        let slot = self.hosts.get_mut(hostname).ok_or_else(|| DomainError::NotFound(hostname.to_owned()))?;
        let Some(live) = slot.live.as_mut() else {
            return Err(DomainError::NotFound(hostname.to_owned()));
        };
        let from = live.state;
        let to = advance(live, seen, grace_secs);
        let transition =
            (from != to).then(|| Transition { hostname: hostname.to_owned(), project: live.project.clone(), from, to, at: now });
        match to {
            BindingState::Detached => {
                retire(slot, BindingState::Detached, now);
            }
            // The new binding proved itself: the previous tombstone stops governing.
            BindingState::Verified if from == BindingState::Pending => slot.retired = None,
            _ => {}
        }
        Ok((to, transition))
    }
}

/// Moves the live binding to `retired` in `state`, replacing any older retired binding.
fn retire(slot: &mut Slot, state: BindingState, now: u64) -> Option<Binding> {
    let mut binding = slot.live.take()?;
    if binding.state != state {
        binding.state = state;
        binding.changed_at = now;
    }
    binding.grace_since = None;
    slot.retired = Some(binding.clone());
    Some(binding)
}

async fn observe(
    resolver: &dyn Resolver,
    hostname: &str,
    mode: DomainMode,
    token: &str,
    edge: &EdgeTarget,
    now: u64,
) -> Result<Observed, DnsError> {
    let want = txt_value(token);
    let txt = resolver.txt(&txt_name(hostname)).await?.iter().any(|v| v.trim() == want);
    let routing = match mode {
        // A CNAME to the edge, or (for flattening DNS providers) addresses that are all ours.
        DomainMode::Cname => {
            let cname = resolver.cname(hostname).await?;
            if cname.is_some_and(|c| normalize_name(&c) == normalize_name(&edge.cname)) {
                true
            } else {
                all_edge_addrs(&resolver.addrs(hostname).await?, edge)
            }
        }
        DomainMode::Apex => all_edge_addrs(&resolver.addrs(hostname).await?, edge),
        DomainMode::NsDelegated => {
            let ns = resolver.ns(hostname).await?;
            !ns.is_empty() && ns.iter().all(|n| edge.nameservers.iter().any(|e| normalize_name(e) == normalize_name(n)))
        }
    };
    Ok(Observed { at: now, txt, routing })
}

/// Non-empty and every address is an edge address: a mixed set would let someone else answer.
fn all_edge_addrs(addrs: &[IpAddr], edge: &EdgeTarget) -> bool {
    !addrs.is_empty() && addrs.iter().all(|a| edge.has_addr(a))
}

/// The state machine. Returns the new state.
fn advance(b: &mut Binding, seen: Observed, grace_secs: u64) -> BindingState {
    let now = seen.at;
    b.observed = Some(seen);
    let next = match b.state {
        BindingState::Pending => {
            if b.mode == DomainMode::NsDelegated && seen.txt {
                b.txt_proven_at.get_or_insert(now);
            }
            let proven = seen.txt || (b.mode == DomainMode::NsDelegated && b.txt_proven_at.is_some());
            if proven && seen.routing { BindingState::Verified } else { BindingState::Pending }
        }
        BindingState::Verified => {
            if seen.txt && seen.routing {
                BindingState::Verified
            } else {
                BindingState::Grace
            }
        }
        BindingState::Grace => {
            if seen.txt && seen.routing {
                BindingState::Verified
            } else if now.saturating_sub(b.grace_since.unwrap_or(now)) >= grace_secs {
                BindingState::Detached
            } else {
                BindingState::Grace
            }
        }
        terminal @ (BindingState::Detached | BindingState::Tombstoned) => terminal,
    };
    if next != b.state {
        match next {
            BindingState::Verified => {
                b.verified_at = Some(now);
                b.grace_since = None;
            }
            BindingState::Grace => b.grace_since = Some(now),
            _ => {}
        }
        b.state = next;
        b.changed_at = now;
    }
    next
}

/// Lowercases and strips a trailing dot, then requires a plain (non-wildcard) hostname.
fn canonical_host(hostname: &str) -> Result<String, DomainError> {
    let host = normalize_name(hostname);
    if cloud_contract::validate::is_hostname(&host, false) { Ok(host) } else { Err(DomainError::InvalidHostname(hostname.to_owned())) }
}

mod hex_secret {
    use serde::{Deserialize, Deserializer, Serializer, de::Error};

    pub fn serialize<S: Serializer>(secret: &[u8; 32], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&hex::encode(secret))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; 32], D::Error> {
        let text = String::deserialize(d)?;
        let bytes = hex::decode(text).map_err(D::Error::custom)?;
        bytes.try_into().map_err(|_| D::Error::custom("token secret must be 32 bytes"))
    }
}
