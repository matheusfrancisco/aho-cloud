//! `Clock` port (README "Port conventions": time comes from a `Clock`). Adapters hold
//! `Arc<dyn Clock>` and SQL never calls `now()` (storage.md "Conventions"), so fakes and real
//! adapters pass the same contract suites under `mage_core::testing::FakeClock`.

use time::OffsetDateTime;

pub trait Clock: Send + Sync {
    /// Current UTC time.
    fn now(&self) -> OffsetDateTime;
}

/// Wall clock, truncated to microseconds: Postgres `timestamptz` keeps microseconds, so a value
/// read back from the database equals the value written.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> OffsetDateTime {
        let now = OffsetDateTime::now_utc();
        now.replace_nanosecond(now.nanosecond() / 1_000 * 1_000).unwrap_or(now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_clock_is_utc_with_microsecond_precision() {
        let now = SystemClock.now();
        assert!(now.offset().is_utc());
        assert_eq!(now.nanosecond() % 1_000, 0);
    }
}
