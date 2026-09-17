//! `.env.example`: the names of the variables a deployment must provide. Values are never
//! read into the draft — an example file sometimes carries a real credential.

use std::path::Path;

use serde_json::json;

use crate::{Fact, Findings, read};

const NAME: &str = ".env.example";

/// Runtime settings that are configuration, not credentials.
const NOT_SECRETS: [&str; 12] = [
    "HOST",
    "HOSTNAME",
    "NODE_ENV",
    "RUST_LOG",
    "RUST_BACKTRACE",
    "LOG_LEVEL",
    "DEBUG",
    "TZ",
    "ENV",
    "APP_ENV",
    "PYTHONUNBUFFERED",
    "WEB_CONCURRENCY",
];

pub(crate) fn import(root: &Path) -> Option<Findings> {
    let text = match read(root, NAME)? {
        Ok(t) => t,
        Err(e) => return Some(Findings::failed(NAME, e)),
    };
    let mut f = Findings::new(NAME);
    for (i, raw) in text.lines().enumerate() {
        let line = i + 1;
        let t = raw.trim();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        let t = t.strip_prefix("export ").unwrap_or(t).trim_start();
        let Some((name, value)) = t.split_once('=') else {
            f.problems.push(format!("{NAME}:{line}: not `NAME=value`"));
            continue;
        };
        let name = name.trim();
        if !is_env_name(name) {
            f.problems.push(format!("{NAME}:{line}: `{name}` is not an upper-case variable name"));
            continue;
        }
        if name == "PORT" {
            let v = value.trim().trim_matches(|c| c == '"' || c == '\'');
            if let Ok(port) = v.parse::<u16>() {
                f.primary_mut().port = Some(Fact::new(json!(port), NAME, line, format!("PORT={port}")));
            }
            continue;
        }
        if NOT_SECRETS.contains(&name) || f.secrets.iter().any(|s| s.value == name) {
            continue;
        }
        f.secrets.push(Fact::new(json!(name), NAME, line, format!("`{name}` is listed in {NAME}")));
    }
    Some(f)
}

fn is_env_name(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty() && b[0].is_ascii_uppercase() && b.iter().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || *c == b'_')
}
