/// OHLCV bar aggregation from Saxo WebSocket tick stream.
///
/// Aggregates incoming price ticks into 1-minute OHLCV bars (configurable).
/// A bar is emitted when:
/// - The current tick falls outside the current bar's time window, OR
/// - [`BarAggregator::flush`] is called explicitly (e.g. at market close).
///
/// Bar window default: 60 seconds (matching Saxo's smallest chart `Horizon`).
use chrono::{DateTime, Utc};
use model::{asset::Asset, bar::Bar};
use std::time::{Duration, UNIX_EPOCH};

/// A single price tick from the Saxo stream.
#[derive(Debug, Clone)]
pub struct Tick {
    pub uic: u64,
    pub price: f64,
    pub volume: f64,
    pub timestamp: DateTime<Utc>,
}

/// In-progress bar being built from ticks.
#[derive(Debug, Clone)]
struct PendingBar {
    window_start: DateTime<Utc>,
    open: f64,
    high: f64,
    low: f64,
    close: f64,
    volume: f64,
}

impl PendingBar {
    fn new(tick: &Tick, window_start: DateTime<Utc>) -> Self {
        Self {
            window_start,
            open: tick.price,
            high: tick.price,
            low: tick.price,
            close: tick.price,
            volume: tick.volume,
        }
    }

    fn update(&mut self, tick: &Tick) {
        if tick.price > self.high {
            self.high = tick.price;
        }
        if tick.price < self.low {
            self.low = tick.price;
        }
        self.close = tick.price;
        self.volume += tick.volume;
    }

    fn to_bar(&self, asset: &Asset, currency: &str) -> Bar {
        let ts = UNIX_EPOCH + Duration::from_secs(self.window_start.timestamp() as u64);
        Bar {
            asset: asset.clone(),
            open: self.open,
            high: self.high,
            low: self.low,
            close: self.close,
            volume: self.volume,
            timestamp: ts,
            currency: currency.to_string(),
        }
    }
}

/// Aggregates ticks into OHLCV bars for a single instrument.
pub struct BarAggregator {
    asset: Asset,
    currency: String,
    window_secs: i64,
    pending: Option<PendingBar>,
}

impl BarAggregator {
    /// Create an aggregator for `asset` with the given bar window.
    ///
    /// `currency` must be the ISO 4217 code from the instrument's registry
    /// entry — never derived from the stream payload alone.
    pub fn new(asset: Asset, currency: impl Into<String>, window_secs: i64) -> Self {
        Self {
            asset,
            currency: currency.into(),
            window_secs,
            pending: None,
        }
    }

    /// Create with the default 1-minute window.
    pub fn one_minute(asset: Asset, currency: impl Into<String>) -> Self {
        Self::new(asset, currency, 60)
    }

    /// Process a tick, returning a completed bar if the window rolled over.
    pub fn process(&mut self, tick: &Tick) -> Option<Bar> {
        let window_start = self.window_start_for(tick.timestamp);

        match &mut self.pending {
            None => {
                // First tick — start a new bar.
                self.pending = Some(PendingBar::new(tick, window_start));
                None
            }
            Some(pending) if pending.window_start == window_start => {
                // Same window — update in place.
                pending.update(tick);
                None
            }
            Some(_) => {
                // New window — emit the completed bar and start fresh.
                let completed = self
                    .pending
                    .take()
                    .unwrap()
                    .to_bar(&self.asset, &self.currency);
                self.pending = Some(PendingBar::new(tick, window_start));
                Some(completed)
            }
        }
    }

    /// Flush any pending partial bar. Called at market close or on disconnect.
    ///
    /// A partial bar (fewer ticks than a full window) is still emitted — the
    /// downstream consumer decides whether to use it.
    pub fn flush(&mut self) -> Option<Bar> {
        self.pending
            .take()
            .map(|p| p.to_bar(&self.asset, &self.currency))
    }

