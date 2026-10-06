//! Signed Mage tokens (server.md "Tokens and credentials"): a compact JWS (RFC 7515) signed with
//! Ed25519 (`alg = EdDSA` only), typed three ways — kind prefix, header `typ`, payload `aud` —
//! plus the runner assertion used on `/runner/v1/register`.
//!
//! ```text
//! mgk_ b64url({"alg":"EdDSA","kid":"v3-1a2b3c4d","typ":"mage-knowledge+jwt"}) . b64url(payload) . b64url(sig)
//! ```
//!
//! Payload: `iss` (= `public_url`), `aud`, `sub`, `iat`, `nbf`, `exp` (NumericDate seconds),
//! `jti` (UUID) and the kind's claims. `sub` is the principal of the kind (runner, run, artifact,
//! blob key, repo). Verification order: prefix → header → `alg`/`typ` → `kid` in the ring →
//! `verify_strict` → `iss`/`aud` → time (30 s leeway) → claims. Revocation, run state and runner
//! generation are checked by the calling use case, not here.
//!
//! Pure and wasm-compatible, so the server (sign + verify), runner (assertions) and indexer
//! (`mgi_` verification from the published JWK set) share one implementation. There is no
//! algorithm negotiation surface: any other `alg` is rejected before a key is looked at.

use std::collections::BTreeMap;
use std::fmt;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::domain::{ArtifactId, CommitSha, Layer, OrgId, RepoId, RunId, RunnerId, TicketId};
use crate::ports::blob::HttpMethod;

/// The only accepted JWS algorithm.
pub const ALG: &str = "EdDSA";
/// Clock-skew allowance for `nbf` and `exp`.
pub const LEEWAY: time::Duration = time::Duration::seconds(30);
/// `typ` of the runner assertion.
pub const ASSERTION_TYP: &str = "mage-runner-assertion+jwt";
/// Upper bound of `exp - iat` for a runner assertion.
pub const ASSERTION_MAX_LIFETIME: time::Duration = time::Duration::seconds(60);
/// Prefix of the one-time runner bootstrap token (256-bit random, not a JWS).
pub const BOOTSTRAP_PREFIX: &str = "mgb_";
/// Public route serving the verifying keys as a [`JwkSet`] (Staged, Active, Retired).
pub const KEYS_PATH: &str = "/.well-known/mage/keys.json";

/// Why a token was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, thiserror::Error)]
pub enum TokenError {
    /// Not three base64url parts, bad JSON, missing or ill-typed claims.
    #[error("malformed token")]
    Malformed,
    /// Prefix or `typ` names another kind than the route expects.
    #[error("wrong token kind")]
    WrongKind,
    /// Header `alg` is not `EdDSA`.
    #[error("unsupported algorithm")]
    UnsupportedAlgorithm,
    /// `kid` is not in the ring (never staged, or destroyed).
    #[error("unknown signing key")]
    UnknownKey,
    #[error("bad signature")]
    BadSignature,
    #[error("wrong issuer")]
    WrongIssuer,
    #[error("wrong audience")]
    WrongAudience,
    #[error("token expired")]
    Expired,
    #[error("token not yet valid")]
    NotYetValid,
    /// Runner assertion with `exp - iat` above [`ASSERTION_MAX_LIFETIME`].
    #[error("token lifetime too long")]
    LifetimeTooLong,
}

/// Token kinds Mage signs (server.md catalog).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenKind {
    /// Runner token, 15 min.
    Runner,
    /// Knowledge MCP, run-bound.
    Knowledge,
    /// Model gateway, run-bound.
    Gateway,
    /// Trace-viewer capability for one artifact, 10 min.
    Artifact,
    /// Local blob backend URL (dev only).
    Blob,
    /// Server → indexer internal API call, ≤ 60 s.
    Indexer,
}

impl TokenKind {
    pub const ALL: [TokenKind; 6] =
        [TokenKind::Runner, TokenKind::Knowledge, TokenKind::Gateway, TokenKind::Artifact, TokenKind::Blob, TokenKind::Indexer];

