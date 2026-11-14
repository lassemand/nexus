//! Simulated trades and their observations in the analyst's database.
//!
//! One `sim_trades` row per traded decision, `sim_observations` rows for the bid
//! at each horizon. Every state change bumps `version`, so the consumer of
//! `analyst.trades` can keep the newest message. Prices are exact decimals.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use model::generated::{AnalystTrade, TradeObservation};
use rust_decimal::Decimal;
use sqlx::{FromRow, Row};

use crate::pnl::Pnl;
use crate::quotes::Quote;
use crate::store::Store;

/// Where a simulated trade is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TradeStatus {
    /// Waiting for the entry quote.
    PendingEntry,
    /// Entered, waiting for the exit.
    Open,
    /// Exited with a result.
    Closed,
    /// No acceptable entry quote within `ENTRY_MAX_WAIT`.
    NoEntryQuote,
    /// No acceptable exit quote.
    NoExitQuote,
    /// The exit target passed while the analyst was down.
    ExitMissed,
    /// The exchange or the year is not in the calendar: no quotes.
    CalendarUnknown,
    /// Entry and exit were quoted in different currencies: no result.
    CurrencyMismatch,
    /// The entry is not before the session cutoff, or its price is not usable.
    InvalidEntry,
}

impl TradeStatus {
    /// The value stored in `sim_trades.status`.
    pub fn as_str(self) -> &'static str {
        match self {
            TradeStatus::PendingEntry => "pending_entry",
            TradeStatus::Open => "open",
            TradeStatus::Closed => "closed",
            TradeStatus::NoEntryQuote => "no_entry_quote",
            TradeStatus::NoExitQuote => "no_exit_quote",
            TradeStatus::ExitMissed => "exit_missed",
            TradeStatus::CalendarUnknown => "calendar_unknown",
            TradeStatus::CurrencyMismatch => "currency_mismatch",
            TradeStatus::InvalidEntry => "invalid_entry",
        }
    }

    /// Reads a stored value.
    pub fn parse(text: &str) -> Option<TradeStatus> {
        Some(match text {
            "pending_entry" => TradeStatus::PendingEntry,
            "open" => TradeStatus::Open,
            "closed" => TradeStatus::Closed,
            "no_entry_quote" => TradeStatus::NoEntryQuote,
            "no_exit_quote" => TradeStatus::NoExitQuote,
            "exit_missed" => TradeStatus::ExitMissed,
            "calendar_unknown" => TradeStatus::CalendarUnknown,
            "currency_mismatch" => TradeStatus::CurrencyMismatch,
            "invalid_entry" => TradeStatus::InvalidEntry,
            _ => return None,
        })
    }

    /// Whether the trade still has quotes to take.
    pub fn is_open(self) -> bool {
        matches!(self, TradeStatus::PendingEntry | TradeStatus::Open)
    }
}

/// How one observation went.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ObservationStatus {
    /// A quote was taken.
    Ok,
    /// The target passed over 2 s before a restart found it.
    Missed,
    /// The horizon is after the session cutoff.
    BeyondClose,
    /// No usable quote was had.
    NoQuote,
}

impl ObservationStatus {
    /// The value stored in `sim_observations.status`.
    pub fn as_str(self) -> &'static str {
        match self {
            ObservationStatus::Ok => "ok",
            ObservationStatus::Missed => "missed",
            ObservationStatus::BeyondClose => "beyond_close",
            ObservationStatus::NoQuote => "no_quote",
        }
    }
}

/// A trade to create from a decision.
#[derive(Debug, Clone)]
pub struct NewTrade {
    /// The decision this trade simulates.
    pub decision_id: i64,
    /// The company's ticker.
    pub ticker: String,
    /// The company's exchange.
    pub exchange_mic: String,
    /// `buy`, `no_buy`, `abstain` or `skipped_no_kernel`.
    pub decision: String,
    /// When MFN published the release.
    pub published_at: DateTime<Utc>,
    /// The hold in trading minutes.
    pub hold_minutes: Decimal,
    /// Where the entry quote is first asked for; `None` when the calendar is unknown.
    pub entry_target_at: Option<DateTime<Utc>>,
    /// `PendingEntry`, or `CalendarUnknown` when there is nothing to quote.
    pub status: TradeStatus,
    /// The release trace (32 lowercase hex).
    pub trace_id: Option<String>,
    /// The W3C `traceparent` of the decision's span.
    pub release_traceparent: Option<String>,
}

