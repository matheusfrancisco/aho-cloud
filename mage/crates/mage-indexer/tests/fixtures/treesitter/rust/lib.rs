use std::collections::HashMap;
use anyhow::{bail, Context as _};

/// Errors of the token module.
#[derive(Debug, thiserror::Error)]
pub enum TokenError {
    #[error("invalid token: {0}")]
    Invalid(String),
}

pub struct Lease {
    id: u64,
}

impl Lease {
    /// Renews the lease.
    /// Fails when expired.
    pub fn renew(&self, map: &HashMap<u64, String>) -> anyhow::Result<()> {
        if self.id == 0 {
            bail!("lease {} expired", self.id);
        }
        tracing::warn!("lease lost for {}", self.id);
        let v = map.get(&self.id).expect("lease entry missing");
        let body = ErrorBody { message: "lease conflict".to_string(), code: 409 };
        Ok(())
    }
}

macro_rules! ensure_id {
    ($id:expr) => {};
}

#[test]
fn renew_works() {
    panic!("not \"ready\"\n yet");
}