    pub const fn prefix(self) -> &'static str {
        match self {
            TokenKind::Runner => "mgr_",
            TokenKind::Knowledge => "mgk_",
            TokenKind::Gateway => "mgg_",
            TokenKind::Artifact => "mgc_",
            TokenKind::Blob => "mgx_",
            TokenKind::Indexer => "mgi_",
        }
    }

    /// Header `typ`, `mage-<kind>+jwt`.
    pub const fn typ(self) -> &'static str {
        match self {
            TokenKind::Runner => "mage-runner+jwt",
            TokenKind::Knowledge => "mage-knowledge+jwt",
            TokenKind::Gateway => "mage-gateway+jwt",
            TokenKind::Artifact => "mage-artifact+jwt",
            TokenKind::Blob => "mage-blob+jwt",
            TokenKind::Indexer => "mage-indexer+jwt",
        }
    }

    /// Payload `aud`, `mage:<kind>`.
    pub const fn audience(self) -> &'static str {
        match self {
            TokenKind::Runner => "mage:runner",
            TokenKind::Knowledge => "mage:knowledge",
            TokenKind::Gateway => "mage:gateway",
            TokenKind::Artifact => "mage:artifact",
            TokenKind::Blob => "mage:blob",
            TokenKind::Indexer => "mage:indexer",
        }
    }

    /// Kind named by a token's prefix, if any.
    pub fn of_token(token: &str) -> Option<TokenKind> {
        TokenKind::ALL.into_iter().find(|k| token.starts_with(k.prefix()))
    }
}

/// Knowledge memory cutoff (knowledge.md `KnowledgeScope.memory`, claim `mem`): `similar_tickets`
/// never returns these tickets, nor anything newer than `as_of` (eval replay).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct MemoryCutoff {
    #[serde(rename = "excl")]
    pub exclude_tickets: Vec<TicketId>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub as_of: Option<OffsetDateTime>,
}

/// What a token grants; the kind derives from the variant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenScope {
    /// Claims `sub` = runner, `gen`, `inst`.
    Runner { runner: RunnerId, generation: u32, instance: Uuid },
    /// Claims `sub` = run, `org`, `att`, `repo`, `sha`, `mem`, `dl`.
    Knowledge { org: OrgId, run: RunId, attempt: u16, repo: RepoId, sha: CommitSha, memory: MemoryCutoff, disabled_layers: Vec<Layer> },
    /// Claims `sub` = run, `org`, `att`, `models` (empty = org allowlist).
    Gateway { org: OrgId, run: RunId, attempt: u16, models: Vec<String> },
    /// Claims `sub` = artifact, `org`.
    Artifact { org: OrgId, artifact: ArtifactId },
    /// Claims `sub` = object key, `org`, `method`, `max_bytes`.
    Blob { org: OrgId, key: String, method: HttpMethod, max_bytes: u64 },
    /// Claims `sub` = repo, `org`.
    Indexer { org: OrgId, repo: RepoId },
}

impl TokenScope {
    pub const fn kind(&self) -> TokenKind {
        match self {
            TokenScope::Runner { .. } => TokenKind::Runner,
            TokenScope::Knowledge { .. } => TokenKind::Knowledge,
            TokenScope::Gateway { .. } => TokenKind::Gateway,
            TokenScope::Artifact { .. } => TokenKind::Artifact,
            TokenScope::Blob { .. } => TokenKind::Blob,
            TokenScope::Indexer { .. } => TokenKind::Indexer,
        }
    }
}

/// A token's content. Times travel as whole seconds; `nbf` is signed equal to `iat`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenClaims {
    pub scope: TokenScope,
    pub iat: OffsetDateTime,
    pub exp: OffsetDateTime,
    pub jti: Uuid,
}

/// Runner-signed proof of key possession for `/runner/v1/register` (aud
/// `{public_url}/runner/v1/register`, `exp - iat` ≤ 60 s, `jti` single use).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunnerAssertion {
    pub runner: RunnerId,
    pub iat: OffsetDateTime,
    pub exp: OffsetDateTime,
    pub jti: Uuid,
}