/// The accepted entry quote.
#[derive(Debug, Clone)]
pub struct Entry {
    /// When our request for it was answered.
    pub at: DateTime<Utc>,
    /// The ask paid.
    pub ask: Decimal,
    /// The bid at that moment, when quoted.
    pub bid: Option<Decimal>,
    /// Saxo's timestamp of the quote.
    pub saxo_at: Option<String>,
    /// The quote's currency.
    pub currency: Option<String>,
    /// `at` minus MFN's publish time, in milliseconds.
    pub release_to_entry_ms: i64,
    /// When the position is closed.
    pub exit_target_at: Option<DateTime<Utc>>,
}

/// The exit of a trade.
#[derive(Debug, Clone)]
pub struct Exit {
    /// When the exit quote was fetched.
    pub at: Option<DateTime<Utc>>,
    /// The bid received.
    pub bid: Option<Decimal>,
    /// The ask at that moment.
    pub ask: Option<Decimal>,
    /// The result, when there is one.
    pub pnl: Option<Pnl>,
    /// `Closed`, or why not.
    pub status: TradeStatus,
}

/// One observation to store.
#[derive(Debug, Clone)]
pub struct NewObservation {
    /// `1m`, `close`, `hold`, ...
    pub horizon: String,
    /// The target instant.
    pub scheduled_at: DateTime<Utc>,
    /// When the quote was fetched, when it was.
    pub fetched_at: Option<DateTime<Utc>>,
    /// The quote, when one was had.
    pub quote: Option<Quote>,
    /// More than 2 s after the target.
    pub late: bool,
    /// How it went.
    pub status: ObservationStatus,
}

/// A trade as stored.
#[derive(Debug, Clone, FromRow)]
pub struct StoredTrade {
    /// The row id.
    pub id: i64,
    /// The decision simulated.
    pub decision_id: i64,
    /// The company's ticker.
    pub ticker: String,
    /// The company's exchange.
    pub exchange_mic: String,
    /// `buy`, `no_buy`, `abstain` or `skipped_no_kernel`.
    pub decision: String,
    /// True for every decision but `buy`.
    pub shadow: bool,
    /// When MFN published the release.
    pub published_at: DateTime<Utc>,
    /// The hold in trading minutes.
    pub hold_minutes: Decimal,
    /// Where the entry quote is first asked for.
    pub entry_target_at: Option<DateTime<Utc>>,
    /// When the entry quote was fetched.
    pub entry_at: Option<DateTime<Utc>>,
    /// The ask paid.
    pub entry_ask: Option<Decimal>,
    /// The bid at entry.
    pub entry_bid: Option<Decimal>,
    /// Saxo's timestamp of the entry quote.
    pub entry_saxo_at: Option<String>,
    /// When the position is closed.
    pub exit_target_at: Option<DateTime<Utc>>,
    /// When the exit quote was fetched.
    pub exit_at: Option<DateTime<Utc>>,
    /// The bid received.
    pub exit_bid: Option<Decimal>,
    /// The ask at exit.
    pub exit_ask: Option<Decimal>,
    /// `exit_bid - entry_ask`.
    pub pnl_per_share: Option<Decimal>,
    /// The result in percent, 4 decimals.
    pub pnl_pct: Option<Decimal>,
    /// The quote currency.
    pub currency: Option<String>,
    /// `entry_at - published_at` in milliseconds.
    pub release_to_entry_ms: Option<i64>,
    /// Where the trade is.
    pub status: String,
    /// Increases with every state change.
    pub version: i64,
    /// The release trace.
    pub trace_id: Option<String>,
    /// The W3C `traceparent` of the decision's span.
    pub release_traceparent: Option<String>,
    /// The trade's own trace.
    pub trade_trace_id: Option<String>,
}

impl StoredTrade {
    /// The parsed status; an unknown stored value reads as `PendingEntry`'s opposite,
    /// `InvalidEntry`, so it is never traded further.
    pub fn status(&self) -> TradeStatus {
        TradeStatus::parse(&self.status).unwrap_or(TradeStatus::InvalidEntry)
    }
}

