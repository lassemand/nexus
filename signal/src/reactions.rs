//! Share-price reaction of each released Nordic report (NEX-163).
//!
//! For a qualifying report (`source = 'mfn'`, `status = 'occurred'`, a quarterly
//! or annual report, not a correction) the reaction is stored on its own
//! `special_events` row. All prices stay `NUMERIC` (text in Rust, arithmetic in
//! Postgres): no `f64` anywhere.
//!
//! # Definitions
//!
//! Everything exchange-specific comes from the `exchanges` table (one row per
//! MIC, loaded once per refresh):
//!
//! | MIC            | country | time zone           | open  | close | half-day close |
//! |----------------|---------|---------------------|-------|-------|----------------|
//! | `FNSE`, `XSTO` | `SE`    | `Europe/Stockholm`  | 09:00 | 17:30 | 13:00          |
//! | `XCSE`, `FNDK` | `DK`    | `Europe/Copenhagen` | 09:00 | 17:00 | none           |
//! | `XHEL`, `FNFI` | `FI`    | `Europe/Helsinki`   | 10:00 | 18:30 | none           |
//!
//! Let `t` be the release instant in the exchange's local time (its own
//! `timezone`, never another city's) and `d` its local date. The regular
//! session runs from `open_local` to `close_local`, or to `half_day_close_local`
//! on a `half_day` (a `half_day` of an exchange with no half-day close counts as
//! a full day, with a one-time warning). A *trading day* is a weekday that is not
//! `closed` in `trading_holidays` for the exchange's `country` (`SE`, `DK`).
//! The exchange's calendar applies, not the company's home country. A MIC that
//! is not in `exchanges` gives `calendar_unknown`.
//!
//! 1. **Session**: on a trading day `d`, `pre_open` before 09:00, `intraday`
//!    from 09:00 up to the close, `after_close` at or after the close; on a
//!    weekend or a `closed` holiday, `non_trading_day`.
//! 2. **Reaction day** `r`: `d` for `pre_open`/`intraday`; the first trading day
//!    after `d` for `after_close`/`non_trading_day`.
//! 3. **Base day** `b`: the last trading day before `r`.
//! 4. **Move** = `(close[r] − close[b]) / close[b] × 100`, in `NUMERIC`, rounded
//!    to 4 decimals (half away from zero) only when stored.
//! 5. **Base price**: the last close on or before `b` (a thin stock may not trade
//!    on `b`). The reaction day is strict: no bar on `r`, no reaction. A later
//!    close is never used to fill a gap (no look-ahead).
//!
//! # Calendar coverage
//!
//! `trading_holidays` covers whole years: from 1 January of its first year to
//! 31 December of its last year for that country. A date outside that range, or
//! an exchange without a country, gives `calendar_unknown`. The calendar is
//! never guessed.
//!
//! # Data status
//!
//! In this order: `calendar_unknown`; no bars at all → `missing_base_bar`;
//! `r` after the latest stored bar → `future`; no bar on `r` →
//! `missing_reaction_bar`; no bar on or before `b` → `missing_base_bar`; base
//! close 0 → `zero_base`; base bar more than 5 trading days before `b` →
//! `stale_base`; otherwise `ok`. `implausible_move` (|move| ≥ 10^15 %) exists
//! only so absurd data can never overflow the column. `move_pct` is NULL unless
//! the status is `ok`; `needs_review` is set when `|move_pct| > 40`.

use chrono::{DateTime, Datelike, NaiveDate, NaiveDateTime, NaiveTime, Utc};
use chrono_tz::Tz;
use model::calendar::{DynamicCalendar, TradingCalendar, TradingStatus};
use rust_decimal::Decimal;
use sqlx::{PgConnection, PgPool, Row};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::str::FromStr;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;
use tracing::{info, warn};

/// A base bar older than this many trading days before `b` is `stale_base`.
pub const STALE_AFTER_TRADING_DAYS: i64 = 5;
/// `needs_review` threshold for `|move_pct|`.
pub const REVIEW_THRESHOLD_PCT: i64 = 40;
/// When daily bars arrive, reports whose reaction day is within this many days
/// before the earliest new bar are recomputed (late price corrections).
pub const CORRECTION_WINDOW_DAYS: i64 = 10;

const QUALIFYING: &str = "source = 'mfn' AND status = 'occurred' \
     AND event_type IN ('quarterly_report', 'annual_report') AND NOT is_correction";

/// Where the release falls relative to the exchange's session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Session {
    PreOpen,
    Intraday,
    AfterClose,
    NonTradingDay,
}

impl Session {
    /// Value stored in `release_session`.
    pub fn as_str(self) -> &'static str {
        match self {
            Session::PreOpen => "pre_open",
            Session::Intraday => "intraday",
            Session::AfterClose => "after_close",
            Session::NonTradingDay => "non_trading_day",
        }
    }
}

/// Why a reaction could or could not be computed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DataStatus {
    Ok,
    MissingReactionBar,
    MissingBaseBar,
    Future,
    ZeroBase,
    StaleBase,
    CalendarUnknown,
    ImplausibleMove,
}

impl DataStatus {
    /// Value stored in `data_status`.
    pub fn as_str(self) -> &'static str {
        match self {
            DataStatus::Ok => "ok",
            DataStatus::MissingReactionBar => "missing_reaction_bar",
            DataStatus::MissingBaseBar => "missing_base_bar",
            DataStatus::Future => "future",
            DataStatus::ZeroBase => "zero_base",
            DataStatus::StaleBase => "stale_base",
            DataStatus::CalendarUnknown => "calendar_unknown",
            DataStatus::ImplausibleMove => "implausible_move",
        }
    }
}

/// One row of the `exchanges` table: where and when a MIC trades.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exchange {
    /// ISO 10383 MIC.
    pub mic: String,
    /// Country whose `trading_holidays` apply.
    pub country: String,
    /// The exchange's own IANA time zone.
    pub timezone: Tz,
    /// Regular session open, local time.
    pub open: NaiveTime,
    /// Regular session close, local time.
    pub close: NaiveTime,
    /// Early close on `half_day` dates, if the exchange has any.
    pub half_day_close: Option<NaiveTime>,
}

impl Exchange {
    /// Loads the row for `mic`. `None` if the MIC is not in `exchanges` or its
    /// time zone is not an IANA name (logged): both mean `calendar_unknown`.
    pub async fn load(conn: &mut PgConnection, mic: &str) -> sqlx::Result<Option<Self>> {
        let Some(r) = sqlx::query(
            "SELECT country, timezone, open_local, close_local, half_day_close_local \
             FROM exchanges WHERE mic = $1",
        )
        .bind(mic)
        .fetch_optional(&mut *conn)
        .await?
        else {
            return Ok(None);
        };
        let tz: String = r.get("timezone");
        let Ok(timezone) = tz.parse::<Tz>() else {
            warn!(mic, timezone = %tz, "exchanges.timezone is not an IANA time zone");
            return Ok(None);
        };
        Ok(Some(Self {
            mic: mic.to_string(),
            country: r.get("country"),
            timezone,
            open: r.get("open_local"),
            close: r.get("close_local"),
            half_day_close: r.get("half_day_close_local"),
        }))
    }

    /// The release instant in the exchange's local time.
    fn local(&self, at: DateTime<Utc>) -> NaiveDateTime {
        at.with_timezone(&self.timezone).naive_local()
    }

    /// The session close on a day with the given status.
    fn close_on(&self, status: TradingStatus) -> NaiveTime {
        match (status, self.half_day_close) {
            (TradingStatus::HalfDay, Some(early)) => early,
            (TradingStatus::HalfDay, None) => {
                // Logged once per MIC, not once per report.
                static WARNED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
                let first = WARNED
                    .get_or_init(Default::default)
                    .lock()
                    .map(|mut w| w.insert(self.mic.clone()))
                    .unwrap_or(false);
                if first {
                    warn!(mic = %self.mic,
                          "half_day in trading_holidays but the exchange has no half-day close; treated as a full day");
                }
                self.close
            }
            _ => self.close,
        }
    }
}

