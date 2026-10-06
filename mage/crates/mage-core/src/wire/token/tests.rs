use serde_json::{Value, json};
use time::macros::datetime;

use super::*;

const ISSUER: &str = "https://mage.example.com";
const NOW: OffsetDateTime = datetime!(2026-10-05 12:00:00 UTC);

fn id<T: std::str::FromStr>(s: &str) -> T
where
    T::Err: fmt::Debug,
{
    s.parse().unwrap_or_else(|e| panic!("{s}: {e:?}"))
}

fn key(version: u32, fill: u8) -> TokenSigningKey {
    TokenSigningKey::from_seed(version, &[fill; 32])
}

fn ring(keys: &[&TokenSigningKey]) -> KeyRing {
    let mut ring = KeyRing::new();
    for k in keys {
        ring.insert(k.kid(), &k.public_key()).unwrap_or_else(|e| panic!("{e}"));
    }
    ring
}

fn claims(scope: TokenScope) -> TokenClaims {
    TokenClaims { scope, iat: NOW, exp: NOW + time::Duration::minutes(15), jti: id("0192f0a4-3b5c-7d6e-8f90-a1b2c3d4e5f6") }
}

fn knowledge() -> TokenScope {
    TokenScope::Knowledge {
        org: id("org_01j9z3k4m5n6p7q8r9s0t1v2w3"),
        run: id("run_01j9z3k4m5n6p7q8r9s0t1v2x4"),
        attempt: 2,
        repo: id("repo_01j9z3k4m5n6p7q8r9s0t1v2z6"),
        sha: id("3f2a1c9e5b7d4f6a8c0e2b4d6f8a0c2e4b6d8f0a"),
        memory: MemoryCutoff {
            exclude_tickets: vec![id("tkt_01j9z3k4m5n6p7q8r9s0t1v3g3")],
            as_of: Some(datetime!(2026-09-01 00:00:00 UTC)),
        },
        disabled_layers: vec![Layer::History],
    }
}

fn every_scope() -> Vec<TokenScope> {
    let org: OrgId = id("org_01j9z3k4m5n6p7q8r9s0t1v2w3");
    vec![
        TokenScope::Runner {
            runner: id("rnr_01j9z3k4m5n6p7q8r9s0t1v3f2"),
            generation: 4,
            instance: id("0192f0a4-3b5c-7d6e-8f90-a1b2c3d4e5f7"),
        },
        knowledge(),
        TokenScope::Gateway { org, run: id("run_01j9z3k4m5n6p7q8r9s0t1v2x4"), attempt: 1, models: vec!["claude-sonnet-4-5".into()] },
        TokenScope::Artifact { org, artifact: id("art_01j9z3k4m5n6p7q8r9s0t1v3a7") },
        TokenScope::Blob { org, key: "org/x/runs/y/video/a.mp4".into(), method: HttpMethod::Put, max_bytes: 1 << 30 },
        TokenScope::Indexer { org, repo: id("repo_01j9z3k4m5n6p7q8r9s0t1v2z6") },
    ]
}

fn sign(k: &TokenSigningKey, c: &TokenClaims) -> String {
    TokenCodec::new(ISSUER).sign(k, c).unwrap_or_else(|e| panic!("{e}"))
}

fn verify(ring: &KeyRing, token: &str, kind: TokenKind, now: OffsetDateTime) -> Result<TokenClaims, TokenError> {
    TokenCodec::new(ISSUER).verify(ring, token, kind, now)
}

fn b64_json(part: &str) -> Value {
    let bytes = URL_SAFE_NO_PAD.decode(part).unwrap_or_else(|e| panic!("{e}"));
    serde_json::from_slice(&bytes).unwrap_or_else(|e| panic!("{e}"))
}

