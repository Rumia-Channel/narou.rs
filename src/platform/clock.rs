//! Clock abstraction.
//!
//! Domain logic that needs the current time for decisions (update checks,
//! crawler scheduling, cache expiry) should take a `Clock` or a timestamp
//! argument instead of calling `chrono::Utc::now()` directly, so tests can
//! inject a fixed time.

use std::time::Duration;

/// Source of wall-clock time.
///
/// Implementations must be cheap and `Send + Sync`. The default
/// [`SystemClock`] wraps `chrono::Utc::now()`.
pub trait Clock: Send + Sync {
    /// Current UTC time.
    fn now_utc(&self) -> chrono::DateTime<chrono::Utc>;

    /// Unix timestamp in seconds. Defaults to `now_utc().timestamp()`.
    fn now_unix_secs(&self) -> i64 {
        self.now_utc().timestamp()
    }

    /// Advance the clock by `duration` (for fake clocks; the system clock
    /// ignores this). Useful for tests that simulate the passage of time
    /// without sleeping.
    fn advance(&self, _duration: Duration) {}
}

/// The real wall clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_utc(&self) -> chrono::DateTime<chrono::Utc> {
        chrono::Utc::now()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicI64, Ordering};

    /// Test-only controllable clock.
    #[derive(Debug, Default)]
    struct FakeClock(AtomicI64);

    impl Clock for FakeClock {
        fn now_utc(&self) -> chrono::DateTime<chrono::Utc> {
            chrono::DateTime::from_timestamp(self.0.load(Ordering::SeqCst), 0).unwrap()
        }
        fn advance(&self, duration: Duration) {
            self.0
                .fetch_add(duration.as_secs() as i64, Ordering::SeqCst);
        }
    }

    #[test]
    fn system_clock_returns_sane_timestamp() {
        let clock = SystemClock;
        let now = clock.now_utc();
        // 2020-01-01 .. 2100-01-01 sanity window.
        assert!(now.timestamp() > 1_577_836_800);
        assert!(now.timestamp() < 4_102_444_800);
    }

    #[test]
    fn clock_defaults_to_timestamp() {
        let clock = FakeClock(AtomicI64::new(1_700_000_000));
        assert_eq!(clock.now_unix_secs(), 1_700_000_000);
        clock.advance(Duration::from_secs(5));
        assert_eq!(clock.now_unix_secs(), 1_700_000_005);
    }
}
