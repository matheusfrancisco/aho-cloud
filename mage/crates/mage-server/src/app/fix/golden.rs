//! Golden-file assertions for the fix renderers. Files live in `src/app/fix/testdata/`; run the
//! tests with `MAGE_UPDATE_GOLDEN=1` to (re)write them, then review the diff.

use std::path::PathBuf;

pub fn assert_golden(name: &str, actual: &str) {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/app/fix/testdata").join(name);
    if std::env::var_os("MAGE_UPDATE_GOLDEN").is_some() {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).unwrap_or_else(|e| panic!("create {}: {e}", dir.display()));
        }
        std::fs::write(&path, actual).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
        return;
    }
    let expected = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("golden file {} missing ({e}); run with MAGE_UPDATE_GOLDEN=1 and review it", path.display()));
    if expected != actual {
        let first = expected.lines().zip(actual.lines()).position(|(a, b)| a != b).unwrap_or_else(|| expected.lines().count().min(actual.lines().count()));
        panic!(
            "{name} differs from the golden file at line {}:\n  golden: {:?}\n  actual: {:?}\n(rerun with MAGE_UPDATE_GOLDEN=1 to accept)",
            first + 1,
            expected.lines().nth(first),
            actual.lines().nth(first)
        );
    }
}