/// An observation as stored.
#[derive(Debug, Clone, FromRow)]
pub struct StoredObservation {
    /// The trade.
    pub trade_id: i64,
    /// `1m`, `close`, `hold`, ...
    pub horizon: String,
    /// The target instant.
    pub scheduled_at: DateTime<Utc>,
    /// When the quote was fetched.
    pub fetched_at: Option<DateTime<Utc>>,
    /// The bid.
    pub bid: Option<Decimal>,
    /// The ask.
    pub ask: Option<Decimal>,
    /// Saxo's bid price type.
    pub bid_type: Option<String>,
    /// Saxo's ask price type.
    pub ask_type: Option<String>,
    /// Minutes the quote was delayed.
    pub delayed_minutes: Option<i64>,
    /// More than 2 s after the target.
    pub late: bool,
    /// `ok`, `missed`, `beyond_close` or `no_quote`.
    pub status: String,
}

const TRADE_COLUMNS: &str = "id, decision_id, ticker, exchange_mic, decision, shadow, published_at, hold_minutes, \
     entry_target_at, entry_at, entry_ask, entry_bid, entry_saxo_at, exit_target_at, exit_at, exit_bid, exit_ask, \
     pnl_per_share, pnl_pct, currency, release_to_entry_ms, status, version, trace_id, release_traceparent, \
     trade_trace_id";

const OBSERVATION_COLUMNS: &str =
    "trade_id, horizon, scheduled_at, fetched_at, bid, ask, bid_type, ask_type, delayed_minutes, late, status";

impl Store {
    /// Creates the trade of a decision. `None` when the decision already has one
    /// (a redelivery), so nothing is scheduled twice.
    pub async fn create_trade(&self, new: &NewTrade) -> Result<Option<StoredTrade>, sqlx::Error> {
        sqlx::query_as(&format!(
            "INSERT INTO sim_trades (decision_id, ticker, exchange_mic, decision, shadow, published_at, \
                 hold_minutes, entry_target_at, status, trace_id, release_traceparent) \
             VALUES ($1, $2, $3, $4, $4 <> 'buy', $5, $6, $7, $8, $9, $10) \
             ON CONFLICT (decision_id) DO NOTHING RETURNING {TRADE_COLUMNS}"
        ))
        .bind(new.decision_id)
        .bind(&new.ticker)
        .bind(&new.exchange_mic)
        .bind(&new.decision)
        .bind(new.published_at)
        .bind(new.hold_minutes)
        .bind(new.entry_target_at)
        .bind(new.status.as_str())
        .bind(&new.trace_id)
        .bind(&new.release_traceparent)
        .fetch_optional(&self.pool)
        .await
    }

    /// One trade by id.
    pub async fn trade(&self, id: i64) -> Result<Option<StoredTrade>, sqlx::Error> {
        sqlx::query_as(&format!(
            "SELECT {TRADE_COLUMNS} FROM sim_trades WHERE id = $1"
        ))
        .bind(id)
        .fetch_optional(&self.pool)
        .await
    }

    /// The observations of a trade, in the order they were scheduled.
    pub async fn observations(&self, trade_id: i64) -> Result<Vec<StoredObservation>, sqlx::Error> {
        sqlx::query_as(&format!(
            "SELECT {OBSERVATION_COLUMNS} FROM sim_observations WHERE trade_id = $1 \
             ORDER BY scheduled_at, horizon"
        ))
        .bind(trade_id)
        .fetch_all(&self.pool)
        .await
    }

    /// The trades that still have quotes to take (`pending_entry` and `open`),
    /// oldest first, for rescheduling at startup.
    pub async fn open_trades(&self) -> Result<Vec<StoredTrade>, sqlx::Error> {
        sqlx::query_as(&format!(
            "SELECT {TRADE_COLUMNS} FROM sim_trades WHERE status IN ('pending_entry', 'open') ORDER BY id"
        ))
        .fetch_all(&self.pool)
        .await
    }

    /// Records the accepted entry quote and opens the trade.
    pub async fn record_entry(&self, id: i64, entry: &Entry) -> Result<StoredTrade, sqlx::Error> {
        sqlx::query_as(&format!(
            "UPDATE sim_trades SET status = 'open', entry_at = $2, entry_ask = $3, entry_bid = $4, \
                 entry_saxo_at = $5, currency = $6, release_to_entry_ms = $7, exit_target_at = $8, \
                 version = version + 1, updated_at = now() \
             WHERE id = $1 RETURNING {TRADE_COLUMNS}"
        ))
        .bind(id)
        .bind(entry.at)
        .bind(entry.ask)
        .bind(entry.bid)
        .bind(&entry.saxo_at)
        .bind(&entry.currency)
        .bind(entry.release_to_entry_ms)
        .bind(entry.exit_target_at)
        .fetch_one(&self.pool)
        .await
    }

