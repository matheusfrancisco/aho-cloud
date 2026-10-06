//! Minimal path glob matcher for the fix policy (fix.md "Adapter internals": `**`, `*`, `?`;
//! no gitignore semantics, no character classes).
//!
//! Semantics follow git's pathspec glob (wildmatch with `WM_PATHNAME`):
//! - `*` matches any run of characters within one path segment (never `/`);
//! - `?` matches exactly one character other than `/`;
//! - a whole segment `**` matches zero or more segments when leading (`**/x`) or in the middle
//!   (`a/**/b`), and one or more segments when trailing (`a/**` matches everything inside `a`,
//!   not `a` itself);
//! - `**` inside a longer segment behaves like `*`;
//! - every other character matches itself.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::domain::error::DomainError;

/// Maximum glob length in bytes (same bound as `RepoPath`).
pub const GLOB_MAX_BYTES: usize = 4096;

/// Validated repo-relative glob: non-empty, `/`-separated, no leading `/`, no empty / `.` / `..`
/// segments, no NUL.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Glob(String);

impl Glob {
    pub fn new(pattern: impl Into<String>) -> Result<Self, DomainError> {
        let pattern = pattern.into();
        validate(&pattern)?;
        Ok(Self(pattern))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether the repo-relative `path` matches this glob.
    pub fn matches(&self, path: &str) -> bool {
        glob_match(&self.0, path)
    }
}

fn validate(pattern: &str) -> Result<(), DomainError> {
    if pattern.is_empty() {
        return Err(DomainError::invalid("glob is empty"));
    }
    if pattern.len() > GLOB_MAX_BYTES {
        return Err(DomainError::invalid("glob longer than 4096 bytes"));
    }
    if pattern.contains('\0') {
        return Err(DomainError::invalid("glob contains NUL"));
    }
    if pattern.starts_with('/') {
        return Err(DomainError::invalid("glob is absolute"));
    }
    for segment in pattern.split('/') {
        match segment {
            "" => return Err(DomainError::invalid("glob has an empty segment")),
            "." | ".." => return Err(DomainError::invalid("glob has a `.` or `..` segment")),
            _ => {}
        }
    }
    Ok(())
}

impl TryFrom<String> for Glob {
    type Error = DomainError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<Glob> for String {
    fn from(value: Glob) -> Self {
        value.0
    }
}

impl FromStr for Glob {
    type Err = DomainError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

impl fmt::Display for Glob {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for Glob {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Glob({:?})", self.0)
    }
}

/// Segment of `s` starting at byte offset `at` and the offset of the next segment.
/// An offset past `s.len()` means "no segments left".
fn segment_at(s: &str, at: usize) -> (&str, usize) {
    let end = s[at..].find('/').map_or(s.len(), |i| at + i);
    (&s[at..end], end + 1)
}

/// `pattern` against `path`, segment-wise. Greedy matching with backtracking to the last `**`
/// (the classic wildcard algorithm lifted from characters to segments); no allocation.
pub(crate) fn glob_match(pattern: &str, path: &str) -> bool {
    let (mut p, mut s) = (0usize, 0usize);
    // (pattern offset after the last non-trailing `**`, path offset it currently resumes from)
    let mut star: Option<(usize, usize)> = None;
    loop {
        if p <= pattern.len() {
            let (pseg, pnext) = segment_at(pattern, p);
            if pseg == "**" {
                if pnext > pattern.len() {
                    // Trailing `**`: one or more remaining segments. Advancing an earlier `**`
                    // only removes segments, so no backtracking can help when none remain.
                    return s <= path.len();
                }
                star = Some((pnext, s));
                p = pnext;
                continue;
            }
            if s <= path.len() {
                let (sseg, snext) = segment_at(path, s);
                if match_segment(pseg.as_bytes(), sseg.as_bytes()) {
                    p = pnext;
                    s = snext;
                    continue;
                }
            }
        } else if s > path.len() {
            return true;
        }
        match star {
            Some((resume, from)) if from <= path.len() => {
                let (_, next) = segment_at(path, from);
                star = Some((resume, next));
                p = resume;
                s = next;
            }
            _ => return false,
        }
    }
}

/// Length of the UTF-8 sequence starting with `first`.
fn utf8_len(first: u8) -> usize {
    match first {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        _ => 4,
    }
}

/// One segment: `*` = any run, `?` = one character. Both inputs are valid UTF-8; literal bytes
/// only ever match whole characters because text offsets stay on character boundaries.
fn match_segment(pat: &[u8], text: &[u8]) -> bool {
    let (mut pi, mut ti) = (0usize, 0usize);
    // (pattern index after the last `*`, text index it currently resumes from)
    let mut star: Option<(usize, usize)> = None;
    while ti < text.len() {
        match pat.get(pi) {
            Some(b'*') => {
                star = Some((pi + 1, ti));
                pi += 1;
                continue;
            }
            Some(b'?') => {
                pi += 1;
                ti = (ti + utf8_len(text[ti])).min(text.len());
                continue;
            }
            Some(&c) if c == text[ti] => {
                pi += 1;
                ti += 1;
                continue;
            }
            _ => {}
        }
        match star {
            Some((resume, from)) => {
                let next = (from + utf8_len(text[from])).min(text.len());
                star = Some((resume, next));
                pi = resume;
                ti = next;
            }
            None => return false,
        }
    }
    pat[pi..].iter().all(|&c| c == b'*')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn match_table() {
        let cases: &[(&str, &str, bool)] = &[
            // literals
            ("a/b.txt", "a/b.txt", true),
            ("a/b.txt", "a/b.txtx", false),
            ("a/b.txt", "x/a/b.txt", false),
            ("a", "a/b", false),
            // `*` stays inside a segment
            ("*.js", "app.js", true),
            ("*.js", "src/app.js", false),
            ("src/*.js", "src/app.js", true),
            ("src/*.js", "src/lib/app.js", false),
            ("*", "anything", true),
            ("*", "a/b", false),
            ("a*b*c", "aXXbYYc", true),
            ("a*b*c", "aXXbYY", false),
            ("*.min.js", "x.min.js", true),
            ("*.min.js", "x.js", false),
            // `?` is one character, never `/`
            ("?.txt", "a.txt", true),
            ("?.txt", "ab.txt", false),
            ("a?b", "a/b", false),
            ("?", "é", true),
            ("é*", "éte", true),
            // leading `**` = zero or more segments
            ("**/dist/**", "dist/app.js", true),
            ("**/dist/**", "pkg/web/dist/app.js", true),
            ("**/dist/**", "pkg/distx/app.js", false),
            ("**/*.min.js", "vendor/jq.min.js", true),
            ("**/*.min.js", "jq.min.js", true),
            ("**/package-lock.json", "package-lock.json", true),
            ("**/package-lock.json", "a/b/package-lock.json", true),
            // middle `**`
            ("a/**/b", "a/b", true),
            ("a/**/b", "a/x/y/b", true),
            ("a/**/b", "a/x/y/c", false),
            ("a/**/b/**/c", "a/1/b/2/b/3/c", true),
            // trailing `**` = one or more segments
            (".mage/**", ".mage/env.toml", true),
            (".mage/**", ".mage/repro/c/x.spec.ts", true),
            (".mage/**", ".mage", false),
            (".mage/**", ".magex/env.toml", false),
            ("**", "a", true),
            ("**", "a/b/c", true),
            ("**/a/**", "a", false),
            ("**/a/**", "x/a/y", true),
            // `**` inside a segment acts like `*`
            ("a**b", "aXb", true),
            ("a**b", "a/b", false),
            // no character classes
            ("[ab].txt", "[ab].txt", true),
            ("[ab].txt", "a.txt", false),
        ];
        for &(pattern, path, want) in cases {
            let glob = Glob::new(pattern).unwrap();
            assert_eq!(glob.matches(path), want, "{pattern} vs {path}");
        }
    }

    #[test]
    fn rejects_invalid_patterns() {
        for bad in ["", "/abs/**", "a//b", "dist/", "a/./b", "../x", "a\0b"] {
            assert!(Glob::new(bad).is_err(), "{bad:?}");
        }
        assert!(Glob::new("x".repeat(GLOB_MAX_BYTES + 1)).is_err());
    }

    #[test]
    fn serde_validates() {
        let glob: Glob = serde_json::from_str("\"src/generated/**\"").unwrap();
        assert_eq!(serde_json::to_string(&glob).unwrap(), "\"src/generated/**\"");
        assert!(serde_json::from_str::<Glob>("\"/etc/**\"").is_err());
    }
}
