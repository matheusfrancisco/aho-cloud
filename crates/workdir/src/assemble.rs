//! The draft manifest for a directory: its `deploy.json` as declared fields, then what the repo
//! files say, then what the aho session says.

use std::path::Path;

use cloud_contract::Manifest;
use cloud_contract::draft::Draft;
use cloud_importers::Overruled;
use serde_json::Value;

use crate::MANIFEST;
use crate::error::{WorkdirError, io};

/// What [`assemble`] made, and what it read on the way.
#[derive(Clone, Debug, Default)]
pub struct Assembled {
    pub draft: Draft,
    /// Machine values that lost to stronger, different ones (usually `deploy.json`'s).
    pub overruled: Vec<Overruled>,
    /// Importers that found their file, highest precedence first.
    pub found: Vec<&'static str>,
    /// What the importers found but could not read.
    pub problems: Vec<String>,
}

/// `<dir>/deploy.json`, if there is one. A `deploy.json` that resolves outside `dir` is
/// [`WorkdirError::Escapes`]; one that does not parse is [`WorkdirError::Manifest`].
pub fn read_manifest(dir: &Path) -> Result<Option<Manifest>, WorkdirError> {
    let path = dir.join(MANIFEST);
    let meta = match std::fs::symlink_metadata(&path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(io(path)(e)),
    };
    if meta.is_symlink() {
        let real = match path.canonicalize() {
            Ok(p) => p,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(io(path)(e)),
        };
        if !real.starts_with(dir.canonicalize().map_err(io(dir))?) {
            return Err(WorkdirError::Escapes(MANIFEST.into()));
        }
    }
    let text = std::fs::read_to_string(&path).map_err(io(&path))?;
    Manifest::from_json(&text).map(Some).map_err(|e| WorkdirError::Manifest { path, reason: e.to_string() })
}

/// `manifest` (the directory's `deploy.json`, see [`read_manifest`]) as declared fields, then the
/// importers' scan of `dir`, then the `session` draft, each merged over the result so far with
/// [`cloud_importers::merge`].
///
/// Precedence follows strength first: declared (`deploy.json`) beats any machine evidence, and
/// hash-chained session evidence beats a file scan's inference. Between equally strong values
/// the later source wins, so the session's word replaces the files' where both only infer.
pub fn assemble(dir: &Path, manifest: Option<&Manifest>, session: Option<&Draft>) -> Assembled {
    let mut draft = manifest.map(cloud_importers::from_manifest).unwrap_or_default();
    let scan = cloud_importers::scan(dir);
    let mut overruled = cloud_importers::merge(&mut draft, &scan.draft);
    if let Some(s) = session {
        overruled.extend(cloud_importers::merge(&mut draft, s));
    }
    Assembled { draft, overruled, found: scan.found, problems: scan.problems }
}

/// Declares a whole subtree (`/source`, `/api_version`), replacing whatever was at or under
/// `pointer`.
pub fn declare_subtree(draft: &mut Draft, pointer: &str, value: Value) {
    let inside = format!("{pointer}/");
    draft.fields.retain(|k, _| k != pointer && !k.starts_with(&inside));
    draft.declare(pointer, value);
}

#[cfg(test)]
mod tests {
    use std::fs;

    use cloud_contract::draft::{Evidence, Strength};
    use serde_json::json;

    use super::*;

    fn manifest_json() -> Value {
        json!({
            "api_version": cloud_contract::manifest::API_VERSION,
            "project": "shop",
            "source": {"kind": "archive", "hash": format!("blake3:{}", "0".repeat(64))},
            "build": {"kind": "dockerfile", "path": "Dockerfile", "context": "."},
            "processes": [{"name": "web", "port": 8080, "expose": "public", "health": {"kind": "http", "path": "/healthz"}}],
            "auth": {"mode": "none"},
            "data_class": "none",
            "owner": {"name": "Ana", "email": "ana@example.com"},
            "budget": {"ceiling_usd_month": 20, "degrade": "sleep"}
        })
    }

    fn session_evidence() -> Evidence {
        Evidence { uri: "aho-log:5e55#ab".into(), note: "the agent ran it".into() }
    }

