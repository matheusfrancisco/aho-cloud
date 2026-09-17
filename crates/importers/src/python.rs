//! Python projects: `pyproject.toml` (`[project]` or `[tool.poetry]` name) and
//! `requirements.txt`; a buildpack build without a Dockerfile.

use std::path::Path;

use serde::Deserialize;
use serde_json::json;
use toml::Spanned;

use crate::{DEFAULT_BUILDER, Fact, Findings, line_of, read, slug};

const NAME: &str = "python";

#[derive(Deserialize)]
struct Pyproject {
    project: Option<Named>,
    tool: Option<Tool>,
}

#[derive(Deserialize)]
struct Tool {
    poetry: Option<Named>,
}

#[derive(Deserialize)]
struct Named {
    name: Option<Spanned<String>>,
}

pub(crate) fn import(root: &Path) -> Option<Findings> {
    let pyproject = read(root, "pyproject.toml");
    let requirements = read(root, "requirements.txt");
    if pyproject.is_none() && requirements.is_none() {
        return None;
    }
    let mut f = Findings::new(NAME);
    let mut evidence: Option<(&str, usize, String)> = None;
    match pyproject {
        Some(Ok(text)) => match toml::from_str::<Pyproject>(&text) {
            Ok(py) => {
                let name = py.project.and_then(|p| p.name).or_else(|| py.tool.and_then(|t| t.poetry).and_then(|p| p.name));
                if let Some(n) = name {
                    let line = line_of(&text, n.span().start);
                    match slug(n.get_ref(), 40) {
                        Some(s) => f.project = Some(Fact::new(json!(s), "pyproject.toml", line, format!("name = \"{}\"", n.get_ref()))),
                        None => f.problems.push(format!("pyproject.toml:{line}: name `{}` cannot be a project slug", n.get_ref())),
                    }
                }
                evidence = Some(("pyproject.toml", 1, "Python project (pyproject.toml)".into()));
            }
            Err(e) => f.problems.push(format!("pyproject.toml: {}", e.message())),
        },
        Some(Err(e)) => f.problems.push(e),
        None => {}
    }
    match requirements {
        Some(Ok(text)) => {
            let first = text.lines().position(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#')).map_or(1, |i| i + 1);
            evidence.get_or_insert(("requirements.txt", first, "Python project (requirements.txt)".into()));
        }
        Some(Err(e)) => f.problems.push(e),
        None => {}
    }
    if let Some((file, line, note)) = evidence
        && !root.join("Dockerfile").is_file()
    {
        f.build =
            Some(Fact::new(json!({"kind": "buildpack", "builder": DEFAULT_BUILDER}), file, line, format!("{note} without a Dockerfile")));
    }
    Some(f)
}
