use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use analyst::{
    parse_horizons, Clock, DecisionHandle, DecisionKind, Entry, Exit, NewObservation, NewTrade,
    Persisted, Quote, QuoteError, QuoteSource, RunnerSettings, StoredObservation, StoredTrade,
    TokioClock, TradeHandOff, TradeRunner, TradeSignal, TradeSink, TradeStatus, TradeStore,
};
use async_trait::async_trait;
use chrono::{DateTime, NaiveDate, NaiveTime, TimeZone, Utc};
use model::generated::AnalystTrade;
use model::session::{Calendar, Exchange, Sessions};
use rust_decimal::Decimal;
use std::str::FromStr;
use telemetry::trace::opentelemetry::Context;

pub fn dec(text: &str) -> Decimal {
    Decimal::from_str(text).expect("decimal")
}

fn d(y: i32, m: u32, day: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, day).expect("date")
}

fn t(h: u32, m: u32) -> NaiveTime {
    NaiveTime::from_hms_opt(h, m, 0).expect("time")
}

pub fn market(
    mic: &str,
    tz: chrono_tz::Tz,
    open: NaiveTime,
    close: NaiveTime,
) -> (Exchange, Calendar) {
    let closed = [d(2026, 1, 1), d(2026, 11, 16), d(2026, 12, 25)];
    (
        Exchange {
            mic: mic.into(),
            country: "XX".into(),
            timezone: tz,
            open,
            close,
            half_day_close: Some(t(13, 0)),
        },
        Calendar::from_holidays(
            closed.into_iter().collect(),
            [d(2026, 12, 24)].into_iter().collect(),
        )
        .expect("calendar"),
    )
}

pub fn sessions() -> Sessions {
    Sessions::new(vec![
        market("XSTO", chrono_tz::Europe::Stockholm, t(9, 0), t(17, 30)),
        market("XHEL", chrono_tz::Europe::Helsinki, t(10, 0), t(18, 30)),
    ])
}

pub fn sto(m: u32, day: u32, h: u32, mi: u32, s: u32) -> DateTime<Utc> {
    chrono_tz::Europe::Stockholm
        .with_ymd_and_hms(2026, m, day, h, mi, s)
        .single()
        .expect("instant")
        .with_timezone(&Utc)
}

pub fn quote(bid: &str, ask: &str) -> Quote {
    Quote {
        bid: Some(dec(bid)),
        ask: Some(dec(ask)),
        bid_type: Some("Tradable".into()),
        ask_type: Some("Tradable".into()),
        delayed_minutes: Some(0),
        currency: Some("SEK".into()),
        saxo_last_updated: Some("2026-11-10T08:00:00Z".into()),
    }
}

type Script = dyn Fn(DateTime<Utc>) -> Result<Quote, QuoteError> + Send + Sync;

pub struct FakeQuotes {
    clock: Arc<dyn Clock>,
    script: Box<Script>,
    pub requests: Mutex<Vec<(String, DateTime<Utc>)>>,
    latency: Duration,
    in_flight: AtomicUsize,
    pub max_in_flight: AtomicUsize,
}

impl FakeQuotes {
    pub fn new(
        clock: Arc<dyn Clock>,
        latency: Duration,
        script: impl Fn(DateTime<Utc>) -> Result<Quote, QuoteError> + Send + Sync + 'static,
    ) -> Arc<FakeQuotes> {
        Arc::new(FakeQuotes {
            clock,
            script: Box::new(script),
            requests: Mutex::new(Vec::new()),
            latency,
            in_flight: AtomicUsize::new(0),
            max_in_flight: AtomicUsize::new(0),
        })
    }

    pub fn times(&self) -> Vec<DateTime<Utc>> {
        self.requests
            .lock()
            .expect("lock")
            .iter()
            .map(|(_, at)| *at)
            .collect()
    }
}

#[async_trait]
impl QuoteSource for FakeQuotes {
    async fn quote(&self, ticker: &str, _mic: &str) -> Result<Quote, QuoteError> {
        let now = self.clock.now();
        self.requests
            .lock()
            .expect("lock")
            .push((ticker.to_owned(), now));
        let current = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_in_flight.fetch_max(current, Ordering::SeqCst);
        if !self.latency.is_zero() {
            tokio::time::sleep(self.latency).await;
        }
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
        (self.script)(now)
    }
}

#[derive(Default)]
pub struct MemSink(pub Mutex<Vec<(String, AnalystTrade)>>);

