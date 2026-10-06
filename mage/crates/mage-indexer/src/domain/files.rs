//! Layer `Files` rules of the indexer (knowledge.md "UC `index.manifest`" step 3-4 and "Layer 3"
//! catalog rules): file classes from paths and content heads, text detection, line counts,
//! catalog paths. Pure.

use mage_core::domain::{FileClass, RepoPath};

/// Bytes inspected for a NUL to decide binary vs text.
pub const BINARY_SNIFF_BYTES: usize = 8000;

/// Bytes of a file head searched for a "Code generated … DO NOT EDIT" marker.
const GENERATED_SNIFF_BYTES: usize = 2048;

/// No NUL byte in the first [`BINARY_SNIFF_BYTES`].
pub fn is_text(bytes: &[u8]) -> bool {
    !bytes[..bytes.len().min(BINARY_SNIFF_BYTES)].contains(&0)
}

/// Count of `\n`, plus one when the last byte is not `\n` (0 for empty content).
pub fn line_count(bytes: &[u8]) -> u32 {
    let newlines = bytes.iter().filter(|b| **b == b'\n').count();
    let partial = usize::from(bytes.last().is_some_and(|b| *b != b'\n'));
    u32::try_from(newlines + partial).unwrap_or(u32::MAX)
}

/// The Go convention (`^// Code generated .* DO NOT EDIT\.$`) and its siblings in other
/// languages: a head line containing both `Code generated` and `DO NOT EDIT`.
pub fn has_generated_header(head: &[u8]) -> bool {
    let head = &head[..head.len().min(GENERATED_SNIFF_BYTES)];
    String::from_utf8_lossy(head).lines().any(|line| {
        line.find("Code generated").is_some_and(|at| line[at..].contains("DO NOT EDIT"))
    })
}

const VENDORED_DIRS: [&str; 3] = ["vendor", "node_modules", "third_party"];
const GENERATED_DIRS: [&str; 2] = ["dist", "build"];
const GENERATED_SUFFIXES: [&str; 5] = [".min.js", ".min.css", "_generated.go", ".pb.go", ".pb.gw.go"];
const TEST_DIRS: [&str; 3] = ["test", "tests", "__tests__"];
const TEST_SUFFIXES: [&str; 18] = [
    "_test.go",
    "_test.clj",
    "_test.cljs",
    "_test.cljc",
    "_test.py",
    "_test.rs",
    ".spec.ts",
    ".spec.tsx",
    ".spec.js",
    ".spec.jsx",
    ".test.ts",
    ".test.tsx",
    ".test.js",
    ".test.jsx",
    ".spec.mjs",
    ".test.mjs",
    ".spec.cjs",
    ".test.cjs",
];
const DOC_DIRS: [&str; 1] = ["docs"];
const DOC_EXTENSIONS: [&str; 5] = ["md", "markdown", "rst", "adoc", "txt"];
const CONFIG_EXTENSIONS: [&str; 13] = ["json", "yaml", "yml", "toml", "ini", "cfg", "conf", "edn", "properties", "xml", "plist", "env", "lock"];
const CONFIG_FILE_NAMES: [&str; 8] = ["Dockerfile", "Makefile", "go.mod", "go.sum", ".editorconfig", ".gitignore", ".gitattributes", ".dockerignore"];
const DATA_EXTENSIONS: [&str; 5] = ["csv", "tsv", "jsonl", "ndjson", "parquet"];

/// Class of a manifest entry (knowledge.md "UC `index.manifest`" step 3), first rule wins:
/// Binary (`!is_text`) → Vendored → Generated (paths, or a generated header in `head`) → Test →
/// Doc → Config → Data → Source.
pub fn classify(path: &RepoPath, is_text: bool, head: Option<&[u8]>) -> FileClass {
    if !is_text {
        return FileClass::Binary;
    }
    let class = classify_path(path);
    if class == FileClass::Source && head.is_some_and(has_generated_header) {
        return FileClass::Generated;
    }
    class
}

/// Path-only rules of [`classify`] (no Binary, no header check).
pub fn classify_path(path: &RepoPath) -> FileClass {
    let name = path.file_name();
    let dirs: Vec<&str> = match path.parent() {
        Some(parent) => parent.split('/').collect(),
        None => Vec::new(),
    };
    let in_dir = |set: &[&str]| dirs.iter().any(|dir| set.contains(dir));
    let ext = path.extension().map(str::to_ascii_lowercase);
    let ext = ext.as_deref();

    if in_dir(&VENDORED_DIRS) {
        return FileClass::Vendored;
    }
    if in_dir(&GENERATED_DIRS) || GENERATED_SUFFIXES.iter().any(|suffix| name.ends_with(suffix)) {
        return FileClass::Generated;
    }
    let python_test = ext == Some("py") && name.starts_with("test_");
    if in_dir(&TEST_DIRS) || python_test || TEST_SUFFIXES.iter().any(|suffix| name.ends_with(suffix)) {
        return FileClass::Test;
    }
    if in_dir(&DOC_DIRS) || ext.is_some_and(|e| DOC_EXTENSIONS.contains(&e)) {
        return FileClass::Doc;
    }
    if CONFIG_FILE_NAMES.contains(&name) || ext.is_some_and(|e| CONFIG_EXTENSIONS.contains(&e)) {
        return FileClass::Config;
    }
    if ext.is_some_and(|e| DATA_EXTENSIONS.contains(&e)) {
        return FileClass::Data;
    }
    FileClass::Source
}