/// Signs arbitrary header/payload JSON with `k`, for attacks the codec itself never produces.
fn forge(prefix: &str, header: &Value, payload: &Value, k: &TokenSigningKey) -> String {
    let mut input = URL_SAFE_NO_PAD.encode(serde_json::to_vec(header).unwrap_or_default());
    input.push('.');
    input.push_str(&URL_SAFE_NO_PAD.encode(serde_json::to_vec(payload).unwrap_or_default()));
    let sig = k.key.sign(input.as_bytes());
    format!("{prefix}{input}.{}", URL_SAFE_NO_PAD.encode(sig.to_bytes()))
}

fn parts(token: &str) -> (&str, Value, Value) {
    let kind = TokenKind::of_token(token).unwrap_or_else(|| panic!("no prefix"));
    let jws = &token[kind.prefix().len()..];
    let mut it = jws.split('.');
    let (h, p) = (it.next().unwrap_or_default(), it.next().unwrap_or_default());
    (kind.prefix(), b64_json(h), b64_json(p))
}

#[test]
fn every_kind_signs_and_verifies() {
    let k = key(1, 7);
    let ring = ring(&[&k]);
    for scope in every_scope() {
        let kind = scope.kind();
        let c = claims(scope);
        let token = sign(&k, &c);
        assert!(token.starts_with(kind.prefix()), "{token}");
        assert!(token.bytes().all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b)), "charset: {token}");
        assert_eq!(verify(&ring, &token, kind, NOW), Ok(c), "{kind:?}");
    }
}

#[test]
fn header_and_claim_names_follow_the_format() {
    let k = key(3, 9);
    let token = sign(&k, &claims(knowledge()));
    let jws = &token["mgk_".len()..];
    let header = URL_SAFE_NO_PAD.decode(jws.split('.').next().unwrap_or_default()).unwrap_or_default();
    assert_eq!(
        String::from_utf8(header).unwrap_or_default(),
        format!(r#"{{"alg":"EdDSA","kid":"{}","typ":"mage-knowledge+jwt"}}"#, k.kid())
    );
    let (_, _, payload) = parts(&token);
    assert_eq!(
        payload,
        json!({
            "iss": ISSUER,
            "aud": "mage:knowledge",
            "sub": "run_01j9z3k4m5n6p7q8r9s0t1v2x4",
            "iat": NOW.unix_timestamp(),
            "nbf": NOW.unix_timestamp(),
            "exp": NOW.unix_timestamp() + 900,
            "jti": "0192f0a4-3b5c-7d6e-8f90-a1b2c3d4e5f6",
            "org": "org_01j9z3k4m5n6p7q8r9s0t1v2w3",
            "att": 2,
            "repo": "repo_01j9z3k4m5n6p7q8r9s0t1v2z6",
            "sha": "3f2a1c9e5b7d4f6a8c0e2b4d6f8a0c2e4b6d8f0a",
            "mem": { "excl": ["tkt_01j9z3k4m5n6p7q8r9s0t1v3g3"], "as_of": "2026-09-01T00:00:00Z" },
            "dl": ["history"]
        })
    );

    let runner = sign(&k, &claims(every_scope().remove(0)));
    let (_, header, payload) = parts(&runner);
    assert_eq!(header["typ"], "mage-runner+jwt");
    assert_eq!(payload["aud"], "mage:runner");
    assert_eq!(payload["sub"], "rnr_01j9z3k4m5n6p7q8r9s0t1v3f2");
    assert_eq!(payload["gen"], 4);
    assert_eq!(payload["inst"], "0192f0a4-3b5c-7d6e-8f90-a1b2c3d4e5f7");
}

#[test]
fn kid_is_version_and_public_key_fingerprint() {
    let a = key(3, 1);
    let b = key(3, 2);
    for k in [&a, &b] {
        let (version, fingerprint) = k.kid().split_once('-').unwrap_or_default();
        assert_eq!(version, "v3");
        assert_eq!(fingerprint.len(), 8);
        assert!(fingerprint.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)));
        assert_eq!(k.kid(), kid_for(3, &k.public_key()));
    }
    assert_ne!(a.kid(), b.kid());
    assert!(!format!("{a:?}").contains(&hex::encode([1u8; 32])), "Debug must not print the seed");
}