/// `kid = v{version}-{hex(sha256(public_key))[..8]}` (server.md "Signing keys and rotation").
pub fn kid_for(version: u32, public_key: &[u8; 32]) -> String {
    let digest = Sha256::digest(public_key);
    format!("v{version}-{}", hex::encode(&digest[..4]))
}

/// One version of the platform signing key (`auth/token_signing_key`, a 32-byte Ed25519 seed).
#[derive(Clone)]
pub struct TokenSigningKey {
    kid: String,
    key: SigningKey,
}

impl TokenSigningKey {
    pub fn from_seed(version: u32, seed: &[u8; 32]) -> Self {
        let key = SigningKey::from_bytes(seed);
        TokenSigningKey { kid: kid_for(version, key.verifying_key().as_bytes()), key }
    }

    pub fn kid(&self) -> &str {
        &self.kid
    }

    pub fn public_key(&self) -> [u8; 32] {
        self.key.verifying_key().to_bytes()
    }
}

impl fmt::Debug for TokenSigningKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TokenSigningKey").field("kid", &self.kid).finish_non_exhaustive()
    }
}

/// Public keys a verifier accepts, by `kid`: the Staged, Active and Retired versions. Destroyed
/// versions are simply absent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KeyRing {
    keys: BTreeMap<String, VerifyingKey>,
}

impl KeyRing {
    pub fn new() -> Self {
        KeyRing::default()
    }

    /// Adds (or replaces) `kid`. Fails if the bytes are not a valid Ed25519 point.
    pub fn insert(&mut self, kid: impl Into<String>, public_key: &[u8; 32]) -> Result<(), TokenError> {
        let key = VerifyingKey::from_bytes(public_key).map_err(|_| TokenError::Malformed)?;
        self.keys.insert(kid.into(), key);
        Ok(())
    }

    pub fn remove(&mut self, kid: &str) -> bool {
        self.keys.remove(kid).is_some()
    }

    pub fn contains(&self, kid: &str) -> bool {
        self.keys.contains_key(kid)
    }

    pub fn kids(&self) -> impl Iterator<Item = &str> {
        self.keys.keys().map(String::as_str)
    }

    /// Ring of every key in a published JWK set (`/.well-known/mage/keys.json`).
    pub fn from_jwks(set: &JwkSet) -> Result<KeyRing, TokenError> {
        let mut ring = KeyRing::new();
        for jwk in &set.keys {
            ring.insert(jwk.kid.clone(), &jwk.public_key()?)?;
        }
        Ok(ring)
    }

    fn get(&self, kid: &str) -> Option<&VerifyingKey> {
        self.keys.get(kid)
    }
}

/// State of a published signing key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyState {
    Staged,
    Active,
    Retired,
}

/// RFC 8037 OKP JWK of an Ed25519 public key, plus Mage's `state`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Jwk {
    /// Always `OKP`.
    pub kty: String,
    /// Always `Ed25519`.
    pub crv: String,
    /// base64url (no padding) of the 32-byte public key.
    pub x: String,
    pub kid: String,
    pub state: KeyState,
}

impl Jwk {
    pub fn ed25519(kid: impl Into<String>, public_key: &[u8; 32], state: KeyState) -> Jwk {
        Jwk { kty: "OKP".into(), crv: "Ed25519".into(), x: URL_SAFE_NO_PAD.encode(public_key), kid: kid.into(), state }
    }

    pub fn public_key(&self) -> Result<[u8; 32], TokenError> {
        if self.kty != "OKP" || self.crv != "Ed25519" {
            return Err(TokenError::Malformed);
        }
        let bytes = URL_SAFE_NO_PAD.decode(&self.x).map_err(|_| TokenError::Malformed)?;
        bytes.try_into().map_err(|_| TokenError::Malformed)
    }
}

/// Body of `GET /.well-known/mage/keys.json`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct JwkSet {
    pub keys: Vec<Jwk>,
}

/// Signs and verifies tokens for one issuer (`public_url`, no trailing slash).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenCodec {
    issuer: String,
}

impl TokenCodec {
    pub fn new(issuer: impl Into<String>) -> Self {
        TokenCodec { issuer: issuer.into() }
    }

    pub fn issuer(&self) -> &str {
        &self.issuer
    }

