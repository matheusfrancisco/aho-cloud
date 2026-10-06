//! Dependency manifest inspection shared by the extractors' `detect`.

use super::ManifestFile;

fn named<'m, 'a>(manifests: &'m [ManifestFile<'a>], name: &'m str) -> impl Iterator<Item = &'m ManifestFile<'a>> + 'm {
    manifests.iter().filter(move |m| m.path.file_name() == name)
}

/// Some manifest named `name` exists in the root.
pub(super) fn has(manifests: &[ManifestFile<'_>], name: &str) -> bool {
    named(manifests, name).next().is_some()
}

/// A `go.mod` in the root requires a module accepted by `accept`.
pub(super) fn go_requires(manifests: &[ManifestFile<'_>], accept: impl Fn(&str) -> bool) -> bool {
    named(manifests, "go.mod").any(|m| go_mod_requires(m.bytes).iter().any(|module| accept(module)))
}

/// `module` or any major version of it (`module/vN`).
pub(super) fn module_any_major(module: &str, base: &str) -> bool {
    match module.strip_prefix(base) {
        Some("") => true,
        Some(rest) => rest.strip_prefix("/v").is_some_and(|v| !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit())),
        None => false,
    }
}

/// Module paths of the `require` directives (single-line and block forms).
fn go_mod_requires(bytes: &[u8]) -> Vec<String> {
    let content = String::from_utf8_lossy(bytes);
    let mut out = Vec::new();
    let mut in_block = false;
    for line in content.lines() {
        let line = line.split("//").next().unwrap_or("").trim();
        if in_block {
            if line.starts_with(')') {
                in_block = false;
            } else if let Some(module) = line.split_whitespace().next() {
                out.push(module.trim_matches('"').to_owned());
            }
            continue;
        }
        let Some(rest) = line.strip_prefix("require") else { continue };
        let rest = rest.trim_start();
        if rest.starts_with('(') {
            in_block = true;
        } else if line.len() > "require".len() && line.as_bytes()["require".len()].is_ascii_whitespace() {
            if let Some(module) = rest.split_whitespace().next() {
                out.push(module.trim_matches('"').to_owned());
            }
        }
    }
    out
}

/// A `package.json` in the root lists `dep` under `dependencies` or `devDependencies`.
pub(super) fn npm_dep(manifests: &[ManifestFile<'_>], dep: &str) -> bool {
    named(manifests, "package.json").any(|m| {
        let Ok(json) = serde_json::from_slice::<serde_json::Value>(m.bytes) else { return false };
        ["dependencies", "devDependencies"]
            .iter()
            .any(|section| json.get(section).and_then(|deps| deps.as_object()).is_some_and(|deps| deps.contains_key(dep)))
    })
}

/// Any of the named manifests mentions one of `needles`.
pub(super) fn mentions(manifests: &[ManifestFile<'_>], files: &[&str], needles: &[&str]) -> bool {
    manifests.iter().filter(|m| files.contains(&m.path.file_name())).any(|m| {
        let content = String::from_utf8_lossy(m.bytes);
        needles.iter().any(|n| content.contains(n))
    })
}

/// Clojure dependency manifests.
pub(super) const CLJ_MANIFESTS: &[&str] = &["deps.edn", "shadow-cljs.edn", "project.clj"];
