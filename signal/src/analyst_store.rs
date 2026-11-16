//! The analyst's decisions and simulated trades (`analyst.decisions` and
//! `analyst.trades` → `analyst_decisions`, `simulated_trades`,
//! `simulated_trade_observations`, NEX-257).
//!
//! Prices and metric values arrive as decimal strings. They are parsed into
//! [`Decimal`] before anything is written, so a string that does not parse
//! rejects the whole message (logged, counted, skipped) and never becomes 0.
//! Decimals are written as text cast to `NUMERIC`, so no digit passes through a
//! float.
//!
//! A decision is upserted by `news_id`. A trade carries its full state and an
//! increasing `version`: it is written only when its version is higher than the
//! stored one, and its observations are replaced in the same transaction, so
//! reordered or redelivered messages never overwrite newer state. A trade and
//! its decision come from different topics and may arrive in either order, so
//! there is no foreign key between them (see the migration).
//!
//! Each message is handled inside a `process <topic>` consumer span whose
//! parent is the `traceparent` header (the release's trace in Tempo), with the
//! database transaction in a child span.

use chrono::{DateTime, Utc};
use model::generated::{AnalystDecision, AnalystTrade};
use prost::Message;
use rdkafka::Message as KafkaMessage;
use rust_decimal::Decimal;
use sqlx::PgPool;
use std::collections::HashSet;
use std::str::FromStr;
use telemetry::trace::OpenTelemetrySpanExt;
use thiserror::Error;
use tracing::{error, field::Empty, info, info_span, warn, Instrument, Span};

use crate::insider_store;

/// Why a message was rejected. Logged and skipped: a redelivery would be
/// rejected the same way.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum Invalid {
    /// A required text field is empty.
    #[error("empty {0}")]
    Empty(&'static str),
    /// A decimal string that does not parse.
    #[error("invalid decimal {field}: {value:?}")]
    Decimal {
        /// The field name.
        field: String,
        /// The offending string.
        value: String,
    },
    /// A timestamp outside the representable range.
    #[error("invalid timestamp {field}: {value}")]
    Time {
        /// The field name.
        field: &'static str,
        /// The offending milliseconds.
        value: i64,
    },
    /// A trace id that is not 32 lowercase hex digits.
    #[error("invalid trace id {0:?}")]
    TraceId(String),
    /// Two observations of one trade for the same horizon.
    #[error("duplicate observation horizon {0:?}")]
    DuplicateHorizon(String),
}

/// One decision metric, values as validated decimal strings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedMetric {
    /// The metric key.
    pub key: String,
    /// The current value.
    pub current: Option<Decimal>,
    /// The prior value.
    pub prior: Option<Decimal>,
    /// The expected value.
    pub expected: Option<Decimal>,
    /// The unit.
    pub unit: String,
}

/// A validated [`AnalystDecision`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedDecision {
    /// MFN's news id of the release.
    pub news_id: String,
    /// The company's ticker.
    pub ticker: String,
    /// The company's exchange.
    pub exchange_mic: String,
    /// The release's fiscal period, when known.
    pub fiscal_period: Option<String>,
    /// The release's language.
    pub lang: String,
    /// When MFN published the release.
    pub published_at: DateTime<Utc>,
    /// When the analyst consumed it.
    pub received_at: DateTime<Utc>,
    /// When the decision was made.
    pub decided_at: DateTime<Utc>,
    /// `buy`, `no_buy`, `abstain`, ...
    pub decision: String,
    /// Why.
    pub reason: String,
    /// The kernel that ran, when one did.
    pub kernel_id: Option<i64>,
    /// Microseconds the kernel took.
    pub kernel_micros: i64,
    /// The error kind of a `kernel_error`.
    pub error: Option<String>,
    /// The metrics the kernel found.
    pub metrics: Vec<ParsedMetric>,
    /// The release's trace id.
    pub trace_id: Option<String>,
}

/// One validated observation of a trade.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedObservation {
    /// `1m`, ..., `close`, `hold`.
    pub horizon: String,
    /// The target instant.
    pub scheduled_at: DateTime<Utc>,
    /// When a quote was fetched.
    pub fetched_at: Option<DateTime<Utc>>,
    /// The bid.
    pub bid: Option<Decimal>,
    /// The ask.
    pub ask: Option<Decimal>,
    /// Saxo's price type of the bid.
    pub bid_type: Option<String>,
    /// Saxo's price type of the ask.
    pub ask_type: Option<String>,
    /// Minutes the quote was delayed; `None` when unknown.
    pub delayed_minutes: Option<i64>,
    /// Fetched more than 2 s after the target.
    pub late: bool,
    /// `ok`, `missed`, `beyond_close`, `no_quote`, ...
    pub status: String,
}

