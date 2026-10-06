//! Repository paths and line ranges (knowledge.md Additions: `RepoPath`, `Language`, `FileClass`).

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::domain::error::DomainError;

/// Maximum `RepoPath` length in bytes.
pub const REPO_PATH_MAX_BYTES: usize = 4096;

/// Normalized repository-relative path: UTF-8, `/`-separated, no empty / `.` / `..` segments,
/// no leading `/`, no NUL, at most 4096 bytes. Case-sensitive.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, ts_rs::TS)]
pub struct RepoPath(String);

impl RepoPath {
    pub fn new(path: impl Into<String>) -> Result<Self, DomainError> {
        let path = path.into();
        validate_repo_path(&path)?;
        Ok(Self(path))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }

    /// Last segment (`a/b/c.ts` → `c.ts`).
    pub fn file_name(&self) -> &str {
        self.0.rsplit('/').next().unwrap_or(&self.0)
    }

    /// Extension of the last segment without the dot; `None` for dotfiles without another dot.
    pub fn extension(&self) -> Option<&str> {
        let name = self.file_name();
        let dot = name.rfind('.')?;
        if dot == 0 { None } else { Some(&name[dot + 1..]) }
    }

    /// Directory part (`a/b/c.ts` → `Some("a/b")`, `c.ts` → `None`).
    pub fn parent(&self) -> Option<&str> {
        self.0.rfind('/').map(|i| &self.0[..i])
    }

    pub fn segments(&self) -> impl Iterator<Item = &str> {
        self.0.split('/')
    }

    /// `self` is `dir` or lies below it (segment-wise; `dir` given without trailing slash).
    pub fn is_within(&self, dir: &str) -> bool {
        match self.0.strip_prefix(dir) {
            Some("") => true,
            Some(rest) => rest.starts_with('/'),
            None => false,
        }
    }
}

fn validate_repo_path(path: &str) -> Result<(), DomainError> {
    if path.is_empty() {
        return Err(DomainError::invalid("repo path is empty"));
    }
    if path.len() > REPO_PATH_MAX_BYTES {
        return Err(DomainError::invalid("repo path longer than 4096 bytes"));
    }
    if path.contains('\0') {
        return Err(DomainError::invalid("repo path contains NUL"));
    }
    if path.starts_with('/') {
        return Err(DomainError::invalid("repo path is absolute"));
    }
    for segment in path.split('/') {
        match segment {
            "" => return Err(DomainError::invalid("repo path has an empty segment")),
            "." | ".." => return Err(DomainError::invalid("repo path has a `.` or `..` segment")),
            _ => {}
        }
    }
    Ok(())
}

impl fmt::Display for RepoPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for RepoPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RepoPath({:?})", self.0)
    }
}

impl FromStr for RepoPath {
    type Err = DomainError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

impl AsRef<str> for RepoPath {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl Serialize for RepoPath {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for RepoPath {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Self::new(s).map_err(serde::de::Error::custom)
    }
}

/// 1-based inclusive line range, `start <= end`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, ts_rs::TS)]
#[serde(try_from = "RawLineRange")]
pub struct LineRange {
    start: u32,
    end: u32,
}

#[derive(Deserialize)]
struct RawLineRange {
    start: u32,
    end: u32,
}

impl TryFrom<RawLineRange> for LineRange {
    type Error = DomainError;

    fn try_from(raw: RawLineRange) -> Result<Self, Self::Error> {
        Self::new(raw.start, raw.end)
    }
}

impl LineRange {
    pub fn new(start: u32, end: u32) -> Result<Self, DomainError> {
        if start == 0 {
            return Err(DomainError::invalid("line numbers are 1-based"));
        }
        if end < start {
            return Err(DomainError::invalid("line range end before start"));
        }
        Ok(Self { start, end })
    }

    pub const fn start(&self) -> u32 {
        self.start
    }

    pub const fn end(&self) -> u32 {
        self.end
    }