impl MemSink {
    pub fn messages(&self) -> Vec<(String, AnalystTrade)> {
        self.0.lock().expect("lock").clone()
    }
}

#[async_trait]
impl TradeSink for MemSink {
    async fn emit(&self, key: &str, trade: &AnalystTrade) -> Result<(), String> {
        self.0
            .lock()
            .expect("lock")
            .push((key.to_owned(), trade.clone()));
        Ok(())
    }
}

#[derive(Default)]
struct State {
    trades: Vec<StoredTrade>,
    observations: Vec<StoredObservation>,
}

#[derive(Default)]
pub struct MemStore(Mutex<State>);

impl MemStore {
    pub fn trades(&self) -> Vec<StoredTrade> {
        self.0.lock().expect("lock").trades.clone()
    }

    pub fn trade(&self) -> StoredTrade {
        let trades = self.trades();
        assert_eq!(trades.len(), 1, "one trade");
        trades[0].clone()
    }

    pub fn observations_of(&self, trade_id: i64) -> Vec<StoredObservation> {
        let mut found: Vec<_> = self
            .0
            .lock()
            .expect("lock")
            .observations
            .iter()
            .filter(|o| o.trade_id == trade_id)
            .cloned()
            .collect();
        found.sort_by_key(|o| (o.scheduled_at, o.horizon.clone()));
        found
    }

    pub fn seed(&self, trade: StoredTrade) {
        self.0.lock().expect("lock").trades.push(trade);
    }

    fn update(
        &self,
        id: i64,
        change: impl FnOnce(&mut StoredTrade),
    ) -> Result<StoredTrade, sqlx::Error> {
        let mut state = self.0.lock().expect("lock");
        let trade = state
            .trades
            .iter_mut()
            .find(|t| t.id == id)
            .ok_or(sqlx::Error::RowNotFound)?;
        change(trade);
        trade.version += 1;
        Ok(trade.clone())
    }
}

#[async_trait]
impl TradeStore for MemStore {
    async fn create_trade(&self, new: &NewTrade) -> Result<Option<StoredTrade>, sqlx::Error> {
        let mut state = self.0.lock().expect("lock");
        if state
            .trades
            .iter()
            .any(|t| t.decision_id == new.decision_id)
        {
            return Ok(None);
        }
        let trade = StoredTrade {
            id: i64::try_from(state.trades.len()).expect("small") + 1,
            decision_id: new.decision_id,
            ticker: new.ticker.clone(),
            exchange_mic: new.exchange_mic.clone(),
            decision: new.decision.clone(),
            shadow: new.decision != "buy",
            published_at: new.published_at,
            hold_minutes: new.hold_minutes,
            entry_target_at: new.entry_target_at,
            entry_at: None,
            entry_ask: None,
            entry_bid: None,
            entry_saxo_at: None,
            exit_target_at: None,
            exit_at: None,
            exit_bid: None,
            exit_ask: None,
            pnl_per_share: None,
            pnl_pct: None,
            currency: None,
            release_to_entry_ms: None,
            status: new.status.as_str().to_owned(),
            version: 1,
            trace_id: new.trace_id.clone(),
            release_traceparent: new.release_traceparent.clone(),
            trade_trace_id: None,
        };
        state.trades.push(trade.clone());
        Ok(Some(trade))
    }

    async fn open_trades(&self) -> Result<Vec<StoredTrade>, sqlx::Error> {
        Ok(self
            .trades()
            .into_iter()
            .filter(|t| t.status().is_open())
            .collect())
    }

    async fn observations(&self, trade_id: i64) -> Result<Vec<StoredObservation>, sqlx::Error> {
        Ok(self.observations_of(trade_id))
    }

    async fn record_entry(&self, id: i64, entry: &Entry) -> Result<StoredTrade, sqlx::Error> {
        self.update(id, |t| {
            t.status = TradeStatus::Open.as_str().to_owned();
            t.entry_at = Some(entry.at);
            t.entry_ask = Some(entry.ask);
            t.entry_bid = entry.bid;
            t.entry_saxo_at = entry.saxo_at.clone();
            t.currency = entry.currency.clone();
            t.release_to_entry_ms = Some(entry.release_to_entry_ms);
            t.exit_target_at = entry.exit_target_at;
        })
    }

