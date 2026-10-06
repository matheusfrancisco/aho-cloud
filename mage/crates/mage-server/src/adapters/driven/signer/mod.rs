//! `TokenSigner` adapter (server.md "Vault and signer", "Signing keys and rotation").

mod ed25519;

pub use ed25519::{Ed25519TokenSigner, RELOAD_EVERY};
