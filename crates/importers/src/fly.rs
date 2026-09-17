//! fly.toml: app name, build, process groups, the HTTP service, checks and mounts.

use std::collections::BTreeMap;
use std::fmt;
use std::marker::PhantomData;
use std::path::Path;

use serde::Deserialize;
use serde::de::{self, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::json;
use toml::Spanned;

use crate::{Fact, Findings, ProcessFacts, line_of, read, shell, size_mb, slug};

const NAME: &str = "fly.toml";

#[derive(Deserialize)]
struct Fly {
    app: Option<Spanned<String>>,
    build: Option<FlyBuild>,
    #[serde(default)]
    processes: BTreeMap<String, Spanned<Command>>,
    http_service: Option<HttpService>,
    #[serde(default)]
    services: Vec<Service>,
    #[serde(default)]
    mounts: OneOrMany<Mount>,
    #[serde(default)]
    checks: BTreeMap<String, TopCheck>,
}

#[derive(Deserialize)]
struct FlyBuild {
    dockerfile: Option<Spanned<String>>,
    builder: Option<Spanned<String>>,
    image: Option<Spanned<String>>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Command {
    Line(String),
    Argv(Vec<String>),
}

#[derive(Deserialize)]
struct HttpService {
    internal_port: Option<Spanned<i64>>,
    #[serde(default)]
    processes: Vec<String>,
    #[serde(default)]
    checks: Vec<HttpCheck>,
}

#[derive(Deserialize)]
struct Service {
    internal_port: Option<Spanned<i64>>,
    #[serde(default)]
    processes: Vec<String>,
    #[serde(default)]
    http_checks: Vec<HttpCheck>,
}

#[derive(Deserialize)]
struct HttpCheck {
    path: Option<Spanned<String>>,
    port: Option<i64>,
}

#[derive(Deserialize)]
struct TopCheck {
    #[serde(rename = "type")]
    kind: Option<String>,
    path: Option<Spanned<String>>,
    port: Option<i64>,
    #[serde(default)]
    processes: Vec<String>,
}

#[derive(Deserialize)]
struct Mount {
    destination: Spanned<String>,
    initial_size: Option<toml::Value>,
    #[serde(default)]
    processes: Vec<String>,
}

/// `[mounts]` or `[[mounts]]`. Visits the table/array directly so spans inside survive (an
/// untagged enum would buffer the content and lose them).
struct OneOrMany<T>(Vec<T>);

impl<T> Default for OneOrMany<T> {
    fn default() -> Self {
        Self(Vec::new())
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for OneOrMany<T> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V<T>(PhantomData<T>);
        impl<'de, T: Deserialize<'de>> Visitor<'de> for V<T> {
            type Value = OneOrMany<T>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a table or an array of tables")
            }
            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Self::Value, A::Error> {
                T::deserialize(de::value::MapAccessDeserializer::new(map)).map(|t| OneOrMany(vec![t]))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, seq: A) -> Result<Self::Value, A::Error> {
                Vec::<T>::deserialize(de::value::SeqAccessDeserializer::new(seq)).map(OneOrMany)
            }
        }
        d.deserialize_any(V(PhantomData))
    }
}

