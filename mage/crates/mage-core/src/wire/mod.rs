//! `mage_core::wire`: serde DTOs every role speaks, so no side keeps a mirror
//! (docs/internals/README.md, runs.md, knowledge.md, server.md, triage.md, executor.md, fix.md).
//!
//! - [`runner`]: the `/runner/v1` protocol between `mage_server` and `mage_runner` (runs.md).
//! - [`indexer`]: the indexer internal API `/internal/v1` and the git/search DTOs shared with
//!   `IndexerLink` (knowledge.md).
//! - [`api`]: the UI JSON API `/api/v1` and SSE payloads (server.md), with ts-rs derives.
//! - [`token`]: compact-JWS codec and claim structs for every signed Mage token (server.md).
//! - [`mcp`]: Run MCP `submit_*` tool inputs, outputs and their checked-in JSON Schemas.
//! - [`ts`]: renders the TypeScript for [`api`] into `mage/web/src/gen/mage.ts`.
//!
//! Conventions (runs.md "Conventions"): times are RFC 3339 UTC; durations are `*_ms` integers;
//! byte arrays are lowercase hex; ids use their prefixed `Display` form. Domain value types appear
//! inside the DTOs as-is; aggregates never go over the runner wire. Route templates use axum's
//! `{name}` placeholders and are filled with [`fill`].

pub mod api;
pub mod indexer;
pub mod mcp;
pub mod runner;
pub mod token;
pub mod ts;

/// Replaces every `{name}` placeholder of a route template with its percent-encoded value.
/// Unknown placeholders become empty, so a missing argument shows up as `//` in the path.
pub fn fill(template: &str, args: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(template.len() + 32);
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let Some(close) = rest[open..].find('}') else {
            out.push_str(&rest[open..]);
            return out;
        };
        let name = &rest[open + 1..open + close];
        if let Some((_, value)) = args.iter().find(|(k, _)| *k == name) {
            encode_segment(value, &mut out);
        }
        rest = &rest[open + close + 1..];
    }
    out.push_str(rest);
    out
}

/// RFC 3986 unreserved characters stay; every other byte becomes `%XX`.
fn encode_segment(value: &str, out: &mut String) {
    for b in value.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(char::from(b));
        } else {
            out.push('%');
            out.push(char::from(HEX[usize::from(b >> 4)]));
            out.push(char::from(HEX[usize::from(b & 0xf)]));
        }
    }
}

const HEX: &[u8; 16] = b"0123456789ABCDEF";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fill_encodes_values_and_leaves_literals() {
        assert_eq!(
            fill(runner::routes::HEARTBEAT, &[("run", "run_01j9z3k4m5n6p7q8r9s0t1v2x4")]),
            "/runner/v1/runs/run_01j9z3k4m5n6p7q8r9s0t1v2x4/heartbeat"
        );
        assert_eq!(
            fill(api::routes::REPO_DEPLOYMENT, &[("org", "hoop"), ("repo", "repo_x"), ("environment", "eu prod/1")]),
            "/api/v1/orgs/hoop/repos/repo_x/deployments/eu%20prod%2F1"
        );
        assert_eq!(fill("/a/{missing}/b", &[]), "/a//b");
        assert_eq!(fill("/unterminated/{x", &[("x", "1")]), "/unterminated/{x");
    }

    #[test]
    fn every_route_placeholder_is_a_known_parameter() {
        const PARAMS: [&str; 13] =
            ["org", "ticket", "cause", "run", "artifact", "repo", "environment", "customer", "kind", "external_id", "invite", "user", "id"];
        for (name, route) in api::routes::ALL.iter().chain(runner::routes::ALL).chain(indexer::routes::ALL) {
            let mut rest = *route;
            while let Some(open) = rest.find('{') {
                let close = rest[open..].find('}').unwrap_or_else(|| panic!("{name}: unterminated placeholder"));
                let param = &rest[open + 1..open + close];
                assert!(PARAMS.contains(&param), "{name}: unknown placeholder {{{param}}}");
                rest = &rest[open + close + 1..];
            }
            assert!(route.starts_with('/'), "{name}");
        }
    }
}