    fn value<'a>(d: &'a Draft, pointer: &str) -> Option<&'a Value> {
        d.fields.get(pointer).map(|f| &f.value)
    }

    #[test]
    fn deploy_json_beats_files_and_session_and_chained_session_evidence_beats_inferred_files() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("Dockerfile"), "FROM node:20\nEXPOSE 3000\nCMD [\"node\", \"server.js\"]\n").unwrap();
        fs::write(dir.path().join(MANIFEST), manifest_json().to_string()).unwrap();
        let manifest = read_manifest(dir.path()).unwrap().unwrap();

        let mut session = Draft::default();
        session.offer("/processes/0/name", json!("web"), Strength::Chained, session_evidence());
        session.offer("/processes/0/port", json!(4000), Strength::Chained, session_evidence());
        session.offer("/processes/0/command", json!(["node", "cluster.js"]), Strength::Chained, session_evidence());
        session.offer("/egress/0", json!("api.stripe.com"), Strength::Inferred, session_evidence());

        let a = assemble(dir.path(), Some(&manifest), Some(&session));
        assert_eq!(a.found, ["Dockerfile"]);
        // deploy.json's port stays against both the Dockerfile and the session.
        let port = &a.draft.fields["/processes/0/port"];
        assert_eq!((&port.value, port.strength), (&json!(8080), Strength::Declared));
        let lost: Vec<(&str, &Value)> = a.overruled.iter().map(|o| (o.pointer.as_str(), &o.offered.value)).collect();
        assert!(lost.contains(&("/processes/0/port", &json!(3000))), "{lost:?}");
        assert!(lost.contains(&("/processes/0/port", &json!(4000))), "{lost:?}");
        // What deploy.json does not say, the session's chained evidence decides over the scan.
        let command = &a.draft.fields["/processes/0/command"];
        assert_eq!((&command.value, command.strength), (&json!(["node", "cluster.js"]), Strength::Chained));
        assert_eq!(value(&a.draft, "/egress/0"), Some(&json!("api.stripe.com")));
        assert_eq!(value(&a.draft, "/owner/email"), Some(&json!("ana@example.com")));
    }

    #[test]
    fn without_a_manifest_files_fill_in_and_the_session_merges_last() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("Dockerfile"), "FROM node:20\nEXPOSE 3000\nCMD [\"node\", \"server.js\"]\n").unwrap();
        let without = assemble(dir.path(), None, None);
        let port = &without.draft.fields["/processes/0/port"];
        assert_eq!((&port.value, port.strength), (&json!(3000), Strength::Inferred));
        assert!(without.overruled.is_empty());

        let mut session = Draft::default();
        session.offer("/egress/0", json!("api.stripe.com"), Strength::Inferred, session_evidence());
        session.offer("/processes/0/name", json!("web"), Strength::Inferred, session_evidence());
        let with = assemble(dir.path(), None, Some(&session));
        assert_eq!(value(&with.draft, "/processes/0/port"), Some(&json!(3000)), "the files' fields stay");
        assert_eq!(value(&with.draft, "/egress/0"), Some(&json!("api.stripe.com")), "the session adds its own");

        // Both only infer the port: the session, merged last, has the final word.
        session.offer("/processes/0/port", json!(5000), Strength::Inferred, session_evidence());
        let tie = assemble(dir.path(), None, Some(&session));
        assert_eq!(value(&tie.draft, "/processes/0/port"), Some(&json!(5000)));
    }

    #[test]
    fn manifest_absent_broken_or_escaping() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_manifest(dir.path()).unwrap().is_none());
        fs::write(dir.path().join(MANIFEST), "{\"api_version\":").unwrap();
        assert!(matches!(read_manifest(dir.path()), Err(WorkdirError::Manifest { .. })));
        #[cfg(unix)]
        {
            let other = tempfile::tempdir().unwrap();
            fs::write(other.path().join(MANIFEST), manifest_json().to_string()).unwrap();
            fs::remove_file(dir.path().join(MANIFEST)).unwrap();
            std::os::unix::fs::symlink(other.path().join(MANIFEST), dir.path().join(MANIFEST)).unwrap();
            assert!(matches!(read_manifest(dir.path()), Err(WorkdirError::Escapes(_))));
            fs::remove_file(dir.path().join(MANIFEST)).unwrap();
            fs::write(dir.path().join("real.json"), manifest_json().to_string()).unwrap();
            std::os::unix::fs::symlink("real.json", dir.path().join(MANIFEST)).unwrap();
            assert_eq!(read_manifest(dir.path()).unwrap().unwrap().project, "shop", "a link inside the directory is fine");
        }
    }

    #[test]
    fn declare_subtree_replaces_everything_under_the_pointer() {
        let mut d = Draft::default();
        d.offer("/source/kind", json!("git"), Strength::Inferred, session_evidence());
        d.offer("/source/url", json!("https://example.com/r.git"), Strength::Inferred, session_evidence());
        d.offer("/sourcemap", json!(true), Strength::Inferred, session_evidence());
        declare_subtree(&mut d, "/source", json!({"kind": "archive", "hash": "blake3:00"}));
        assert_eq!(d.fields.keys().collect::<Vec<_>>(), ["/source", "/sourcemap"]);
        assert_eq!(d.fields["/source"].strength, Strength::Declared);
    }
}
