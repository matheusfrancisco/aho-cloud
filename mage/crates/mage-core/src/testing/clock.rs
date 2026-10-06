//! Deterministic `Clock` and `IdGen` fakes.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use parking_lot::Mutex;
use time::OffsetDateTime;
use time::macros::datetime;
use uuid::{Builder, Uuid};

use crate::ports::clock::Clock;
use crate::ports::ids::IdGen;

/// Settable clock. Starts at 2026-01-01T00:00:00Z unless built with [`FakeClock::at`].
#[derive(Debug)]
pub struct FakeClock {
    now: Mutex<OffsetDateTime>,
}

impl FakeClock {
    pub const DEFAULT_START: OffsetDateTime = datetime!(2026-01-01 00:00:00 UTC);

    pub fn new() -> Self {
        Self::at(Self::DEFAULT_START)
    }

    pub fn at(start: OffsetDateTime) -> Self {
        Self { now: Mutex::new(start) }
    }

    pub fn set(&self, at: OffsetDateTime) {
        *self.now.lock() = at;
    }

    pub fn advance(&self, by: Duration) {
        let mut now = self.now.lock();
        *now += by;
    }
}

impl Default for FakeClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for FakeClock {
    fn now(&self) -> OffsetDateTime {
        *self.now.lock()
    }
}

/// Deterministic ids: the n-th call (from 1) of each method encodes `n`. `uuid_v7` values are
/// valid v7 uuids with timestamp 0 that sort in creation order; `uuid_v4` values are valid v4
/// uuids; nonces are `n` big-endian.
#[derive(Debug, Default)]
pub struct SeqIdGen {
    next: AtomicU64,
}

impl SeqIdGen {
    pub fn new() -> Self {
        Self::default()
    }

    fn bump(&self) -> u64 {
        self.next.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// 10 bytes for the uuid builders: `n` big-endian in the last 8 (the builders overwrite the
    /// version nibble of byte 0 and the variant bits of byte 2, which stay clear of `n` below 2^62).
    fn counter_bytes(n: u64) -> [u8; 10] {
        let mut bytes = [0u8; 10];
        bytes[2..].copy_from_slice(&n.to_be_bytes());
        bytes
    }
}

impl IdGen for SeqIdGen {
    fn uuid_v7(&self) -> Uuid {
        Builder::from_unix_timestamp_millis(0, &Self::counter_bytes(self.bump())).into_uuid()
    }

    fn uuid_v4(&self) -> Uuid {
        let mut bytes = [0u8; 16];
        bytes[6..].copy_from_slice(&Self::counter_bytes(self.bump()));
        Builder::from_random_bytes(bytes).into_uuid()
    }

    fn nonce(&self) -> [u8; 16] {
        u128::from(self.bump()).to_be_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fake_clock_is_settable_and_advances() {
        let clock = FakeClock::new();
        assert_eq!(clock.now(), FakeClock::DEFAULT_START);
        clock.advance(Duration::from_secs(90));
        assert_eq!(clock.now(), FakeClock::DEFAULT_START + Duration::from_secs(90));
        clock.set(OffsetDateTime::UNIX_EPOCH);
        assert_eq!(clock.now(), OffsetDateTime::UNIX_EPOCH);
    }

    #[test]
    fn seq_ids_are_valid_distinct_and_ordered() {
        let ids = SeqIdGen::new();
        let a = ids.uuid_v7();
        let b = ids.uuid_v7();
        assert_eq!((a.get_version_num(), b.get_version_num()), (7, 7));
        assert!(a < b);
        assert_eq!(ids.uuid_v4().get_version_num(), 4);
        assert_ne!(ids.nonce(), ids.nonce());
        let again = SeqIdGen::new();
        assert_eq!(again.uuid_v7(), a, "a fresh generator repeats the sequence");
    }
}
