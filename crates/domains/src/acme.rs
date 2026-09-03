//! ACME issuance: a small [`Acme`] trait with the `instant-acme` adapter, the HTTP-01 token store
//! the edge serves, and the [`Issuer`] that picks the challenge per domain mode, publishes it,
//! finalizes the order and stores the certificate.

use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use futures::future::BoxFuture;
use instant_acme::{Account, AuthorizationStatus, ChallengeType, Identifier, NewAccount, NewOrder, OrderStatus, RetryPolicy};
use parking_lot::RwLock;

use crate::authority::{Zones, acme_challenge_name};
use crate::certs::{CertError, CertInfo, CertStore, inspect};
use crate::registry::{Binding, BindingState, DomainMode};

/// Path prefix the edge serves HTTP-01 key authorizations under.
pub const HTTP01_PREFIX: &str = "/.well-known/acme-challenge/";

/// Let's Encrypt production directory.
pub const LETS_ENCRYPT: &str = "https://acme-v02.api.letsencrypt.org/directory";
/// Let's Encrypt staging directory.
pub const LETS_ENCRYPT_STAGING: &str = "https://acme-staging-v02.api.letsencrypt.org/directory";

/// How the CA validates control of a name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChallengeKind {
    /// `http://<name>/.well-known/acme-challenge/<token>` → key authorization.
    Http01,
    /// `_acme-challenge.<name> TXT <base64url(sha256(key authorization))>`; required for wildcards.
    Dns01,
}

/// One authorization's challenge, ready to publish.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Challenge {
    /// As ordered, e.g. `*.shop.acme.com`.
    pub identifier: String,
    pub token: String,
    pub key_authorization: String,
    /// The DNS-01 TXT value.
    pub dns_value: String,
}

/// An order whose challenges must be published before [`Acme::finish`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingOrder {
    pub url: String,
    pub kind: ChallengeKind,
    pub names: Vec<String>,
    /// Empty when the CA already holds valid authorizations for every name.
    pub challenges: Vec<Challenge>,
}

/// A certificate chain (leaf first) and its freshly generated private key, both PEM.
#[derive(Clone)]
pub struct IssuedCert {
    pub chain_pem: String,
    pub key_pem: String,
}

impl std::fmt::Debug for IssuedCert {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IssuedCert").field("chain_pem", &self.chain_pem).finish_non_exhaustive()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AcmeError {
    #[error("ACME: {0}")]
    Protocol(String),
    #[error("the CA offered no {kind:?} challenge for {identifier}")]
    NoChallenge { identifier: String, kind: ChallengeKind },
    #[error("authorization for {identifier} is {status}")]
    Authorization { identifier: String, status: String },
    #[error("order ended {0}")]
    Order(String),
}

impl From<instant_acme::Error> for AcmeError {
    fn from(e: instant_acme::Error) -> Self {
        Self::Protocol(e.to_string())
    }
}

/// An ACME CA account.
pub trait Acme: Send + Sync {
    /// Places an order for `names` and returns the `kind` challenge of each pending authorization.
    fn order<'a>(&'a self, names: &'a [String], kind: ChallengeKind) -> BoxFuture<'a, Result<PendingOrder, AcmeError>>;
    /// Signals the published challenges ready, waits for validation, finalizes with a new key
    /// and downloads the chain.
    fn finish<'a>(&'a self, order: &'a PendingOrder) -> BoxFuture<'a, Result<IssuedCert, AcmeError>>;
}

/// [`Acme`] over `instant-acme` (RFC 8555; HTTP-01 and DNS-01).
pub struct InstantAcme {
    account: Account,
    retry: RetryPolicy,
}

impl std::fmt::Debug for InstantAcme {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InstantAcme").field("account", &self.account.id()).finish_non_exhaustive()
    }
}