/// i18n message catalog paths (knowledge.md "Layer 3" step 3): `**/locales/**/*.{json,yml,yaml}`,
/// `**/i18n/**/*.{json,yml,yaml,edn}`, `**/*.po`, `**/messages*.properties`, `**/translations/**`.
pub fn is_catalog_path(path: &RepoPath) -> bool {
    let name = path.file_name();
    let dirs: Vec<&str> = match path.parent() {
        Some(parent) => parent.split('/').collect(),
        None => Vec::new(),
    };
    let ext = path.extension().map(str::to_ascii_lowercase);
    let ext = ext.as_deref();
    let under = |dir: &str| dirs.contains(&dir);
    (under("locales") && matches!(ext, Some("json" | "yml" | "yaml")))
        || (under("i18n") && matches!(ext, Some("json" | "yml" | "yaml" | "edn")))
        || ext == Some("po")
        || (name.starts_with("messages") && ext == Some("properties"))
        || under("translations")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> RepoPath {
        RepoPath::new(s).unwrap()
    }

    #[test]
    fn line_count_counts_a_trailing_partial_line() {
        assert_eq!(line_count(b""), 0);
        assert_eq!(line_count(b"a"), 1);
        assert_eq!(line_count(b"a\n"), 1);
        assert_eq!(line_count(b"a\nb"), 2);
        assert_eq!(line_count(b"\n\n"), 2);
    }

    #[test]
    fn text_detection_only_looks_at_the_sniff_window() {
        assert!(is_text(b"plain text\n"));
        assert!(!is_text(b"PNG\0\x01"));
        let mut late_nul = vec![b'a'; BINARY_SNIFF_BYTES];
        late_nul.push(0);
        assert!(is_text(&late_nul));
    }

    #[test]
    fn classes_follow_the_documented_precedence() {
        let cases = [
            ("vendor/github.com/x/y.go", FileClass::Vendored),
            ("web/node_modules/react/index.js", FileClass::Vendored),
            ("third_party/x/test/a_test.go", FileClass::Vendored),
            ("web/dist/app.js", FileClass::Generated),
            ("build/out.rs", FileClass::Generated),
            ("static/jquery.min.js", FileClass::Generated),
            ("api/v1/api.pb.go", FileClass::Generated),
            ("models/zz_generated.go", FileClass::Generated),
            ("gateway/conn_test.go", FileClass::Test),
            ("test/helpers.ts", FileClass::Test),
            ("src/__tests__/a.tsx", FileClass::Test),
            ("src/app.spec.ts", FileClass::Test),
            ("webapp/src/x_test.cljs", FileClass::Test),
            ("pkg/test_utils.py", FileClass::Test),
            ("README.md", FileClass::Doc),
            ("docs/guide/setup.go", FileClass::Doc),
            ("config/app.yaml", FileClass::Config),
            ("deps.edn", FileClass::Config),
            ("go.mod", FileClass::Config),
            ("Dockerfile", FileClass::Config),
            ("fixtures/users.csv", FileClass::Data),
            ("gateway/api/conn.go", FileClass::Source),
            ("src/main.rs", FileClass::Source),
            ("Makefile.in", FileClass::Source),
        ];
        for (path, class) in cases {
            assert_eq!(classify_path(&p(path)), class, "{path}");
        }
    }

    #[test]
    fn content_overrides_source_but_not_path_classes() {
        let header = b"// Code generated by protoc-gen-go. DO NOT EDIT.\npackage x\n";
        assert_eq!(classify(&p("api/x.go"), true, Some(header)), FileClass::Generated);
        assert_eq!(classify(&p("api/x_test.go"), true, Some(header)), FileClass::Test);
        assert_eq!(classify(&p("api/x.go"), true, Some(b"// Code is not generated\n")), FileClass::Source);
        assert_eq!(classify(&p("api/x.go"), false, None), FileClass::Binary);
        assert_eq!(classify(&p("vendor/a.png"), false, None), FileClass::Binary);
    }

    #[test]
    fn catalog_paths() {
        for path in [
            "webapp/public/locales/en/errors.json",
            "locales/pt-BR.yml",
            "src/i18n/en.edn",
            "app/i18n/messages/fr.yaml",
            "po/de.po",
            "src/main/resources/messages_fr.properties",
            "translations/en/strings.xliff",
        ] {
            assert!(is_catalog_path(&p(path)), "{path}");
        }
        for path in ["locales.json", "src/i18n.ts", "config/app.properties", "src/locales/index.ts", "i18n/README.md"] {
            assert!(!is_catalog_path(&p(path)), "{path}");
        }
    }
}
