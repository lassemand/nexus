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

/// A clock that follows tokio's time: the wall time it reads at the start plus
/// what tokio's clock has run since. With tokio's time paused, tests move it by
/// sleeping; in production it equals the system clock.
#[derive(Debug, Clone, Copy)]
pub struct TokioClock {
    wall: DateTime<Utc>,
    started: tokio::time::Instant,
}

impl TokioClock {
    /// A clock that reads `wall` now and moves with tokio's time.
    pub fn starting_at(wall: DateTime<Utc>) -> TokioClock {
        TokioClock {
            wall,
            started: tokio::time::Instant::now(),
        }
    }
}

impl Clock for TokioClock {
    fn now(&self) -> DateTime<Utc> {
        self.wall + Duration::from_std(self.started.elapsed()).unwrap_or(Duration::MAX)
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

    #[tokio::test(start_paused = true)]
    async fn a_tokio_clock_moves_with_paused_time() {
        let start = Utc
            .with_ymd_and_hms(2026, 11, 11, 7, 30, 0)
            .single()
            .expect("valid");
        let clock = TokioClock::starting_at(start);
        tokio::time::sleep(std::time::Duration::from_secs(90 * 60)).await;
        assert_eq!(clock.now(), start + Duration::minutes(90));
    }

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