#[test]
fn wrong_kind_by_prefix_or_typ() {
    let k = key(1, 7);
    let ring = ring(&[&k]);
    let token = sign(&k, &claims(knowledge()));
    assert_eq!(verify(&ring, &token, TokenKind::Gateway, NOW), Err(TokenError::WrongKind));

    // Re-prefixed: the header still says knowledge, so it fails before the signature check.
    let swapped = token.replacen("mgk_", "mgg_", 1);
    assert_eq!(verify(&ring, &swapped, TokenKind::Gateway, NOW), Err(TokenError::WrongKind));

    // Signed with a gateway prefix but a knowledge `typ`.
    let (_, header, payload) = parts(&token);
    let forged = forge("mgg_", &header, &payload, &k);
    assert_eq!(verify(&ring, &forged, TokenKind::Gateway, NOW), Err(TokenError::WrongKind));

    assert_eq!(verify(&ring, "Bearer abc", TokenKind::Knowledge, NOW), Err(TokenError::Malformed));
}

#[test]
fn wrong_audience_or_issuer() {
    let k = key(1, 7);
    let ring = ring(&[&k]);
    let token = sign(&k, &claims(knowledge()));
    let (prefix, header, mut payload) = parts(&token);
    payload["aud"] = json!("mage:gateway");
    assert_eq!(verify(&ring, &forge(prefix, &header, &payload, &k), TokenKind::Knowledge, NOW), Err(TokenError::WrongAudience));

    let other = TokenCodec::new("https://evil.example.com");
    assert_eq!(other.verify(&ring, &token, TokenKind::Knowledge, NOW), Err(TokenError::WrongIssuer));
}

#[test]
fn unknown_kid_is_refused() {
    let signer = key(1, 7);
    let other = key(2, 8);
    let token = sign(&signer, &claims(knowledge()));
    assert_eq!(verify(&ring(&[&other]), &token, TokenKind::Knowledge, NOW), Err(TokenError::UnknownKey));
}

#[test]
fn expiry_and_not_before_have_30s_leeway() {
    let k = key(1, 7);
    let ring = ring(&[&k]);
    let c = claims(knowledge());
    let token = sign(&k, &c);
    let exp = c.exp;
    assert!(verify(&ring, &token, TokenKind::Knowledge, exp + time::Duration::seconds(29)).is_ok());
    assert_eq!(verify(&ring, &token, TokenKind::Knowledge, exp + time::Duration::seconds(30)), Err(TokenError::Expired));
    assert_eq!(verify(&ring, &token, TokenKind::Knowledge, exp + time::Duration::hours(1)), Err(TokenError::Expired));

    assert!(verify(&ring, &token, TokenKind::Knowledge, NOW - time::Duration::seconds(30)).is_ok());
    assert_eq!(verify(&ring, &token, TokenKind::Knowledge, NOW - time::Duration::seconds(31)), Err(TokenError::NotYetValid));

    // `nbf` later than `iat` is honoured too.
    let (prefix, header, mut payload) = parts(&token);
    payload["nbf"] = json!(NOW.unix_timestamp() + 600);
    let later = forge(prefix, &header, &payload, &k);
    assert_eq!(verify(&ring, &later, TokenKind::Knowledge, NOW), Err(TokenError::NotYetValid));
}