/// A validated [`AnalystTrade`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedTrade {
    /// The analyst's trade id.
    pub trade_id: i64,
    /// MFN's news id of the release.
    pub news_id: String,
    /// The company's ticker.
    pub ticker: String,
    /// The company's exchange.
    pub exchange_mic: String,
    /// `buy`, `no_buy`, `abstain`, `skipped_no_kernel`.
    pub decision: String,
    /// True for every decision but `buy`.
    pub shadow: bool,
    /// Where the trade stands.
    pub status: String,
    /// Increases with every state change.
    pub version: i64,
    /// The hold in trading minutes.
    pub hold_minutes: Decimal,
    /// When MFN published the release.
    pub published_at: DateTime<Utc>,
    /// When the entry quote is first asked for.
    pub entry_target_at: Option<DateTime<Utc>>,
    /// When the entry was taken.
    pub entry_at: Option<DateTime<Utc>>,
    /// The ask paid.
    pub entry_ask: Option<Decimal>,
    /// The bid at entry.
    pub entry_bid: Option<Decimal>,
    /// When the exit is due.
    pub exit_target_at: Option<DateTime<Utc>>,
    /// When the exit was taken.
    pub exit_at: Option<DateTime<Utc>>,
    /// The bid received.
    pub exit_bid: Option<Decimal>,
    /// The ask at exit.
    pub exit_ask: Option<Decimal>,
    /// Exit bid minus entry ask.
    pub pnl_per_share: Option<Decimal>,
    /// The same in percent of the entry ask.
    pub pnl_pct: Option<Decimal>,
    /// The quote currency.
    pub currency: Option<String>,
    /// Entry minus publish time, in milliseconds.
    pub release_to_entry_ms: Option<i64>,
    /// The release trace id.
    pub trace_id: Option<String>,
    /// The trade's own trace id.
    pub trade_trace_id: Option<String>,
    /// The observations so far.
    pub observations: Vec<ParsedObservation>,
}

fn required(field: &'static str, value: &str) -> Result<String, Invalid> {
    if value.is_empty() {
        Err(Invalid::Empty(field))
    } else {
        Ok(value.to_owned())
    }
}

fn optional(value: &str) -> Option<String> {
    (!value.is_empty()).then(|| value.to_owned())
}

fn decimal(field: &str, value: &str) -> Result<Option<Decimal>, Invalid> {
    if value.is_empty() {
        return Ok(None);
    }
    Decimal::from_str(value.trim())
        .map(Some)
        .map_err(|_| Invalid::Decimal {
            field: field.to_owned(),
            value: value.to_owned(),
        })
}

fn time(field: &'static str, millis: i64) -> Result<DateTime<Utc>, Invalid> {
    DateTime::from_timestamp_millis(millis).ok_or(Invalid::Time {
        field,
        value: millis,
    })
}

fn optional_time(field: &'static str, millis: i64) -> Result<Option<DateTime<Utc>>, Invalid> {
    if millis == 0 {
        Ok(None)
    } else {
        time(field, millis).map(Some)
    }
}

fn trace_id(value: &str) -> Result<Option<String>, Invalid> {
    if value.is_empty() {
        return Ok(None);
    }
    let valid = value.len() == 32
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    if valid {
        Ok(Some(value.to_owned()))
    } else {
        Err(Invalid::TraceId(value.to_owned()))
    }
}

/// Validates a decision message.
pub fn parse_decision(message: &AnalystDecision) -> Result<ParsedDecision, Invalid> {
    let metrics = message
        .metrics
        .iter()
        .map(|m| {
            Ok(ParsedMetric {
                key: required("metric key", &m.key)?,
                current: decimal(&format!("metric {} current", m.key), &m.current)?,
                prior: decimal(&format!("metric {} prior", m.key), &m.prior)?,
                expected: decimal(&format!("metric {} expected", m.key), &m.expected)?,
                unit: m.unit.clone(),
            })
        })
        .collect::<Result<Vec<_>, Invalid>>()?;
    Ok(ParsedDecision {
        news_id: required("news_id", &message.news_id)?,
        ticker: required("ticker", &message.ticker)?,
        exchange_mic: required("exchange_mic", &message.exchange_mic)?,
        fiscal_period: optional(&message.fiscal_period),
        lang: message.lang.clone(),
        published_at: time("published_at", message.published_at_unix_millis)?,
        received_at: time("received_at", message.received_at_unix_millis)?,
        decided_at: time("decided_at", message.decided_at_unix_millis)?,
        decision: required("decision", &message.decision)?,
        reason: message.reason.clone(),
        kernel_id: (message.kernel_id != 0).then_some(message.kernel_id),
        kernel_micros: message.kernel_micros,
        error: optional(&message.error),
        metrics,
        trace_id: trace_id(&message.trace_id)?,
    })
}

