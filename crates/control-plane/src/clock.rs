//! Wall-clock time as a port, so expiry rules (device codes, sessions, standing orders, domain
//! grace) are testable without sleeping.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Unix seconds.
pub trait Clock: Send + Sync {
    fn now(&self) -> u64;
}

/// The host clock.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> u64 {
        SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
    }
}

/// A clock that moves only when told to.
#[derive(Debug, Default)]
pub struct ManualClock(AtomicU64);

impl ManualClock {
    pub fn new(start: u64) -> Self {
        Self(AtomicU64::new(start))
    }

    pub fn set(&self, now: u64) {
        self.0.store(now, Ordering::SeqCst);
    }

    pub fn advance(&self, secs: u64) {
        self.0.fetch_add(secs, Ordering::SeqCst);
    }
}

impl Clock for ManualClock {
    fn now(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}
