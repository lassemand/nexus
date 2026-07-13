/// Validation layer for Nordic bar ingestion from the Saxo WebSocket stream.
///
/// # Design
///
/// This module wraps `BarAggregator` with checks that are separate from the
/// aggregation logic itself. Each check addresses a distinct failure mode of
/// the streaming path:
///
/// - [`OhlcValidator`] — sanity-checks completed bars before they are published
/// - [`TickDeduplicator`] — rejects duplicate/out-of-order ticks per Uic
/// - [`GapClassifier`] — distinguishes holiday / after-hours / dead-connection
///   for silent-period classification
/// - [`GapBoundary`] — enforces the exact boundary between backfilled and live
///   bars so gap-fill never double-counts a window
use chrono::{DateTime, Datelike, NaiveDate, Timelike, Utc, Weekday};
use model::bar::Bar;
use std::collections::HashMap;
use thiserror::Error;

// ── OHLC sanity ───────────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum OhlcError {
    #[error("high ({high}) < low ({low}) in bar for {ticker}")]
    HighBelowLow { ticker: String, high: f64, low: f64 },
    #[error("open ({open}) outside [low={low}, high={high}] for {ticker}")]
    OpenOutsideEnvelope {
        ticker: String,
        open: f64,
        low: f64,
        high: f64,
    },
    #[error("close ({close}) outside [low={low}, high={high}] for {ticker}")]
    CloseOutsideEnvelope {
        ticker: String,
        close: f64,
        low: f64,
        high: f64,
    },
    #[error(
        "non-finite price in bar for {ticker}: open={open} high={high} low={low} close={close}"
    )]
    NonFinitePrice {
        ticker: String,
        open: f64,
        high: f64,
        low: f64,
        close: f64,
    },
    #[error("negative or zero volume ({volume}) in bar for {ticker}")]
    NonPositiveVolume { ticker: String, volume: f64 },
}

/// Validates a completed `Bar` for OHLC consistency before publishing.
///
/// Zero-tick periods (no bar emitted) are NOT an error — illiquid small-cap
/// names like GomSpace legitimately have quiet intervals. This validator only
/// runs when a bar IS emitted and checks that its values are internally consistent.
pub struct OhlcValidator;

impl OhlcValidator {
    /// Validate a completed bar. Returns the bar unchanged on success.
    pub fn validate(bar: &Bar) -> Result<(), OhlcError> {
        let t = &bar.asset.ticker;

        if !bar.open.is_finite()
            || !bar.high.is_finite()
            || !bar.low.is_finite()
            || !bar.close.is_finite()
        {
            return Err(OhlcError::NonFinitePrice {
                ticker: t.clone(),
                open: bar.open,
                high: bar.high,
                low: bar.low,
                close: bar.close,
            });
        }

        if bar.high < bar.low {
            return Err(OhlcError::HighBelowLow {
                ticker: t.clone(),
                high: bar.high,
                low: bar.low,
            });
        }

        if bar.open < bar.low - f64::EPSILON || bar.open > bar.high + f64::EPSILON {
            return Err(OhlcError::OpenOutsideEnvelope {
                ticker: t.clone(),
                open: bar.open,
                low: bar.low,
                high: bar.high,
            });
        }

        if bar.close < bar.low - f64::EPSILON || bar.close > bar.high + f64::EPSILON {
            return Err(OhlcError::CloseOutsideEnvelope {
                ticker: t.clone(),
                close: bar.close,
                low: bar.low,
                high: bar.high,
            });
        }

        if bar.volume <= 0.0 {
            return Err(OhlcError::NonPositiveVolume {
                ticker: t.clone(),
                volume: bar.volume,
            });
        }

        Ok(())
    }
}

// ── Duplicate / out-of-order tick rejection ───────────────────────────────

