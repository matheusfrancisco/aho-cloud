//! What an app (or the crawler) runs on `X-Principal-JWT`: ES256 signature against the
//! project's JWKS (`/.edge/jwks.json`), then the contract's claim checks.

use cloud_contract::ledger::Stage;
use cloud_contract::principal::{ClaimsError, PrincipalClaims};

use crate::jwt;
use crate::keys::Jwks;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum VerifyError {
    #[error("not a compact JWS")]
    Malformed,
    #[error("algorithm `{0}` is not accepted")]
    Algorithm(String),
    #[error("no matching key in the JWKS")]
    UnknownKey,
    #[error("JWKS key is unusable")]
    BadKey,
    #[error("signature does not verify")]
    Signature,
    #[error("claims: {0}")]
    Payload(String),
    #[error(transparent)]
    Claims(#[from] ClaimsError),
}

/// Verifies a principal token for `project` at `stage` at unix time `now`.
pub fn verify_principal(token: &str, jwks: &Jwks, project: &str, stage: Stage, now: u64) -> Result<PrincipalClaims, VerifyError> {
    let parts = jwt::parse(token)?;
    if parts.header.alg != "ES256" {
        return Err(VerifyError::Algorithm(parts.header.alg));
    }
    let key = jwt::select(jwks, parts.header.kid.as_deref(), "ES256")?;
    jwt::verify("ES256", key, parts.signing_input.as_bytes(), &parts.signature)?;
    let claims: PrincipalClaims = serde_json::from_slice(&parts.payload).map_err(|e| VerifyError::Payload(e.to_string()))?;
    claims.check(project, stage, now)?;
    Ok(claims)
}

#[cfg(test)]
mod tests {
    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use cloud_contract::principal::{TTL_SECS, audience, issuer};

    use super::*;
    use crate::keys::KeyStore;

    fn claims(project: &str, stage: Stage, iat: u64) -> PrincipalClaims {
        PrincipalClaims {
            iss: issuer(project),
            aud: audience(project, stage),
            sub: "pwd:shared".into(),
            email: None,
            email_verified: false,
            groups: vec![],
            amr: vec!["pwd".into()],
            sid: "s".into(),
            iat,
            exp: iat + TTL_SECS,
        }
    }

    #[test]
    fn round_trip_and_every_rejection() {
        let dir = tempfile::tempdir().unwrap();
        let ks = KeyStore::open(dir.path()).unwrap();
        let jwks = ks.jwks("shop", Stage::Live).unwrap();
        let token = ks.mint("shop", Stage::Live, &claims("shop", Stage::Live, 1000)).unwrap();

        assert_eq!(verify_principal(&token, &jwks, "shop", Stage::Live, 1010).unwrap().sub, "pwd:shared");
        assert!(matches!(
            verify_principal(&token, &jwks, "shop", Stage::Staging, 1010),
            Err(VerifyError::Claims(ClaimsError::Audience(..)))
        ));
        assert!(matches!(verify_principal(&token, &jwks, "other", Stage::Live, 1010), Err(VerifyError::Claims(ClaimsError::Issuer(..)))));
        assert_eq!(
            verify_principal(&token, &jwks, "shop", Stage::Live, 1000 + TTL_SECS + 10),
            Err(VerifyError::Claims(ClaimsError::Expired))
        );

        // Another project's key (same claims) never verifies against this JWKS.
        let other = ks.mint("other", Stage::Live, &claims("shop", Stage::Live, 1000)).unwrap();
        assert_eq!(verify_principal(&other, &jwks, "shop", Stage::Live, 1010), Err(VerifyError::UnknownKey));

        // Tampered payload.
        let mut parts: Vec<&str> = token.split('.').collect();
        let mut forged = claims("shop", Stage::Live, 1000);
        forged.sub = "google:admin".into();
        let body = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&forged).unwrap());
        parts[1] = &body;
        assert_eq!(verify_principal(&parts.join("."), &jwks, "shop", Stage::Live, 1010), Err(VerifyError::Signature));

        // alg=none downgrade.
        let none_header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#);
        let none = format!("{none_header}.{body}.");
        assert!(matches!(verify_principal(&none, &jwks, "shop", Stage::Live, 1010), Err(VerifyError::Algorithm(_))));
    }
}