impl InstantAcme {
    /// Registers a new account (terms agreed) at `directory_url`; returns the adapter and the
    /// account credentials as JSON, to be stored as a secret for [`InstantAcme::from_credentials`].
    pub async fn create(directory_url: &str, contact: &[&str]) -> Result<(Self, String), AcmeError> {
        let (account, credentials) = Account::builder()?
            .create(&NewAccount { contact, terms_of_service_agreed: true, only_return_existing: false }, directory_url.to_owned(), None)
            .await?;
        let json = serde_json::to_string(&credentials).map_err(|e| AcmeError::Protocol(e.to_string()))?;
        Ok((Self { account, retry: RetryPolicy::default() }, json))
    }

    /// Restores an account from credentials produced by [`InstantAcme::create`].
    pub async fn from_credentials(credentials_json: &str) -> Result<Self, AcmeError> {
        let credentials = serde_json::from_str(credentials_json).map_err(|e| AcmeError::Protocol(e.to_string()))?;
        let account = Account::builder()?.from_credentials(credentials).await?;
        Ok(Self { account, retry: RetryPolicy::default() })
    }

    /// Polling policy for validation and issuance (default: 250 ms doubling, 30 s timeout).
    pub fn with_retry(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }
}

fn challenge_type(kind: ChallengeKind) -> ChallengeType {
    match kind {
        ChallengeKind::Http01 => ChallengeType::Http01,
        ChallengeKind::Dns01 => ChallengeType::Dns01,
    }
}

impl Acme for InstantAcme {
    fn order<'a>(&'a self, names: &'a [String], kind: ChallengeKind) -> BoxFuture<'a, Result<PendingOrder, AcmeError>> {
        Box::pin(async move {
            let identifiers: Vec<Identifier> = names.iter().map(|n| Identifier::Dns(n.clone())).collect();
            let mut order = self.account.new_order(&NewOrder::new(&identifiers)).await?;
            let mut challenges = Vec::new();
            let mut authorizations = order.authorizations();
            while let Some(result) = authorizations.next().await {
                let mut authz = result?;
                match authz.status {
                    AuthorizationStatus::Pending => {}
                    AuthorizationStatus::Valid => continue,
                    status => {
                        return Err(AcmeError::Authorization { identifier: authz.identifier().to_string(), status: format!("{status:?}") });
                    }
                }
                let identifier = authz.identifier().to_string();
                let challenge =
                    authz.challenge(challenge_type(kind)).ok_or_else(|| AcmeError::NoChallenge { identifier: identifier.clone(), kind })?;
                let key_authorization = challenge.key_authorization();
                challenges.push(Challenge {
                    identifier,
                    token: challenge.token.clone(),
                    key_authorization: key_authorization.as_str().to_owned(),
                    dns_value: key_authorization.dns_value(),
                });
            }
            Ok(PendingOrder { url: order.url().to_owned(), kind, names: names.to_vec(), challenges })
        })
    }