/// Validates a trade message.
pub fn parse_trade(message: &AnalystTrade) -> Result<ParsedTrade, Invalid> {
    let mut horizons = HashSet::new();
    let observations = message
        .observations
        .iter()
        .map(|o| {
            let horizon = required("horizon", &o.horizon)?;
            if !horizons.insert(horizon.clone()) {
                return Err(Invalid::DuplicateHorizon(horizon));
            }
            Ok(ParsedObservation {
                bid: decimal(&format!("observation {horizon} bid"), &o.bid)?,
                ask: decimal(&format!("observation {horizon} ask"), &o.ask)?,
                horizon,
                scheduled_at: time("observation scheduled_at", o.scheduled_at_unix_millis)?,
                fetched_at: optional_time("observation fetched_at", o.fetched_at_unix_millis)?,
                bid_type: optional(&o.bid_type),
                ask_type: optional(&o.ask_type),
                delayed_minutes: (o.delayed_minutes >= 0).then_some(o.delayed_minutes),
                late: o.late,
                status: required("observation status", &o.status)?,
            })
        })
        .collect::<Result<Vec<_>, Invalid>>()?;
    let entry_at = optional_time("entry_at", message.entry_at_unix_millis)?;
    Ok(ParsedTrade {
        trade_id: message.trade_id,
        news_id: required("news_id", &message.news_id)?,
        ticker: required("ticker", &message.ticker)?,
        exchange_mic: required("exchange_mic", &message.exchange_mic)?,
        decision: required("decision", &message.decision)?,
        shadow: message.shadow,
        status: required("status", &message.status)?,
        version: message.version,
        hold_minutes: decimal("hold_minutes", &message.hold_minutes)?
            .ok_or(Invalid::Empty("hold_minutes"))?,
        published_at: time("published_at", message.published_at_unix_millis)?,
        entry_target_at: optional_time("entry_target_at", message.entry_target_at_unix_millis)?,
        entry_at,
        entry_ask: decimal("entry_ask", &message.entry_ask)?,
        entry_bid: decimal("entry_bid", &message.entry_bid)?,
        exit_target_at: optional_time("exit_target_at", message.exit_target_at_unix_millis)?,
        exit_at: optional_time("exit_at", message.exit_at_unix_millis)?,
        exit_bid: decimal("exit_bid", &message.exit_bid)?,
        exit_ask: decimal("exit_ask", &message.exit_ask)?,
        pnl_per_share: decimal("pnl_per_share", &message.pnl_per_share)?,
        pnl_pct: decimal("pnl_pct", &message.pnl_pct)?,
        currency: optional(&message.currency),
        release_to_entry_ms: entry_at.map(|_| message.release_to_entry_ms),
        trace_id: trace_id(&message.trace_id)?,
        trade_trace_id: trace_id(&message.trade_trace_id)?,
        observations,
    })
}

fn text(value: Option<Decimal>) -> Option<String> {
    value.map(|d| d.to_string())
}

/// Inserts the decision, or overwrites the stored one for the same release.
pub async fn store_decision(pool: &PgPool, d: &ParsedDecision) -> sqlx::Result<()> {
    let keys: Vec<&str> = d.metrics.iter().map(|m| m.key.as_str()).collect();
    let current: Vec<Option<String>> = d.metrics.iter().map(|m| text(m.current)).collect();
    let prior: Vec<Option<String>> = d.metrics.iter().map(|m| text(m.prior)).collect();
    let expected: Vec<Option<String>> = d.metrics.iter().map(|m| text(m.expected)).collect();
    let units: Vec<&str> = d.metrics.iter().map(|m| m.unit.as_str()).collect();
    sqlx::query(
        r#"
        INSERT INTO analyst_decisions
            (news_id, ticker, exchange_mic, fiscal_period, lang, published_at, received_at,
             decided_at, decision, reason, kernel_id, kernel_micros, error, metrics, trace_id)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13,
            (SELECT COALESCE(jsonb_agg(jsonb_build_object(
                    'key', m.key, 'current', m.current, 'prior', m.prior,
                    'expected', m.expected, 'unit', m.unit) ORDER BY m.ord), '[]'::jsonb)
             FROM unnest($14::text[], $15::text[], $16::text[], $17::text[], $18::text[])
                  WITH ORDINALITY AS m(key, current, prior, expected, unit, ord)),
            $19)
        ON CONFLICT (news_id) DO UPDATE SET
            ticker        = EXCLUDED.ticker,
            exchange_mic  = EXCLUDED.exchange_mic,
            fiscal_period = EXCLUDED.fiscal_period,
            lang          = EXCLUDED.lang,
            published_at  = EXCLUDED.published_at,
            received_at   = EXCLUDED.received_at,
            decided_at    = EXCLUDED.decided_at,
            decision      = EXCLUDED.decision,
            reason        = EXCLUDED.reason,
            kernel_id     = EXCLUDED.kernel_id,
            kernel_micros = EXCLUDED.kernel_micros,
            error         = EXCLUDED.error,
            metrics       = EXCLUDED.metrics,
            trace_id      = EXCLUDED.trace_id,
            updated_at    = now()
        "#,
    )
    .bind(&d.news_id)
    .bind(&d.ticker)
    .bind(&d.exchange_mic)
    .bind(&d.fiscal_period)
    .bind(&d.lang)
    .bind(d.published_at)
    .bind(d.received_at)
    .bind(d.decided_at)
    .bind(&d.decision)
    .bind(&d.reason)
    .bind(d.kernel_id)
    .bind(d.kernel_micros)
    .bind(&d.error)
    .bind(keys)
    .bind(current)
    .bind(prior)
    .bind(expected)
    .bind(units)
    .bind(&d.trace_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// What storing a trade did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Upsert {
    /// The message was newer than the stored state and replaced it.
    Applied,
    /// The stored version is the same or newer; nothing was written.
    OlderVersionIgnored,
}

impl Upsert {
    /// The value of the `signal.upsert` span attribute.
    pub fn as_str(self) -> &'static str {
        match self {
            Upsert::Applied => "applied",
            Upsert::OlderVersionIgnored => "older_version_ignored",
        }
    }
}

