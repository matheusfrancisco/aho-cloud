//! Webhook signature verifiers for `public` routes with `verify`: Stripe, GitHub and generic
//! hex HMAC-SHA256. Every comparison is constant-time (`Mac::verify_slice`). The signing
//! helpers exist so tests and the crawler can produce valid and invalid signatures.

use cloud_contract::edge::Verifier;
use hmac::{Hmac, Mac};
use http::HeaderMap;
use sha2::Sha256;

/// Largest body the edge buffers to verify a signature.
pub const MAX_BODY: usize = 1 << 20;
/// Accepted distance between a Stripe signature's `t=` and now.
pub const STRIPE_TOLERANCE_SECS: u64 = 300;

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum WebhookError {
    #[error("missing signature header `{0}`")]
    Missing(String),
    #[error("malformed signature header")]
    Malformed,
    #[error("signature timestamp outside the {STRIPE_TOLERANCE_SECS}s tolerance")]
    Expired,
    #[error("signature does not match")]
    Mismatch,
}

/// Checks `body` against the signature `verifier` expects, keyed with `secret`.
pub fn verify(verifier: &Verifier, secret: &str, headers: &HeaderMap, body: &[u8], now: u64) -> Result<(), WebhookError> {
    match verifier {
        Verifier::Stripe { .. } => stripe(secret, header(headers, "stripe-signature")?, body, now),
        Verifier::Github { .. } => {
            let value = header(headers, "x-hub-signature-256")?;
            let hex_sig = value.strip_prefix("sha256=").ok_or(WebhookError::Malformed)?;
            check(secret, &[body], hex_sig)
        }
        Verifier::Hmac { header: name, .. } => check(secret, &[body], header(headers, name)?.trim()),
    }
}

fn header<'h>(headers: &'h HeaderMap, name: &str) -> Result<&'h str, WebhookError> {
    headers
        .get(name.to_ascii_lowercase().as_str())
        .ok_or_else(|| WebhookError::Missing(name.to_owned()))?
        .to_str()
        .map_err(|_| WebhookError::Malformed)
}

fn stripe(secret: &str, value: &str, body: &[u8], now: u64) -> Result<(), WebhookError> {
    let mut t: Option<&str> = None;
    let mut v1 = Vec::new();
    for item in value.split(',') {
        match item.trim().split_once('=') {
            Some(("t", ts)) if t.is_none() => t = Some(ts),
            Some(("t", _)) => return Err(WebhookError::Malformed),
            Some(("v1", sig)) => v1.push(sig),
            _ => {}
        }
    }
    let ts = t.ok_or(WebhookError::Malformed)?;
    let when: u64 = ts.parse().map_err(|_| WebhookError::Malformed)?;
    if v1.is_empty() {
        return Err(WebhookError::Malformed);
    }
    let prefix = format!("{ts}.");
    if !v1.iter().any(|sig| check(secret, &[prefix.as_bytes(), body], sig).is_ok()) {
        return Err(WebhookError::Mismatch);
    }
    if now.abs_diff(when) > STRIPE_TOLERANCE_SECS {
        return Err(WebhookError::Expired);
    }
    Ok(())
}

fn check(secret: &str, parts: &[&[u8]], hex_sig: &str) -> Result<(), WebhookError> {
    let sig = hex::decode(hex_sig).map_err(|_| WebhookError::Malformed)?;
    let mut mac = mac(secret);
    for p in parts {
        mac.update(p);
    }
    mac.verify_slice(&sig).map_err(|_| WebhookError::Mismatch)
}

fn mac(secret: &str) -> HmacSha256 {
    // HMAC accepts keys of any length; this cannot fail.
    <HmacSha256 as Mac>::new_from_slice(secret.as_bytes()).unwrap_or_else(|_| unreachable!("HMAC takes any key"))
}

/// Hex HMAC-SHA256 of `body` (the `hmac` verifier's header value).
pub fn hmac_hex(secret: &str, body: &[u8]) -> String {
    let mut m = mac(secret);
    m.update(body);
    hex::encode(m.finalize().into_bytes())
}

