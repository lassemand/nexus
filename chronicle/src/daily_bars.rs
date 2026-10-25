//! Daily-bar sync run by `saxo-stream` (NEX-162).
//!
//! Fetches official daily bars from Saxo's chart API through the stream's own
//! [`ChartClient`] (no separate authentication: refresh tokens rotate on use)
//! and **publishes** them through a [`BarSink`]. Prices are stored by signal,
//! never here. The only thing written to chronicle's database is progress, in
//! `saxo_daily_bars_state`, and only after the sink has acknowledged a page.

use alpha::saxo::{ChartClient, ChartError, DailyBarRow};
use anyhow::{anyhow, Context};
use chrono::{DateTime, Datelike, Duration, NaiveDate, Utc, Weekday};
use model::generated::DailyBar;
use sqlx::{PgPool, Row};
use tracing::{debug, info};

/// How far back the first backfill goes.
pub fn backfill_floor() -> NaiveDate {
    NaiveDate::from_ymd_opt(2015, 1, 1).expect("valid date")
}

/// Incremental runs re-fetch this many weekdays before `last_published_date`
/// so late corrections from Saxo are picked up.
pub const REFETCH_WEEKDAYS: i64 = 10;

/// Safety cap on chart requests per instrument and run.
const MAX_PAGES: usize = 30;

/// Value of `DailyBar.source`.
pub const SOURCE: &str = "saxo";

/// An instrument to sync.
#[derive(Debug, Clone)]
pub struct Instrument {
    pub ticker: String,
    pub exchange_mic: String,
    pub uic: u64,
    pub currency: String,
}

/// Where bars go (Kafka in production). `publish` returns once the bar is
/// acknowledged.
#[async_trait::async_trait]
pub trait BarSink: Send + Sync {
    async fn publish(&self, bar: &DailyBar) -> Result<(), String>;
}

/// Progress stored in `saxo_daily_bars_state`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct State {
    pub backfilled_from: Option<NaiveDate>,
    pub last_published_date: Option<NaiveDate>,
}

/// Reads progress; `None` if the instrument was never synced.
pub async fn load_state(pool: &PgPool, ticker: &str, mic: &str) -> sqlx::Result<Option<State>> {
    let row = sqlx::query(
        "SELECT backfilled_from, last_published_date FROM saxo_daily_bars_state \
         WHERE ticker = $1 AND exchange_mic = $2",
    )
    .bind(ticker)
    .bind(mic)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| State {
        backfilled_from: r.get("backfilled_from"),
        last_published_date: r.get("last_published_date"),
    }))
}

async fn save_state(pool: &PgPool, ticker: &str, mic: &str, s: &State) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO saxo_daily_bars_state \
         (ticker, exchange_mic, backfilled_from, last_published_date, updated_at) \
         VALUES ($1, $2, $3, $4, now()) \
         ON CONFLICT (ticker, exchange_mic) DO UPDATE SET \
            backfilled_from = EXCLUDED.backfilled_from, \
            last_published_date = EXCLUDED.last_published_date, \
            updated_at = now()",
    )
    .bind(ticker)
    .bind(mic)
    .bind(s.backfilled_from)
    .bind(s.last_published_date)
    .execute(pool)
    .await?;
    Ok(())
}

/// The date `n` weekdays before `date` (holidays are not modelled; re-fetching a
/// day too many is harmless).
pub fn weekdays_before(date: NaiveDate, n: i64) -> NaiveDate {
    let mut d = date;
    let mut left = n;
    while left > 0 {
        d = d.pred_opt().expect("valid date");
        if !matches!(d.weekday(), Weekday::Sat | Weekday::Sun) {
            left -= 1;
        }
    }
    d
}

fn weekdays_between(from: NaiveDate, to: NaiveDate) -> i64 {
    let mut n = 0;
    let mut d = from;
    while d < to {
        if !matches!(d.weekday(), Weekday::Sat | Weekday::Sun) {
            n += 1;
        }
        d = d.succ_opt().expect("valid date");
    }
    n
}

/// Builds the wire message. Prices stay decimal strings.
pub fn to_proto(i: &Instrument, row: &DailyBarRow, fetched_at: DateTime<Utc>) -> DailyBar {
    DailyBar {
        ticker: i.ticker.clone(),
        exchange_mic: i.exchange_mic.clone(),
        date: row.date.to_string(),
        open: row.open.to_string(),
        high: row.high.to_string(),
        low: row.low.to_string(),
        close: row.close.to_string(),
        volume: row.volume,
        currency: i.currency.clone(),
        source: SOURCE.to_string(),
        fetched_at_unix_secs: fetched_at.timestamp(),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Walking back through history: also moves `backfilled_from`.
    Backfill,
    /// Re-fetching recent days: only moves `last_published_date`.
    Recent,
}

/// What a sync did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SyncOutcome {
    /// Bars published.
    pub published: usize,
    /// Chart requests made.
    pub requests: usize,
}

