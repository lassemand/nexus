use std::sync::Mutex;

use chrono::{DateTime, Duration, Utc};

/// The one source of "now" the scheduler and the notes consumer use, so tests
/// can control time and no two clocks are ever mixed. Every instant is UTC.
pub trait Clock: Send + Sync {
    /// The current instant.
    fn now(&self) -> DateTime<Utc>;
}

/// The system clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

/// A clock a test moves by hand.
#[derive(Debug)]
pub struct ManualClock(Mutex<DateTime<Utc>>);

impl ManualClock {
    /// A clock that reads `start` until it is moved.
    pub fn new(start: DateTime<Utc>) -> ManualClock {
        ManualClock(Mutex::new(start))
    }

    /// Moves the clock to `instant`.
    pub fn set(&self, instant: DateTime<Utc>) {
        *self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = instant;
    }

    /// Moves the clock forward by `by`.
    pub fn advance(&self, by: Duration) {
        let mut now = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *now += by;
    }
}

impl Clock for ManualClock {
    fn now(&self) -> DateTime<Utc> {
        *self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    #[test]
    fn a_manual_clock_stands_still_until_moved() {
        let start = Utc
            .with_ymd_and_hms(2026, 11, 11, 12, 0, 0)
            .single()
            .expect("valid");
        let clock = ManualClock::new(start);
        assert_eq!(clock.now(), start);
        clock.advance(Duration::hours(2));
        assert_eq!(clock.now(), start + Duration::hours(2));
        clock.set(start);
        assert_eq!(clock.now(), start);
    }
}
