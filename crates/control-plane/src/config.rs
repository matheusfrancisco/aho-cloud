//! Runtime configuration: everything the server needs that is not a port or a store.

use std::fmt;

use cloud_domains::EdgeTarget;

/// The 32-byte master key. Every server-side key (secret encryption, fingerprints, domain tokens,
/// site hashes) is derived from it with a distinct blake3 context. Never printed.
#[derive(Clone)]
pub struct MasterKey([u8; 32]);

impl MasterKey {
    pub fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Parses 64 hex digits or 43 base64url characters.
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        if text.len() == 64 {
            let mut out = [0u8; 32];
            for (i, chunk) in text.as_bytes().chunks(2).enumerate() {
                let hi = (chunk[0] as char).to_digit(16)?;
                let lo = (chunk[1] as char).to_digit(16)?;
                out[i] = u8::try_from(hi * 16 + lo).ok()?;
            }
            return Some(Self(out));
        }
        let bytes = crate::crypto::b64url_decode(text)?;
        <[u8; 32]>::try_from(bytes.as_slice()).ok().map(Self)
    }

    /// A key for one purpose; `context` must be unique per purpose.
    pub fn derive(&self, context: &str) -> [u8; 32] {
        blake3::derive_key(context, &self.0)
    }
}

impl fmt::Debug for MasterKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("MasterKey(<redacted>)")
    }
}

/// Server configuration.
#[derive(Clone, Debug)]
pub struct Config {
    /// Base URL other services reach this server at (used for builder archive URLs).
    pub public_url: String,
    /// WebAuthn relying-party id (the console's registrable domain).
    pub rp_id: String,
    /// Exact origin the console is served from, e.g. `https://console.aho.cloud`.
    pub rp_origin: String,
    pub rp_name: String,
    /// Shared service token for internal routes and on-call automation.
    pub service_token: String,
    pub master_key: MasterKey,
    /// Where customer DNS records must point.
    pub edge_target: EdgeTarget,
    /// Preview hostnames are `<project>.<preview_domain>`.
    pub preview_domain: String,
    pub domain_grace_secs: u64,
    pub session_ttl_secs: u64,
    pub device_code_ttl_secs: u64,
    pub device_poll_interval_secs: u32,
    pub ceremony_ttl_secs: u64,
    /// Upper bound on a standing-order grant.
    pub max_standing_ttl_secs: u64,
    /// Largest accepted archive or session-log upload.
    pub max_upload_bytes: usize,
}

impl Config {
    /// A configuration with conventional defaults for everything but the identity of the
    /// deployment.
    pub fn new(master_key: MasterKey, service_token: impl Into<String>, rp_id: impl Into<String>, rp_origin: impl Into<String>) -> Self {
        let rp_origin = rp_origin.into();
        Self {
            public_url: "http://127.0.0.1:8080".into(),
            rp_id: rp_id.into(),
            rp_origin,
            rp_name: "aho cloud".into(),
            service_token: service_token.into(),
            master_key,
            edge_target: EdgeTarget { cname: "edge.aho.cloud".into(), ipv4: Vec::new(), ipv6: Vec::new(), nameservers: Vec::new() },
            preview_domain: "preview.aho.cloud".into(),
            domain_grace_secs: 7 * 24 * 3600,
            session_ttl_secs: 30 * 24 * 3600,
            device_code_ttl_secs: 600,
            device_poll_interval_secs: 5,
            ceremony_ttl_secs: 300,
            max_standing_ttl_secs: 90 * 24 * 3600,
            max_upload_bytes: 256 * 1024 * 1024,
        }
    }

    /// Verification page shown by `ahocloud login`.
    pub fn verification_uri(&self) -> String {
        format!("{}/device", self.rp_origin.trim_end_matches('/'))
    }

    /// Preview hostname of a project.
    pub fn preview_host(&self, project: &str) -> String {
        format!("{project}.{}", self.preview_domain.trim_matches('.')).to_ascii_lowercase()
    }
}
