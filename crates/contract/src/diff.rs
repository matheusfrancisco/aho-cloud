//! What a release changes against the one it replaces, classified the way aho's policy lattice
//! is: narrowing applies freely, widening needs a human. The approval card is this diff.

use std::collections::{BTreeMap, BTreeSet};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::manifest::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    /// Source or build changed; the surface did not.
    Code,
    Process,
    /// A public port appeared, moved, or went internal.
    Exposure,
    Volume,
    Egress,
    Secret,
    SecretReach,
    DataClass,
    Region,
    Domain,
    Auth,
    Budget,
    Owner,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Narrow,
    Neutral,
    Widen,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
pub struct Change {
    pub category: Category,
    pub direction: Direction,
    /// One line for the approval card, e.g. `egress + api.stripe.com`.
    pub detail: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
pub struct SurfaceDiff {
    pub changes: Vec<Change>,
}

impl SurfaceDiff {
    pub fn widenings(&self) -> impl Iterator<Item = &Change> {
        self.changes.iter().filter(|c| c.direction == Direction::Widen)
    }

    pub fn is_widening(&self) -> bool {
        self.widenings().next().is_some()
    }

    /// Categories that widen, deduplicated.
    pub fn widened(&self) -> BTreeSet<Category> {
        self.widenings().map(|c| c.category).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }
}

/// `prev = None` is the first release: everything it declares is a widening from nothing.
pub fn diff(prev: Option<&Manifest>, next: &Manifest) -> SurfaceDiff {
    let mut d = Diff::default();
    let Some(prev) = prev else {
        first_release(next, &mut d);
        return SurfaceDiff { changes: d.0 };
    };
    if prev.source != next.source || prev.build != next.build {
        d.push(Category::Code, Direction::Neutral, "source or build changed".into());
    }
    processes(prev, next, &mut d);
    sets(&mut d, Category::Egress, "egress", set(&prev.egress), set(&next.egress));
    secrets(prev, next, &mut d);
    if next.data_class != prev.data_class {
        let dir = if next.data_class > prev.data_class { Direction::Widen } else { Direction::Narrow };
        d.push(Category::DataClass, dir, format!("data class {:?} → {:?}", prev.data_class, next.data_class));
    }
    if next.region != prev.region {
        d.push(Category::Region, Direction::Widen, format!("region {:?} → {:?}", prev.region, next.region));
    }
    domains(prev, next, &mut d);
    if next.auth != prev.auth {
        let dir = if matches!(prev.auth, Auth::None) { Direction::Narrow } else { Direction::Widen };
        d.push(Category::Auth, dir, format!("auth {} → {}", auth_label(&prev.auth), auth_label(&next.auth)));
    }
    if next.budget.ceiling_usd_month != prev.budget.ceiling_usd_month {
        let dir = if next.budget.ceiling_usd_month > prev.budget.ceiling_usd_month { Direction::Widen } else { Direction::Narrow };
        d.push(Category::Budget, dir, format!("budget ${} → ${}", prev.budget.ceiling_usd_month, next.budget.ceiling_usd_month));
    }
    if next.owner != prev.owner {
        d.push(Category::Owner, Direction::Neutral, "owner or escalation changed".into());
    }
    SurfaceDiff { changes: d.0 }
}

/// A rollback target fits under a ceiling when switching to it from the ceiling widens nothing.
pub fn fits_within(candidate: &Manifest, ceiling: &Manifest) -> bool {
    !diff(Some(ceiling), candidate).is_widening()
}

#[derive(Default)]
struct Diff(Vec<Change>);

impl Diff {
    fn push(&mut self, category: Category, direction: Direction, detail: String) {
        self.0.push(Change { category, direction, detail });
    }
}

fn first_release(m: &Manifest, d: &mut Diff) {
    d.push(Category::Code, Direction::Neutral, "first release".into());
    for p in &m.processes {
        d.push(Category::Process, Direction::Widen, format!("process + {}", p.name));
        if let (Expose::Public, Some(port)) = (p.expose, p.port) {
            d.push(Category::Exposure, Direction::Widen, format!("{} public on :{port}", p.name));
        }
        if let Some(v) = &p.volume {
            d.push(Category::Volume, Direction::Widen, format!("{} volume {} ({} MB)", p.name, v.mount, v.size_mb));
        }
    }
    for h in &m.egress {
        d.push(Category::Egress, Direction::Widen, format!("egress + {h}"));
    }
    for s in &m.secrets {
        d.push(Category::Secret, Direction::Widen, format!("secret + {} → {}", s.name, s.reach.join(", ")));
    }
    if m.data_class != DataClass::None {
        d.push(Category::DataClass, Direction::Widen, format!("data class {:?}", m.data_class));
    }
    for dom in &m.domains {
        d.push(Category::Domain, Direction::Widen, format!("domain + {}", dom.hostname));
    }
    d.push(Category::Auth, Direction::Widen, format!("auth {}", auth_label(&m.auth)));
    d.push(Category::Budget, Direction::Widen, format!("budget ${}", m.budget.ceiling_usd_month));
}

fn processes(prev: &Manifest, next: &Manifest, d: &mut Diff) {
    let before: BTreeMap<&str, &Process> = prev.processes.iter().map(|p| (p.name.as_str(), p)).collect();
    let after: BTreeMap<&str, &Process> = next.processes.iter().map(|p| (p.name.as_str(), p)).collect();
    for (name, p) in &after {
        let Some(old) = before.get(name) else {
            d.push(Category::Process, Direction::Widen, format!("process + {name}"));
            if let (Expose::Public, Some(port)) = (p.expose, p.port) {
                d.push(Category::Exposure, Direction::Widen, format!("{name} public on :{port}"));
            }
            if let Some(v) = &p.volume {
                d.push(Category::Volume, Direction::Widen, format!("{name} volume {} ({} MB)", v.mount, v.size_mb));
            }
            continue;
        };
        let was = public_port(old);
        let now = public_port(p);
        if was != now {
            let dir = if now.is_none() { Direction::Narrow } else { Direction::Widen };
            d.push(Category::Exposure, dir, format!("{name} public {} → {}", port_label(was), port_label(now)));
        }
        if old.command != p.command || old.health != p.health {
            d.push(Category::Process, Direction::Neutral, format!("{name} command or health changed"));
        }
        match (&old.volume, &p.volume) {
            (None, Some(v)) => d.push(Category::Volume, Direction::Widen, format!("{name} volume + {}", v.mount)),
            (Some(v), None) => d.push(Category::Volume, Direction::Narrow, format!("{name} volume − {}", v.mount)),
            (Some(a), Some(b)) if a != b => {
                let dir = if b.size_mb > a.size_mb || b.mount != a.mount { Direction::Widen } else { Direction::Narrow };
                d.push(Category::Volume, dir, format!("{name} volume {} {} MB → {} {} MB", a.mount, a.size_mb, b.mount, b.size_mb));
            }
            _ => {}
        }
    }
    for name in before.keys().filter(|n| !after.contains_key(*n)) {
        d.push(Category::Process, Direction::Narrow, format!("process − {name}"));
    }
}

fn public_port(p: &Process) -> Option<u16> {
    if p.expose == Expose::Public { p.port } else { None }
}

fn port_label(p: Option<u16>) -> String {
    p.map_or_else(|| "none".into(), |p| format!(":{p}"))
}

fn secrets(prev: &Manifest, next: &Manifest, d: &mut Diff) {
    for s in &next.secrets {
        match prev.secret(&s.name) {
            None => d.push(Category::Secret, Direction::Widen, format!("secret + {} → {}", s.name, s.reach.join(", "))),
            Some(old) => sets(d, Category::SecretReach, &format!("{} reach", s.name), set(&old.reach), set(&s.reach)),
        }
    }
    for s in prev.secrets.iter().filter(|s| next.secret(&s.name).is_none()) {
        d.push(Category::Secret, Direction::Narrow, format!("secret − {}", s.name));
    }
}

fn domains(prev: &Manifest, next: &Manifest, d: &mut Diff) {
    let before: BTreeMap<&str, DomainMode> = prev.domains.iter().map(|x| (x.hostname.as_str(), x.verification)).collect();
    let after: BTreeMap<&str, DomainMode> = next.domains.iter().map(|x| (x.hostname.as_str(), x.verification)).collect();
    for (host, mode) in &after {
        match before.get(host) {
            None => d.push(Category::Domain, Direction::Widen, format!("domain + {host}")),
            Some(old) if old != mode => d.push(Category::Domain, Direction::Widen, format!("domain {host} {old:?} → {mode:?}")),
            Some(_) => {}
        }
    }
    for host in before.keys().filter(|h| !after.contains_key(*h)) {
        d.push(Category::Domain, Direction::Narrow, format!("domain − {host}"));
    }
}

fn set(v: &[String]) -> BTreeSet<&str> {
    v.iter().map(String::as_str).collect()
}

fn sets(d: &mut Diff, category: Category, label: &str, before: BTreeSet<&str>, after: BTreeSet<&str>) {
    for added in after.difference(&before) {
        d.push(category, Direction::Widen, format!("{label} + {added}"));
    }
    for removed in before.difference(&after) {
        d.push(category, Direction::Narrow, format!("{label} − {removed}"));
    }
}

fn auth_label(a: &Auth) -> String {
    match a {
        Auth::None => "none".into(),
        Auth::Password => "password".into(),
        Auth::MagicLink { .. } => "magic_link".into(),
        Auth::Oidc { provider } => match provider {
            OidcProvider::Google => "oidc:google".into(),
            OidcProvider::Github => "oidc:github".into(),
            OidcProvider::Microsoft => "oidc:microsoft".into(),
            OidcProvider::Custom { issuer, .. } => format!("oidc:{issuer}"),
        },
        Auth::AppOwned => "app_owned".into(),
    }
}
