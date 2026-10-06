//! [`FakeTokenSigner`] (fixed seed, one Active key) and the generic `TokenSigner` contract suite
//! (server.md "Testing"), run against the fake and `Ed25519TokenSigner` over a vault.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::SigningKey;
use time::OffsetDateTime;
use uuid::Uuid;

use mage_core::domain::{ArtifactId, CommitSha, Layer, OrgId, RepoId, RunId, RunnerId, TicketId};
use mage_core::ports::blob::HttpMethod;
use mage_core::testing::FakeClock;
use mage_core::wire::token::{KeyRing, LEEWAY, TokenCodec, TokenSigningKey};

use crate::ports::audit::Actor;
use crate::ports::auth::{
    MemoryCutoff, PublicKeyInfo, RunnerAssertion, SignedToken, TokenClaims, TokenError, TokenKind, TokenScope, TokenSigner,
};
use crate::ports::vault::{Activation, SecretBytes, SecretName, SecretScope, SecretState, SecretVault, SecretVersion, names};

/// `TokenSigner` over one fixed Ed25519 key (version 1, Active). `reload` changes nothing.
pub struct FakeTokenSigner {
    codec: TokenCodec,
    key: TokenSigningKey,
    ring: KeyRing,
}

impl FakeTokenSigner {
    /// Seed of the fake's key.
    pub const SEED: [u8; 32] = [0x5a; 32];

    /// `issuer` = the server's `public_url`.
    pub fn new(issuer: &str) -> FakeTokenSigner {
        let key = TokenSigningKey::from_seed(1, &FakeTokenSigner::SEED);
        let mut ring = KeyRing::new();
        ring.insert(key.kid(), &key.public_key()).expect("valid Ed25519 public key");
        FakeTokenSigner { codec: TokenCodec::new(issuer), key, ring }
    }

    pub fn kid(&self) -> &str {
        self.key.kid()
    }
}

#[async_trait]
impl TokenSigner for FakeTokenSigner {
    fn sign(&self, claims: &TokenClaims) -> Result<SignedToken, TokenError> {
        Ok(SignedToken::new(self.codec.sign(&self.key, claims)?))
    }

    fn verify(&self, token: &str, expected: TokenKind, now: OffsetDateTime) -> Result<TokenClaims, TokenError> {
        Ok(self.codec.verify(&self.ring, token, expected, now)?)
    }

    fn verify_assertion(&self, assertion: &str, runner_public_key: &[u8; 32], now: OffsetDateTime) -> Result<RunnerAssertion, TokenError> {
        Ok(self.codec.verify_assertion(assertion, runner_public_key, now)?)
    }

    async fn reload(&self) -> Result<(), TokenError> {
        Ok(())
    }

    fn public_keys(&self) -> Vec<PublicKeyInfo> {
        vec![PublicKeyInfo { kid: self.key.kid().to_owned(), state: SecretState::Active, public_key: self.key.public_key() }]
    }
}

// ---------------------------------------------------------------------------------------------
// Contract suite
// ---------------------------------------------------------------------------------------------

/// One signer under test, loaded and able to sign.
pub struct SignerFixture {
    pub signer: Arc<dyn TokenSigner>,
    /// The issuer the signer was built with.
    pub issuer: String,
    /// The vault holding the signer's ring (`auth/token_signing_key` with exactly one version,
    /// Active); the rotation scenarios run only when it is given.
    pub vault: Option<Arc<dyn SecretVault>>,
}

/// Runs every `TokenSigner` scenario, each against a fresh `make(clock)` fixture.
pub async fn contract_token_signer<F, Fut>(make: F)
where
    F: Fn(Arc<FakeClock>) -> Fut,
    Fut: Future<Output = SignerFixture>,
{
    round_trip_per_kind(&make).await;
    wrong_kind_is_rejected(&make).await;
    forged_and_damaged_tokens_are_rejected(&make).await;
    time_checks_have_leeway(&make).await;
    assertions_need_the_runner_key_and_audience(&make).await;
    ring_follows_key_states(&make).await;
}