/// Writes the trade when its version is higher than the stored one, and
/// replaces its observations in the same transaction.
pub async fn store_trade(pool: &PgPool, t: &ParsedTrade) -> sqlx::Result<Upsert> {
    let mut tx = pool.begin().await?;
    let applied = sqlx::query(
        r#"
        INSERT INTO simulated_trades
            (trade_id, news_id, ticker, exchange_mic, decision, shadow, published_at, hold_minutes,
             entry_target_at, entry_at, entry_ask, entry_bid, exit_target_at, exit_at, exit_bid,
             exit_ask, pnl_per_share, pnl_pct, currency, release_to_entry_ms, status, version,
             trace_id, trade_trace_id)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8::numeric, $9, $10, $11::numeric, $12::numeric, $13,
                $14, $15::numeric, $16::numeric, $17::numeric, $18::numeric, $19, $20, $21, $22,
                $23, $24)
        ON CONFLICT (trade_id) DO UPDATE SET
            news_id             = EXCLUDED.news_id,
            ticker              = EXCLUDED.ticker,
            exchange_mic        = EXCLUDED.exchange_mic,
            decision            = EXCLUDED.decision,
            shadow              = EXCLUDED.shadow,
            published_at        = EXCLUDED.published_at,
            hold_minutes        = EXCLUDED.hold_minutes,
            entry_target_at     = EXCLUDED.entry_target_at,
            entry_at            = EXCLUDED.entry_at,
            entry_ask           = EXCLUDED.entry_ask,
            entry_bid           = EXCLUDED.entry_bid,
            exit_target_at      = EXCLUDED.exit_target_at,
            exit_at             = EXCLUDED.exit_at,
            exit_bid            = EXCLUDED.exit_bid,
            exit_ask            = EXCLUDED.exit_ask,
            pnl_per_share       = EXCLUDED.pnl_per_share,
            pnl_pct             = EXCLUDED.pnl_pct,
            currency            = EXCLUDED.currency,
            release_to_entry_ms = EXCLUDED.release_to_entry_ms,
            status              = EXCLUDED.status,
            version             = EXCLUDED.version,
            trace_id            = EXCLUDED.trace_id,
            trade_trace_id      = EXCLUDED.trade_trace_id,
            updated_at          = now()
        WHERE simulated_trades.version < EXCLUDED.version
        RETURNING trade_id
        "#,
    )
    .bind(t.trade_id)
    .bind(&t.news_id)
    .bind(&t.ticker)
    .bind(&t.exchange_mic)
    .bind(&t.decision)
    .bind(t.shadow)
    .bind(t.published_at)
    .bind(t.hold_minutes.to_string())
    .bind(t.entry_target_at)
    .bind(t.entry_at)
    .bind(text(t.entry_ask))
    .bind(text(t.entry_bid))
    .bind(t.exit_target_at)
    .bind(t.exit_at)
    .bind(text(t.exit_bid))
    .bind(text(t.exit_ask))
    .bind(text(t.pnl_per_share))
    .bind(text(t.pnl_pct))
    .bind(&t.currency)
    .bind(t.release_to_entry_ms)
    .bind(&t.status)
    .bind(t.version)
    .bind(&t.trace_id)
    .bind(&t.trade_trace_id)
    .fetch_optional(&mut *tx)
    .await?;
    if applied.is_none() {
        tx.rollback().await?;
        return Ok(Upsert::OlderVersionIgnored);
    }
    sqlx::query("DELETE FROM simulated_trade_observations WHERE trade_id = $1")
        .bind(t.trade_id)
        .execute(&mut *tx)
        .await?;
    if !t.observations.is_empty() {
        let o = &t.observations;
        sqlx::query(
            r#"
            INSERT INTO simulated_trade_observations
                (trade_id, horizon, scheduled_at, fetched_at, bid, ask, bid_type, ask_type,
                 delayed_minutes, late, status)
            SELECT $1, horizon, scheduled_at, fetched_at, bid::numeric, ask::numeric, bid_type,
                   ask_type, delayed_minutes, late, status
            FROM unnest($2::text[], $3::timestamptz[], $4::timestamptz[], $5::text[], $6::text[],
                        $7::text[], $8::text[], $9::bigint[], $10::bool[], $11::text[])
                 AS x(horizon, scheduled_at, fetched_at, bid, ask, bid_type, ask_type,
                      delayed_minutes, late, status)
            "#,
        )
        .bind(t.trade_id)
        .bind(o.iter().map(|x| x.horizon.clone()).collect::<Vec<_>>())
        .bind(o.iter().map(|x| x.scheduled_at).collect::<Vec<_>>())
        .bind(o.iter().map(|x| x.fetched_at).collect::<Vec<_>>())
        .bind(o.iter().map(|x| text(x.bid)).collect::<Vec<_>>())
        .bind(o.iter().map(|x| text(x.ask)).collect::<Vec<_>>())
        .bind(o.iter().map(|x| x.bid_type.clone()).collect::<Vec<_>>())
        .bind(o.iter().map(|x| x.ask_type.clone()).collect::<Vec<_>>())
        .bind(o.iter().map(|x| x.delayed_minutes).collect::<Vec<_>>())
        .bind(o.iter().map(|x| x.late).collect::<Vec<_>>())
        .bind(o.iter().map(|x| x.status.clone()).collect::<Vec<_>>())
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(Upsert::Applied)
}