/// An exchange together with its country's calendar.
#[derive(Debug, Clone)]
pub struct Market {
    pub exchange: Exchange,
    pub calendar: Calendar,
}

/// A country's trading calendar plus the date range it is known for.
#[derive(Debug, Clone)]
pub struct Calendar {
    days: DynamicCalendar,
    covered_from: NaiveDate,
    covered_to: NaiveDate,
}

impl Calendar {
    /// Builds a calendar from holiday rows. Coverage is the whole years from the
    /// first to the last holiday year. Returns `None` for no rows at all.
    pub fn from_holidays(
        closed: HashSet<NaiveDate>,
        half_days: HashSet<NaiveDate>,
    ) -> Option<Self> {
        let years: Vec<i32> = closed
            .iter()
            .chain(half_days.iter())
            .map(|d| d.year())
            .collect();
        let (first, last) = (*years.iter().min()?, *years.iter().max()?);
        Some(Self {
            covered_from: NaiveDate::from_ymd_opt(first, 1, 1)?,
            covered_to: NaiveDate::from_ymd_opt(last, 12, 31)?,
            days: DynamicCalendar::new(closed, half_days),
        })
    }

    fn covers(&self, d: NaiveDate) -> bool {
        d >= self.covered_from && d <= self.covered_to
    }

    fn is_trading_day(&self, d: NaiveDate) -> bool {
        self.days.is_trading_day(d)
    }

    /// The first trading day after `d`, if still inside the covered range.
    pub fn next_trading_day(&self, d: NaiveDate) -> Option<NaiveDate> {
        let mut day = d;
        loop {
            day = day.succ_opt()?;
            if !self.covers(day) {
                return None;
            }
            if self.is_trading_day(day) {
                return Some(day);
            }
        }
    }

    /// The last trading day before `d`, if still inside the covered range.
    pub fn prev_trading_day(&self, d: NaiveDate) -> Option<NaiveDate> {
        let mut day = d;
        loop {
            day = day.pred_opt()?;
            if !self.covers(day) {
                return None;
            }
            if self.is_trading_day(day) {
                return Some(day);
            }
        }
    }

    /// Trading days in `(after, up_to]`.
    pub fn trading_days_between(&self, after: NaiveDate, up_to: NaiveDate) -> i64 {
        let mut n = 0;
        let mut d = after;
        while d < up_to {
            d = d.succ_opt().expect("valid date");
            if self.is_trading_day(d) {
                n += 1;
            }
        }
        n
    }
}

/// The dates a release maps to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Plan {
    pub session: Session,
    /// The exchange's local date of the release (`d`).
    pub release_date: NaiveDate,
    /// First trading day the market could react (`r`).
    pub reaction_date: NaiveDate,
    /// Last trading day before `r` (`b`).
    pub base_date: NaiveDate,
}

/// Classifies a release instant on `ex`. `None` means `calendar_unknown`: `d`,
/// `r` or `b` falls outside the calendar's covered range.
pub fn plan(occurred_at: DateTime<Utc>, cal: &Calendar, ex: &Exchange) -> Option<Plan> {
    let local = ex.local(occurred_at);
    let (d, t) = (local.date(), local.time());
    if !cal.covers(d) {
        return None;
    }
    let session = match cal.days.trading_status(d) {
        TradingStatus::Closed => Session::NonTradingDay,
        status => {
            if t < ex.open {
                Session::PreOpen
            } else if t < ex.close_on(status) {
                Session::Intraday
            } else {
                Session::AfterClose
            }
        }
    };
    let reaction_date = match session {
        Session::PreOpen | Session::Intraday => d,
        Session::AfterClose | Session::NonTradingDay => cal.next_trading_day(d)?,
    };
    Some(Plan {
        session,
        release_date: d,
        reaction_date,
        base_date: cal.prev_trading_day(reaction_date)?,
    })
}

/// How many reports ended in each status.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Counts {
    /// Rows whose reaction was (re)computed.
    pub computed: usize,
    /// Rows whose reaction columns were cleared (no longer qualifying).
    pub cleared: u64,
    /// Computed rows by `data_status`.
    pub by_status: BTreeMap<&'static str, usize>,
}

impl Counts {
    fn add(&mut self, other: &Counts) {
        self.computed += other.computed;
        self.cleared += other.cleared;
        for (k, v) in &other.by_status {
            *self.by_status.entry(k).or_default() += v;
        }
    }
}

/// The exchange and its country's calendar. `None` (→ `calendar_unknown`) for a
/// MIC missing from `exchanges`, or a country without holiday rows.
async fn load_market(conn: &mut PgConnection, mic: &str) -> sqlx::Result<Option<Market>> {
    let Some(exchange) = Exchange::load(conn, mic).await? else {
        return Ok(None);
    };
    let rows = sqlx::query("SELECT date, status FROM trading_holidays WHERE country = $1")
        .bind(&exchange.country)
        .fetch_all(&mut *conn)
        .await?;
    let (mut closed, mut half) = (HashSet::new(), HashSet::new());
    for r in rows {
        let date: NaiveDate = r.get("date");
        if r.get::<String, _>("status") == "closed" {
            closed.insert(date);
        } else {
            half.insert(date);
        }
    }
    Ok(Calendar::from_holidays(closed, half).map(|calendar| Market { exchange, calendar }))
}

struct Bar {
    close: String,
    volume: i64,
}

async fn bar_on(
    conn: &mut PgConnection,
    t: &str,
    mic: &str,
    date: NaiveDate,
) -> sqlx::Result<Option<Bar>> {
    Ok(sqlx::query(
        "SELECT close::text AS close, volume FROM daily_bars \
         WHERE ticker = $1 AND exchange_mic = $2 AND date = $3",
    )
    .bind(t)
    .bind(mic)
    .bind(date)
    .fetch_optional(&mut *conn)
    .await?
    .map(|r| Bar {
        close: r.get("close"),
        volume: r.get("volume"),
    }))
}

/// Everything written to one report row.
struct Outcome {
    session: Option<Session>,
    reaction_date: Option<NaiveDate>,
    base_date: Option<NaiveDate>,
    base_close: Option<String>,
    reaction_close: Option<String>,
    move_pct: Option<String>,
    volume: Option<i64>,
    status: DataStatus,
    needs_review: bool,
}

impl Outcome {
    fn unknown() -> Self {
        Self {
            session: None,
            reaction_date: None,
            base_date: None,
            base_close: None,
            reaction_close: None,
            move_pct: None,
            volume: None,
            status: DataStatus::CalendarUnknown,
            needs_review: false,
        }
    }
}