fn org() -> OrgId {
    OrgId::from_uuid(Uuid::from_u128(0x5160_0001))
}

fn run() -> RunId {
    RunId::from_uuid(Uuid::from_u128(0x5160_0002))
}

fn repo() -> RepoId {
    RepoId::from_uuid(Uuid::from_u128(0x5160_0003))
}

/// One scope of every kind, with non-default claim values.
fn scopes() -> Vec<TokenScope> {
    vec![
        TokenScope::Runner { runner: RunnerId::from_uuid(Uuid::from_u128(0x5160_0004)), generation: 7, instance: Uuid::from_u128(0x5160_0005) },
        TokenScope::Knowledge {
            org: org(),
            run: run(),
            attempt: 2,
            repo: repo(),
            sha: CommitSha::from_bytes([0xab; 20]),
            memory: MemoryCutoff {
                exclude_tickets: vec![TicketId::from_uuid(Uuid::from_u128(0x5160_0006))],
                as_of: Some(time::macros::datetime!(2026-03-01 12:00 UTC)),
            },
            disabled_layers: vec![Layer::History],
        },
        TokenScope::Gateway { org: org(), run: run(), attempt: 3, models: vec!["claude-sonnet-4-5".into()] },
        TokenScope::Artifact { org: org(), artifact: ArtifactId::from_uuid(Uuid::from_u128(0x5160_0007)) },
        TokenScope::Blob { org: org(), key: "orgs/o/runs/r/video.webm".into(), method: HttpMethod::Put, max_bytes: 1 << 20 },
        TokenScope::Indexer { org: org(), repo: repo() },
    ]
}

fn claims(scope: TokenScope, iat: OffsetDateTime, ttl: time::Duration) -> TokenClaims {
    TokenClaims { scope, iat, exp: iat + ttl, jti: Uuid::from_u128(0x5160_0100) }
}

fn runner_claims(iat: OffsetDateTime) -> TokenClaims {
    claims(scopes().remove(0), iat, time::Duration::minutes(15))
}

/// Unix-second precision, as tokens carry it.
fn now_secs(clock: &FakeClock) -> OffsetDateTime {
    use mage_core::ports::Clock;
    let now = clock.now();
    now.replace_nanosecond(0).unwrap_or(now)
}

fn rejected(result: Result<TokenClaims, TokenError>, what: &str) -> TokenError {
    match result {
        Err(TokenError::Unavailable(m)) => panic!("{what}: rejection must not be Unavailable ({m})"),
        Err(e) => e,
        Ok(_) => panic!("{what}: accepted"),
    }
}

/// `(header json, payload json, signature b64)` of a signed token without its prefix.
fn split(token: &str) -> (String, String, String) {
    let jws = &token[4..];
    let mut parts = jws.split('.');
    let header = String::from_utf8(URL_SAFE_NO_PAD.decode(parts.next().unwrap()).unwrap()).unwrap();
    let payload = String::from_utf8(URL_SAFE_NO_PAD.decode(parts.next().unwrap()).unwrap()).unwrap();
    (header, payload, parts.next().unwrap().to_owned())
}

fn join(prefix: &str, header: &str, payload: &str, signature: &str) -> String {
    format!("{prefix}{}.{}.{signature}", URL_SAFE_NO_PAD.encode(header), URL_SAFE_NO_PAD.encode(payload))
}