/// The `process <topic>` consumer span. Its parent is the message's
/// `traceparent`; without one it is a new root marked `nexus.trace_parent_missing`.
/// The fields known only after decoding are recorded later.
fn consumer_span<M: KafkaMessage>(message: &M, name: &'static str) -> Span {
    let key = message
        .key()
        .map(|k| String::from_utf8_lossy(k).into_owned())
        .unwrap_or_default();
    let span = match name {
        "analyst.decisions" => info_span!(
            "process analyst.decisions",
            otel.kind = "consumer",
            messaging.system = "kafka",
            messaging.destination.name = message.topic(),
            messaging.kafka.message.key = key.as_str(),
            messaging.kafka.message.offset = message.offset(),
            messaging.destination.partition.id = message.partition(),
            nexus.ticker = Empty,
            nexus.exchange_mic = Empty,
            mfn.news_id = Empty,
            nexus.trace_parent_missing = Empty,
            otel.status_code = Empty,
            otel.status_description = Empty
        ),
        _ => info_span!(
            "process analyst.trades",
            otel.kind = "consumer",
            messaging.system = "kafka",
            messaging.destination.name = message.topic(),
            messaging.kafka.message.key = key.as_str(),
            messaging.kafka.message.offset = message.offset(),
            messaging.destination.partition.id = message.partition(),
            nexus.ticker = Empty,
            nexus.exchange_mic = Empty,
            mfn.news_id = Empty,
            analyst.trade_id = Empty,
            analyst.version = Empty,
            signal.upsert = Empty,
            nexus.trace_parent_missing = Empty,
            otel.status_code = Empty,
            otel.status_description = Empty
        ),
    };
    let linked = telemetry::kafka::extract(message)
        .map(|parent| span.set_parent(parent).is_ok())
        .unwrap_or(false);
    if !linked {
        span.record("nexus.trace_parent_missing", true);
    }
    span
}

fn fail(span: &Span, kind: &str) {
    span.record("otel.status_code", "ERROR");
    span.record("otel.status_description", kind);
}

fn retried(topic: &str) -> impl FnMut() + '_ {
    move || {
        metrics::counter!("signal_messages_total", "topic" => topic.to_string(), "outcome" => "db_error_retried").increment(1);
        metrics::counter!("signal_store_retries_total", "topic" => topic.to_string()).increment(1);
    }
}

/// Handles one `analyst.decisions` message; returns the outcome label.
pub async fn handle_decision<M: KafkaMessage>(pool: &PgPool, message: &M) -> &'static str {
    let span = consumer_span(message, "analyst.decisions");
    handle_decision_in(pool, message, &span)
        .instrument(span.clone())
        .await
}

async fn handle_decision_in<M: KafkaMessage>(
    pool: &PgPool,
    message: &M,
    span: &Span,
) -> &'static str {
    let Some(payload) = message.payload() else {
        warn!(topic = message.topic(), "empty payload, skipping");
        return "skipped_empty";
    };
    let decoded = match AnalystDecision::decode(payload) {
        Ok(d) => d,
        Err(e) => {
            error!(error = %e, "malformed AnalystDecision protobuf, skipping");
            fail(span, "decode_error");
            return "skipped_malformed";
        }
    };
    span.record("nexus.ticker", decoded.ticker.as_str());
    span.record("nexus.exchange_mic", decoded.exchange_mic.as_str());
    span.record("mfn.news_id", decoded.news_id.as_str());
    let parsed = match parse_decision(&decoded) {
        Ok(p) => p,
        Err(e) => {
            error!(news_id = %decoded.news_id, error = %e, "invalid AnalystDecision, skipping");
            fail(span, "invalid_message");
            return "skipped_invalid";
        }
    };
    let store = info_span!(parent: span, "signal.store_analyst_decision");
    let result = insider_store::store_with_retry_counted(
        "analyst decision",
        || store_decision(pool, &parsed),
        crate::INITIAL_STORE_BACKOFF,
        crate::MAX_STORE_BACKOFF,
        retried(message.topic()),
    )
    .instrument(store)
    .await;
    match result {
        Ok(()) => {
            info!(news_id = %parsed.news_id, ticker = %parsed.ticker, decision = %parsed.decision,
                  "analyst decision stored");
            "stored"
        }
        Err(e) => {
            error!(news_id = %parsed.news_id, error = %e,
                   "analyst decision rejected by the database, skipped");
            fail(span, "db_error");
            metrics::counter!("signal_analyst_db_errors_total", "topic" => "analyst.decisions")
                .increment(1);
            "db_error_committed"
        }
    }
}

/// Handles one `analyst.trades` message; returns the outcome label.
pub async fn handle_trade<M: KafkaMessage>(pool: &PgPool, message: &M) -> &'static str {
    let span = consumer_span(message, "analyst.trades");
    handle_trade_in(pool, message, &span)
        .instrument(span.clone())
        .await
}