    /// `aud` of runner assertions: `{public_url}/runner/v1/register`.
    pub fn assertion_audience(&self) -> String {
        format!("{}{}", self.issuer, crate::wire::runner::routes::REGISTER)
    }

    /// `"<prefix>" + compact JWS`, charset `[A-Za-z0-9_.-]`.
    pub fn sign(&self, key: &TokenSigningKey, claims: &TokenClaims) -> Result<String, TokenError> {
        let kind = claims.scope.kind();
        let header = Header { alg: ALG.into(), kid: key.kid.clone(), typ: kind.typ().into() };
        let registered = |sub: String| Registered {
            iss: self.issuer.clone(),
            aud: kind.audience().into(),
            sub,
            iat: claims.iat.unix_timestamp(),
            nbf: claims.iat.unix_timestamp(),
            exp: claims.exp.unix_timestamp(),
            jti: claims.jti,
        };
        let payload = match &claims.scope {
            TokenScope::Runner { runner, generation, instance } => {
                payload_json(registered(runner.to_string()), &RunnerWire { generation: *generation, inst: *instance })
            }
            TokenScope::Knowledge { org, run, attempt, repo, sha, memory, disabled_layers } => payload_json(
                registered(run.to_string()),
                &KnowledgeWire { org: *org, att: *attempt, repo: *repo, sha: *sha, mem: memory.clone(), dl: disabled_layers.clone() },
            ),
            TokenScope::Gateway { org, run, attempt, models } => {
                payload_json(registered(run.to_string()), &GatewayWire { org: *org, att: *attempt, models: models.clone() })
            }
            TokenScope::Artifact { org, artifact } => payload_json(registered(artifact.to_string()), &OrgWire { org: *org }),
            TokenScope::Blob { org, key: blob_key, method, max_bytes } => {
                payload_json(registered(blob_key.clone()), &BlobWire { org: *org, method: *method, max_bytes: *max_bytes })
            }
            TokenScope::Indexer { org, repo } => payload_json(registered(repo.to_string()), &OrgWire { org: *org }),
        }?;
        let mut token = String::from(kind.prefix());
        token.push_str(&compact(&header, &payload, &key.key)?);
        Ok(token)
    }

    /// Stateless checks of a token presented on a route that expects `expected`.
    pub fn verify(&self, ring: &KeyRing, token: &str, expected: TokenKind, now: OffsetDateTime) -> Result<TokenClaims, TokenError> {
        let Some(jws) = token.strip_prefix(expected.prefix()) else {
            return Err(if TokenKind::of_token(token).is_some() { TokenError::WrongKind } else { TokenError::Malformed });
        };
        let parts = Parts::split(jws)?;
        let header: Header = parts.header()?;
        if header.alg != ALG {
            return Err(TokenError::UnsupportedAlgorithm);
        }
        if header.typ != expected.typ() {
            return Err(TokenError::WrongKind);
        }
        let key = ring.get(&header.kid).ok_or(TokenError::UnknownKey)?;
        parts.verify(key)?;
        let (registered, claims) = parts.payload()?;
        if registered.iss != self.issuer {
            return Err(TokenError::WrongIssuer);
        }
        if registered.aud != expected.audience() {
            return Err(TokenError::WrongAudience);
        }
        let (iat, exp) = registered.check_time(now)?;
        let sub = registered.sub;
        let scope = match expected {
            TokenKind::Runner => {
                let RunnerWire { generation, inst } = claims_as(claims)?;
                TokenScope::Runner { runner: parse_sub(&sub)?, generation, instance: inst }
            }
            TokenKind::Knowledge => {
                let KnowledgeWire { org, att, repo, sha, mem, dl } = claims_as(claims)?;
                TokenScope::Knowledge { org, run: parse_sub(&sub)?, attempt: att, repo, sha, memory: mem, disabled_layers: dl }
            }
            TokenKind::Gateway => {
                let GatewayWire { org, att, models } = claims_as(claims)?;
                TokenScope::Gateway { org, run: parse_sub(&sub)?, attempt: att, models }
            }
            TokenKind::Artifact => {
                let OrgWire { org } = claims_as(claims)?;
                TokenScope::Artifact { org, artifact: parse_sub(&sub)? }
            }
            TokenKind::Blob => {
                let BlobWire { org, method, max_bytes } = claims_as(claims)?;
                TokenScope::Blob { org, key: sub, method, max_bytes }
            }
            TokenKind::Indexer => {
                let OrgWire { org } = claims_as(claims)?;
                TokenScope::Indexer { org, repo: parse_sub(&sub)? }
            }
        };
        Ok(TokenClaims { scope, iat, exp, jti: registered.jti })
    }