#[test]
fn tampering_breaks_the_signature() {
    let k = key(1, 7);
    let other = key(2, 8);
    let ring = ring(&[&k, &other]);
    let token = sign(&k, &claims(knowledge()));
    let (prefix, header, mut payload) = parts(&token);
    let segments: Vec<&str> = token[prefix.len()..].split('.').collect();

    // Payload changed (attempt bumped), signature kept.
    payload["att"] = json!(3);
    let tampered =
        format!("{prefix}{}.{}.{}", segments[0], URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).unwrap_or_default()), segments[2]);
    assert_eq!(verify(&ring, &tampered, TokenKind::Knowledge, NOW), Err(TokenError::BadSignature));

    // Header pointing at another valid key.
    let mut header = header;
    header["kid"] = json!(other.kid());
    let rekeyed =
        format!("{prefix}{}.{}.{}", URL_SAFE_NO_PAD.encode(serde_json::to_vec(&header).unwrap_or_default()), segments[1], segments[2]);
    assert_eq!(verify(&ring, &rekeyed, TokenKind::Knowledge, NOW), Err(TokenError::BadSignature));

    // One flipped signature character.
    let mut sig: Vec<u8> = segments[2].bytes().collect();
    sig[10] = if sig[10] == b'A' { b'B' } else { b'A' };
    let flipped = format!("{prefix}{}.{}.{}", segments[0], segments[1], String::from_utf8(sig).unwrap_or_default());
    assert!(matches!(verify(&ring, &flipped, TokenKind::Knowledge, NOW), Err(TokenError::BadSignature | TokenError::Malformed)));

    // Truncated.
    let truncated = format!("{prefix}{}.{}", segments[0], segments[1]);
    assert_eq!(verify(&ring, &truncated, TokenKind::Knowledge, NOW), Err(TokenError::Malformed));
    assert_eq!(verify(&ring, &token[..token.len() - 5], TokenKind::Knowledge, NOW), Err(TokenError::Malformed));
    // Extra segment.
    assert_eq!(verify(&ring, &format!("{token}.e30"), TokenKind::Knowledge, NOW), Err(TokenError::Malformed));
}

#[test]
fn only_eddsa_is_accepted_and_checked_before_the_key() {
    let k = key(1, 7);
    let ring = ring(&[&k]);
    let token = sign(&k, &claims(knowledge()));
    let (prefix, mut header, payload) = parts(&token);
    for alg in ["none", "HS256", "ES256", "eddsa"] {
        header["alg"] = json!(alg);
        header["kid"] = json!("v9-unknown");
        let forged = forge(prefix, &header, &payload, &k);
        assert_eq!(verify(&ring, &forged, TokenKind::Knowledge, NOW), Err(TokenError::UnsupportedAlgorithm), "{alg}");
    }
    // Extra header members (e.g. `jku`, `crit`) are not tolerated.
    let (_, mut header, _) = parts(&token);
    header["jku"] = json!("https://evil.example.com/keys");
    assert_eq!(verify(&ring, &forge(prefix, &header, &payload, &k), TokenKind::Knowledge, NOW), Err(TokenError::Malformed));
}

#[test]
fn rotation_keeps_retired_keys_verifiable_until_destroyed() {
    let old = key(1, 7);
    let new = key(2, 8);
    let issued_before = sign(&old, &claims(knowledge()));
    let issued_after = sign(&new, &claims(knowledge()));

    // Staged new key published before activation; then old retired: both verify.
    let mut ring = ring(&[&old, &new]);
    assert!(verify(&ring, &issued_before, TokenKind::Knowledge, NOW).is_ok());
    assert!(verify(&ring, &issued_after, TokenKind::Knowledge, NOW).is_ok());

    // Old key destroyed: its tokens die, new ones live.
    assert!(ring.remove(old.kid()));
    assert_eq!(verify(&ring, &issued_before, TokenKind::Knowledge, NOW), Err(TokenError::UnknownKey));
    assert!(verify(&ring, &issued_after, TokenKind::Knowledge, NOW).is_ok());
}