async fn handle_trade_in<M: KafkaMessage>(pool: &PgPool, message: &M, span: &Span) -> &'static str {
    let Some(payload) = message.payload() else {
        warn!(topic = message.topic(), "empty payload, skipping");
        return "skipped_empty";
    };
    let decoded = match AnalystTrade::decode(payload) {
        Ok(d) => d,
        Err(e) => {
            error!(error = %e, "malformed AnalystTrade protobuf, skipping");
            fail(span, "decode_error");
            return "skipped_malformed";
        }
    };
    span.record("nexus.ticker", decoded.ticker.as_str());
    span.record("nexus.exchange_mic", decoded.exchange_mic.as_str());
    span.record("mfn.news_id", decoded.news_id.as_str());
    span.record("analyst.trade_id", decoded.trade_id);
    span.record("analyst.version", decoded.version);
    let parsed = match parse_trade(&decoded) {
        Ok(p) => p,
        Err(e) => {
            error!(trade_id = decoded.trade_id, error = %e, "invalid AnalystTrade, skipping");
            fail(span, "invalid_message");
            return "skipped_invalid";
        }
    };
    let store = info_span!(parent: span, "signal.store_analyst_trade");
    let result = insider_store::store_with_retry_counted(
        "analyst trade",
        || store_trade(pool, &parsed),
        crate::INITIAL_STORE_BACKOFF,
        crate::MAX_STORE_BACKOFF,
        retried(message.topic()),
    )
    .instrument(store)
    .await;
    match result {
        Ok(upsert) => {
            span.record("signal.upsert", upsert.as_str());
            info!(trade_id = parsed.trade_id, version = parsed.version, status = %parsed.status,
                  upsert = upsert.as_str(), "analyst trade handled");
            "stored"
        }
        Err(e) => {
            error!(trade_id = parsed.trade_id, error = %e,
                   "analyst trade rejected by the database, skipped");
            fail(span, "db_error");
            metrics::counter!("signal_analyst_db_errors_total", "topic" => "analyst.trades")
                .increment(1);
            "db_error_committed"
        }
    }
}