async fn round_trip_per_kind<F, Fut>(make: &F)
where
    F: Fn(Arc<FakeClock>) -> Fut,
    Fut: Future<Output = SignerFixture>,
{
    let clock = Arc::new(FakeClock::new());
    let f = make(clock.clone()).await;
    let now = now_secs(&clock);
    for scope in scopes() {
        let kind = scope.kind();
        let claims = claims(scope, now, time::Duration::minutes(10));
        let token = f.signer.sign(&claims).expect("sign");
        assert!(token.as_str().starts_with(kind.prefix()), "{kind:?} prefix");
        assert!(token.as_str().bytes().all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b)), "{kind:?} charset");
        let (header, payload, _) = split(token.as_str());
        let kid = f.signer.public_keys().into_iter().find(|k| k.state == SecretState::Active).expect("an Active key").kid;
        assert_eq!(header, format!(r#"{{"alg":"EdDSA","kid":"{kid}","typ":"{}"}}"#, kind.typ()), "{kind:?} header");
        assert!(payload.contains(&format!(r#""iss":"{}""#, f.issuer)), "{kind:?} iss");
        assert_eq!(f.signer.verify(token.as_str(), kind, now).expect("verify"), claims, "{kind:?} round trip");
    }
}

async fn wrong_kind_is_rejected<F, Fut>(make: &F)
where
    F: Fn(Arc<FakeClock>) -> Fut,
    Fut: Future<Output = SignerFixture>,
{
    let clock = Arc::new(FakeClock::new());
    let f = make(clock.clone()).await;
    let now = now_secs(&clock);
    let runner = f.signer.sign(&runner_claims(now)).expect("sign");
    for other in TokenKind::ALL.into_iter().filter(|k| *k != TokenKind::Runner) {
        assert_eq!(f.signer.verify(runner.as_str(), other, now).err(), Some(TokenError::WrongKind), "prefix {other:?}");
        // Same signed bytes behind the other kind's prefix: the header `typ` still says runner.
        let relabelled = format!("{}{}", other.prefix(), &runner.as_str()[4..]);
        assert_eq!(f.signer.verify(&relabelled, other, now).err(), Some(TokenError::WrongKind), "typ {other:?}");
    }
    assert_eq!(rejected(f.signer.verify(&runner.as_str()[4..], TokenKind::Runner, now), "no prefix"), TokenError::Malformed);
}

async fn forged_and_damaged_tokens_are_rejected<F, Fut>(make: &F)
where
    F: Fn(Arc<FakeClock>) -> Fut,
    Fut: Future<Output = SignerFixture>,
{
    let clock = Arc::new(FakeClock::new());
    let f = make(clock.clone()).await;
    let now = now_secs(&clock);
    let token = f.signer.sign(&runner_claims(now)).expect("sign");
    let (header, payload, signature) = split(token.as_str());
    let verify = |t: &str, what: &str| rejected(f.signer.verify(t, TokenKind::Runner, now), what);

    for alg in ["none", "HS256", "ES256", "eddsa"] {
        let forged = join("mgr_", &header.replace(r#""alg":"EdDSA""#, &format!(r#""alg":"{alg}""#)), &payload, &signature);
        assert_eq!(verify(&forged, alg), TokenError::UnsupportedAlgorithm, "alg {alg}");
    }
    let tampered_payload = join("mgr_", &header, &payload.replace(r#""gen":7"#, r#""gen":8"#), &signature);
    assert_eq!(verify(&tampered_payload, "payload"), TokenError::BadSignature);
    let tampered_header = join("mgr_", &header.replace(r#"{"alg""#, r#"{ "alg""#), &payload, &signature);
    assert_eq!(verify(&tampered_header, "header"), TokenError::BadSignature);
    let extra_header_field = join("mgr_", &header.replace('}', r#","jku":"https://evil.example"}"#), &payload, &signature);
    verify(&extra_header_field, "extra header field");

    let s = token.as_str();
    for cut in [1, 2, signature.len(), signature.len() + 1, s.len() - 4] {
        verify(&s[..s.len() - cut], &format!("truncated by {cut}"));
    }
    verify("mgr_", "empty");
    verify("mgr_a.b", "two parts");
    verify(&format!("{s}.x"), "four parts");
    verify(&s.replace('.', "!"), "charset");

    // A well-formed token from a key the ring never held.
    let stranger = TokenSigningKey::from_seed(99, &[0x77; 32]);
    let foreign = TokenCodec::new(&f.issuer).sign(&stranger, &runner_claims(now)).expect("sign with stranger");
    assert_eq!(verify(&foreign, "unknown kid"), TokenError::UnknownKey);
    // Known kid, signature by another key.
    let (_, _, foreign_sig) = split(&foreign);
    assert_eq!(verify(&join("mgr_", &header, &payload, &foreign_sig), "kid swap"), TokenError::BadSignature);
    // Other issuer.
    let other_issuer = join("mgr_", &header, &payload.replace(&f.issuer, "https://other.example"), &signature);
    assert_eq!(verify(&other_issuer, "issuer"), TokenError::BadSignature, "payload changes break the signature first");
}

async fn time_checks_have_leeway<F, Fut>(make: &F)
where
    F: Fn(Arc<FakeClock>) -> Fut,
    Fut: Future<Output = SignerFixture>,
{
    let clock = Arc::new(FakeClock::new());
    let f = make(clock.clone()).await;
    let iat = now_secs(&clock);
    let claims = claims(scopes().remove(3), iat, time::Duration::minutes(10));
    let token = f.signer.sign(&claims).expect("sign");
    let at = |now: OffsetDateTime| f.signer.verify(token.as_str(), TokenKind::Artifact, now);
    let second = time::Duration::seconds(1);

    assert!(at(claims.exp + LEEWAY - second).is_ok(), "inside the leeway after exp");
    assert_eq!(at(claims.exp + LEEWAY).err(), Some(TokenError::Expired), "leeway is exclusive at exp");
    assert_eq!(at(claims.exp + time::Duration::hours(1)).err(), Some(TokenError::Expired));
    assert!(at(iat - LEEWAY).is_ok(), "nbf = iat, accepted up to the leeway early");
    assert_eq!(at(iat - LEEWAY - second).err(), Some(TokenError::NotYetValid));
}

async fn assertions_need_the_runner_key_and_audience<F, Fut>(make: &F)
where
    F: Fn(Arc<FakeClock>) -> Fut,
    Fut: Future<Output = SignerFixture>,
{
    let clock = Arc::new(FakeClock::new());
    let f = make(clock.clone()).await;
    let now = now_secs(&clock);
    let runner_key = SigningKey::from_bytes(&[0x42; 32]);
    let runner_pk = runner_key.verifying_key().to_bytes();
    let assertion = RunnerAssertion {
        runner: RunnerId::from_uuid(Uuid::from_u128(0x5160_0004)),
        iat: now,
        exp: now + time::Duration::seconds(60),
        jti: Uuid::from_u128(0x5160_0200),
    };
    let codec = TokenCodec::new(&f.issuer);
    let signed = codec.sign_assertion(&runner_key, &assertion).expect("sign assertion");
    assert_eq!(f.signer.verify_assertion(&signed, &runner_pk, now).expect("verify assertion"), assertion);

    let other_pk = SigningKey::from_bytes(&[0x43; 32]).verifying_key().to_bytes();
    assert_eq!(f.signer.verify_assertion(&signed, &other_pk, now).err(), Some(TokenError::BadSignature), "wrong key");
    let elsewhere = TokenCodec::new("https://other.example").sign_assertion(&runner_key, &assertion).expect("sign");
    assert_eq!(f.signer.verify_assertion(&elsewhere, &runner_pk, now).err(), Some(TokenError::WrongAudience), "wrong aud");
    let long = RunnerAssertion { exp: now + time::Duration::seconds(61), ..assertion };
    let long = codec.sign_assertion(&runner_key, &long).expect("sign");
    assert_eq!(f.signer.verify_assertion(&long, &runner_pk, now).err(), Some(TokenError::LifetimeTooLong));
    let server_token = f.signer.sign(&runner_claims(now)).expect("sign");
    assert!(f.signer.verify_assertion(server_token.as_str(), &runner_pk, now).is_err(), "a server token is no assertion");
}

/// Staged and Retired kids verify, Destroyed ones stop verifying after `reload`; the Active key
/// signs; `public_keys` lists exactly the non-destroyed versions.
async fn ring_follows_key_states<F, Fut>(make: &F)
where
    F: Fn(Arc<FakeClock>) -> Fut,
    Fut: Future<Output = SignerFixture>,
{
    let clock = Arc::new(FakeClock::new());
    let f = make(clock.clone()).await;
    let Some(vault) = f.vault.clone() else {
        return;
    };
    let scope = SecretScope::Platform;
    let name = SecretName::new(names::TOKEN_SIGNING_KEY).expect("name");
    let now = now_secs(&clock);
    let kid_of = |infos: &[PublicKeyInfo], state: SecretState| infos.iter().filter(|k| k.state == state).map(|k| k.kid.clone()).collect::<Vec<_>>();

    let v1_token = f.signer.sign(&runner_claims(now)).expect("sign with v1");
    let v1_kids = kid_of(&f.signer.public_keys(), SecretState::Active);
    assert_eq!(v1_kids.len(), 1);
    assert!(v1_kids[0].starts_with("v1-"), "kid = v{{version}}-…: {}", v1_kids[0]);

    // v2 staged: published and verifiable, not used for signing.
    vault.put(&scope, &name, SecretBytes::new(vec![0x99; 32]), Activation::Staged, &Actor::System).await.expect("stage v2");
    f.signer.reload().await.expect("reload");
    let keys = f.signer.public_keys();
    assert_eq!(kid_of(&keys, SecretState::Active), v1_kids);
    let staged_kid = kid_of(&keys, SecretState::Staged).pop().expect("staged key published");
    assert!(staged_kid.starts_with("v2-"));
    let staged_key = TokenSigningKey::from_seed(2, &[0x99; 32]);
    assert_eq!(staged_key.kid(), staged_kid, "kid derives from version and public key");
    let staged_token = TokenCodec::new(&f.issuer).sign(&staged_key, &runner_claims(now)).expect("sign with staged key");
    assert!(f.signer.verify(&staged_token, TokenKind::Runner, now).is_ok(), "staged kid verifies");
    let still_v1 = f.signer.sign(&runner_claims(now)).expect("sign");
    assert!(split(still_v1.as_str()).0.contains(&v1_kids[0]), "Active key still signs");

    // v2 active, v1 retired: both verify, v2 signs.
    vault.set_state(&scope, &name, SecretVersion(2), SecretState::Active, &Actor::System).await.expect("activate v2");
    f.signer.reload().await.expect("reload");
    let keys = f.signer.public_keys();
    assert_eq!(kid_of(&keys, SecretState::Retired), v1_kids);
    assert_eq!(kid_of(&keys, SecretState::Active), vec![staged_kid.clone()]);
    assert!(f.signer.verify(v1_token.as_str(), TokenKind::Runner, now).is_ok(), "retired kid verifies");
    let v2_token = f.signer.sign(&runner_claims(now)).expect("sign with v2");
    assert!(split(v2_token.as_str()).0.contains(&staged_kid));

    // v1 destroyed: unknown after reload.
    vault.set_state(&scope, &name, SecretVersion(1), SecretState::Destroyed, &Actor::System).await.expect("destroy v1");
    assert!(f.signer.verify(v1_token.as_str(), TokenKind::Runner, now).is_ok(), "the ring changes on reload only");
    f.signer.reload().await.expect("reload");
    assert_eq!(f.signer.verify(v1_token.as_str(), TokenKind::Runner, now).err(), Some(TokenError::UnknownKey), "destroyed kid");
    assert!(f.signer.verify(v2_token.as_str(), TokenKind::Runner, now).is_ok());
    let kids: Vec<String> = f.signer.public_keys().into_iter().map(|k| k.kid).collect();
    assert_eq!(kids, vec![staged_kid], "destroyed keys are not published");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fake_signer_passes_the_signer_contract() {
        contract_token_signer(|_clock| async move {
            SignerFixture { signer: Arc::new(FakeTokenSigner::new("https://mage.test")), issuer: "https://mage.test".into(), vault: None }
        })
        .await;
    }
}
