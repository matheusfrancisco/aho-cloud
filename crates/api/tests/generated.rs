//! `UPDATE_GENERATED=1 cargo test -p cloud-api` rewrites the generated TypeScript.

use std::path::PathBuf;

#[test]
fn typescript_is_current() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../web/src/gen/api.ts");
    let want = cloud_api::ts::typescript();
    if std::env::var_os("UPDATE_GENERATED").is_some() {
        std::fs::write(&path, &want).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        return;
    }
    let have = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(have == want, "web/src/gen/api.ts is stale; run `UPDATE_GENERATED=1 cargo test -p cloud-api`");
}

#[test]
fn routes_fill_every_placeholder() {
    let path = cloud_api::routes::fill(cloud_api::routes::APPROVE, &[("p", "shop"), ("r", "r1")]);
    assert_eq!(path, "/v1/projects/shop/releases/r1/approve");
}

#[test]
fn check_result_collects_manifest_and_edge_errors() {
    let text = include_str!("../../contract/tests/fixtures/web-api.json");
    let mut m = cloud_contract::Manifest::from_json(text).unwrap_or_else(|e| panic!("{e}"));
    assert!(cloud_api::CheckResult::of(&m, None).accepted());
    m.budget.ceiling_usd_month = 0;
    let r = cloud_api::CheckResult::of(&m, Some("[[route]]\npath = \"x\"\nmode = \"public\"\n"));
    assert_eq!(r.refusals.len(), 1);
    assert_eq!(r.edge_errors.len(), 1);
    assert!(!r.accepted());
}
