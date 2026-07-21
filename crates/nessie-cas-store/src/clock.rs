//! Injectable time for grace cutoffs — never ambient, so tests are deterministic
//! and offline (faithful to the workspace "no real clock in unit tests" law).

use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// A source of monotonic wall-clock-ish time (as a [`Duration`] since some fixed
/// epoch). Only *differences* are meaningful — used for the durable/cache `gc_grace`
/// cutoff.
pub trait Clock: Send + Sync {
    /// The current time as a duration since the clock's epoch.
    fn now(&self) -> Duration;
}

/// The real clock: time since the Unix epoch.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Duration {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
    }
}

/// A test clock whose time only moves when [`MockClock::advance`] is called, so
/// grace-window behavior is exercised deterministically with no real time.
#[derive(Default)]
pub struct MockClock {
    now: Mutex<Duration>,
}

impl MockClock {
    /// A clock starting at zero.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A clock starting at `start`.
    #[must_use]
    pub fn at(start: Duration) -> Self {
        Self {
            now: Mutex::new(start),
        }
    }

    /// Advance the clock by `by`.
    pub fn advance(&self, by: Duration) {
        let mut now = self.now.lock().expect("mock clock mutex poisoned");
        *now += by;
    }
}

impl Clock for MockClock {
    fn now(&self) -> Duration {
        *self.now.lock().expect("mock clock mutex poisoned")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_clock_advances_only_on_demand() {
        let c = MockClock::new();
        assert_eq!(c.now(), Duration::ZERO);
        c.advance(Duration::from_secs(5));
        assert_eq!(c.now(), Duration::from_secs(5));
        c.advance(Duration::from_secs(2));
        assert_eq!(c.now(), Duration::from_secs(7));
    }
}
