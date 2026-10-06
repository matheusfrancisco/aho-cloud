//! Layer guard (docs/internals/README.md "Dependency rule", "Inside a role crate"):
//! - a crate may only depend on the Mage crates its row allows;
//! - production code in `app/` and `domain/` of a role crate never imports `crate::adapters`
//!   (test code may compose real adapters: test-only files and everything after `#[cfg(test)]`
//!   in a file are skipped).

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

fn crates_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
}

/// Mage-owned dependencies (`mage-*`, `shared-*`) declared in `[dependencies]` of `krate`.
fn internal_deps(krate: &str) -> BTreeSet<String> {
    let manifest = crates_dir().join(krate).join("Cargo.toml");
    let text = fs::read_to_string(&manifest).unwrap_or_else(|e| panic!("{}: {e}", manifest.display()));
    let doc: toml::Table = text.parse().unwrap_or_else(|e| panic!("{}: {e}", manifest.display()));
    doc.get("dependencies")
        .and_then(toml::Value::as_table)
        .map(|deps| {
            deps.keys()
                .filter(|name| name.starts_with("mage-") || name.starts_with("shared-"))
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn crates_depend_only_on_allowed_mage_crates() {
    let rules: &[(&str, &[&str])] = &[
        ("mage-core", &[]),
        ("mage-infra", &["mage-core"]),
        ("mage-indexer", &["mage-core", "mage-infra"]),
        ("mage-server", &["mage-core", "mage-infra", "shared-auth"]),
        // The runner holds no DB, blob, vault or provider credentials: core only.
        ("mage-runner", &["mage-core"]),
    ];
    let mut violations = Vec::new();
    for (krate, allowed) in rules {
        let allowed: BTreeSet<&str> = allowed.iter().copied().collect();
        for dep in internal_deps(krate) {
            if !allowed.contains(dep.as_str()) {
                violations.push(format!("{krate} -> {dep}"));
            }
        }
    }
    assert!(violations.is_empty(), "forbidden crate edges: {violations:?}");
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn app_and_domain_never_import_adapters() {
    let mut violations = Vec::new();
    for krate in ["mage-server", "mage-indexer", "mage-runner"] {
        for layer in ["app", "domain"] {
            let mut files = Vec::new();
            rust_files(&crates_dir().join(krate).join("src").join(layer), &mut files);
            for file in files {
                let name = file.file_name().and_then(|n| n.to_str()).unwrap_or_default();
                let in_tests_dir = file.components().any(|c| c.as_os_str() == "tests");
                if name == "tests.rs" || name.ends_with("_tests.rs") || in_tests_dir {
                    continue;
                }
                let text = fs::read_to_string(&file).unwrap_or_default();
                for (n, line) in text.lines().enumerate() {
                    if line.trim_start().starts_with("#[cfg(test)]") {
                        break;
                    }
                    let code = line.split("//").next().unwrap_or_default();
                    if code.contains("crate::adapters") || code.contains("super::adapters") {
                        violations.push(format!("{}:{}", file.display(), n + 1));
                    }
                }
            }
        }
    }
    assert!(violations.is_empty(), "app/domain importing adapters: {violations:#?}");
}
