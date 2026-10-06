//! `Parser::detect_language`: extension, then well-known file names, then the shebang line.

use mage_core::domain::{Language, RepoPath};

pub(super) fn detect_language(path: &RepoPath, head: &[u8]) -> Option<Language> {
    if let Some(ext) = path.extension() {
        if let Some(lang) = by_extension(&ext.to_ascii_lowercase()) {
            return Some(lang);
        }
    }
    by_file_name(path.file_name()).or_else(|| by_shebang(head))
}

fn by_extension(ext: &str) -> Option<Language> {
    Some(match ext {
        "go" => Language::Go,
        "ts" | "mts" | "cts" => Language::TypeScript,
        "tsx" => Language::Tsx,
        // JSX is parsed with the javascript grammar.
        "js" | "jsx" | "mjs" | "cjs" => Language::JavaScript,
        "py" | "pyi" => Language::Python,
        "rs" => Language::Rust,
        "clj" | "edn" => Language::Clojure,
        "cljs" => Language::ClojureScript,
        "cljc" => Language::Cljc,
        "java" => Language::Java,
        "rb" => Language::Ruby,
        "sql" => Language::Sql,
        "json" => Language::Json,
        "yaml" | "yml" => Language::Yaml,
        "toml" => Language::Toml,
        "md" | "markdown" => Language::Markdown,
        "html" | "htm" => Language::Html,
        "css" | "scss" => Language::Css,
        "sh" | "bash" | "zsh" => Language::Shell,
        "proto" => Language::Proto,
        _ => return None,
    })
}

/// Extensionless or misleadingly named files whose format is known. `Dockerfile` / `Makefile`
/// have no `Language` variant and stay `None`.
fn by_file_name(name: &str) -> Option<Language> {
    Some(match name {
        "Cargo.lock" | "Pipfile" | "poetry.lock" | "uv.lock" => Language::Toml,
        "Gemfile" | "Rakefile" => Language::Ruby,
        ".bashrc" | ".bash_profile" | ".zshrc" | ".profile" => Language::Shell,
        _ => return None,
    })
}

/// `#!/usr/bin/env python3`, `#!/bin/bash`, `#!/usr/bin/env -S node --flag`.
fn by_shebang(head: &[u8]) -> Option<Language> {
    let rest = head.strip_prefix(b"#!")?;
    let line_end = rest.iter().position(|b| *b == b'\n').unwrap_or(rest.len());
    let line = std::str::from_utf8(&rest[..line_end]).ok()?;
    let mut words = line.split_whitespace();
    let mut program = words.next()?.rsplit('/').next()?;
    if program == "env" {
        program = words.find(|w| !w.starts_with('-'))?.rsplit('/').next()?;
    }
    let base = program.trim_end_matches(|c: char| c.is_ascii_digit() || c == '.');
    match base {
        "python" | "pypy" => Some(Language::Python),
        "node" | "nodejs" => Some(Language::JavaScript),
        "bash" | "sh" | "zsh" | "dash" | "ksh" => Some(Language::Shell),
        "ruby" => Some(Language::Ruby),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn detect(path: &str, head: &str) -> Option<Language> {
        detect_language(&RepoPath::new(path).unwrap(), head.as_bytes())
    }

    #[test]
    fn extensions_map_to_languages() {
        let cases = [
            ("cmd/main.go", Some(Language::Go)),
            ("web/a.ts", Some(Language::TypeScript)),
            ("web/a.mts", Some(Language::TypeScript)),
            ("web/a.cts", Some(Language::TypeScript)),
            ("web/types.d.ts", Some(Language::TypeScript)),
            ("web/App.tsx", Some(Language::Tsx)),
            ("web/App.jsx", Some(Language::JavaScript)),
            ("web/a.mjs", Some(Language::JavaScript)),
            ("web/a.cjs", Some(Language::JavaScript)),
            ("svc/a.py", Some(Language::Python)),
            ("svc/a.pyi", Some(Language::Python)),
            ("src/lib.rs", Some(Language::Rust)),
            ("src/core.clj", Some(Language::Clojure)),
            ("deps.edn", Some(Language::Clojure)),
            ("src/app.cljs", Some(Language::ClojureScript)),
            ("src/shared.cljc", Some(Language::Cljc)),
            ("A.java", Some(Language::Java)),
            ("a.rb", Some(Language::Ruby)),
            ("db/1.sql", Some(Language::Sql)),
            ("package.json", Some(Language::Json)),
            ("ci.yml", Some(Language::Yaml)),
            ("ci.YAML", Some(Language::Yaml)),
            ("Cargo.toml", Some(Language::Toml)),
            ("README.MD", Some(Language::Markdown)),
            ("index.htm", Some(Language::Html)),
            ("a.scss", Some(Language::Css)),
            ("run.zsh", Some(Language::Shell)),
            ("api.proto", Some(Language::Proto)),
            ("notes.txt", None),
            ("Dockerfile", None),
            ("Makefile", None),
        ];
        for (path, want) in cases {
            assert_eq!(detect(path, ""), want, "{path}");
        }
    }

    #[test]
    fn file_names_map_to_languages() {
        assert_eq!(detect("Cargo.lock", ""), Some(Language::Toml));
        assert_eq!(detect("Gemfile", ""), Some(Language::Ruby));
        assert_eq!(detect("home/.bashrc", ""), Some(Language::Shell));
    }

    #[test]
    fn shebang_detects_extensionless_scripts() {
        assert_eq!(detect("bin/tool", "#!/usr/bin/env python3\nimport sys\n"), Some(Language::Python));
        assert_eq!(detect("bin/tool", "#!/usr/bin/python3.11\n"), Some(Language::Python));
        assert_eq!(detect("bin/serve", "#!/usr/bin/env -S node --experimental\n"), Some(Language::JavaScript));
        assert_eq!(detect("bin/run", "#!/bin/bash\nset -e\n"), Some(Language::Shell));
        assert_eq!(detect("bin/run", "#!/bin/sh\n"), Some(Language::Shell));
        assert_eq!(detect("bin/run", "#!/usr/bin/env perl\n"), None);
        assert_eq!(detect("bin/run", "echo hi\n"), None);
    }

    #[test]
    fn extension_wins_over_shebang() {
        assert_eq!(detect("scripts/x.py", "#!/bin/bash\n"), Some(Language::Python));
    }
}