async fn compute(
    conn: &mut PgConnection,
    ticker: &str,
    mic: &str,
    occurred_at: DateTime<Utc>,
    market: Option<&Market>,
) -> sqlx::Result<Outcome> {
    let Some(market) = market else {
        return Ok(Outcome::unknown());
    };
    let cal = &market.calendar;
    let Some(p) = plan(occurred_at, cal, &market.exchange) else {
        return Ok(Outcome::unknown());
    };
    let mut out = Outcome {
        session: Some(p.session),
        reaction_date: Some(p.reaction_date),
        base_date: Some(p.base_date),
        status: DataStatus::Ok,
        ..Outcome::unknown()
    };

    let latest: Option<NaiveDate> = sqlx::query_scalar(
        "SELECT max(date) FROM daily_bars WHERE ticker = $1 AND exchange_mic = $2",
    )
    .bind(ticker)
    .bind(mic)
    .fetch_one(&mut *conn)
    .await?;
    let Some(latest) = latest else {
        out.status = DataStatus::MissingBaseBar; // no price history at all
        return Ok(out);
    };

    let reaction = bar_on(conn, ticker, mic, p.reaction_date).await?;
    if let Some(r) = &reaction {
        out.reaction_close = Some(r.close.clone());
        out.volume = Some(r.volume);
    }
    // Base: the last close on or before b. Never a later one.
    let base = sqlx::query(
        "SELECT date, close::text AS close FROM daily_bars \
         WHERE ticker = $1 AND exchange_mic = $2 AND date <= $3 ORDER BY date DESC LIMIT 1",
    )
    .bind(ticker)
    .bind(mic)
    .bind(p.base_date)
    .fetch_optional(&mut *conn)
    .await?
    .map(|r| (r.get::<NaiveDate, _>("date"), r.get::<String, _>("close")));
    if let Some((_, close)) = &base {
        out.base_close = Some(close.clone());
    }

    out.status = if p.reaction_date > latest {
        DataStatus::Future
    } else if reaction.is_none() {
        DataStatus::MissingReactionBar
    } else if base.is_none() {
        DataStatus::MissingBaseBar
    } else {
        DataStatus::Ok
    };
    if out.status != DataStatus::Ok {
        return Ok(out);
    }
    let (base_bar_date, base_close) = base.expect("checked above");
    let reaction_close = out.reaction_close.clone().expect("checked above");

    if Decimal::from_str(&base_close)
        .map(|d| d.is_zero())
        .unwrap_or(false)
    {
        out.status = DataStatus::ZeroBase;
    } else if cal.trading_days_between(base_bar_date, p.base_date) > STALE_AFTER_TRADING_DAYS {
        out.status = DataStatus::StaleBase;
    }
    if out.status != DataStatus::Ok {
        return Ok(out);
    }

    // The move, computed in NUMERIC by Postgres.
    let move_text: String = sqlx::query_scalar(
        "SELECT round((($1::numeric - $2::numeric) / $2::numeric) * 100, 4)::text",
    )
    .bind(&reaction_close)
    .bind(&base_close)
    .fetch_one(&mut *conn)
    .await?;
    match Decimal::from_str(&move_text) {
        Ok(m) if m.abs() < Decimal::from(1_000_000_000_000_000_i64) => {
            out.needs_review = m.abs() > Decimal::from(REVIEW_THRESHOLD_PCT);
            out.move_pct = Some(move_text);
        }
        _ => {
            out.status = DataStatus::ImplausibleMove;
            out.needs_review = true;
        }
    }
    Ok(out)
}

async fn refresh_in(
    conn: &mut PgConnection,
    ticker: &str,
    mic: &str,
    since: Option<NaiveDate>,
) -> sqlx::Result<Counts> {
    // Rows that no longer qualify (a correction, a scheduled row, another source)
    // must not carry reaction values.
    let cleared = sqlx::query(&format!(
        "UPDATE special_events SET release_session = NULL, reaction_date = NULL, \
                base_date = NULL, base_close = NULL, reaction_close = NULL, move_pct = NULL, \
                reaction_volume = NULL, data_status = NULL, needs_review = NULL, \
                reaction_computed_at = NULL \
         WHERE ticker = $1 AND exchange_mic = $2 \
           AND (data_status IS NOT NULL OR release_session IS NOT NULL \
                OR reaction_computed_at IS NOT NULL) \
           AND NOT ({QUALIFYING})"
    ))
    .bind(ticker)
    .bind(mic)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    let mut counts = Counts {
        cleared,
        ..Counts::default()
    };

    // Once per refresh, not once per report.
    let market = load_market(conn, mic).await?;
    let rows = sqlx::query(&format!(
        "SELECT id, occurred_at FROM special_events \
         WHERE ticker = $1 AND exchange_mic = $2 AND {QUALIFYING} \
           AND ($3::date IS NULL OR data_status IS DISTINCT FROM 'ok' OR reaction_date IS NULL \
                OR reaction_date >= $3::date - $4::int) \
         ORDER BY occurred_at FOR UPDATE"
    ))
    .bind(ticker)
    .bind(mic)
    .bind(since)
    .bind(CORRECTION_WINDOW_DAYS as i32)
    .fetch_all(&mut *conn)
    .await?;

    for row in rows {
        let id: i64 = row.get("id");
        let occurred_at: DateTime<Utc> = row.get("occurred_at");
        let o = compute(conn, ticker, mic, occurred_at, market.as_ref()).await?;
        sqlx::query(
            "UPDATE special_events SET release_session = $2, reaction_date = $3, base_date = $4, \
                    base_close = $5::numeric, reaction_close = $6::numeric, \
                    move_pct = $7::numeric, reaction_volume = $8, data_status = $9, \
                    needs_review = $10, reaction_computed_at = now() \
             WHERE id = $1",
        )
        .bind(id)
        .bind(o.session.map(Session::as_str))
        .bind(o.reaction_date)
        .bind(o.base_date)
        .bind(&o.base_close)
        .bind(&o.reaction_close)
        .bind(&o.move_pct)
        .bind(o.volume)
        .bind(o.status.as_str())
        .bind(o.needs_review)
        .execute(&mut *conn)
        .await?;
        counts.computed += 1;
        *counts.by_status.entry(o.status.as_str()).or_default() += 1;
    }
    Ok(counts)
}

/// Recomputes the reaction of every qualifying report of a company, in one
/// transaction. Idempotent.
pub async fn refresh_report_reactions(
    pool: &PgPool,
    ticker: &str,
    mic: &str,
) -> sqlx::Result<Counts> {
    let mut tx = pool.begin().await?;
    let counts = refresh_in(&mut tx, ticker, mic, None).await?;
    tx.commit().await?;
    Ok(counts)
}

/// Like [`refresh_report_reactions`], after daily bars from `earliest_new_bar`
/// on were stored: only reports that are not `ok`, never computed, or whose
/// reaction day is within [`CORRECTION_WINDOW_DAYS`] before that date.
pub async fn refresh_after_bars(
    pool: &PgPool,
    ticker: &str,
    mic: &str,
    earliest_new_bar: NaiveDate,
) -> sqlx::Result<Counts> {
    let mut tx = pool.begin().await?;
    let counts = refresh_in(&mut tx, ticker, mic, Some(earliest_new_bar)).await?;
    tx.commit().await?;
    Ok(counts)
}

/// Catch-up sweep at startup: every company that has Nordic reports.
pub async fn refresh_all(pool: &PgPool) -> sqlx::Result<Counts> {
    let companies = sqlx::query(
        "SELECT DISTINCT ticker, exchange_mic FROM special_events WHERE source = 'mfn' \
         ORDER BY ticker, exchange_mic",
    )
    .fetch_all(pool)
    .await?;
    let mut total = Counts::default();
    for c in &companies {
        let (ticker, mic): (String, String) = (c.get("ticker"), c.get("exchange_mic"));
        match refresh_report_reactions(pool, &ticker, &mic).await {
            Ok(counts) => total.add(&counts),
            Err(e) => warn!(ticker, mic, error = %e, "report reaction sweep failed for company"),
        }
    }
    info!(
        companies = companies.len(),
        computed = total.computed,
        cleared = total.cleared,
        by_status = ?total.by_status,
        "report reaction catch-up sweep done"
    );
    Ok(total)
}

/// Collects instruments whose daily bars changed and releases them once the bar
/// stream goes quiet, so a backfill of thousands of bars triggers one recompute
/// per instrument instead of one per bar.
#[derive(Debug)]
pub struct ReactionQueue {
    pending: HashMap<(String, String), NaiveDate>,
    first_added: Option<Instant>,
    last_added: Option<Instant>,
    quiet: std::time::Duration,
    max_wait: std::time::Duration,
}

impl ReactionQueue {
    /// Due once nothing was added for `quiet`, or the oldest entry waited `max_wait`.
    pub fn new(quiet: std::time::Duration, max_wait: std::time::Duration) -> Self {
        Self {
            pending: HashMap::new(),
            first_added: None,
            last_added: None,
            quiet,
            max_wait,
        }
    }

    /// Notes that a bar dated `date` was stored for the instrument.
    pub fn add(&mut self, ticker: &str, mic: &str, date: NaiveDate, now: Instant) {
        let e = self
            .pending
            .entry((ticker.to_string(), mic.to_string()))
            .or_insert(date);
        *e = (*e).min(date);
        self.first_added.get_or_insert(now);
        self.last_added = Some(now);
    }

