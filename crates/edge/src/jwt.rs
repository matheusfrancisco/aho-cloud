//! Compact JWS parsing and signature checks against a JWK (ES256, RS256). Only asymmetric
//! algorithms exist here: `none` and HMAC algorithms are never accepted.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use p256::ecdsa::signature::Verifier as _;
use serde::Deserialize;

use crate::keys::{Jwk, Jwks};
use crate::verify::VerifyError;

/// RSA keys shorter than this are refused.
const MIN_RSA_BITS: usize = 2048;

#[derive(Debug, Deserialize)]
pub(crate) struct Header {
    pub alg: String,
    #[serde(default)]
    pub kid: Option<String>,
}

/// A split, decoded token. `signing_input` borrows the original `header.payload`.
pub(crate) struct Parts<'a> {
    pub header: Header,
    pub payload: Vec<u8>,
    pub signing_input: &'a str,
    pub signature: Vec<u8>,
}

pub(crate) fn parse(token: &str) -> Result<Parts<'_>, VerifyError> {
    let (signing_input, sig) = token.rsplit_once('.').ok_or(VerifyError::Malformed)?;
    let (header, payload) = signing_input.split_once('.').ok_or(VerifyError::Malformed)?;
    if payload.contains('.') {
        return Err(VerifyError::Malformed);
    }
    let header: Header = serde_json::from_slice(&unb64(header)?).map_err(|_| VerifyError::Malformed)?;
    Ok(Parts { header, payload: unb64(payload)?, signing_input, signature: unb64(sig)? })
}

fn unb64(s: &str) -> Result<Vec<u8>, VerifyError> {
    URL_SAFE_NO_PAD.decode(s).map_err(|_| VerifyError::Malformed)
}

fn compatible(jwk: &Jwk, alg: &str) -> bool {
    let kty_ok = match alg {
        "ES256" => jwk.kty == "EC" && jwk.crv.as_deref() == Some("P-256"),
        "RS256" => jwk.kty == "RSA",
        _ => false,
    };
    kty_ok && jwk.alg.as_deref().is_none_or(|a| a == alg) && jwk.use_.as_deref().is_none_or(|u| u == "sig")
}

/// The key for `kid` (or the only compatible key when the token names none).
pub(crate) fn select<'k>(jwks: &'k Jwks, kid: Option<&str>, alg: &str) -> Result<&'k Jwk, VerifyError> {
    let mut candidates = jwks.keys.iter().filter(|k| compatible(k, alg));
    match kid {
        Some(kid) => candidates.find(|k| k.kid.as_deref() == Some(kid)).ok_or(VerifyError::UnknownKey),
        None => match (candidates.next(), candidates.next()) {
            (Some(k), None) => Ok(k),
            _ => Err(VerifyError::UnknownKey),
        },
    }
}

/// Checks `sig` over `input` with `jwk` under `alg`.
pub(crate) fn verify(alg: &str, jwk: &Jwk, input: &[u8], sig: &[u8]) -> Result<(), VerifyError> {
    match alg {
        "ES256" => {
            let (Some(x), Some(y)) = (&jwk.x, &jwk.y) else { return Err(VerifyError::BadKey) };
            let (x, y) = (unb64(x).map_err(|_| VerifyError::BadKey)?, unb64(y).map_err(|_| VerifyError::BadKey)?);
            if x.len() != 32 || y.len() != 32 {
                return Err(VerifyError::BadKey);
            }
            let point =
                p256::EncodedPoint::from_affine_coordinates(p256::FieldBytes::from_slice(&x), p256::FieldBytes::from_slice(&y), false);
            let key = p256::ecdsa::VerifyingKey::from_encoded_point(&point).map_err(|_| VerifyError::BadKey)?;
            let sig = p256::ecdsa::Signature::from_slice(sig).map_err(|_| VerifyError::Signature)?;
            key.verify(input, &sig).map_err(|_| VerifyError::Signature)
        }
        "RS256" => {
            use rsa::traits::PublicKeyParts;
            let (Some(n), Some(e)) = (&jwk.n, &jwk.e) else { return Err(VerifyError::BadKey) };
            let n = rsa::BigUint::from_bytes_be(&unb64(n).map_err(|_| VerifyError::BadKey)?);
            let e = rsa::BigUint::from_bytes_be(&unb64(e).map_err(|_| VerifyError::BadKey)?);
            let key = rsa::RsaPublicKey::new(n, e).map_err(|_| VerifyError::BadKey)?;
            if key.size() * 8 < MIN_RSA_BITS {
                return Err(VerifyError::BadKey);
            }
            let key = rsa::pkcs1v15::VerifyingKey::<sha2::Sha256>::new(key);
            let sig = rsa::pkcs1v15::Signature::try_from(sig).map_err(|_| VerifyError::Signature)?;
            key.verify(input, &sig).map_err(|_| VerifyError::Signature)
        }
        other => Err(VerifyError::Algorithm(other.to_owned())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ec(kid: Option<&str>) -> Jwk {
        Jwk {
            kty: "EC".into(),
            kid: kid.map(str::to_owned),
            alg: None,
            use_: None,
            crv: Some("P-256".into()),
            x: Some(String::new()),
            y: Some(String::new()),
            n: None,
            e: None,
        }
    }

    #[test]
    fn select_by_kid_or_sole_key() {
        let one = Jwks { keys: vec![ec(Some("a"))] };
        assert!(select(&one, None, "ES256").is_ok());
        assert!(select(&one, Some("a"), "ES256").is_ok());
        assert!(matches!(select(&one, Some("b"), "ES256"), Err(VerifyError::UnknownKey)));
        assert!(matches!(select(&one, Some("a"), "RS256"), Err(VerifyError::UnknownKey)), "kty must fit alg");
        let two = Jwks { keys: vec![ec(Some("a")), ec(Some("b"))] };
        assert!(matches!(select(&two, None, "ES256"), Err(VerifyError::UnknownKey)), "ambiguous without kid");
        let mut enc = ec(Some("a"));
        enc.use_ = Some("enc".into());
        assert!(select(&Jwks { keys: vec![enc] }, Some("a"), "ES256").is_err(), "encryption keys never verify");
    }

    #[test]
    fn parse_rejects_wrong_shapes() {
        assert!(matches!(parse("a.b"), Err(VerifyError::Malformed)));
        assert!(matches!(parse("a.b.c.d"), Err(VerifyError::Malformed)));
        assert!(matches!(parse("!!.e30.AA"), Err(VerifyError::Malformed)));
    }

    #[test]
    fn symmetric_and_none_algorithms_are_refused() {
        for alg in ["none", "HS256", "PS256"] {
            assert!(matches!(verify(alg, &ec(None), b"x", b"y"), Err(VerifyError::Algorithm(_))), "{alg}");
        }
    }
}