#[test]
fn indexer_verifies_from_the_published_jwk_set() {
    let retired = key(1, 7);
    let active = key(2, 8);
    let set = JwkSet {
        keys: vec![
            Jwk::ed25519(retired.kid(), &retired.public_key(), KeyState::Retired),
            Jwk::ed25519(active.kid(), &active.public_key(), KeyState::Active),
        ],
    };
    let published = serde_json::to_value(&set).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(published["keys"][0]["kty"], "OKP");
    assert_eq!(published["keys"][0]["crv"], "Ed25519");
    assert_eq!(published["keys"][0]["state"], "retired");
    assert_eq!(published["keys"][1]["x"], URL_SAFE_NO_PAD.encode(active.public_key()));

    let fetched: JwkSet = serde_json::from_value(published).unwrap_or_else(|e| panic!("{e}"));
    let ring = KeyRing::from_jwks(&fetched).unwrap_or_else(|e| panic!("{e}"));
    let scope = TokenScope::Indexer { org: id("org_01j9z3k4m5n6p7q8r9s0t1v2w3"), repo: id("repo_01j9z3k4m5n6p7q8r9s0t1v2z6") };
    let c = TokenClaims { exp: NOW + time::Duration::seconds(60), ..claims(scope) };
    for signer in [&retired, &active] {
        assert_eq!(verify(&ring, &sign(signer, &c), TokenKind::Indexer, NOW), Ok(c.clone()));
    }

    let mut bad = fetched.clone();
    bad.keys[0].crv = "X25519".into();
    assert_eq!(KeyRing::from_jwks(&bad), Err(TokenError::Malformed));
    let mut short = fetched;
    short.keys[0].x = URL_SAFE_NO_PAD.encode([1u8; 31]);
    assert_eq!(KeyRing::from_jwks(&short), Err(TokenError::Malformed));
}

#[test]
fn runner_assertion_round_trip_and_rejections() {
    let runner_key = SigningKey::from_bytes(&[5; 32]);
    let public = runner_key.verifying_key().to_bytes();
    let codec = TokenCodec::new(ISSUER);
    let assertion = RunnerAssertion {
        runner: id("rnr_01j9z3k4m5n6p7q8r9s0t1v3f2"),
        iat: NOW,
        exp: NOW + time::Duration::seconds(60),
        jti: id("0192f0a4-3b5c-7d6e-8f90-a1b2c3d4e5f8"),
    };
    let jws = codec.sign_assertion(&runner_key, &assertion).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(TokenKind::of_token(&jws), None, "assertions carry no kind prefix");
    assert_eq!(codec.verify_assertion(&jws, &public, NOW), Ok(assertion));

    let (h, p) = jws.split_once('.').unwrap_or_default();
    let header = b64_json(h);
    let payload = b64_json(p.split('.').next().unwrap_or_default());
    assert_eq!(header, json!({ "alg": "EdDSA", "kid": "rnr_01j9z3k4m5n6p7q8r9s0t1v3f2", "typ": ASSERTION_TYP }));
    assert_eq!(payload["aud"], "https://mage.example.com/runner/v1/register");

    // Another runner's key.
    let stranger = SigningKey::from_bytes(&[6; 32]).verifying_key().to_bytes();
    assert_eq!(codec.verify_assertion(&jws, &stranger, NOW), Err(TokenError::BadSignature));
    // Another server's register URL.
    assert_eq!(TokenCodec::new("https://other.example.com").verify_assertion(&jws, &public, NOW), Err(TokenError::WrongAudience));
    // Too old.
    assert_eq!(codec.verify_assertion(&jws, &public, NOW + time::Duration::seconds(90)), Err(TokenError::Expired));
    // Lifetime above 60 s.
    let long = RunnerAssertion { exp: NOW + time::Duration::seconds(61), ..assertion };
    let long = codec.sign_assertion(&runner_key, &long).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(codec.verify_assertion(&long, &public, NOW), Err(TokenError::LifetimeTooLong));
    // A server-issued runner token is not an assertion.
    let server_key = key(1, 7);
    let runner_token = sign(&server_key, &claims(every_scope().remove(0)));
    assert_eq!(codec.verify_assertion(&runner_token, &server_key.public_key(), NOW), Err(TokenError::WrongKind));
}