    /// Runner side: a compact JWS (no prefix) signed with the runner's own key; `kid`, `iss` and
    /// `sub` are the runner id.
    pub fn sign_assertion(&self, runner_key: &SigningKey, assertion: &RunnerAssertion) -> Result<String, TokenError> {
        let runner = assertion.runner.to_string();
        let header = Header { alg: ALG.into(), kid: runner.clone(), typ: ASSERTION_TYP.into() };
        let registered = Registered {
            iss: runner.clone(),
            aud: self.assertion_audience(),
            sub: runner,
            iat: assertion.iat.unix_timestamp(),
            nbf: assertion.iat.unix_timestamp(),
            exp: assertion.exp.unix_timestamp(),
            jti: assertion.jti,
        };
        let payload = payload_json(registered, &NoClaims {})?;
        compact(&header, &payload, runner_key)
    }

    /// Server side: verify against the runner's registered public key.
    pub fn verify_assertion(
        &self,
        assertion: &str,
        runner_public_key: &[u8; 32],
        now: OffsetDateTime,
    ) -> Result<RunnerAssertion, TokenError> {
        if TokenKind::of_token(assertion).is_some() {
            return Err(TokenError::WrongKind);
        }
        let parts = Parts::split(assertion)?;
        let header: Header = parts.header()?;
        if header.alg != ALG {
            return Err(TokenError::UnsupportedAlgorithm);
        }
        if header.typ != ASSERTION_TYP {
            return Err(TokenError::WrongKind);
        }
        let key = VerifyingKey::from_bytes(runner_public_key).map_err(|_| TokenError::UnknownKey)?;
        parts.verify(&key)?;
        let (registered, _) = parts.payload()?;
        if registered.aud != self.assertion_audience() {
            return Err(TokenError::WrongAudience);
        }
        let runner: RunnerId = parse_sub(&registered.sub)?;
        if registered.iss != registered.sub || header.kid != registered.sub {
            return Err(TokenError::WrongIssuer);
        }
        let (iat, exp) = registered.check_time(now)?;
        if exp - iat > ASSERTION_MAX_LIFETIME {
            return Err(TokenError::LifetimeTooLong);
        }
        Ok(RunnerAssertion { runner, iat, exp, jti: registered.jti })
    }
}

// ---------------------------------------------------------------------------------------------
// Encoding
// ---------------------------------------------------------------------------------------------

/// Field order is the serialization order: `{"alg":…,"kid":…,"typ":…}`.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Header {
    alg: String,
    kid: String,
    typ: String,
}

#[derive(Serialize, Deserialize)]
struct Registered {
    iss: String,
    aud: String,
    sub: String,
    iat: i64,
    nbf: i64,
    exp: i64,
    jti: Uuid,
}

impl Registered {
    fn check_time(&self, now: OffsetDateTime) -> Result<(OffsetDateTime, OffsetDateTime), TokenError> {
        let at = |secs| OffsetDateTime::from_unix_timestamp(secs).map_err(|_| TokenError::Malformed);
        let (iat, nbf, exp) = (at(self.iat)?, at(self.nbf)?, at(self.exp)?);
        if now + LEEWAY < nbf {
            return Err(TokenError::NotYetValid);
        }
        if now - LEEWAY >= exp {
            return Err(TokenError::Expired);
        }
        Ok((iat, exp))
    }
}

#[derive(Serialize)]
struct PayloadOut<'a, C: Serialize> {
    #[serde(flatten)]
    registered: Registered,
    #[serde(flatten)]
    claims: &'a C,
}