    /// Ends a trade without an exit quote (or marks it invalid) with `status`.
    pub async fn set_trade_status(
        &self,
        id: i64,
        status: TradeStatus,
    ) -> Result<StoredTrade, sqlx::Error> {
        sqlx::query_as(&format!(
            "UPDATE sim_trades SET status = $2, version = version + 1, updated_at = now() \
             WHERE id = $1 RETURNING {TRADE_COLUMNS}"
        ))
        .bind(id)
        .bind(status.as_str())
        .fetch_one(&self.pool)
        .await
    }

    /// Records the exit and its result.
    pub async fn record_exit(&self, id: i64, exit: &Exit) -> Result<StoredTrade, sqlx::Error> {
        sqlx::query_as(&format!(
            "UPDATE sim_trades SET status = $2, exit_at = $3, exit_bid = $4, exit_ask = $5, \
                 pnl_per_share = $6, pnl_pct = $7, version = version + 1, updated_at = now() \
             WHERE id = $1 RETURNING {TRADE_COLUMNS}"
        ))
        .bind(id)
        .bind(exit.status.as_str())
        .bind(exit.at)
        .bind(exit.bid)
        .bind(exit.ask)
        .bind(exit.pnl.map(|p| p.per_share))
        .bind(exit.pnl.map(|p| p.pct))
        .fetch_one(&self.pool)
        .await
    }

    /// Stores the trace id of the trade's own trace (a resumed trade gets a new one).
    pub async fn set_trade_trace(&self, id: i64, trade_trace_id: &str) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE sim_trades SET trade_trace_id = $2, updated_at = now() WHERE id = $1")
            .bind(id)
            .bind(trade_trace_id)
            .execute(&self.pool)
            .await
            .map(|_| ())
    }

    /// Stores an observation (replacing one of the same horizon) and bumps the
    /// trade's version, in one transaction. Returns the trade as it is now.
    pub async fn record_observation(
        &self,
        trade_id: i64,
        observation: &NewObservation,
    ) -> Result<StoredTrade, sqlx::Error> {
        let quote = observation.quote.as_ref();
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO sim_observations (trade_id, horizon, scheduled_at, fetched_at, bid, ask, \
                 bid_type, ask_type, delayed_minutes, late, status) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11) \
             ON CONFLICT (trade_id, horizon) DO UPDATE SET scheduled_at = EXCLUDED.scheduled_at, \
                 fetched_at = EXCLUDED.fetched_at, bid = EXCLUDED.bid, ask = EXCLUDED.ask, \
                 bid_type = EXCLUDED.bid_type, ask_type = EXCLUDED.ask_type, \
                 delayed_minutes = EXCLUDED.delayed_minutes, late = EXCLUDED.late, status = EXCLUDED.status",
        )
        .bind(trade_id)
        .bind(&observation.horizon)
        .bind(observation.scheduled_at)
        .bind(observation.fetched_at)
        .bind(quote.and_then(|q| q.bid))
        .bind(quote.and_then(|q| q.ask))
        .bind(quote.and_then(|q| q.bid_type.clone()))
        .bind(quote.and_then(|q| q.ask_type.clone()))
        .bind(quote.and_then(|q| q.delayed_minutes))
        .bind(observation.late)
        .bind(observation.status.as_str())
        .execute(&mut *tx)
        .await?;
        let trade = sqlx::query_as(&format!(
            "UPDATE sim_trades SET version = version + 1, updated_at = now() \
             WHERE id = $1 RETURNING {TRADE_COLUMNS}"
        ))
        .bind(trade_id)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(trade)
    }

    /// The number of rows in `sim_trades` with each status, for tests and audits.
    pub async fn trade_count(&self, status: TradeStatus) -> Result<i64, sqlx::Error> {
        let row = sqlx::query("SELECT COUNT(*) AS n FROM sim_trades WHERE status = $1")
            .bind(status.as_str())
            .fetch_one(&self.pool)
            .await?;
        row.try_get("n")
    }
}

fn millis(at: Option<DateTime<Utc>>) -> i64 {
    at.map(|t| t.timestamp_millis()).unwrap_or(0)
}

fn text(value: Option<Decimal>) -> String {
    value.map(|d| d.to_string()).unwrap_or_default()
}