    fn finish<'a>(&'a self, pending: &'a PendingOrder) -> BoxFuture<'a, Result<IssuedCert, AcmeError>> {
        Box::pin(async move {
            let mut order = self.account.order(pending.url.clone()).await?;
            let mut authorizations = order.authorizations();
            while let Some(result) = authorizations.next().await {
                let mut authz = result?;
                if !matches!(authz.status, AuthorizationStatus::Pending) {
                    continue;
                }
                let identifier = authz.identifier().to_string();
                let mut challenge =
                    authz.challenge(challenge_type(pending.kind)).ok_or(AcmeError::NoChallenge { identifier, kind: pending.kind })?;
                challenge.set_ready().await?;
            }
            let status = order.poll_ready(&self.retry).await?;
            if status != OrderStatus::Ready {
                return Err(AcmeError::Order(format!("{status:?}")));
            }
            let key_pem = order.finalize().await?;
            let chain_pem = order.poll_certificate(&self.retry).await?;
            Ok(IssuedCert { chain_pem, key_pem })
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Http01Error {
    #[error("`{0}` is not an ACME token")]
    InvalidToken(String),
    #[error("HTTP-01 store I/O: {0}")]
    Io(#[from] io::Error),
}

/// ACME tokens are base64url without padding (RFC 8555 §8.3).
fn valid_token(token: &str) -> bool {
    !token.is_empty() && token.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// HTTP-01 key authorizations by token. In memory, optionally mirrored to a directory so an edge
/// in another process sees what the issuer published. Clones share state.
#[derive(Clone, Debug, Default)]
pub struct Http01Store {
    mem: Arc<RwLock<HashMap<String, String>>>,
    dir: Option<Arc<PathBuf>>,
}

impl Http01Store {
    /// In-memory store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Store mirrored to `dir` (created if missing): `<dir>/<token>` holds the key authorization.
    pub fn with_dir(dir: impl Into<PathBuf>) -> Result<Self, io::Error> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir)?;
        Ok(Self { mem: Arc::default(), dir: Some(Arc::new(dir)) })
    }

    /// Publishes `key_authorization` for `token`.
    pub fn insert(&self, token: &str, key_authorization: &str) -> Result<(), Http01Error> {
        if !valid_token(token) {
            return Err(Http01Error::InvalidToken(token.to_owned()));
        }
        if let Some(dir) = &self.dir {
            let tmp = dir.join(format!(".{token}.tmp"));
            std::fs::write(&tmp, key_authorization)?;
            std::fs::rename(&tmp, dir.join(token))?;
        }
        self.mem.write().insert(token.to_owned(), key_authorization.to_owned());
        Ok(())
    }

    /// Withdraws `token`; absent tokens are fine.
    pub fn remove(&self, token: &str) -> Result<(), Http01Error> {
        if !valid_token(token) {
            return Err(Http01Error::InvalidToken(token.to_owned()));
        }
        self.mem.write().remove(token);
        if let Some(dir) = &self.dir {
            match std::fs::remove_file(dir.join(token)) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }

    /// The key authorization for `token`, if published.
    pub fn get(&self, token: &str) -> Option<String> {
        if !valid_token(token) {
            return None;
        }
        if let Some(v) = self.mem.read().get(token) {
            return Some(v.clone());
        }
        let dir = self.dir.as_ref()?;
        std::fs::read_to_string(dir.join(token)).ok().map(|s| s.trim().to_owned())
    }

    /// The response body for a request path, when it is `/.well-known/acme-challenge/<token>`.
    pub fn for_path(&self, path: &str) -> Option<String> {
        self.get(path.strip_prefix(HTTP01_PREFIX)?)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum IssueError {
    #[error("{hostname} is {state:?}; certificates are issued only for verified bindings")]
    NotVerified { hostname: String, state: BindingState },
    #[error("{0} is not served by the zone server; DNS-01 cannot validate")]
    ZoneNotServed(String),
    #[error("issued certificate does not cover {missing:?}")]
    Incomplete { missing: Vec<String> },
    #[error(transparent)]
    Acme(#[from] AcmeError),
    #[error(transparent)]
    Http01(#[from] Http01Error),
    #[error(transparent)]
    Cert(#[from] CertError),
}

/// Names and challenge for a binding: HTTP-01 for the hostname (`Cname`, `Apex`), DNS-01 for the
/// hostname plus its `*.` wildcard (`NsDelegated`, where we answer the zone).
pub fn issuance_plan(binding: &Binding) -> (Vec<String>, ChallengeKind) {
    match binding.mode {
        DomainMode::Cname | DomainMode::Apex => (vec![binding.hostname.clone()], ChallengeKind::Http01),
        DomainMode::NsDelegated => (vec![binding.hostname.clone(), format!("*.{}", binding.hostname)], ChallengeKind::Dns01),
    }
}

/// Runs one issuance end to end. Must share its [`Http01Store`] with whatever serves
/// `/.well-known/acme-challenge/` and its [`Zones`] with the running [`crate::ZoneServer`].
pub struct Issuer {
    acme: Arc<dyn Acme>,
    http01: Http01Store,
    zones: Zones,
    certs: Arc<CertStore>,
}

impl std::fmt::Debug for Issuer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Issuer").field("certs", &self.certs).finish_non_exhaustive()
    }
}

impl Issuer {
    /// `acme` places orders; challenges go to `http01` / `zones`; certificates land in `certs`.
    pub fn new(acme: Arc<dyn Acme>, http01: Http01Store, zones: Zones, certs: Arc<CertStore>) -> Self {
        Self { acme, http01, zones, certs }
    }

    /// Issues (or renews) the certificate for a `Verified` binding and stores it. Published
    /// challenges are withdrawn whether or not issuance succeeds.
    pub async fn issue(&self, binding: &Binding) -> Result<CertInfo, IssueError> {
        if binding.state != BindingState::Verified {
            return Err(IssueError::NotVerified { hostname: binding.hostname.clone(), state: binding.state });
        }
        let (names, kind) = issuance_plan(binding);
        if kind == ChallengeKind::Dns01 && !self.zones.serves(&binding.hostname) {
            return Err(IssueError::ZoneNotServed(binding.hostname.clone()));
        }
        let order = self.acme.order(&names, kind).await?;
        let published = self.publish(&order);
        let issued = match published {
            Ok(()) => self.acme.finish(&order).await.map_err(IssueError::from),
            Err(e) => Err(e),
        };
        self.withdraw(&order);
        let issued = issued?;
        let info = inspect(&issued.chain_pem)?;
        let missing: Vec<String> = names.iter().filter(|n| !info.names.contains(n)).cloned().collect();
        if !missing.is_empty() {
            return Err(IssueError::Incomplete { missing });
        }
        Ok(self.certs.insert(&issued.chain_pem, &issued.key_pem)?)
    }

    fn publish(&self, order: &PendingOrder) -> Result<(), IssueError> {
        for c in &order.challenges {
            match order.kind {
                ChallengeKind::Http01 => self.http01.insert(&c.token, &c.key_authorization)?,
                ChallengeKind::Dns01 => self.zones.add_challenge(&acme_challenge_name(&c.identifier), &c.dns_value),
            }
        }
        Ok(())
    }

    fn withdraw(&self, order: &PendingOrder) {
        for c in &order.challenges {
            match order.kind {
                ChallengeKind::Http01 => {
                    if let Err(error) = self.http01.remove(&c.token) {
                        tracing::warn!(%error, token = %c.token, "could not withdraw HTTP-01 challenge");
                    }
                }
                ChallengeKind::Dns01 => self.zones.remove_challenge(&acme_challenge_name(&c.identifier), &c.dns_value),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http01_store_serves_only_well_formed_tokens_under_the_prefix() {
        let store = Http01Store::new();
        store.insert("tok_EN-1", "tok_EN-1.thumb").unwrap();
        assert_eq!(store.for_path("/.well-known/acme-challenge/tok_EN-1").as_deref(), Some("tok_EN-1.thumb"));
        assert_eq!(store.for_path("/tok_EN-1"), None);
        assert_eq!(store.for_path("/.well-known/acme-challenge/"), None);
        assert!(matches!(store.insert("../x", "v"), Err(Http01Error::InvalidToken(_))));
        store.remove("tok_EN-1").unwrap();
        assert_eq!(store.get("tok_EN-1"), None);
    }

    #[test]
    fn http01_dir_mode_is_visible_to_another_store_on_the_same_dir() {
        let dir = tempfile::tempdir().unwrap();
        let issuer_side = Http01Store::with_dir(dir.path()).unwrap();
        let edge_side = Http01Store::with_dir(dir.path()).unwrap();
        issuer_side.insert("abc", "abc.thumb").unwrap();
        assert_eq!(edge_side.get("abc").as_deref(), Some("abc.thumb"));
        // Tokens that are not base64url never reach the filesystem.
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub").join("x"), "planted").unwrap();
        assert_eq!(edge_side.get("sub/x"), None);
        issuer_side.remove("abc").unwrap();
        assert_eq!(edge_side.get("abc"), None);
    }
}