    /// True if the queue should be flushed.
    pub fn is_due(&self, now: Instant) -> bool {
        match (self.first_added, self.last_added) {
            (Some(first), Some(last)) => now - last >= self.quiet || now - first >= self.max_wait,
            _ => false,
        }
    }

    /// Takes everything queued: `(ticker, mic, earliest new bar)`.
    pub fn take(&mut self) -> Vec<(String, String, NaiveDate)> {
        self.first_added = None;
        self.last_added = None;
        self.pending.drain().map(|((t, m), d)| (t, m, d)).collect()
    }
}

/// Recomputes the queued instruments. Errors are logged: a failed recompute is
/// repaired by the next bar, the next report event or the startup sweep.
pub async fn flush(pool: &PgPool, queue: &mut ReactionQueue) {
    for (ticker, mic, earliest) in queue.take() {
        match refresh_after_bars(pool, &ticker, &mic, earliest).await {
            Ok(c) if c.computed > 0 || c.cleared > 0 => {
                info!(ticker, mic, computed = c.computed, by_status = ?c.by_status,
                      "report reactions refreshed after daily bars");
            }
            Ok(_) => {}
            Err(e) => warn!(ticker, mic, error = %e, "report reaction refresh failed"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn d(s: &str) -> NaiveDate {
        s.parse().unwrap()
    }

    /// 2026 Easter and Christmas, plus 2015 and 2027 as range markers.
    fn cal() -> Calendar {
        let closed = [
            "2015-01-01",
            "2026-04-03",
            "2026-04-06",
            "2026-12-25",
            "2027-12-31",
        ]
        .iter()
        .map(|s| d(s))
        .collect();
        let half = ["2026-12-24"].iter().map(|s| d(s)).collect();
        Calendar::from_holidays(closed, half).unwrap()
    }

    fn hm(h: u32, m: u32) -> NaiveTime {
        NaiveTime::from_hms_opt(h, m, 0).unwrap()
    }

    /// The `exchanges` rows of FNSE/XSTO and XCSE/FNDK.
    fn sto() -> Exchange {
        Exchange {
            mic: "FNSE".into(),
            country: "SE".into(),
            timezone: "Europe/Stockholm".parse().unwrap(),
            open: hm(9, 0),
            close: hm(17, 30),
            half_day_close: Some(hm(13, 0)),
        }
    }

    fn cph() -> Exchange {
        Exchange {
            mic: "XCSE".into(),
            country: "DK".into(),
            timezone: "Europe/Copenhagen".parse().unwrap(),
            open: hm(9, 0),
            close: hm(17, 0),
            half_day_close: None,
        }
    }

    fn utc(y: i32, m: u32, day: u32, h: u32, mi: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, m, day, h, mi, 0).unwrap()
    }

    #[test]
    fn sessions_and_reaction_days() {
        let c = cal();
        // (instant, session, reaction day, base day)
        let cases = [
            // Tuesday 08:00 CEST: pre-open → same day.
            (
                utc(2026, 8, 25, 6, 0),
                Session::PreOpen,
                "2026-08-25",
                "2026-08-24",
            ),
            // Intraday 13:30 CEST.
            (
                utc(2026, 8, 25, 11, 30),
                Session::Intraday,
                "2026-08-25",
                "2026-08-24",
            ),
            // Friday 18:00 CEST: after the close → Monday.
            (
                utc(2026, 8, 28, 16, 0),
                Session::AfterClose,
                "2026-08-31",
                "2026-08-28",
            ),
            // Saturday 10:00 → Monday.
            (
                utc(2026, 8, 29, 8, 0),
                Session::NonTradingDay,
                "2026-08-31",
                "2026-08-28",
            ),
            // Sunday.
            (
                utc(2026, 8, 30, 12, 0),
                Session::NonTradingDay,
                "2026-08-31",
                "2026-08-28",
            ),
            // Thursday before Easter 18:00 CEST → after Good Friday and Easter Monday.
            (
                utc(2026, 4, 2, 16, 0),
                Session::AfterClose,
                "2026-04-07",
                "2026-04-02",
            ),
            // Good Friday itself is a closed day.
            (
                utc(2026, 4, 3, 6, 0),
                Session::NonTradingDay,
                "2026-04-07",
                "2026-04-02",
            ),
            // Half day 14:00 CET (13:00Z): after the 13:00 close.
            (
                utc(2026, 12, 24, 13, 0),
                Session::AfterClose,
                "2026-12-28",
                "2026-12-24",
            ),
            // Winter 08:00 CET (07:00Z) is pre-open too.
            (
                utc(2026, 1, 13, 7, 0),
                Session::PreOpen,
                "2026-01-13",
                "2026-01-12",
            ),
            // Summer 08:00 CEST (06:00Z) likewise.
            (
                utc(2026, 7, 14, 6, 0),
                Session::PreOpen,
                "2026-07-14",
                "2026-07-13",
            ),
        ];
        for (at, session, r, b) in cases {
            let p = plan(at, &c, &sto()).unwrap_or_else(|| panic!("{at} unknown"));
            assert_eq!(p.session, session, "{at}");
            assert_eq!(p.reaction_date, d(r), "{at} reaction");
            assert_eq!(p.base_date, d(b), "{at} base");
        }
    }

    #[test]
    fn session_boundaries() {
        let c = cal();
        let s = |at| plan(at, &c, &sto()).unwrap().session;
        // 09:00 CEST = 07:00Z is intraday, 08:59 is pre-open.
        assert_eq!(s(utc(2026, 8, 25, 6, 59)), Session::PreOpen);
        assert_eq!(s(utc(2026, 8, 25, 7, 0)), Session::Intraday);
        // 17:30 CEST = 15:30Z is already after the close.
        assert_eq!(s(utc(2026, 8, 25, 15, 29)), Session::Intraday);
        assert_eq!(s(utc(2026, 8, 25, 15, 30)), Session::AfterClose);
        // Half day closes at 13:00 CET = 12:00Z.
        assert_eq!(s(utc(2026, 12, 24, 11, 59)), Session::Intraday);
        assert_eq!(s(utc(2026, 12, 24, 12, 0)), Session::AfterClose);
    }

    #[test]
    fn copenhagen_closes_at_1700_and_has_no_half_day() {
        // DK: Dec 24 closed all day; no half-day rows.
        let closed = ["2015-01-01", "2026-12-24", "2027-12-31"]
            .iter()
            .map(|s| d(s))
            .collect();
        let c = Calendar::from_holidays(closed, HashSet::new()).unwrap();
        let s = |at| plan(at, &c, &cph()).unwrap().session;
        // 17:15 CEST = 15:15Z: after Copenhagen's close (Stockholm's would be intraday).
        assert_eq!(s(utc(2026, 8, 25, 15, 14)), Session::AfterClose);
        assert_eq!(s(utc(2026, 8, 25, 14, 59)), Session::Intraday);
        assert_eq!(
            plan(utc(2026, 8, 25, 15, 15), &cal(), &sto())
                .unwrap()
                .session,
            Session::Intraday
        );
        // The Danish Christmas Eve is a closed day.
        assert_eq!(s(utc(2026, 12, 24, 9, 0)), Session::NonTradingDay);
    }

    #[test]
    fn a_half_day_without_a_half_day_close_is_a_full_day() {
        // The SE calendar's 2026-12-24 half day on an exchange with no early close.
        let c = cal();
        let p = |at| plan(at, &c, &cph()).unwrap().session;
        assert_eq!(p(utc(2026, 12, 24, 13, 0)), Session::Intraday); // 14:00 CET < 17:00
        assert_eq!(p(utc(2026, 12, 24, 16, 0)), Session::AfterClose);
    }

    #[test]
    fn the_release_is_converted_in_the_exchanges_own_zone() {
        let c = cal();
        // Winter 08:00 CET (07:00Z) and summer 08:00 CEST (06:00Z), Copenhagen.
        let s = |at| plan(at, &c, &cph()).unwrap().session;
        assert_eq!(s(utc(2026, 1, 13, 7, 0)), Session::PreOpen);
        assert_eq!(s(utc(2026, 7, 14, 6, 0)), Session::PreOpen);
        assert_eq!(s(utc(2026, 1, 13, 8, 0)), Session::Intraday);
        // A zone with another offset must not be read as Stockholm: same
        // instant, 08:00Z is 09:00 CET (open) but 03:00 in New York (pre-open).
        let ny = Exchange {
            timezone: "America/New_York".parse().unwrap(),
            ..cph()
        };
        assert_eq!(
            plan(utc(2026, 1, 13, 8, 0), &c, &ny).unwrap().session,
            Session::PreOpen
        );
    }

    #[test]
    fn the_calendar_is_never_guessed() {
        let c = cal();
        // Covered range is 2015-01-01..=2027-12-31 (whole years).
        assert!(plan(utc(2030, 5, 5, 6, 0), &c, &sto()).is_none());
        assert!(plan(utc(2014, 12, 30, 6, 0), &c, &sto()).is_none());
        // The reaction day would fall in 2028: unknown, although `d` is covered.
        assert!(plan(utc(2027, 12, 31, 16, 0), &c, &sto()).is_none());
        // The base day would fall before the range.
        assert!(plan(utc(2015, 1, 2, 6, 0), &c, &sto()).is_none());
        assert!(Calendar::from_holidays(HashSet::new(), HashSet::new()).is_none());
    }

    #[test]
    fn trading_days_between_counts_open_days() {
        let c = cal();
        assert_eq!(c.trading_days_between(d("2026-08-21"), d("2026-08-31")), 6);
        assert_eq!(c.trading_days_between(d("2026-04-02"), d("2026-04-07")), 1);
        assert_eq!(c.trading_days_between(d("2026-08-31"), d("2026-08-31")), 0);
    }

    #[test]
    fn queue_releases_after_a_quiet_period_or_the_max_wait() {
        use std::time::Duration;
        let t0 = Instant::now();
        let mut q = ReactionQueue::new(Duration::from_secs(2), Duration::from_secs(30));
        assert!(!q.is_due(t0));
        q.add("GOMX", "FNSE", d("2026-08-26"), t0);
        q.add("GOMX", "FNSE", d("2026-08-20"), t0 + Duration::from_secs(1));
        q.add(
            "VOLV B",
            "XSTO",
            d("2026-08-27"),
            t0 + Duration::from_secs(1),
        );
        assert!(!q.is_due(t0 + Duration::from_secs(2)));
        assert!(q.is_due(t0 + Duration::from_secs(3)));
        let mut taken = q.take();
        taken.sort();
        assert_eq!(
            taken,
            vec![
                ("GOMX".into(), "FNSE".into(), d("2026-08-20")),
                ("VOLV B".into(), "XSTO".into(), d("2026-08-27")),
            ]
        );
        assert!(!q.is_due(t0 + Duration::from_secs(100)));

        // A steady stream still flushes after max_wait.
        for s in 0..40u64 {
            q.add("GOMX", "FNSE", d("2026-08-26"), t0 + Duration::from_secs(s));
        }
        assert!(q.is_due(t0 + Duration::from_secs(31)));
    }

    // ── Database tests ──────────────────────────────────────────────────────

    const MIC: &str = "FNSE";

    /// Inserts a qualifying report (MFN, occurred, quarterly, not a correction).
    async fn report(pool: &PgPool, ticker: &str, mic: &str, at: DateTime<Utc>) -> i64 {
        sqlx::query_scalar(
            "INSERT INTO special_events (ticker, exchange_mic, event_type, occurred_at, status, \
               source, source_id, report_kind) \
             VALUES ($1, $2, 'quarterly_report', $3, 'occurred', 'mfn', $4, 'interim') RETURNING id",
        )
        .bind(ticker)
        .bind(mic)
        .bind(at)
        .bind(format!("{ticker}-{mic}-{}", at.timestamp()))
        .fetch_one(pool)
        .await
        .unwrap()
    }

    async fn bar(pool: &PgPool, ticker: &str, date: &str, close: &str, volume: i64) {
        sqlx::query(
            "INSERT INTO daily_bars (ticker, exchange_mic, date, open, high, low, close, volume, \
               currency, source, fetched_at) \
             VALUES ($1, 'FNSE', $2::date, $3::numeric, $3::numeric, $3::numeric, $3::numeric, $4, \
               'SEK', 'saxo', now())",
        )
        .bind(ticker)
        .bind(date)
        .bind(close)
        .bind(volume)
        .execute(pool)
        .await
        .unwrap();
    }

    #[derive(Debug)]
    struct ReactionRow {
        session: Option<String>,
        reaction_date: Option<String>,
        base_date: Option<String>,
        base_close: Option<String>,
        reaction_close: Option<String>,
        move_pct: Option<String>,
        volume: Option<i64>,
        status: Option<String>,
        needs_review: Option<bool>,
        computed: bool,
    }

    async fn row(pool: &PgPool, id: i64) -> ReactionRow {
        let r = sqlx::query(
            "SELECT release_session, reaction_date::text AS rd, base_date::text AS bd, \
                    base_close::text AS bc, reaction_close::text AS rc, move_pct::text AS mv, \
                    reaction_volume, data_status, needs_review, \
                    reaction_computed_at IS NOT NULL AS computed \
             FROM special_events WHERE id = $1",
        )
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap();
        ReactionRow {
            session: r.get("release_session"),
            reaction_date: r.get("rd"),
            base_date: r.get("bd"),
            base_close: r.get("bc"),
            reaction_close: r.get("rc"),
            move_pct: r.get("mv"),
            volume: r.get("reaction_volume"),
            status: r.get("data_status"),
            needs_review: r.get("needs_review"),
            computed: r.get("computed"),
        }
    }

    async fn refresh(pool: &PgPool, ticker: &str) -> Counts {
        refresh_report_reactions(pool, ticker, MIC).await.unwrap()
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn pre_open_release_on_a_tuesday(pool: PgPool) {
        let id = report(&pool, "GOMX", MIC, utc(2026, 8, 25, 6, 0)).await;
        bar(&pool, "GOMX", "2026-08-24", "100.000000", 10).await;
        bar(&pool, "GOMX", "2026-08-25", "105.000000", 500).await;
        let counts = refresh(&pool, "GOMX").await;
        assert_eq!(counts.by_status.get("ok"), Some(&1));
        let r = row(&pool, id).await;
        assert_eq!(r.session.as_deref(), Some("pre_open"));
        assert_eq!(
            (r.reaction_date.as_deref(), r.base_date.as_deref()),
            (Some("2026-08-25"), Some("2026-08-24"))
        );
        assert_eq!(r.base_close.as_deref(), Some("100.000000"));
        assert_eq!(r.reaction_close.as_deref(), Some("105.000000"));
        assert_eq!(r.move_pct.as_deref(), Some("5.0000"));
        assert_eq!(
            (r.volume, r.status.as_deref(), r.needs_review),
            (Some(500), Some("ok"), Some(false))
        );
        assert!(r.computed);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn after_close_friday_saturday_easter_and_half_day(pool: PgPool) {
        // (report instant, session, reaction day, base day, base close, reaction close, move)
        let cases = [
            (
                "FRI",
                utc(2026, 8, 28, 16, 0),
                "after_close",
                "2026-08-31",
                "2026-08-28",
                "200.000000",
                "190.000000",
                "-5.0000",
            ),
            (
                "SAT",
                utc(2026, 8, 29, 8, 0),
                "non_trading_day",
                "2026-08-31",
                "2026-08-28",
                "50.000000",
                "51.000000",
                "2.0000",
            ),
            (
                "EASTER",
                utc(2026, 4, 2, 16, 0),
                "after_close",
                "2026-04-07",
                "2026-04-02",
                "80.000000",
                "88.000000",
                "10.0000",
            ),
            (
                "HALF",
                utc(2026, 12, 24, 13, 0),
                "after_close",
                "2026-12-28",
                "2026-12-24",
                "40.000000",
                "30.000000",
                "-25.0000",
            ),
        ];
        for (ticker, at, session, r_day, b_day, base, close, mv) in cases {
            let id = report(&pool, ticker, MIC, at).await;
            bar(&pool, ticker, b_day, base, 1).await;
            bar(&pool, ticker, r_day, close, 2).await;
            refresh(&pool, ticker).await;
            let r = row(&pool, id).await;
            assert_eq!(r.session.as_deref(), Some(session), "{ticker}");
            assert_eq!(r.reaction_date.as_deref(), Some(r_day), "{ticker}");
            assert_eq!(r.base_date.as_deref(), Some(b_day), "{ticker}");
            assert_eq!(r.move_pct.as_deref(), Some(mv), "{ticker}");
            assert_eq!(r.status.as_deref(), Some("ok"), "{ticker}");
        }
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn intraday_and_dst_releases(pool: PgPool) {
        let intraday = report(&pool, "INTRA", MIC, utc(2026, 8, 25, 11, 30)).await; // 13:30 CEST
        bar(&pool, "INTRA", "2026-08-24", "10.000000", 1).await;
        bar(&pool, "INTRA", "2026-08-25", "11.000000", 1).await;
        refresh(&pool, "INTRA").await;
        let r = row(&pool, intraday).await;
        assert_eq!(
            (r.session.as_deref(), r.reaction_date.as_deref()),
            (Some("intraday"), Some("2026-08-25"))
        );

        // 08:00 local in summer (06:00Z) and in winter (07:00Z): both pre-open.
        let summer = report(&pool, "DST", MIC, utc(2026, 7, 14, 6, 0)).await;
        let winter = report(&pool, "DST", MIC, utc(2026, 1, 13, 7, 0)).await;
        for (b, r) in [("2026-07-13", "2026-07-14"), ("2026-01-12", "2026-01-13")] {
            bar(&pool, "DST", b, "10.000000", 1).await;
            bar(&pool, "DST", r, "10.500000", 1).await;
        }
        refresh(&pool, "DST").await;
        for id in [summer, winter] {
            let r = row(&pool, id).await;
            assert_eq!(r.session.as_deref(), Some("pre_open"));
            assert_eq!(r.move_pct.as_deref(), Some("5.0000"));
        }
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn data_status_when_prices_are_missing_or_unusable(pool: PgPool) {
        let at = utc(2026, 8, 25, 6, 0); // reaction 2026-08-25, base 2026-08-24
        let ids = [
            ("NOREACT", report(&pool, "NOREACT", MIC, at).await),
            ("FUTURE", report(&pool, "FUTURE", MIC, at).await),
            ("ZERO", report(&pool, "ZERO", MIC, at).await),
            ("STALE", report(&pool, "STALE", MIC, at).await),
            ("NOBASE", report(&pool, "NOBASE", MIC, at).await),
            ("NOBARS", report(&pool, "NOBARS", MIC, at).await),
        ];
        // A later bar exists but not on the reaction day.
        bar(&pool, "NOREACT", "2026-08-24", "10.000000", 1).await;
        bar(&pool, "NOREACT", "2026-08-26", "99.000000", 1).await;
        // The reaction day is after the latest bar.
        bar(&pool, "FUTURE", "2026-08-24", "10.000000", 1).await;
        // Base close is zero: never divide.
        bar(&pool, "ZERO", "2026-08-24", "0.000000", 1).await;
        bar(&pool, "ZERO", "2026-08-25", "5.000000", 1).await;
        // The last close is 11 trading days before b.
        bar(&pool, "STALE", "2026-08-10", "10.000000", 1).await;
        bar(&pool, "STALE", "2026-08-25", "11.000000", 1).await;
        // No bar on or before b (the first bar is the reaction day).
        bar(&pool, "NOBASE", "2026-08-25", "11.000000", 1).await;

        for (ticker, _) in &ids {
            refresh(&pool, ticker).await;
        }
        let expect = [
            ("NOREACT", "missing_reaction_bar", Some("10.000000"), None),
            ("FUTURE", "future", Some("10.000000"), None),
            ("ZERO", "zero_base", Some("0.000000"), Some("5.000000")),
            ("STALE", "stale_base", Some("10.000000"), Some("11.000000")),
            ("NOBASE", "missing_base_bar", None, Some("11.000000")),
            ("NOBARS", "missing_base_bar", None, None),
        ];
        for ((ticker, id), (_, status, base, react)) in ids.iter().zip(expect) {
            let r = row(&pool, *id).await;
            assert_eq!(r.status.as_deref(), Some(status), "{ticker}");
            assert_eq!(r.move_pct, None, "{ticker}: no move unless ok");
            assert_eq!(r.base_close.as_deref(), base, "{ticker} base");
            assert_eq!(r.reaction_close.as_deref(), react, "{ticker} reaction");
            assert_eq!(r.needs_review, Some(false), "{ticker}");
            // The dates are still known.
            assert_eq!(r.reaction_date.as_deref(), Some("2026-08-25"), "{ticker}");
        }
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn no_trade_on_the_base_day_uses_the_previous_close(pool: PgPool) {
        let id = report(&pool, "THIN", MIC, utc(2026, 8, 25, 6, 0)).await;
        // No bar on Monday 2026-08-24 (b); the Friday close still counts.
        bar(&pool, "THIN", "2026-08-21", "100.000000", 1).await;
        bar(&pool, "THIN", "2026-08-25", "110.000000", 1).await;
        refresh(&pool, "THIN").await;
        let r = row(&pool, id).await;
        assert_eq!(r.status.as_deref(), Some("ok"));
        assert_eq!(r.base_date.as_deref(), Some("2026-08-24"));
        assert_eq!(r.base_close.as_deref(), Some("100.000000"));
        assert_eq!(r.move_pct.as_deref(), Some("10.0000"));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_later_close_is_never_used_as_the_base(pool: PgPool) {
        let id = report(&pool, "LOOK", MIC, utc(2026, 8, 25, 6, 0)).await;
        // Only bars from the reaction day on exist: there is no base.
        bar(&pool, "LOOK", "2026-08-25", "110.000000", 1).await;
        bar(&pool, "LOOK", "2026-08-26", "120.000000", 1).await;
        refresh(&pool, "LOOK").await;
        let r = row(&pool, id).await;
        assert_eq!(r.status.as_deref(), Some("missing_base_bar"));
        assert_eq!(r.base_close, None);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn decimals_are_exact_and_big_moves_need_review(pool: PgPool) {
        let cases = [
            ("EXACT", "10.000000", "10.333333", "3.3333", false),
            ("BIG", "100.000000", "160.000000", "60.0000", true),
            ("EDGE", "100.000000", "140.000000", "40.0000", false),
            ("DROP", "100.000000", "55.000000", "-45.0000", true),
        ];
        for (ticker, base, close, mv, review) in cases {
            let id = report(&pool, ticker, MIC, utc(2026, 8, 25, 6, 0)).await;
            bar(&pool, ticker, "2026-08-24", base, 1).await;
            bar(&pool, ticker, "2026-08-25", close, 1).await;
            refresh(&pool, ticker).await;
            let r = row(&pool, id).await;
            assert_eq!(r.move_pct.as_deref(), Some(mv), "{ticker}");
            assert_eq!(r.needs_review, Some(review), "{ticker}");
        }
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn calendar_unknown_for_far_dates_and_unmapped_exchanges(pool: PgPool) {
        let far = report(&pool, "FAR", MIC, utc(2030, 5, 7, 6, 0)).await;
        // Oslo is not in `exchanges`: never guessed, even with prices.
        let unmapped = report(&pool, "NOSTOCK", "XOSL", utc(2026, 8, 25, 6, 0)).await;
        sqlx::query(
            "INSERT INTO daily_bars (ticker, exchange_mic, date, open, high, low, close, volume, \
               currency, source, fetched_at) \
             VALUES ('NOSTOCK','XOSL','2026-08-24',1,1,1,1,1,'NOK','saxo',now()), \
                    ('NOSTOCK','XOSL','2026-08-25',2,2,2,2,1,'NOK','saxo',now())",
        )
        .execute(&pool)
        .await
        .unwrap();
        refresh(&pool, "FAR").await;
        refresh_report_reactions(&pool, "NOSTOCK", "XOSL")
            .await
            .unwrap();
        for id in [far, unmapped] {
            let r = row(&pool, id).await;
            assert_eq!(r.status.as_deref(), Some("calendar_unknown"));
            assert_eq!((r.session, r.reaction_date, r.move_pct), (None, None, None));
        }
    }

    // ── Denmark (XCSE/FNDK): the `exchanges` row and DK holidays decide ─────

    /// DK closures for 2026 as `nexus calendar sync --country DK` would store
    /// them (no half days), with 2015 and 2027 as range markers.
    async fn seed_denmark(pool: &PgPool) {
        sqlx::query(
            "INSERT INTO trading_holidays (country, date, status, note) VALUES \
             ('DK','2015-01-01','closed','New Year'), ('DK','2026-01-01','closed','New Year'), \
             ('DK','2026-04-02','closed','Maundy Thursday'), ('DK','2026-04-03','closed','Good Friday'), \
             ('DK','2026-04-06','closed','Easter Monday'), ('DK','2026-05-14','closed','Ascension Day'), \
             ('DK','2026-05-15','closed','Friday after Ascension'), ('DK','2026-05-25','closed','Whit Monday'), \
             ('DK','2026-06-05','closed','Constitution Day'), ('DK','2026-12-24','closed','Christmas Eve'), \
             ('DK','2026-12-25','closed','Christmas Day'), ('DK','2026-12-31','closed','New Year''s Eve'), \
             ('DK','2027-12-31','closed','New Year''s Eve')",
        )
        .execute(pool)
        .await
        .unwrap();
    }

    async fn bar_of(pool: &PgPool, ticker: &str, mic: &str, date: &str, close: &str) {
        sqlx::query(
            "INSERT INTO daily_bars (ticker, exchange_mic, date, open, high, low, close, volume, \
               currency, source, fetched_at) \
             VALUES ($1, $2, $3::date, $4::numeric, $4::numeric, $4::numeric, $4::numeric, 1, \
               'DKK', 'saxo', now())",
        )
        .bind(ticker)
        .bind(mic)
        .bind(date)
        .bind(close)
        .execute(pool)
        .await
        .unwrap();
    }

    /// Stores a report at `at` with a 100 → 110 move between `b` and `r`, refreshes,
    /// and asserts the session and both days.
    async fn expect(
        pool: &PgPool,
        (ticker, mic): (&str, &str),
        at: DateTime<Utc>,
        (session, r, b): (&str, &str, &str),
    ) {
        let id = report(pool, ticker, mic, at).await;
        bar_of(pool, ticker, mic, b, "100.000000").await;
        bar_of(pool, ticker, mic, r, "110.000000").await;
        refresh_report_reactions(pool, ticker, mic).await.unwrap();
        let got = row(pool, id).await;
        assert_eq!(got.session.as_deref(), Some(session), "{ticker}/{mic} {at}");
        assert_eq!(
            got.reaction_date.as_deref(),
            Some(r),
            "{ticker}/{mic} {at} r"
        );
        assert_eq!(got.base_date.as_deref(), Some(b), "{ticker}/{mic} {at} b");
        assert_eq!(
            got.move_pct.as_deref(),
            Some("10.0000"),
            "{ticker}/{mic} {at}"
        );
        assert_eq!(got.status.as_deref(), Some("ok"), "{ticker}/{mic} {at}");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn novo_reports_after_the_close_and_the_move_is_taken_the_next_day(pool: PgPool) {
        seed_denmark(&pool).await;
        // Q2 2026: 16:37:02Z = 18:37 Copenhagen.
        expect(
            &pool,
            ("NOVO-B", "XCSE"),
            Utc.with_ymd_and_hms(2026, 8, 4, 16, 37, 2).unwrap(),
            ("after_close", "2026-08-05", "2026-08-04"),
        )
        .await;
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_1715_copenhagen_release_is_after_the_close_not_intraday(pool: PgPool) {
        seed_denmark(&pool).await;
        // 17:15 CEST = 15:15Z. Stockholm's 17:30 close would call it intraday.
        let at = utc(2026, 8, 25, 15, 15);
        expect(
            &pool,
            ("DK1", "XCSE"),
            at,
            ("after_close", "2026-08-26", "2026-08-25"),
        )
        .await;
        expect(
            &pool,
            ("DK2", "FNDK"),
            at,
            ("after_close", "2026-08-26", "2026-08-25"),
        )
        .await;
        expect(
            &pool,
            ("SE1", "FNSE"),
            at,
            ("intraday", "2026-08-25", "2026-08-24"),
        )
        .await;
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn the_friday_after_ascension_is_closed_in_copenhagen_only(pool: PgPool) {
        seed_denmark(&pool).await;
        let at = utc(2026, 5, 15, 6, 0); // 08:00 CEST
                                         // Closed: next trading day Monday the 18th, base Wednesday the 13th
                                         // (Thursday the 14th is Ascension Day).
        expect(
            &pool,
            ("DK", "XCSE"),
            at,
            ("non_trading_day", "2026-05-18", "2026-05-13"),
        )
        .await;
        // Stockholm trades that day.
        expect(
            &pool,
            ("SE", "FNSE"),
            at,
            ("pre_open", "2026-05-15", "2026-05-13"),
        )
        .await;
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn constitution_day_is_closed_in_denmark_but_not_in_sweden(pool: PgPool) {
        seed_denmark(&pool).await;
        let at = utc(2026, 6, 5, 6, 0);
        expect(
            &pool,
            ("DK", "XCSE"),
            at,
            ("non_trading_day", "2026-06-08", "2026-06-04"),
        )
        .await;
        expect(
            &pool,
            ("SE", "XSTO"),
            at,
            ("pre_open", "2026-06-05", "2026-06-04"),
        )
        .await;
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn midsummer_eve_is_closed_in_sweden_but_not_in_denmark(pool: PgPool) {
        seed_denmark(&pool).await;
        let at = utc(2026, 6, 19, 6, 0);
        expect(
            &pool,
            ("SE", "FNSE"),
            at,
            ("non_trading_day", "2026-06-22", "2026-06-18"),
        )
        .await;
        expect(
            &pool,
            ("DK", "XCSE"),
            at,
            ("pre_open", "2026-06-19", "2026-06-18"),
        )
        .await;
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn christmas_eve_is_closed_all_day_in_copenhagen_and_a_half_day_in_stockholm(
        pool: PgPool,
    ) {
        seed_denmark(&pool).await;
        let at = utc(2026, 12, 24, 9, 0); // 10:00 CET
        expect(
            &pool,
            ("DK", "XCSE"),
            at,
            ("non_trading_day", "2026-12-28", "2026-12-23"),
        )
        .await;
        expect(
            &pool,
            ("SE", "FNSE"),
            at,
            ("intraday", "2026-12-24", "2026-12-23"),
        )
        .await;
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn copenhagen_is_pre_open_at_0800_local_in_summer_and_winter(pool: PgPool) {
        seed_denmark(&pool).await;
        expect(
            &pool,
            ("SUM", "XCSE"),
            utc(2026, 7, 14, 6, 0),
            ("pre_open", "2026-07-14", "2026-07-13"),
        )
        .await;
        // 2026-01-13 07:00Z = 08:00 CET. 2026-01-01 is the only Danish holiday in January.
        expect(
            &pool,
            ("WIN", "XCSE"),
            utc(2026, 1, 13, 7, 0),
            ("pre_open", "2026-01-13", "2026-01-12"),
        )
        .await;
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn exchanges_and_the_alpha_country_map_agree(pool: PgPool) {
        let rows = sqlx::query("SELECT mic, country FROM exchanges")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(rows.len(), 6, "FNSE, XSTO, XCSE, FNDK, XHEL, FNFI");
        for r in rows {
            let (mic, country): (String, String) = (r.get("mic"), r.get("country"));
            assert_eq!(
                alpha::calendar::country_for_exchange(&mic),
                Some(country.as_str()),
                "{mic}"
            );
        }
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn finnish_exchanges_are_helsinki_10_to_1830_in_eur(pool: PgPool) {
        let mut conn = pool.acquire().await.unwrap();
        for mic in ["XHEL", "FNFI"] {
            let ex = Exchange::load(&mut conn, mic).await.unwrap().unwrap();
            assert_eq!(ex.country, "FI", "{mic}");
            assert_eq!(ex.timezone, chrono_tz::Europe::Helsinki, "{mic}");
            assert_eq!(ex.open, chrono::NaiveTime::from_hms_opt(10, 0, 0).unwrap());
            assert_eq!(
                ex.close,
                chrono::NaiveTime::from_hms_opt(18, 30, 0).unwrap()
            );
            assert_eq!(ex.half_day_close, None);
        }
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_bad_time_zone_is_calendar_unknown(pool: PgPool) {
        sqlx::query(
            "INSERT INTO exchanges (mic, country, timezone, open_local, close_local, currency) \
             VALUES ('XBAD', 'SE', 'Mars/Olympus', '09:00', '17:00', 'SEK')",
        )
        .execute(&pool)
        .await
        .unwrap();
        let mut conn = pool.acquire().await.unwrap();
        assert_eq!(Exchange::load(&mut conn, "XBAD").await.unwrap(), None);
        let xcse = Exchange::load(&mut conn, "XCSE").await.unwrap().unwrap();
        assert_eq!(
            (xcse.country.as_str(), xcse.close, xcse.half_day_close),
            ("DK", hm(17, 0), None)
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn corrections_scheduled_rows_and_other_sources_get_no_values(pool: PgPool) {
        bar(&pool, "GOMX", "2026-08-24", "10.000000", 1).await;
        bar(&pool, "GOMX", "2026-08-25", "11.000000", 1).await;
        let id = report(&pool, "GOMX", MIC, utc(2026, 8, 25, 6, 0)).await;
        let correction: i64 = sqlx::query_scalar(
            "INSERT INTO special_events (ticker, exchange_mic, event_type, occurred_at, status, \
               source, source_id, is_correction) \
             VALUES ('GOMX','FNSE','report_correction','2026-08-25T07:00:00Z','occurred','mfn','c1',true) \
             RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let scheduled: i64 = sqlx::query_scalar(
            "INSERT INTO special_events (ticker, exchange_mic, event_type, occurred_at, status, \
               source, report_kind, expected_date, calendar_label) \
             VALUES ('GOMX','FNSE','quarterly_report','2026-11-05T07:00:00Z','scheduled','mfn', \
               'interim','2026-11-05','Delårsrapport') RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let legacy: i64 = sqlx::query_scalar(
            "INSERT INTO special_events (ticker, event_type, occurred_at) \
             VALUES ('GOMX','earnings','2026-08-25T06:00:00Z') RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        refresh(&pool, "GOMX").await;
        assert_eq!(row(&pool, id).await.status.as_deref(), Some("ok"));
        for other in [correction, scheduled, legacy] {
            let r = row(&pool, other).await;
            assert_eq!(
                (r.status, r.session, r.move_pct, r.needs_review),
                (None, None, None, None)
            );
            assert!(!r.computed);
        }

        // The report is later re-typed as a same-day re-send: its values are cleared.
        sqlx::query(
            "UPDATE special_events SET is_correction = true, event_type = 'report_correction' \
             WHERE id = $1",
        )
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
        let counts = refresh(&pool, "GOMX").await;
        assert_eq!(counts.cleared, 1);
        let r = row(&pool, id).await;
        assert_eq!(
            (
                r.status,
                r.session,
                r.move_pct,
                r.reaction_date,
                r.needs_review
            ),
            (None, None, None, None, None)
        );
        assert!(!r.computed);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_late_bar_turns_missing_reaction_into_ok(pool: PgPool) {
        let id = report(&pool, "LATE", MIC, utc(2026, 8, 25, 6, 0)).await;
        bar(&pool, "LATE", "2026-08-24", "10.000000", 1).await;
        refresh(&pool, "LATE").await;
        assert_eq!(row(&pool, id).await.status.as_deref(), Some("future"));

        bar(&pool, "LATE", "2026-08-26", "12.000000", 1).await; // a later day arrives first
        refresh(&pool, "LATE").await;
        assert_eq!(
            row(&pool, id).await.status.as_deref(),
            Some("missing_reaction_bar")
        );

        bar(&pool, "LATE", "2026-08-25", "10.500000", 1).await; // the missing day arrives
        let counts = refresh_after_bars(&pool, "LATE", MIC, d("2026-08-25"))
            .await
            .unwrap();
        assert_eq!(counts.computed, 1);
        let r = row(&pool, id).await;
        assert_eq!(
            (r.status.as_deref(), r.move_pct.as_deref()),
            (Some("ok"), Some("5.0000"))
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn refresh_is_idempotent_and_after_bars_only_touches_what_could_change(pool: PgPool) {
        let old = report(&pool, "OLD", MIC, utc(2026, 3, 3, 7, 0)).await; // reaction 2026-03-03
        bar(&pool, "OLD", "2026-03-02", "10.000000", 1).await;
        bar(&pool, "OLD", "2026-03-03", "11.000000", 1).await;
        refresh(&pool, "OLD").await;
        let first = row(&pool, old).await;
        refresh(&pool, "OLD").await;
        let second = row(&pool, old).await;
        assert_eq!(format!("{first:?}"), format!("{second:?}"));

        let stamp = |pool: PgPool| async move {
            sqlx::query_scalar::<_, DateTime<Utc>>(
                "SELECT reaction_computed_at FROM special_events WHERE id = $1",
            )
            .bind(old)
            .fetch_one(&pool)
            .await
            .unwrap()
        };
        let before = stamp(pool.clone()).await;
        // New bars far after the reaction day: the ok report is left alone.
        let c = refresh_after_bars(&pool, "OLD", MIC, d("2026-08-01"))
            .await
            .unwrap();
        assert_eq!(c.computed, 0);
        assert_eq!(stamp(pool.clone()).await, before);
        // A correction within 10 days of the reaction day recomputes it.
        let c = refresh_after_bars(&pool, "OLD", MIC, d("2026-03-10"))
            .await
            .unwrap();
        assert_eq!(c.computed, 1);
        assert!(stamp(pool.clone()).await > before);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn storing_a_release_computes_its_reaction(pool: PgPool) {
        use model::generated::SpecialEvent;
        bar(&pool, "GOMX", "2026-08-25", "10.000000", 1).await;
        bar(&pool, "GOMX", "2026-08-26", "9.000000", 1).await;
        // Released 2026-08-26 08:00 CEST (pre-open) through the real consumer path.
        let event = SpecialEvent {
            ticker: "GOMX".into(),
            exchange_mic: "FNSE".into(),
            event_type: "quarterly_report".into(),
            occurred_at_unix_secs: utc(2026, 8, 26, 6, 0).timestamp(),
            description: "Q2 report".into(),
            source: crate::reports::SOURCE_MFN.into(),
            source_id: "mfn-1".into(),
            ..Default::default()
        };
        crate::reports::handle(&pool, &crate::reports::parse(&event).unwrap())
            .await
            .unwrap();
        let (status, mv): (Option<String>, Option<String>) = sqlx::query_as(
            "SELECT data_status, move_pct::text FROM special_events WHERE source_id = 'mfn-1'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            (status.as_deref(), mv.as_deref()),
            (Some("ok"), Some("-10.0000"))
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn the_startup_sweep_computes_every_company(pool: PgPool) {
        for t in ["AAA", "BBB"] {
            report(&pool, t, MIC, utc(2026, 8, 25, 6, 0)).await;
            bar(&pool, t, "2026-08-24", "10.000000", 1).await;
            bar(&pool, t, "2026-08-25", "11.000000", 1).await;
        }
        let counts = refresh_all(&pool).await.unwrap();
        assert_eq!((counts.computed, counts.by_status.get("ok")), (2, Some(&2)));
    }
}