/// Installs an enabling process-wide default subscriber once, so that a
/// callsite first reached by a test without a scoped subscriber is not cached as
/// disabled for the tests that run in parallel with it.
#[cfg(test)]
pub(crate) fn install_default_subscriber() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = tracing::subscriber::set_global_default(tracing_subscriber::registry());
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use model::generated::{DecisionMetric, TradeObservation};
    use rdkafka::message::{Header, OwnedHeaders, OwnedMessage, Timestamp};
    use sqlx::Row;
    use telemetry::trace::opentelemetry::trace::{SpanKind, Status};
    use telemetry::trace::testing::Harness;
    use tracing::instrument::WithSubscriber;

    const TRACE: &str = "4bf92f3577b34da6a3ce929d0e0e4736";
    const PARENT_SPAN: &str = "00f067aa0ba902b7";

    fn decision(news_id: &str) -> AnalystDecision {
        AnalystDecision {
            ticker: "AAA".into(),
            exchange_mic: "XSTO".into(),
            news_id: news_id.into(),
            lang: "en".into(),
            fiscal_period: "Q3 2026".into(),
            decision: "buy".into(),
            reason: "revenue above expected".into(),
            metrics: vec![DecisionMetric {
                key: "revenue".into(),
                current: "120.5".into(),
                prior: "100".into(),
                expected: String::new(),
                unit: "MSEK".into(),
            }],
            published_at_unix_millis: 1_000_000,
            received_at_unix_millis: 1_000_050,
            decided_at_unix_millis: 1_000_060,
            kernel_micros: 85,
            trace_id: TRACE.into(),
            kernel_id: 4,
            ..Default::default()
        }
    }

    fn observation(horizon: &str, bid: &str) -> TradeObservation {
        TradeObservation {
            horizon: horizon.into(),
            scheduled_at_unix_millis: 2_000_000,
            fetched_at_unix_millis: 2_000_100,
            bid: bid.into(),
            ask: "101.000000".into(),
            bid_type: "Tradable".into(),
            ask_type: "Tradable".into(),
            delayed_minutes: 0,
            status: "ok".into(),
            ..Default::default()
        }
    }

    fn trade(version: i64, observations: Vec<TradeObservation>) -> AnalystTrade {
        AnalystTrade {
            trade_id: 7,
            decision_id: 3,
            news_id: "n1".into(),
            ticker: "AAA".into(),
            exchange_mic: "XSTO".into(),
            decision: "buy".into(),
            status: if version >= 3 { "closed" } else { "open" }.into(),
            version,
            hold_minutes: "120".into(),
            published_at_unix_millis: 1_000_000,
            entry_target_at_unix_millis: 1_000_100,
            entry_at_unix_millis: 1_000_200,
            entry_ask: "100.000000".into(),
            entry_bid: "99.900000".into(),
            exit_bid: if version >= 3 { "101.5" } else { "" }.into(),
            pnl_pct: if version >= 3 { "1.5000" } else { "" }.into(),
            currency: "SEK".into(),
            release_to_entry_ms: 200,
            trace_id: TRACE.into(),
            trade_trace_id: "0af7651916cd43dd8448eb211c80319c".into(),
            observations,
            ..Default::default()
        }
    }

    fn message(topic: &str, payload: Vec<u8>, traceparent: Option<&str>) -> OwnedMessage {
        let headers = traceparent.map(|value| {
            OwnedHeaders::new().insert(Header {
                key: "traceparent",
                value: Some(value),
            })
        });
        OwnedMessage::new(
            Some(payload),
            Some(b"AAA:XSTO".to_vec()),
            topic.to_string(),
            Timestamp::NotAvailable,
            0,
            1,
            headers,
        )
    }

    fn trades_message(t: &AnalystTrade, traceparent: Option<&str>) -> OwnedMessage {
        message("analyst.trades", t.encode_to_vec(), traceparent)
    }

    fn parent() -> String {
        format!("00-{TRACE}-{PARENT_SPAN}-01")
    }

    async fn count(pool: &PgPool, table: &str) -> i64 {
        sqlx::query(&format!("SELECT COUNT(*) AS n FROM {table}"))
            .fetch_one(pool)
            .await
            .unwrap()
            .get("n")
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn decision_then_trade_versions_end_at_v3_with_all_observations(pool: PgPool) {
        install_default_subscriber();
        assert_eq!(
            handle_decision(
                &pool,
                &message("analyst.decisions", decision("n1").encode_to_vec(), None)
            )
            .await,
            "stored"
        );
        for (version, observations) in [
            (1, vec![]),
            (2, vec![observation("1m", "100.400000")]),
            (
                3,
                vec![
                    observation("1m", "100.400000"),
                    observation("30m", "101.500000"),
                    observation("hold", "101.500000"),
                ],
            ),
        ] {
            let outcome =
                handle_trade(&pool, &trades_message(&trade(version, observations), None)).await;
            assert_eq!(outcome, "stored");
        }
        let row = sqlx::query(
            "SELECT version, status, pnl_pct::text AS pnl, exit_bid::text AS exit_bid FROM simulated_trades WHERE trade_id = 7",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row.get::<i64, _>("version"), 3);
        assert_eq!(row.get::<String, _>("status"), "closed");
        assert_eq!(row.get::<String, _>("pnl"), "1.5000");
        assert_eq!(row.get::<String, _>("exit_bid"), "101.500000");
        assert_eq!(count(&pool, "simulated_trades").await, 1);
        assert_eq!(count(&pool, "simulated_trade_observations").await, 3);
        let metrics: String =
            sqlx::query("SELECT metrics::text AS m FROM analyst_decisions WHERE news_id = 'n1'")
                .fetch_one(&pool)
                .await
                .unwrap()
                .get("m");
        assert!(metrics.contains(r#""current": "120.5""#), "{metrics}");
        assert!(metrics.contains(r#""expected": null"#), "{metrics}");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn an_older_version_never_overwrites_a_newer_one(pool: PgPool) {
        install_default_subscriber();
        let v3 = parse_trade(&trade(3, vec![observation("1m", "100.400000")])).unwrap();
        let v2 = parse_trade(&trade(2, vec![])).unwrap();
        assert_eq!(store_trade(&pool, &v3).await.unwrap(), Upsert::Applied);
        assert_eq!(
            store_trade(&pool, &v2).await.unwrap(),
            Upsert::OlderVersionIgnored
        );
        assert_eq!(
            store_trade(&pool, &v3).await.unwrap(),
            Upsert::OlderVersionIgnored
        );
        let version: i64 = sqlx::query("SELECT version FROM simulated_trades")
            .fetch_one(&pool)
            .await
            .unwrap()
            .get("version");
        assert_eq!(version, 3);
        assert_eq!(count(&pool, "simulated_trade_observations").await, 1);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_trade_before_its_decision_is_stored_and_joined_later(pool: PgPool) {
        install_default_subscriber();
        assert_eq!(
            handle_trade(&pool, &trades_message(&trade(1, vec![]), None)).await,
            "stored"
        );
        let join = "SELECT d.decision AS d FROM simulated_trades t \
                    JOIN analyst_decisions d ON d.news_id = t.news_id";
        assert!(sqlx::query(join)
            .fetch_optional(&pool)
            .await
            .unwrap()
            .is_none());
        handle_decision(
            &pool,
            &message("analyst.decisions", decision("n1").encode_to_vec(), None),
        )
        .await;
        let joined: String = sqlx::query(join).fetch_one(&pool).await.unwrap().get("d");
        assert_eq!(joined, "buy");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_malformed_decimal_rejects_the_message_and_stores_nothing(pool: PgPool) {
        install_default_subscriber();
        let mut bad = trade(2, vec![observation("1m", "100.4")]);
        bad.entry_ask = "12,5".into();
        assert_eq!(
            handle_trade(&pool, &trades_message(&bad, None)).await,
            "skipped_invalid"
        );
        let mut bad_observation = trade(2, vec![observation("1m", "abc")]);
        bad_observation.entry_ask = "100".into();
        assert_eq!(
            handle_trade(&pool, &trades_message(&bad_observation, None)).await,
            "skipped_invalid"
        );
        let mut bad_metric = decision("n1");
        bad_metric.metrics[0].current = "1.2.3".into();
        assert_eq!(
            handle_decision(
                &pool,
                &message("analyst.decisions", bad_metric.encode_to_vec(), None)
            )
            .await,
            "skipped_invalid"
        );
        assert_eq!(count(&pool, "simulated_trades").await, 0);
        assert_eq!(count(&pool, "simulated_trade_observations").await, 0);
        assert_eq!(count(&pool, "analyst_decisions").await, 0);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn trace_ids_are_stored_from_the_protos(pool: PgPool) {
        install_default_subscriber();
        handle_decision(
            &pool,
            &message("analyst.decisions", decision("n1").encode_to_vec(), None),
        )
        .await;
        handle_trade(&pool, &trades_message(&trade(1, vec![]), None)).await;
        let d: String = sqlx::query("SELECT trace_id FROM analyst_decisions")
            .fetch_one(&pool)
            .await
            .unwrap()
            .get("trace_id");
        assert_eq!(d, TRACE);
        let row = sqlx::query("SELECT trace_id, trade_trace_id FROM simulated_trades")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(row.get::<String, _>("trace_id"), TRACE);
        assert_eq!(
            row.get::<String, _>("trade_trace_id"),
            "0af7651916cd43dd8448eb211c80319c"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn an_undecodable_payload_is_skipped(pool: PgPool) {
        install_default_subscriber();
        let outcome = handle_trade(
            &pool,
            &message("analyst.trades", vec![0xff, 0xff, 0xff], None),
        )
        .await;
        assert_eq!(outcome, "skipped_malformed");
    }

    #[test]
    fn parsing_rejects_bad_decimals_and_trace_ids_but_accepts_empty_ones() {
        let mut t = trade(1, vec![]);
        t.exit_bid = String::new();
        assert!(parse_trade(&t).is_ok());
        t.pnl_pct = "NaN".into();
        assert!(matches!(parse_trade(&t), Err(Invalid::Decimal { .. })));
        let mut t = trade(1, vec![]);
        t.trace_id = "XYZ".into();
        assert!(matches!(parse_trade(&t), Err(Invalid::TraceId(_))));
        let t = trade(1, vec![observation("1m", "1"), observation("1m", "2")]);
        assert!(matches!(parse_trade(&t), Err(Invalid::DuplicateHorizon(_))));
    }

    fn attribute(span: &telemetry::trace::testing::SpanData, key: &str) -> Option<String> {
        span.attributes
            .iter()
            .find(|a| a.key.as_str() == key)
            .map(|a| a.value.to_string())
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_trade_with_a_traceparent_continues_that_trace(pool: PgPool) {
        install_default_subscriber();
        let harness = Harness::new();
        let traceparent = parent();
        async {
            handle_trade(
                &pool,
                &trades_message(&trade(2, vec![]), Some(&traceparent)),
            )
            .await;
            handle_trade(
                &pool,
                &trades_message(&trade(1, vec![]), Some(&traceparent)),
            )
            .await;
        }
        .with_subscriber(harness.subscriber())
        .await;

        let spans = harness.spans();
        let consumers: Vec<_> = spans
            .iter()
            .filter(|s| s.name == "process analyst.trades")
            .collect();
        assert_eq!(consumers.len(), 2);
        for consumer in &consumers {
            assert_eq!(consumer.span_context.trace_id().to_string(), TRACE);
            assert_eq!(consumer.parent_span_id.to_string(), PARENT_SPAN);
            assert_eq!(consumer.span_kind, SpanKind::Consumer);
            assert_eq!(
                attribute(consumer, "analyst.trade_id").as_deref(),
                Some("7")
            );
            assert_eq!(attribute(consumer, "mfn.news_id").as_deref(), Some("n1"));
            assert_eq!(attribute(consumer, "nexus.trace_parent_missing"), None);
        }
        let upserts: Vec<_> = consumers
            .iter()
            .filter_map(|s| attribute(s, "signal.upsert"))
            .collect();
        assert_eq!(upserts, ["applied", "older_version_ignored"]);
        let stores: Vec<_> = spans
            .iter()
            .filter(|s| s.name == "signal.store_analyst_trade")
            .collect();
        assert_eq!(stores.len(), 2);
        assert!(stores.iter().all(|s| {
            s.span_context.trace_id().to_string() == TRACE
                && consumers
                    .iter()
                    .any(|c| c.span_context.span_id() == s.parent_span_id)
        }));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_message_without_a_header_starts_a_root_and_errors_are_marked(pool: PgPool) {
        install_default_subscriber();
        let harness = Harness::new();
        async {
            handle_decision(
                &pool,
                &message("analyst.decisions", decision("n1").encode_to_vec(), None),
            )
            .await;
            let mut bad = trade(1, vec![]);
            bad.hold_minutes = "x".into();
            handle_trade(&pool, &trades_message(&bad, Some(&parent()))).await;
            handle_trade(&pool, &message("analyst.trades", vec![0xff, 0xff], None)).await;
        }
        .with_subscriber(harness.subscriber())
        .await;

        let decision_span = harness.span("process analyst.decisions");
        assert_eq!(decision_span.parent_span_id.to_string(), "0000000000000000");
        assert_eq!(
            attribute(&decision_span, "nexus.trace_parent_missing").as_deref(),
            Some("true")
        );
        assert!(harness
            .spans()
            .iter()
            .any(|s| s.name == "signal.store_analyst_decision"));
        let errored: Vec<_> = harness
            .spans()
            .into_iter()
            .filter(|s| s.name == "process analyst.trades")
            .collect();
        assert_eq!(
            errored.len(),
            2,
            "{:?}",
            harness
                .spans()
                .iter()
                .map(|s| s.name.to_string())
                .collect::<Vec<_>>()
        );
        assert!(errored
            .iter()
            .all(|s| matches!(s.status, Status::Error { .. })));
        assert!(harness
            .spans()
            .iter()
            .all(|s| s.name != "signal.store_analyst_trade"));
    }
}