    /// Compute the start of the window that contains `ts`.
    fn window_start_for(&self, ts: DateTime<Utc>) -> DateTime<Utc> {
        let secs = ts.timestamp();
        let aligned = (secs / self.window_secs) * self.window_secs;
        DateTime::from_timestamp(aligned, 0).unwrap_or(ts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use model::asset::mic;

    fn gomx_asset() -> Asset {
        Asset::with_mic("GOMX", mic::FNSE)
    }

    fn tick(price: f64, volume: f64, ts_secs: i64) -> Tick {
        Tick {
            uic: 4769462,
            price,
            volume,
            timestamp: DateTime::from_timestamp(ts_secs, 0).unwrap(),
        }
    }

    #[test]
    fn first_tick_starts_bar_no_emit() {
        let mut agg = BarAggregator::one_minute(gomx_asset(), "SEK");
        let result = agg.process(&tick(17.5, 100.0, 1_000_000));
        assert!(result.is_none(), "first tick must not emit a bar");
    }

    #[test]
    fn ticks_in_same_window_aggregate_ohlcv() {
        let mut agg = BarAggregator::one_minute(gomx_asset(), "SEK");
        // All within minute 0 (secs 0–59).
        agg.process(&tick(10.0, 100.0, 0));
        agg.process(&tick(15.0, 200.0, 30));
        agg.process(&tick(8.0, 150.0, 45));
        agg.process(&tick(12.0, 50.0, 59));

        let bar = agg.flush().unwrap();
        assert_eq!(bar.open, 10.0);
        assert_eq!(bar.high, 15.0);
        assert_eq!(bar.low, 8.0);
        assert_eq!(bar.close, 12.0);
        assert_eq!(bar.volume, 500.0);
        assert_eq!(bar.currency, "SEK");
        assert_eq!(bar.asset.exchange_mic, mic::FNSE);
    }

    #[test]
    fn tick_in_new_window_emits_completed_bar() {
        let mut agg = BarAggregator::one_minute(gomx_asset(), "SEK");
        agg.process(&tick(10.0, 100.0, 0)); // minute 0
        agg.process(&tick(11.0, 50.0, 30)); // minute 0

        // Tick in minute 1 → should emit the minute-0 bar.
        let bar = agg.process(&tick(12.0, 75.0, 60)).unwrap();
        assert_eq!(bar.open, 10.0);
        assert_eq!(bar.close, 11.0);
        assert_eq!(bar.volume, 150.0);

        // Minute-1 pending bar flushed.
        let bar2 = agg.flush().unwrap();
        assert_eq!(bar2.open, 12.0);
        assert_eq!(bar2.close, 12.0);
        assert_eq!(bar2.volume, 75.0);
    }

    #[test]
    fn currency_comes_from_registry_not_stream() {
        // Verify currency is exactly what was passed at construction,
        // not derived from the tick payload.
        let mut agg = BarAggregator::one_minute(gomx_asset(), "SEK");
        agg.process(&tick(17.5, 100.0, 0));
        let bar = agg.flush().unwrap();
        assert_eq!(bar.currency, "SEK");
        assert_ne!(
            bar.currency, "USD",
            "must not default to USD for FNSE instrument"
        );
    }

    #[test]
    fn flush_empty_returns_none() {
        let mut agg = BarAggregator::one_minute(gomx_asset(), "SEK");
        assert!(agg.flush().is_none());
    }

    #[test]
    fn window_alignment_is_correct() {
        let mut agg = BarAggregator::one_minute(gomx_asset(), "SEK");
        // Tick at second 90 → window starts at 60, not 90.
        agg.process(&tick(10.0, 100.0, 90));
        // Tick at second 121 → new window (120), emits 60-119 bar.
        let bar = agg.process(&tick(11.0, 50.0, 121)).unwrap();
        // Bar timestamp should be aligned to minute boundary.
        let bar_ts = bar
            .timestamp
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert_eq!(
            bar_ts % 60,
            0,
            "bar timestamp must align to minute boundary"
        );
    }
}