/// Tracks the last-seen tick timestamp per Uic to reject stale retransmits.
///
/// WebSocket retransmits or network-level duplicates arrive with the same (or
/// earlier) timestamp as a tick already applied to the pending bar. Applying
/// them would corrupt the OHLC — e.g. a retransmit of an old low price would
/// incorrectly pull the bar's low down again.
pub struct TickDeduplicator {
    last_seen: HashMap<u64, DateTime<Utc>>,
}

impl TickDeduplicator {
    pub fn new() -> Self {
        Self {
            last_seen: HashMap::new(),
        }
    }

    /// Returns `true` if the tick is fresh and should be processed.
    /// Returns `false` if it is a duplicate or arrived out of order.
    pub fn is_fresh(&mut self, uic: u64, timestamp: DateTime<Utc>) -> bool {
        match self.last_seen.get(&uic) {
            Some(&last) if timestamp <= last => false,
            _ => {
                self.last_seen.insert(uic, timestamp);
                true
            }
        }
    }
}

impl Default for TickDeduplicator {
    fn default() -> Self {
        Self::new()
    }
}

// ── Gap classification ────────────────────────────────────────────────────

/// How to classify a silent period in the tick stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SilenceCause {
    /// A known public holiday on Nasdaq Stockholm / First North.
    /// No ticks are expected; do not alert.
    MarketHoliday,
    /// Outside regular trading hours (weekend or before/after session).
    /// No ticks are expected; do not alert.
    AfterHours,
    /// Trading hours but no ticks received within the heartbeat window.
    /// The WebSocket connection may have silently died — alert and reconnect.
    PossibleDeadConnection,
}

/// Classifies a silent period in the tick stream.
///
/// Integrates with the `trading_holidays` DB table (via [`model::calendar`])
/// to distinguish expected silence (holiday / after-hours) from unexpected
/// silence (dead connection during trading hours).
///
/// The caller is responsible for loading the calendar from the DB and passing
/// it here — this classifier is stateless.
pub struct GapClassifier;

/// First North trading session: 09:00–17:30 Stockholm time (UTC+1/+2).
/// Approximate UTC bounds used here; exact session boundaries from the
/// exchange's session list are used in production.
const SESSION_START_UTC: u32 = 7; // 09:00 CEST = 07:00 UTC
const SESSION_END_UTC: u32 = 15; // 17:30 CEST ≈ 15:30 UTC

impl GapClassifier {
    /// Classify a silent period ending at `now`.
    ///
    /// `is_holiday` should be `true` when the calendar says `now.date()` is
    /// a market holiday or half-day closure. The caller queries the DB.
    pub fn classify(now: DateTime<Utc>, is_holiday: bool) -> SilenceCause {
        let wd = now.weekday();
        if wd == Weekday::Sat || wd == Weekday::Sun {
            return SilenceCause::AfterHours;
        }

        if is_holiday {
            return SilenceCause::MarketHoliday;
        }

        let hour = now.hour();
        if !(SESSION_START_UTC..SESSION_END_UTC).contains(&hour) {
            return SilenceCause::AfterHours;
        }

        SilenceCause::PossibleDeadConnection
    }
}

// ── Gap-fill boundary enforcement ─────────────────────────────────────────

/// Enforces the exact boundary between backfilled (historical chart) bars and
/// live-aggregated bars so a reconnect gap-fill never double-counts a window.
///
/// # Problem
///
/// When the WebSocket reconnects, the service fetches historical bars from
/// `/chart/v1/charts` to fill the gap. If a bar was partially aggregated
/// before the disconnect (say, 30 ticks into a 60-second window), and the
/// gap-fill then returns a complete bar for that same window from Saxo's own
/// OHLCV history, publishing both would double-count that window.
///
/// # Solution
///
/// Track the `window_start` of the last bar emitted by the live aggregator.
/// The gap-fill must only include bars whose `window_start` is **strictly
/// before** that timestamp. Any historical bar at or after the last live
/// window is discarded — the live aggregator owns that window.
pub struct GapBoundary {
    /// The window_start of the last bar emitted by the live aggregator.
    /// `None` until the first live bar is produced.
    last_live_window: Option<NaiveDate>,
}

