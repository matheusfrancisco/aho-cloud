//! Generated artifacts must match the Rust types. `UPDATE_GENERATED=1 cargo test -p cloud-contract`
//! rewrites them.

use std::path::PathBuf;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn check(rel: &str, want: &str) {
    let path = root().join(rel);
    if std::env::var_os("UPDATE_GENERATED").is_some() {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display()));
        }
        std::fs::write(&path, want).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        return;
    }
    let have = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(have == want, "{rel} is stale; run `UPDATE_GENERATED=1 cargo test -p cloud-contract`");
}

#[test]
fn typescript_is_current() {
    check("web/src/gen/contract.ts", &cloud_contract::ts::typescript());
}

#[test]
fn manifest_schema_is_current() {
    let schema = serde_json::to_string_pretty(&cloud_contract::manifest_schema()).unwrap_or_default() + "\n";
    check("schema/manifest.v1.json", &schema);
}