/// The `analyst.trades` message for the current state of `trade`: the full
/// state and every observation so far.
pub fn to_proto(trade: &StoredTrade, observations: &[StoredObservation]) -> AnalystTrade {
    AnalystTrade {
        trade_id: trade.id,
        decision_id: trade.decision_id,
        ticker: trade.ticker.clone(),
        exchange_mic: trade.exchange_mic.clone(),
        decision: trade.decision.clone(),
        shadow: trade.shadow,
        status: trade.status.clone(),
        version: trade.version,
        hold_minutes: trade.hold_minutes.normalize().to_string(),
        published_at_unix_millis: trade.published_at.timestamp_millis(),
        entry_target_at_unix_millis: millis(trade.entry_target_at),
        entry_at_unix_millis: millis(trade.entry_at),
        entry_ask: text(trade.entry_ask),
        entry_bid: text(trade.entry_bid),
        exit_target_at_unix_millis: millis(trade.exit_target_at),
        exit_at_unix_millis: millis(trade.exit_at),
        exit_bid: text(trade.exit_bid),
        exit_ask: text(trade.exit_ask),
        pnl_per_share: text(trade.pnl_per_share),
        pnl_pct: text(trade.pnl_pct),
        currency: trade.currency.clone().unwrap_or_default(),
        release_to_entry_ms: trade.release_to_entry_ms.unwrap_or(0),
        trace_id: trade.trace_id.clone().unwrap_or_default(),
        trade_trace_id: trade.trade_trace_id.clone().unwrap_or_default(),
        observations: observations
            .iter()
            .map(|o| TradeObservation {
                horizon: o.horizon.clone(),
                scheduled_at_unix_millis: o.scheduled_at.timestamp_millis(),
                fetched_at_unix_millis: millis(o.fetched_at),
                bid: text(o.bid),
                ask: text(o.ask),
                bid_type: o.bid_type.clone().unwrap_or_default(),
                ask_type: o.ask_type.clone().unwrap_or_default(),
                delayed_minutes: o.delayed_minutes.unwrap_or(-1),
                late: o.late,
                status: o.status.clone(),
            })
            .collect(),
    }
}

/// What the trade runner needs of the database, so its timers can be tested
/// without one. [`Store`] implements it.
#[async_trait]
pub trait TradeStore: Send + Sync {
    /// See [`Store::create_trade`].
    async fn create_trade(&self, new: &NewTrade) -> Result<Option<StoredTrade>, sqlx::Error>;
    /// See [`Store::open_trades`].
    async fn open_trades(&self) -> Result<Vec<StoredTrade>, sqlx::Error>;
    /// See [`Store::observations`].
    async fn observations(&self, trade_id: i64) -> Result<Vec<StoredObservation>, sqlx::Error>;
    /// See [`Store::record_entry`].
    async fn record_entry(&self, id: i64, entry: &Entry) -> Result<StoredTrade, sqlx::Error>;
    /// See [`Store::record_observation`].
    async fn record_observation(
        &self,
        trade_id: i64,
        observation: &NewObservation,
    ) -> Result<StoredTrade, sqlx::Error>;
    /// See [`Store::record_exit`].
    async fn record_exit(&self, id: i64, exit: &Exit) -> Result<StoredTrade, sqlx::Error>;
    /// See [`Store::set_trade_status`].
    async fn set_trade_status(
        &self,
        id: i64,
        status: TradeStatus,
    ) -> Result<StoredTrade, sqlx::Error>;
}

#[async_trait]
impl TradeStore for Store {
    async fn create_trade(&self, new: &NewTrade) -> Result<Option<StoredTrade>, sqlx::Error> {
        Store::create_trade(self, new).await
    }
    async fn open_trades(&self) -> Result<Vec<StoredTrade>, sqlx::Error> {
        Store::open_trades(self).await
    }
    async fn observations(&self, trade_id: i64) -> Result<Vec<StoredObservation>, sqlx::Error> {
        Store::observations(self, trade_id).await
    }
    async fn record_entry(&self, id: i64, entry: &Entry) -> Result<StoredTrade, sqlx::Error> {
        Store::record_entry(self, id, entry).await
    }
    async fn record_observation(
        &self,
        trade_id: i64,
        observation: &NewObservation,
    ) -> Result<StoredTrade, sqlx::Error> {
        Store::record_observation(self, trade_id, observation).await
    }
    async fn record_exit(&self, id: i64, exit: &Exit) -> Result<StoredTrade, sqlx::Error> {
        Store::record_exit(self, id, exit).await
    }
    async fn set_trade_status(
        &self,
        id: i64,
        status: TradeStatus,
    ) -> Result<StoredTrade, sqlx::Error> {
        Store::set_trade_status(self, id, status).await
    }
}
