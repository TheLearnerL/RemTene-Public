//! Clock implementation using system time.

use std::time::{SystemTime, UNIX_EPOCH};

use remtene_application::ports::Clock;
use remtene_domain::TimestampMs;

/// System clock implementation.
///
/// Returns milliseconds since Unix epoch.
pub struct SystemClock;

impl SystemClock {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn now(&self) -> TimestampMs {
        let duration = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("System time before Unix epoch");

        TimestampMs::new(duration.as_millis() as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_returns_positive_timestamp() {
        let clock = SystemClock::new();
        let now = clock.now();
        assert!(now.get() > 0);
    }

    #[test]
    fn clock_is_monotonic() {
        let clock = SystemClock::new();
        let t1 = clock.now();
        std::thread::sleep(std::time::Duration::from_millis(10));
        let t2 = clock.now();
        assert!(t2.get() > t1.get());
    }

    #[test]
    fn clock_returns_reasonable_timestamp() {
        let clock = SystemClock::new();
        let now = clock.now();

        // After 2020-01-01 (1577836800000 ms)
        assert!(now.get() > 1_577_836_800_000);

        // Before 2030-01-01 (1893456000000 ms)
        assert!(now.get() < 1_893_456_000_000);
    }
}
