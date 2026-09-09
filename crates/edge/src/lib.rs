//! The aho cloud edge: a multi-tenant reverse proxy in front of every project hostname.
//!
//! - TLS (rustls, SNI-selected certificates) with HTTP/1.1 and h2; a port-80 listener that
//!   answers ACME HTTP-01 challenges and redirects everything else to https.
//! - Per-hostname [`Site`](cloud_api::internal::Site) records from a [`Directory`]
//!   ([`PollingDirectory`] against the control plane, [`StaticDirectory`] for tests).
//! - `edge.toml` route modes (`edge`, `optional`, `bearer`, `public` + webhook verifiers,
//!   `passthrough`); edge login by password, magic link or OIDC; the principal JWT
//!   ([`cloud_contract::principal`]) minted per request with per-(project, stage) keys from a
//!   [`KeyStore`] and published at `/.edge/jwks.json`.
//! - Inbound `x-principal-jwt`, `x-edge-*`, forwarding and hop-by-hop headers are always
//!   stripped; the app only ever sees what the edge vouches for.
//!
//! Apps (and the crawler) check the principal with [`verify::verify_principal`].

#![forbid(unsafe_code)]

pub mod directory;
pub mod keys;
pub mod mail;
pub mod oidc;
pub mod verify;
pub mod webhook;

mod headers;
mod jwt;
mod login;
mod pages;
mod proxy;
mod server;
mod session;

pub use directory::{Directory, DirectoryError, PollingDirectory, StaticDirectory};
pub use keys::{Jwk, Jwks, KeyError, KeyStore};
pub use mail::{MagicLinkMail, MailError, Mailer, SmtpMailer};
pub use oidc::{ClientCredentials, GithubEndpoints, OidcSettings};
pub use server::{Components, Edge, EdgeError, Http01, Listener, Settings, tls_server_config};