    async fn record_observation(
        &self,
        trade_id: i64,
        observation: &NewObservation,
    ) -> Result<StoredTrade, sqlx::Error> {
        let quote = observation.quote.as_ref();
        {
            let mut state = self.0.lock().expect("lock");
            state
                .observations
                .retain(|o| !(o.trade_id == trade_id && o.horizon == observation.horizon));
            state.observations.push(StoredObservation {
                trade_id,
                horizon: observation.horizon.clone(),
                scheduled_at: observation.scheduled_at,
                fetched_at: observation.fetched_at,
                bid: quote.and_then(|q| q.bid),
                ask: quote.and_then(|q| q.ask),
                bid_type: quote.and_then(|q| q.bid_type.clone()),
                ask_type: quote.and_then(|q| q.ask_type.clone()),
                delayed_minutes: quote.and_then(|q| q.delayed_minutes),
                late: observation.late,
                status: observation.status.as_str().to_owned(),
            });
        }
        self.update(trade_id, |_| {})
    }

    async fn record_exit(&self, id: i64, exit: &Exit) -> Result<StoredTrade, sqlx::Error> {
        self.update(id, |t| {
            t.status = exit.status.as_str().to_owned();
            t.exit_at = exit.at;
            t.exit_bid = exit.bid;
            t.exit_ask = exit.ask;
            t.pnl_per_share = exit.pnl.map(|p| p.per_share);
            t.pnl_pct = exit.pnl.map(|p| p.pct);
        })
    }

    async fn set_trade_status(
        &self,
        id: i64,
        status: TradeStatus,
    ) -> Result<StoredTrade, sqlx::Error> {
        self.update(id, |t| t.status = status.as_str().to_owned())
    }
}

pub struct Rig {
    pub clock: Arc<TokioClock>,
    pub store: Arc<MemStore>,
    pub quotes: Arc<FakeQuotes>,
    pub sink: Arc<MemSink>,
    pub runner: TradeRunner,
}

pub fn settings(concurrency: usize) -> RunnerSettings {
    RunnerSettings {
        hold: Duration::from_secs(30 * 60),
        horizons: parse_horizons("1m,5m,15m,30m,60m,close").expect("horizons"),
        close_buffer: Duration::from_secs(5 * 60),
        entry_max_wait: Duration::from_secs(5 * 60),
        quote_concurrency: concurrency,
    }
}

/// A rig whose clock starts at `start`, with tokio's time paused (call from a
/// `start_paused` test).
pub fn rig(
    start: DateTime<Utc>,
    latency: Duration,
    concurrency: usize,
    script: impl Fn(DateTime<Utc>) -> Result<Quote, QuoteError> + Send + Sync + 'static,
) -> Rig {
    let clock = Arc::new(TokioClock::starting_at(start));
    let store = Arc::new(MemStore::default());
    let quotes = FakeQuotes::new(clock.clone(), latency, script);
    let sink = Arc::new(MemSink::default());
    let runner = TradeRunner::new(
        store.clone(),
        analyst::SessionsCache::new(sessions()),
        quotes.clone(),
        sink.clone(),
        clock.clone(),
        settings(concurrency),
    );
    Rig {
        clock,
        store,
        quotes,
        sink,
        runner,
    }
}

pub fn signal(
    id: i64,
    ticker: &str,
    mic: &str,
    kind: DecisionKind,
    decided_at: DateTime<Utc>,
) -> TradeSignal {
    TradeSignal {
        news_id: format!("news-{id}"),
        ticker: ticker.to_owned(),
        exchange_mic: mic.to_owned(),
        lang: "en".into(),
        fiscal_period: Some("2026-Q3".into()),
        decision: kind,
        baseline: kind == DecisionKind::SkippedNoKernel,
        published_at: decided_at - chrono::Duration::milliseconds(300),
        received_at: decided_at,
        decided_at,
        context: Context::new(),
        persisted: DecisionHandle::ready(Persisted { id, kind }),
    }
}

pub fn hand_off(rig: &Rig, signal: TradeSignal) {
    rig.runner.hand_off(signal);
}

pub fn versions(sink: &MemSink, trade_id: i64) -> Vec<i64> {
    let mut found: Vec<i64> = sink
        .messages()
        .iter()
        .filter(|(_, m)| m.trade_id == trade_id)
        .map(|(_, m)| m.version)
        .collect();
    found.sort();
    found
}

pub fn horizons_of(observations: &[StoredObservation]) -> HashSet<String> {
    observations.iter().map(|o| o.horizon.clone()).collect()
}