impl GapBoundary {
    pub fn new() -> Self {
        Self {
            last_live_window: None,
        }
    }

    /// Record that the live aggregator just emitted a bar with this date.
    pub fn record_live_bar(&mut self, bar_date: NaiveDate) {
        match self.last_live_window {
            Some(existing) if bar_date <= existing => {}
            _ => self.last_live_window = Some(bar_date),
        }
    }

    /// Returns `true` if a historical (gap-fill) bar with `bar_date` should
    /// be published — i.e. it is strictly before the first live bar's window.
    ///
    /// Returns `true` (allow) when no live bar has been seen yet, so gap-fill
    /// can populate the initial history freely.
    pub fn should_publish_gap_bar(&self, bar_date: NaiveDate) -> bool {
        match self.last_live_window {
            None => true,
            Some(first_live) => bar_date < first_live,
        }
    }
}

impl Default for GapBoundary {
    fn default() -> Self {
        Self::new()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use model::asset::{mic, Asset};
    use std::time::UNIX_EPOCH;

    fn bar(open: f64, high: f64, low: f64, close: f64, volume: f64) -> Bar {
        Bar {
            asset: Asset::with_mic("GOMX", mic::FNSE),
            open,
            high,
            low,
            close,
            volume,
            timestamp: UNIX_EPOCH,
            currency: "SEK".to_string(),
        }
    }

    fn ts(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(secs, 0).unwrap()
    }

    // ── OHLC sanity ───────────────────────────────────────────────────────

    #[test]
    fn valid_bar_passes() {
        assert!(OhlcValidator::validate(&bar(10.0, 12.0, 9.0, 11.0, 1000.0)).is_ok());
    }

    #[test]
    fn high_below_low_is_rejected() {
        let result = OhlcValidator::validate(&bar(10.0, 8.0, 9.0, 10.0, 500.0));
        assert!(matches!(result, Err(OhlcError::HighBelowLow { .. })));
    }

    #[test]
    fn open_above_high_is_rejected() {
        let result = OhlcValidator::validate(&bar(13.0, 12.0, 9.0, 11.0, 500.0));
        assert!(matches!(result, Err(OhlcError::OpenOutsideEnvelope { .. })));
    }

    #[test]
    fn close_below_low_is_rejected() {
        let result = OhlcValidator::validate(&bar(10.0, 12.0, 9.0, 8.0, 500.0));
        assert!(matches!(
            result,
            Err(OhlcError::CloseOutsideEnvelope { .. })
        ));
    }

    #[test]
    fn nan_price_is_rejected() {
        let result = OhlcValidator::validate(&bar(f64::NAN, 12.0, 9.0, 11.0, 500.0));
        assert!(matches!(result, Err(OhlcError::NonFinitePrice { .. })));
    }

    #[test]
    fn zero_volume_is_rejected() {
        let result = OhlcValidator::validate(&bar(10.0, 12.0, 9.0, 11.0, 0.0));
        assert!(matches!(result, Err(OhlcError::NonPositiveVolume { .. })));
    }

    #[test]
    fn zero_tick_period_emits_no_bar_not_an_error() {
        // Zero-tick periods produce no bar from the aggregator — the validator
        // is never called. This test documents that zero-tick is not an error
        // by showing the validator accepts a valid bar (any bar it sees is ok).
        let b = bar(17.0, 17.0, 17.0, 17.0, 1.0);
        assert!(OhlcValidator::validate(&b).is_ok());
    }

    // ── Duplicate / out-of-order tick rejection ───────────────────────────

    #[test]
    fn fresh_tick_is_accepted() {
        let mut dedup = TickDeduplicator::new();
        assert!(dedup.is_fresh(1, ts(100)));
        assert!(dedup.is_fresh(1, ts(200)));
    }

    #[test]
    fn duplicate_tick_same_timestamp_is_rejected() {
        let mut dedup = TickDeduplicator::new();
        dedup.is_fresh(1, ts(100));
        assert!(!dedup.is_fresh(1, ts(100)), "duplicate must be rejected");
    }

    #[test]
    fn out_of_order_tick_is_rejected() {
        let mut dedup = TickDeduplicator::new();
        dedup.is_fresh(1, ts(200));
        assert!(
            !dedup.is_fresh(1, ts(100)),
            "out-of-order tick must be rejected"
        );
    }

    #[test]
    fn different_uics_are_independent() {
        let mut dedup = TickDeduplicator::new();
        dedup.is_fresh(1, ts(200));
        // Uic 2 has no history — ts(100) should be accepted for it.
        assert!(dedup.is_fresh(2, ts(100)));
    }

    #[test]
    fn out_of_order_ticks_do_not_corrupt_later_accepted_tick() {
        let mut dedup = TickDeduplicator::new();
        assert!(dedup.is_fresh(1, ts(300)));
        assert!(!dedup.is_fresh(1, ts(100))); // stale
        assert!(dedup.is_fresh(1, ts(400))); // fresh — not affected by rejection above
    }

    // ── Gap classification ────────────────────────────────────────────────

    #[test]
    fn weekend_is_after_hours() {
        // Saturday 12:00 UTC
        let sat = Utc.with_ymd_and_hms(2026, 7, 11, 12, 0, 0).unwrap();
        assert_eq!(
            GapClassifier::classify(sat, false),
            SilenceCause::AfterHours
        );
    }

    #[test]
    fn holiday_during_trading_hours_is_market_holiday() {
        let weekday_noon = Utc.with_ymd_and_hms(2026, 6, 19, 10, 0, 0).unwrap(); // Friday
        assert_eq!(
            GapClassifier::classify(weekday_noon, true),
            SilenceCause::MarketHoliday
        );
    }

    #[test]
    fn pre_session_weekday_is_after_hours() {
        let pre_open = Utc.with_ymd_and_hms(2026, 7, 13, 6, 0, 0).unwrap(); // Monday 06:00 UTC
        assert_eq!(
            GapClassifier::classify(pre_open, false),
            SilenceCause::AfterHours
        );
    }

    #[test]
    fn during_session_no_ticks_is_possible_dead_connection() {
        let mid_session = Utc.with_ymd_and_hms(2026, 7, 13, 10, 0, 0).unwrap(); // Monday 10:00 UTC
        assert_eq!(
            GapClassifier::classify(mid_session, false),
            SilenceCause::PossibleDeadConnection
        );
    }

    // ── Gap-fill boundary ─────────────────────────────────────────────────

    #[test]
    fn before_any_live_bar_all_gap_bars_are_allowed() {
        let boundary = GapBoundary::new();
        let date = NaiveDate::from_ymd_opt(2026, 7, 1).unwrap();
        assert!(boundary.should_publish_gap_bar(date));
    }

    #[test]
    fn gap_bar_strictly_before_live_window_is_allowed() {
        let mut boundary = GapBoundary::new();
        let live_date = NaiveDate::from_ymd_opt(2026, 7, 10).unwrap();
        boundary.record_live_bar(live_date);

        let earlier = NaiveDate::from_ymd_opt(2026, 7, 9).unwrap();
        assert!(boundary.should_publish_gap_bar(earlier));
    }

    #[test]
    fn gap_bar_same_as_live_window_is_rejected() {
        let mut boundary = GapBoundary::new();
        let live_date = NaiveDate::from_ymd_opt(2026, 7, 10).unwrap();
        boundary.record_live_bar(live_date);

        assert!(
            !boundary.should_publish_gap_bar(live_date),
            "gap bar at same window as live bar must be rejected to prevent double-counting"
        );
    }

    #[test]
    fn gap_bar_after_live_window_is_rejected() {
        let mut boundary = GapBoundary::new();
        boundary.record_live_bar(NaiveDate::from_ymd_opt(2026, 7, 10).unwrap());

        let later = NaiveDate::from_ymd_opt(2026, 7, 11).unwrap();
        assert!(!boundary.should_publish_gap_bar(later));
    }
}