/// `X-Hub-Signature-256` value for `body`.
pub fn github_signature(secret: &str, body: &[u8]) -> String {
    format!("sha256={}", hmac_hex(secret, body))
}

/// `Stripe-Signature` value for `body` signed at `t`.
pub fn stripe_signature(secret: &str, t: u64, body: &[u8]) -> String {
    let mut m = mac(secret);
    m.update(format!("{t}.").as_bytes());
    m.update(body);
    format!("t={t},v1={}", hex::encode(m.finalize().into_bytes()))
}

#[cfg(test)]
mod tests {
    use http::HeaderValue;

    use super::*;

    fn h(name: &'static str, v: &str) -> HeaderMap {
        let mut m = HeaderMap::new();
        m.insert(name, HeaderValue::from_str(v).unwrap());
        m
    }

    const STRIPE: Verifier = Verifier::Stripe { secret: String::new() };

    #[test]
    fn stripe_accepts_any_matching_v1_within_tolerance() {
        let body = br#"{"id":"evt"}"#;
        let good = stripe_signature("whsec", 1000, body);
        assert_eq!(verify(&STRIPE, "whsec", &h("stripe-signature", &good), body, 1000 + 300), Ok(()));
        let rotated = format!("{good},v1={}", "00".repeat(32));
        assert_eq!(verify(&STRIPE, "whsec", &h("stripe-signature", &rotated), body, 1000), Ok(()));
        let v = good.replace("t=1000,", "t=1000,v1=deadbeef,");
        assert_eq!(verify(&STRIPE, "whsec", &h("stripe-signature", &v), body, 1000), Ok(()));
    }

    #[test]
    fn stripe_boundaries() {
        let body = b"{}";
        let sig = stripe_signature("whsec", 1000, body);
        let hdr = h("stripe-signature", &sig);
        assert_eq!(verify(&STRIPE, "whsec", &hdr, body, 1301), Err(WebhookError::Expired));
        assert_eq!(verify(&STRIPE, "whsec", &hdr, body, 699), Err(WebhookError::Expired), "future-dated too");
        assert_eq!(verify(&STRIPE, "other", &hdr, body, 1000), Err(WebhookError::Mismatch));
        assert_eq!(verify(&STRIPE, "whsec", &hdr, b"{ }", 1000), Err(WebhookError::Mismatch));
        // The timestamp is signed: moving it breaks the signature.
        let moved = sig.replace("t=1000", "t=2000");
        assert_eq!(verify(&STRIPE, "whsec", &h("stripe-signature", &moved), body, 2000), Err(WebhookError::Mismatch));
        assert_eq!(verify(&STRIPE, "whsec", &h("stripe-signature", "v1=00"), body, 0), Err(WebhookError::Malformed));
        assert_eq!(verify(&STRIPE, "whsec", &h("stripe-signature", "t=1,t=2,v1=00"), body, 0), Err(WebhookError::Malformed));
        assert!(matches!(verify(&STRIPE, "whsec", &HeaderMap::new(), body, 0), Err(WebhookError::Missing(_))));
    }

    #[test]
    fn github_and_hmac() {
        let gh = Verifier::Github { secret: String::new() };
        let body = b"payload";
        assert_eq!(verify(&gh, "s", &h("x-hub-signature-256", &github_signature("s", body)), body, 0), Ok(()));
        assert_eq!(verify(&gh, "s", &h("x-hub-signature-256", &hmac_hex("s", body)), body, 0), Err(WebhookError::Malformed));
        assert_eq!(verify(&gh, "s", &h("x-hub-signature-256", &github_signature("t", body)), body, 0), Err(WebhookError::Mismatch));
        let hm = Verifier::Hmac { header: "X-Signature".into(), secret: String::new() };
        assert_eq!(verify(&hm, "s", &h("x-signature", &hmac_hex("s", body).to_uppercase()), body, 0), Ok(()));
        assert_eq!(verify(&hm, "s", &h("x-signature", "zz"), body, 0), Err(WebhookError::Malformed));
        assert_eq!(verify(&hm, "s", &h("x-signature", &hmac_hex("s", b"other")), body, 0), Err(WebhookError::Mismatch));
    }
}