    /// Number of lines (≥ 1; a range is never empty).
    #[allow(clippy::len_without_is_empty)]
    pub const fn len(&self) -> u32 {
        self.end - self.start + 1
    }

    pub const fn contains(&self, line: u32) -> bool {
        self.start <= line && line <= self.end
    }

    pub const fn overlaps(&self, other: &LineRange) -> bool {
        self.start <= other.end && other.start <= self.end
    }
}

impl fmt::Display for LineRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}", self.start, self.end)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    Go,
    TypeScript,
    Tsx,
    JavaScript,
    Python,
    Rust,
    Java,
    Ruby,
    Clojure,
    ClojureScript,
    Cljc,
    Sql,
    Json,
    Yaml,
    Toml,
    Markdown,
    Html,
    Css,
    Shell,
    Proto,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileClass {
    Source,
    Test,
    Generated,
    Vendored,
    Doc,
    Config,
    Data,
    Binary,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_path_accepts_normal_paths() {
        for p in ["a", "src/main.rs", ".github/workflows/ci.yml", "a/.b/c..d", "dir with space/x"] {
            assert_eq!(RepoPath::new(p).map(|r| r.into_string()), Ok(p.to_string()));
        }
    }

    #[test]
    fn repo_path_rejects_non_normalized() {
        for p in ["", "/abs", "a//b", "a/", "./a", "a/./b", "a/../b", "..", "a\0b"] {
            assert!(RepoPath::new(p).is_err(), "{p:?} must be rejected");
        }
        let long = "a/".repeat(2048) + "b";
        assert!(long.len() > REPO_PATH_MAX_BYTES);
        assert!(RepoPath::new(long).is_err());
        let max = "a".repeat(REPO_PATH_MAX_BYTES);
        assert!(RepoPath::new(max).is_ok());
    }

    #[test]
    fn repo_path_parts() {
        let p = RepoPath::new("web/src/app.test.tsx").unwrap();
        assert_eq!(p.file_name(), "app.test.tsx");
        assert_eq!(p.extension(), Some("tsx"));
        assert_eq!(p.parent(), Some("web/src"));
        assert_eq!(RepoPath::new(".env").unwrap().extension(), None);
        assert_eq!(RepoPath::new("Makefile").unwrap().parent(), None);
        assert!(p.is_within("web"));
        assert!(p.is_within("web/src"));
        assert!(!p.is_within("we"));
        assert!(p.is_within("web/src/app.test.tsx"));
    }

    #[test]
    fn repo_path_serde_validates() {
        assert!(serde_json::from_str::<RepoPath>("\"a/../b\"").is_err());
        let p: RepoPath = serde_json::from_str("\"a/b\"").unwrap();
        assert_eq!(p.as_str(), "a/b");
    }

    #[test]
    fn line_range_bounds() {
        assert!(LineRange::new(0, 1).is_err());
        assert!(LineRange::new(5, 4).is_err());
        let r = LineRange::new(3, 3).unwrap();
        assert_eq!(r.len(), 1);
        let r = LineRange::new(10, 20).unwrap();
        assert!(r.contains(10) && r.contains(20) && !r.contains(21) && !r.contains(9));
        assert!(r.overlaps(&LineRange::new(20, 30).unwrap()));
        assert!(!r.overlaps(&LineRange::new(21, 30).unwrap()));
        assert!(serde_json::from_str::<LineRange>(r#"{"start":4,"end":2}"#).is_err());
        assert_eq!(serde_json::from_str::<LineRange>(r#"{"start":2,"end":4}"#).unwrap(), LineRange::new(2, 4).unwrap());
    }

    #[test]
    fn language_serde_names() {
        assert_eq!(serde_json::to_string(&Language::TypeScript).unwrap(), "\"typescript\"");
        assert_eq!(serde_json::to_string(&Language::ClojureScript).unwrap(), "\"clojurescript\"");
        assert_eq!(serde_json::to_string(&FileClass::Vendored).unwrap(), "\"vendored\"");
    }
}
