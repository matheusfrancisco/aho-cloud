//! Pure shared knowledge functions (knowledge.md "Crate placement" and "Additions"): the indexer
//! builds blob keys, error patterns, ticket links, and app-map keys with these; the server reads
//! and matches against them. Both roles must compute them byte for byte identically.
//!
//! - [`tokenize`], [`to_pattern`]: error-site templates (Layer 3), versioned by `ErrorsRev`.
//! - [`ticket_ids_in_text`], [`ticket_ids_in_branch`]: ticket identifier scanner (Layer 6).
//! - [`RoutePattern`], [`RouteKey`], [`segment_match`], [`route_match`]: URL/route
//!   canonicalization and segment matching (Layer 4 linker, `route_info`).
//! - [`blob_content_key`]: object key of a blob's raw content (Layer 1, storage.md "Blob layout").
//!
//! `ports::knowledge` fact types that carry these inputs (`UrlTemplate`/`UrlPart`) re-export them
//! from here: the domain cannot depend on ports.

mod pattern;
mod route;
mod ticket_ids;

pub use pattern::{ErrorPatternDraft, Seg, Template, VarClass, to_pattern, tokenize};
pub use route::{
    MethodMatch, RouteKey, RouteMatch, RoutePattern, RouteSegment, SegmentMatch, UrlPart, UrlTemplate, route_match, segment_match,
};
pub use ticket_ids::{ticket_ids_in_branch, ticket_ids_in_text};

use crate::domain::ids::{BlobOid, OrgId};

/// Object key of a text blob's raw content: `org/{org}/blobs/{oid_hex[0..2]}/{oid_hex}`, with
/// `{org}` the canonical lowercase hyphenated UUID (storage.md "Keys"), so every char is in
/// `BlobKey`'s `[a-z0-9._/-]`. `BlobKey::blob_content` wraps this.
pub fn blob_content_key(org: OrgId, oid: BlobOid) -> String {
    format!("org/{}/blobs/{:02x}/{oid}", org.as_uuid().hyphenated(), oid.0[0])
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::*;

    #[test]
    fn blob_content_key_uses_uuid_and_two_char_fanout() {
        let org = OrgId::from_uuid(Uuid::from_u128(0x0190_f8a1_2b3c_7d4e_8f00_1122_3344_5566));
        let oid: BlobOid = "ab12cd34ef56ab12cd34ef56ab12cd34ef56ab12".parse().unwrap();
        let key = blob_content_key(org, oid);
        assert_eq!(key, "org/0190f8a1-2b3c-7d4e-8f00-112233445566/blobs/ab/ab12cd34ef56ab12cd34ef56ab12cd34ef56ab12");
        assert!(key.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._/-".contains(&b)));
        // The fan-out directory keeps its leading zero.
        let oid = BlobOid::from_bytes([0x0a; 20]);
        assert!(blob_content_key(org, oid).contains("/blobs/0a/0a0a"));
    }
}