#[derive(Deserialize)]
struct PayloadIn {
    #[serde(flatten)]
    registered: Registered,
    #[serde(flatten)]
    claims: serde_json::Map<String, serde_json::Value>,
}

#[derive(Serialize, Deserialize)]
struct RunnerWire {
    #[serde(rename = "gen")]
    generation: u32,
    inst: Uuid,
}

#[derive(Serialize, Deserialize)]
struct KnowledgeWire {
    org: OrgId,
    att: u16,
    repo: RepoId,
    sha: CommitSha,
    mem: MemoryCutoff,
    dl: Vec<Layer>,
}

#[derive(Serialize, Deserialize)]
struct GatewayWire {
    org: OrgId,
    att: u16,
    models: Vec<String>,
}

#[derive(Serialize, Deserialize)]
struct OrgWire {
    org: OrgId,
}

#[derive(Serialize, Deserialize)]
struct BlobWire {
    org: OrgId,
    method: HttpMethod,
    max_bytes: u64,
}

#[derive(Serialize)]
struct NoClaims {}

fn payload_json<C: Serialize>(registered: Registered, claims: &C) -> Result<Vec<u8>, TokenError> {
    serde_json::to_vec(&PayloadOut { registered, claims }).map_err(|_| TokenError::Malformed)
}

fn compact(header: &Header, payload: &[u8], key: &SigningKey) -> Result<String, TokenError> {
    let header = serde_json::to_vec(header).map_err(|_| TokenError::Malformed)?;
    let mut out = URL_SAFE_NO_PAD.encode(header);
    out.push('.');
    out.push_str(&URL_SAFE_NO_PAD.encode(payload));
    let signature = key.sign(out.as_bytes());
    out.push('.');
    out.push_str(&URL_SAFE_NO_PAD.encode(signature.to_bytes()));
    Ok(out)
}

fn claims_as<T: DeserializeOwned>(claims: serde_json::Map<String, serde_json::Value>) -> Result<T, TokenError> {
    serde_json::from_value(serde_json::Value::Object(claims)).map_err(|_| TokenError::Malformed)
}

fn parse_sub<T: std::str::FromStr>(sub: &str) -> Result<T, TokenError> {
    sub.parse().map_err(|_| TokenError::Malformed)
}

/// The three parts of a compact JWS; `signing_input` is `header.payload`.
struct Parts<'a> {
    signing_input: &'a str,
    header: &'a str,
    payload: &'a str,
    signature: &'a str,
}

impl<'a> Parts<'a> {
    fn split(jws: &'a str) -> Result<Self, TokenError> {
        if !jws.bytes().all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b)) {
            return Err(TokenError::Malformed);
        }
        let (signing_input, signature) = jws.rsplit_once('.').ok_or(TokenError::Malformed)?;
        let (header, payload) = signing_input.split_once('.').ok_or(TokenError::Malformed)?;
        if header.is_empty() || payload.is_empty() || signature.is_empty() || payload.contains('.') {
            return Err(TokenError::Malformed);
        }
        Ok(Parts { signing_input, header, payload, signature })
    }

    fn header(&self) -> Result<Header, TokenError> {
        let bytes = URL_SAFE_NO_PAD.decode(self.header).map_err(|_| TokenError::Malformed)?;
        serde_json::from_slice(&bytes).map_err(|_| TokenError::Malformed)
    }

    fn verify(&self, key: &VerifyingKey) -> Result<(), TokenError> {
        let bytes = URL_SAFE_NO_PAD.decode(self.signature).map_err(|_| TokenError::Malformed)?;
        let signature = Signature::from_slice(&bytes).map_err(|_| TokenError::Malformed)?;
        key.verify_strict(self.signing_input.as_bytes(), &signature).map_err(|_| TokenError::BadSignature)
    }

    fn payload(&self) -> Result<(Registered, serde_json::Map<String, serde_json::Value>), TokenError> {
        let bytes = URL_SAFE_NO_PAD.decode(self.payload).map_err(|_| TokenError::Malformed)?;
        let PayloadIn { registered, claims } = serde_json::from_slice(&bytes).map_err(|_| TokenError::Malformed)?;
        Ok((registered, claims))
    }
}

#[cfg(test)]
mod tests;