/// Pages back from `start` until bars reach `stop_at` or the data runs out,
/// publishing each page and recording progress only after it was acknowledged.
#[allow(clippy::too_many_arguments)]
async fn page_back(
    pool: &PgPool,
    client: &mut ChartClient,
    sink: &dyn BarSink,
    inst: &Instrument,
    state: &mut State,
    mode: Mode,
    start: DateTime<Utc>,
    stop_at: NaiveDate,
    now: DateTime<Utc>,
    out: &mut SyncOutcome,
) -> anyhow::Result<()> {
    let mut cursor = start;
    let mut previous_earliest: Option<DateTime<Utc>> = None;
    let count = match mode {
        Mode::Backfill => alpha::saxo::chart::MAX_COUNT,
        Mode::Recent => (weekdays_between(stop_at, now.date_naive()) + 5)
            .clamp(5, alpha::saxo::chart::MAX_COUNT as i64) as u32,
    };

    for _ in 0..MAX_PAGES {
        let rows = client
            .fetch_up_to(inst.uic, cursor, count)
            .await
            .map_err(|e: ChartError| anyhow!(e))
            .with_context(|| format!("chart request for {}", inst.ticker))?;
        out.requests += 1;

        let Some(earliest) = rows.first().map(|r| r.time) else {
            // No more data: the history is complete as far as Saxo knows.
            if mode == Mode::Backfill {
                state.backfilled_from = Some(backfill_floor());
                save_state(pool, &inst.ticker, &inst.exchange_mic, state).await?;
            }
            return Ok(());
        };
        if previous_earliest.is_some_and(|p| earliest >= p) {
            return Err(anyhow!("chart API did not advance for {}", inst.ticker));
        }
        previous_earliest = Some(earliest);

        let in_scope: Vec<&DailyBarRow> = rows.iter().filter(|r| r.date >= stop_at).collect();
        for row in &in_scope {
            sink.publish(&to_proto(inst, row, now))
                .await
                .map_err(|e| anyhow!("publishing {} {}: {e}", inst.ticker, row.date))?;
            out.published += 1;
        }

        // The whole page is acknowledged: record progress.
        if let Some(newest) = in_scope.iter().map(|r| r.date).max() {
            state.last_published_date = state.last_published_date.max(Some(newest));
        }
        let reached = rows.first().is_some_and(|r| r.date <= stop_at);
        if mode == Mode::Backfill {
            let oldest = in_scope.iter().map(|r| r.date).min();
            state.backfilled_from = if reached {
                Some(backfill_floor().min(stop_at))
            } else {
                match (state.backfilled_from, oldest) {
                    (Some(a), Some(b)) => Some(a.min(b)),
                    (a, b) => a.or(b),
                }
            };
        }
        save_state(pool, &inst.ticker, &inst.exchange_mic, state).await?;
        debug!(ticker = %inst.ticker, mode = mode_name(mode), published = out.published, "page published");

        if reached {
            return Ok(());
        }
        cursor = earliest - Duration::seconds(1);
    }
    Err(anyhow!("page cap reached for {}", inst.ticker))
}

fn mode_name(m: Mode) -> &'static str {
    match m {
        Mode::Backfill => "backfill",
        Mode::Recent => "recent",
    }
}

/// Syncs one instrument: backfills history to 2015-01-01 if it was never (fully)
/// backfilled, and re-fetches the last [`REFETCH_WEEKDAYS`] weekdays of what was
/// already published.
pub async fn sync_instrument(
    pool: &PgPool,
    client: &mut ChartClient,
    sink: &dyn BarSink,
    inst: &Instrument,
    now: DateTime<Utc>,
) -> anyhow::Result<SyncOutcome> {
    let floor = backfill_floor();
    let mut out = SyncOutcome::default();
    let existing = load_state(pool, &inst.ticker, &inst.exchange_mic).await?;
    let mut state = existing.clone().unwrap_or_default();

    match existing {
        None => {
            page_back(
                pool,
                client,
                sink,
                inst,
                &mut state,
                Mode::Backfill,
                now,
                floor,
                now,
                &mut out,
            )
            .await?;
        }
        Some(s) => {
            let recent_from = s
                .last_published_date
                .map(|d| weekdays_before(d, REFETCH_WEEKDAYS))
                .unwrap_or(floor);
            page_back(
                pool,
                client,
                sink,
                inst,
                &mut state,
                Mode::Recent,
                now,
                recent_from,
                now,
                &mut out,
            )
            .await?;
            if s.backfilled_from.is_none_or(|b| b > floor) {
                // Resume the unfinished backfill just before what we already have.
                let resume = s
                    .backfilled_from
                    .and_then(|b| b.and_hms_opt(0, 0, 0))
                    .map(|t| t.and_utc())
                    .unwrap_or(now);
                page_back(
                    pool,
                    client,
                    sink,
                    inst,
                    &mut state,
                    Mode::Backfill,
                    resume,
                    floor,
                    now,
                    &mut out,
                )
                .await?;
            }
        }
    }
    info!(ticker = %inst.ticker, published = out.published, requests = out.requests, "daily bars synced");
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> NaiveDate {
        s.parse().unwrap()
    }

    #[test]
    fn weekdays_before_skips_weekends() {
        // Friday 2026-08-28 minus 10 weekdays = Friday 2026-08-14.
        assert_eq!(weekdays_before(d("2026-08-28"), 10), d("2026-08-14"));
        // Monday minus 1 weekday = previous Friday.
        assert_eq!(weekdays_before(d("2026-08-31"), 1), d("2026-08-28"));
        assert_eq!(weekdays_before(d("2026-08-31"), 0), d("2026-08-31"));
    }

    #[test]
    fn weekdays_between_counts_working_days() {
        assert_eq!(weekdays_between(d("2026-08-24"), d("2026-08-31")), 5);
        assert_eq!(weekdays_between(d("2026-08-31"), d("2026-08-31")), 0);
    }
}
