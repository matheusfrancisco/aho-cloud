//! Reading the pushed image digest back from the build tools.
//!
//! - `buildctl … --metadata-file <f>` writes JSON whose `containerimage.digest` is the manifest
//!   digest it pushed.
//! - `pack build --publish` runs the lifecycle exporter, which prints `*** Digest: sha256:…` for
//!   registry images (pack prefixes lifecycle lines with `[exporter] `). The exporter's
//!   `report.toml` stays inside the lifecycle container, so pack's output is the only place the
//!   host can read the digest.

use serde_json::Value;

/// Key of the pushed manifest digest in buildctl's metadata file.
pub const BUILDCTL_DIGEST_KEY: &str = "containerimage.digest";

const PACK_DIGEST_MARKER: &str = "*** Digest:";

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DigestError {
    #[error("buildctl metadata is not JSON: {0}")]
    NotJson(String),
    #[error("buildctl metadata has no `{BUILDCTL_DIGEST_KEY}`")]
    MissingFromMetadata,
    #[error("pack printed no `{PACK_DIGEST_MARKER}` line")]
    MissingFromPack,
    #[error("`{0}` is not a sha256 image digest")]
    Malformed(String),
}

/// `sha256:` followed by 64 lowercase hex digits: the only digest form the cloud accepts.
pub fn is_sha256_digest(s: &str) -> bool {
    s.strip_prefix("sha256:").is_some_and(|h| h.len() == 64 && h.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)))
}

/// The pushed digest from the contents of buildctl's `--metadata-file`.
pub fn from_buildctl_metadata(metadata: &[u8]) -> Result<String, DigestError> {
    let value: Value = serde_json::from_slice(metadata).map_err(|e| DigestError::NotJson(e.to_string()))?;
    match value.get(BUILDCTL_DIGEST_KEY) {
        None | Some(Value::Null) => Err(DigestError::MissingFromMetadata),
        Some(Value::String(s)) if is_sha256_digest(s) => Ok(s.clone()),
        Some(Value::String(s)) => Err(DigestError::Malformed(s.clone())),
        Some(other) => Err(DigestError::Malformed(other.to_string())),
    }
}

/// The pushed digest from pack's output: the last `*** Digest:` line, colour codes and the
/// `[exporter]` prefix ignored.
pub fn from_pack_output(output: &str) -> Result<String, DigestError> {
    let mut found = None;
    for line in output.lines() {
        let line = strip_ansi(line);
        if let Some(at) = line.find(PACK_DIGEST_MARKER) {
            let rest = &line[at + PACK_DIGEST_MARKER.len()..];
            found = Some(rest.split_whitespace().next().unwrap_or_default().to_owned());
        }
    }
    match found {
        None => Err(DigestError::MissingFromPack),
        Some(d) if is_sha256_digest(&d) => Ok(d),
        Some(d) => Err(DigestError::Malformed(d)),
    }
}

/// Drops ANSI CSI sequences (`ESC [ … final`), which pack emits when it thinks it has a terminal.
fn strip_ansi(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        if chars.next() == Some('[') {
            for c in chars.by_ref() {
                if ('\u{40}'..='\u{7e}').contains(&c) {
                    break;
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const D: &str = "sha256:6c3c624b58dbbcd3c0dd82b4c53f04194d1247c6eebdaab7c610cf7d66709b3b";

    #[test]
    fn digest_format_is_strict() {
        assert!(is_sha256_digest(D));
        assert!(!is_sha256_digest(&D.to_uppercase()));
        assert!(!is_sha256_digest(&D[..D.len() - 1]));
        assert!(!is_sha256_digest(&format!("{D}0")));
        assert!(!is_sha256_digest(&D.replace("sha256", "sha512")));
        assert!(!is_sha256_digest("registry/app@sha256:abc"));
    }

    #[test]
    fn buildctl_metadata_valid() {
        let meta =
            format!(r#"{{"containerimage.config.digest":"sha256:{}","{BUILDCTL_DIGEST_KEY}":"{D}","image.name":"r/p:1"}}"#, "0".repeat(64));
        assert_eq!(from_buildctl_metadata(meta.as_bytes()), Ok(D.to_owned()));
    }

    #[test]
    fn buildctl_metadata_missing_or_malformed() {
        assert_eq!(from_buildctl_metadata(br#"{"image.name":"r/p:1"}"#), Err(DigestError::MissingFromMetadata));
        assert_eq!(from_buildctl_metadata(br#"{"containerimage.digest":null}"#), Err(DigestError::MissingFromMetadata));
        assert_eq!(from_buildctl_metadata(br#"{"containerimage.digest":"sha256:xyz"}"#), Err(DigestError::Malformed("sha256:xyz".into())));
        assert_eq!(from_buildctl_metadata(br#"{"containerimage.digest":42}"#), Err(DigestError::Malformed("42".into())));
        assert!(matches!(from_buildctl_metadata(b""), Err(DigestError::NotJson(_))));
        assert!(matches!(from_buildctl_metadata(b"{\"containerimage.digest\":"), Err(DigestError::NotJson(_))));
    }

    #[test]
    fn pack_output_valid_with_prefix_colour_and_last_line_winning() {
        let other = format!("sha256:{}", "a".repeat(64));
        let out = format!(
            "===> EXPORTING\n[exporter] *** Digest: {other}\n[exporter] Saving r/p:1...\n\
             \u{1b}[36m[exporter]\u{1b}[0m *** Images (6c3c624b58db):\n[exporter]       r/p:1\n\
             \u{1b}[36m[exporter]\u{1b}[0m *** Digest: {D}\nSuccessfully built image r/p:1\n"
        );
        assert_eq!(from_pack_output(&out), Ok(D.to_owned()));
    }

    #[test]
    fn pack_output_missing_or_malformed() {
        assert_eq!(from_pack_output("[exporter] *** Images (6c3c624b58db):\n"), Err(DigestError::MissingFromPack));
        assert_eq!(from_pack_output(""), Err(DigestError::MissingFromPack));
        assert_eq!(from_pack_output("[exporter] *** Digest: sha256:short\n"), Err(DigestError::Malformed("sha256:short".into())));
        assert_eq!(from_pack_output("[exporter] *** Digest:\n"), Err(DigestError::Malformed(String::new())));
    }
}
