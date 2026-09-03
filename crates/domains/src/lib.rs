//! Domain bindings for aho cloud.
//!
//! - [`Registry`]: per-hostname bindings, TXT tokens, the Pending → Verified → Grace → Detached
//!   state machine, tombstones and the takeover guard.
//! - [`Resolver`] / [`HickoryResolver`]: the DNS lookups verification needs.
//! - [`Zones`] / [`ZoneServer`]: authoritative DNS for NS-delegated subzones.
//! - [`CertStore`]: PEM pairs on disk, resolved by SNI for rustls.
//! - [`Acme`] / [`InstantAcme`] / [`Http01Store`] / [`Issuer`]: certificate issuance.
//! - [`Rdap`] / [`RdapClient`]: registration expiry.

#![forbid(unsafe_code)]

pub mod acme;
pub mod authority;
pub mod certs;
pub mod rdap;
pub mod registry;
pub mod resolver;
mod token;

pub use acme::{
    Acme, AcmeError, Challenge, ChallengeKind, HTTP01_PREFIX, Http01Error, Http01Store, InstantAcme, IssueError, IssuedCert, Issuer,
    LETS_ENCRYPT, LETS_ENCRYPT_STAGING, PendingOrder, issuance_plan,
};
pub use authority::{ACME_CHALLENGE_LABEL, AuthorityError, Zone, ZoneServer, Zones, acme_challenge_name};
pub use certs::{CertError, CertInfo, CertStore, inspect};
pub use rdap::{Expiry, RDAP_ORG, Rdap, RdapClient, RdapError, parse_expiration};
pub use registry::{
    Binding, BindingState, DomainError, DomainMode, EdgeTarget, Observed, Record, Registry, Serving, TXT_LABEL, TXT_VALUE_PREFIX,
    Transition, txt_name, txt_value,
};
pub use resolver::{DnsError, HickoryResolver, Resolver, normalize_name};
pub use token::token;