pub(crate) fn import(root: &Path) -> Option<Findings> {
    let text = match read(root, NAME)? {
        Ok(t) => t,
        Err(e) => return Some(Findings::failed(NAME, e)),
    };
    let fly: Fly = match toml::from_str(&text) {
        Ok(f) => f,
        Err(e) => return Some(Findings::failed(NAME, format!("{NAME}: {}", e.message()))),
    };
    let line = |span: std::ops::Range<usize>| line_of(&text, span.start);
    let mut f = Findings::new(NAME);

    if let Some(app) = &fly.app {
        match slug(app.get_ref(), 40) {
            Some(s) => f.project = Some(Fact::new(json!(s), NAME, line(app.span()), format!("app = \"{}\"", app.get_ref()))),
            None => f.problems.push(format!("{NAME}:{}: app name `{}` cannot be a project slug", line(app.span()), app.get_ref())),
        }
    }
    if let Some(b) = &fly.build {
        if let Some(df) = &b.dockerfile {
            let v = json!({"kind": "dockerfile", "path": df.get_ref(), "context": "."});
            f.build = Some(Fact::new(v, NAME, line(df.span()), format!("dockerfile = \"{}\"", df.get_ref())));
        } else if let Some(builder) = &b.builder {
            let v = json!({"kind": "buildpack", "builder": builder.get_ref()});
            f.build = Some(Fact::new(v, NAME, line(builder.span()), format!("builder = \"{}\"", builder.get_ref())));
        } else if let Some(image) = &b.image {
            f.problems.push(format!(
                "{NAME}:{}: image `{}` is a tag; the cloud builds from source or runs an image by digest",
                line(image.span()),
                image.get_ref()
            ));
        }
    }
    // Without [build], fly builds ./Dockerfile: the Dockerfile importer offers that.

    // Which process group serves HTTP.
    let http_group = fly
        .http_service
        .as_ref()
        .and_then(|h| h.processes.first())
        .or_else(|| fly.services.iter().find_map(|s| s.processes.first()))
        .cloned()
        .or_else(|| (fly.processes.contains_key("app") || fly.processes.is_empty()).then(|| "app".to_owned()))
        .or_else(|| fly.processes.keys().next().cloned());

    let mut groups: BTreeMap<String, ProcessFacts> = BTreeMap::new();
    for (group, cmd) in &fly.processes {
        let Some(name) = slug(group, 30) else {
            f.problems.push(format!("{NAME}:{}: process group `{group}` cannot be a process name", line(cmd.span())));
            continue;
        };
        let at = line(cmd.span());
        let (argv, text_cmd) = match cmd.get_ref() {
            Command::Line(s) => (shell::argv(s), s.clone()),
            Command::Argv(v) => (v.clone(), v.join(" ")),
        };
        let mut p = ProcessFacts {
            name: Some(Fact::new(json!(name), NAME, at, format!("process group `{group}`"))),
            command: (!argv.is_empty()).then(|| Fact::new(json!(argv), NAME, at, text_cmd.clone())),
            ..Default::default()
        };
        if let Some(port) = shell::port(&text_cmd) {
            p.port = Some(Fact::new(json!(port), NAME, at, format!("`{text_cmd}` binds {port}")));
        }
        groups.insert(group.clone(), p);
    }
    let mut primary = match &http_group {
        Some(g) => groups.remove(g).unwrap_or_else(|| ProcessFacts {
            name: slug(g, 30).and_then(|n| {
                fly.app.as_ref().map(|a| Fact::new(json!(n), NAME, line(a.span()), format!("fly's default process group `{g}`")))
            }),
            ..Default::default()
        }),
        None => ProcessFacts::default(),
    };

    if let Some(h) = &fly.http_service {
        if let Some(port) = &h.internal_port {
            let at = line(port.span());
            if let Ok(p) = u16::try_from(*port.get_ref()) {
                primary.port = Some(Fact::new(json!(p), NAME, at, format!("[http_service] internal_port = {p}")));
                primary.expose = Some(Fact::new(json!("public"), NAME, at, "[http_service] is reachable from the internet"));
            }
        }
        if let Some(c) = h.checks.iter().find(|c| c.path.is_some()) {
            health(&mut primary, c.path.as_ref(), c.port, "[[http_service.checks]]", &line);
        }
    }
    for s in &fly.services {
        let serves_primary = s.processes.is_empty() || s.processes.first() == http_group.as_ref();
        let target = if serves_primary {
            &mut primary
        } else {
            match s.processes.first().and_then(|g| groups.get_mut(g)) {
                Some(p) => p,
                None => continue,
            }
        };
        if let Some(port) = &s.internal_port
            && target.port.is_none()
            && let Ok(p) = u16::try_from(*port.get_ref())
        {
            let at = line(port.span());
            target.port = Some(Fact::new(json!(p), NAME, at, format!("[[services]] internal_port = {p}")));
            target.expose = Some(Fact::new(json!("public"), NAME, at, "[[services]] are reachable from the internet"));
        }
        if target.health.is_none()
            && let Some(c) = s.http_checks.iter().find(|c| c.path.is_some())
        {
            health(target, c.path.as_ref(), c.port, "[[services.http_checks]]", &line);
        }
    }
    for c in fly.checks.values().filter(|c| c.kind.as_deref() == Some("http")) {
        let target = match c.processes.first() {
            Some(g) if Some(g) != http_group.as_ref() => groups.get_mut(g),
            _ => Some(&mut primary),
        };
        if let Some(t) = target
            && t.health.is_none()
        {
            health(t, c.path.as_ref(), c.port, "[checks]", &line);
        }
    }
    for m in &fly.mounts.0 {
        let at = line(m.destination.span());
        let size = m.initial_size.as_ref().and_then(|v| match v {
            toml::Value::String(s) => size_mb(s),
            toml::Value::Integer(i) => u32::try_from(*i).ok().and_then(|gb| gb.checked_mul(1024)),
            _ => None,
        });
        // fly's default volume size is 1 GB.
        let volume = json!({"mount": m.destination.get_ref(), "size_mb": size.unwrap_or(1024)});
        let fact = Fact::new(volume, NAME, at, format!("[mounts] destination = \"{}\"", m.destination.get_ref()));
        let targets: Vec<&String> = if m.processes.is_empty() { http_group.iter().collect() } else { m.processes.iter().collect() };
        for g in targets {
            if Some(g) == http_group.as_ref() {
                primary.volume = Some(fact.clone());
            } else if let Some(p) = groups.get_mut(g) {
                p.volume = Some(fact.clone());
            }
        }
    }

    if primary != ProcessFacts::default() {
        f.primary = Some(primary);
    }
    for (group, p) in groups {
        if let Some(name) = slug(&group, 30) {
            f.named.push((name, p));
        }
    }
    Some(f)
}

fn health(
    p: &mut ProcessFacts,
    path: Option<&Spanned<String>>,
    port: Option<i64>,
    section: &str,
    line: &dyn Fn(std::ops::Range<usize>) -> usize,
) {
    let Some(path) = path else { return };
    let mut v = json!({"kind": "http", "path": path.get_ref()});
    let own = p.port.as_ref().and_then(|f| f.value.as_i64());
    if let Some(port) = port.filter(|p| Some(*p) != own) {
        v["port"] = json!(port);
    }
    p.health = Some(Fact::new(v, NAME, line(path.span()), format!("{section} path = \"{}\"", path.get_ref())));
}
