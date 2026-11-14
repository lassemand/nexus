//! Runs simulated trades: from a decision handed over by the release path to
//! the entry quote, the exit and every observation, as one task per trade.
//!
//! The entry request of a decision taken during trading leaves before anything
//! else is done (no database read or write comes first); the trade row is created
//! once the decision's row is known, and a trade waiting for the session is
//! stored before it sleeps, so a restart finds it. Every quote is requested at a
//! deadline computed from the wall clock when it is scheduled and slept to with
//! `tokio::time::sleep_until`: there is no loop that checks whether it is time.
//! Quote requests of different trades run concurrently, at most
//! `QUOTE_CONCURRENCY` at once. After every state change the full trade is
//! emitted on `analyst.trades` without waiting for the broker.
//!
//! Only the read-only quote endpoint is used. Nothing is ever traded.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use rust_decimal::Decimal;
use tokio::sync::Semaphore;
use tokio::time::Instant;
use tracing::{error, info, warn};

use crate::calendar::SessionsCache;
use crate::clock::Clock;
use crate::config::{Config, Horizon};
use crate::decision::DecisionKind;
use crate::handoff::{TradeHandOff, TradeSignal};
use crate::metrics;
use crate::pnl::{compute, PnlProblem};
use crate::quotes::{Quote, QuoteError, QuoteSource};
use crate::release::{with_retry, RetryPolicy};
use crate::sink::TradeSink;
use crate::trades::{
    to_proto, Entry, Exit, NewObservation, NewTrade, ObservationStatus, StoredTrade, TradeStatus,
    TradeStore,
};
use crate::trading::{exit_target, observation_slots, plan_entry, session_cutoff, EntryPlan, Slot};

/// A quote fetched later than this after its target is `late`; after a restart
/// a target that passed longer ago than this is `missed`, never fetched.
pub const LATE_AFTER: Duration = Duration::from_secs(2);
/// Between two requests for the same quote.
pub const RETRY_EVERY: Duration = Duration::from_secs(1);
/// How long the exit quote is retried, never past the session cutoff.
pub const EXIT_RETRY_FOR: Duration = Duration::from_secs(30);
const EMIT_ATTEMPTS: u32 = 10;
const EMIT_PAUSE: Duration = Duration::from_millis(500);

/// The runner's settings, from [`Config`].
#[derive(Debug, Clone)]
pub struct RunnerSettings {
    /// How long a trade is held, in trading time.
    pub hold: Duration,
    /// When, after entry, the bid is observed.
    pub horizons: Vec<Horizon>,
    /// Continuous trading ends this long before the close.
    pub close_buffer: Duration,
    /// How long the entry quote is retried.
    pub entry_max_wait: Duration,
    /// The most quote requests in flight at once.
    pub quote_concurrency: usize,
}

impl RunnerSettings {
    /// The settings in `config`.
    pub fn from_config(config: &Config) -> RunnerSettings {
        RunnerSettings {
            hold: config.hold,
            horizons: config.obs_horizons.clone(),
            close_buffer: config.close_buffer,
            entry_max_wait: config.entry_max_wait,
            quote_concurrency: config.quote_concurrency as usize,
        }
    }
}

type Fetched = (DateTime<Utc>, Result<Quote, QuoteError>);

enum Event {
    Observe(String),
    Beyond(String),
    Exit,
}

struct Inner {
    store: Arc<dyn TradeStore>,
    sessions: SessionsCache,
    quotes: Arc<dyn QuoteSource>,
    sink: Arc<dyn TradeSink>,
    clock: Arc<dyn Clock>,
    settings: RunnerSettings,
    permits: Semaphore,
    retry: RetryPolicy,
}

/// Simulates the trade of every decision handed to it. Implements
/// [`TradeHandOff`]: handing over spawns the trade's task and returns at once.
#[derive(Clone)]
pub struct TradeRunner {
    inner: Arc<Inner>,
}

fn chrono(d: Duration) -> ChronoDuration {
    ChronoDuration::from_std(d).unwrap_or(ChronoDuration::MAX)
}

fn tradable(kind: DecisionKind) -> bool {
    matches!(
        kind,
        DecisionKind::Buy
            | DecisionKind::NoBuy
            | DecisionKind::Abstain
            | DecisionKind::SkippedNoKernel
    )
}

impl TradeRunner {
    /// A runner over `store`, reading quotes from `quotes` and emitting on `sink`.
    pub fn new(
        store: Arc<dyn TradeStore>,
        sessions: SessionsCache,
        quotes: Arc<dyn QuoteSource>,
        sink: Arc<dyn TradeSink>,
        clock: Arc<dyn Clock>,
        settings: RunnerSettings,
    ) -> TradeRunner {
        let permits = Semaphore::new(settings.quote_concurrency.max(1));
        TradeRunner {
            inner: Arc::new(Inner {
                store,
                sessions,
                quotes,
                sink,
                clock,
                settings,
                permits,
                retry: RetryPolicy::default(),
            }),
        }
    }

    /// Replaces the retry policy of database calls (tests use short pauses).
    pub fn with_retry_policy(self, retry: RetryPolicy) -> TradeRunner {
        let inner = Arc::try_unwrap(self.inner)
            .unwrap_or_else(|_| panic!("the retry policy is set before the runner is shared"));
        TradeRunner {
            inner: Arc::new(Inner { retry, ..inner }),
        }
    }

    /// Reschedules the trades that were open when the analyst stopped, and
    /// returns how many. A quote whose target passed more than 2 s ago is
    /// `missed`, not fetched late: a late price would be a different price.
    pub async fn resume(&self) -> Result<usize, sqlx::Error> {
        let open = self.inner.store.open_trades().await?;
        let count = open.len();
        for trade in open {
            let inner = Arc::clone(&self.inner);
            tokio::spawn(async move { inner.resume_trade(trade).await });
        }
        Ok(count)
    }
}

impl TradeHandOff for TradeRunner {
    fn hand_off(&self, signal: TradeSignal) {
        if !tradable(signal.decision) {
            return;
        }
        let inner = Arc::clone(&self.inner);
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move { inner.run_signal(signal).await });
            }
            Err(_) => error!("no runtime to simulate the trade on"),
        }
    }
}

impl Inner {
    async fn db<T, F, Fut>(&self, step: &'static str, operation: F) -> Option<T>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Result<T, sqlx::Error>>,
    {
        match with_retry(step, self.retry, operation).await {
            Ok(value) => Some(value),
            Err(error) => {
                error!(step, %error, "the database call of a simulated trade failed for good");
                None
            }
        }
    }

    async fn sleep_until(&self, target: DateTime<Utc>) {
        let wait = (target - self.clock.now())
            .to_std()
            .unwrap_or(Duration::ZERO);
        if !wait.is_zero() {
            tokio::time::sleep_until(Instant::now() + wait).await;
        }
    }

    async fn fetch(&self, ticker: &str, mic: &str) -> Fetched {
        let _permit = self.permits.acquire().await;
        let result = self.quotes.quote(ticker, mic).await;
        (self.clock.now(), result)
    }

    fn record_lateness(&self, scheduled: DateTime<Utc>, fetched: DateTime<Utc>) -> bool {
        let late = fetched - scheduled;
        metrics::quote_lateness(late.num_milliseconds() as f64 / 1000.0);
        late > chrono(LATE_AFTER)
    }

    async fn run_signal(self: Arc<Self>, signal: TradeSignal) {
        let sessions = self.sessions.get();
        let plan = plan_entry(
            &sessions,
            &signal.exchange_mic,
            signal.decided_at,
            self.settings.close_buffer,
        );
        let first = match plan {
            EntryPlan::Now => Some(self.fetch(&signal.ticker, &signal.exchange_mic).await),
            _ => None,
        };
        let target = plan.target(signal.decided_at);
        let status = match plan {
            EntryPlan::CalendarUnknown => TradeStatus::CalendarUnknown,
            _ => TradeStatus::PendingEntry,
        };
        let Some(trade) = self.create(&signal, target, status).await else {
            return;
        };
        if status == TradeStatus::CalendarUnknown {
            self.finish(&trade).await;
            return;
        }
        self.enter(trade, first, matches!(plan, EntryPlan::Now), false)
            .await;
    }

    async fn create(
        &self,
        signal: &TradeSignal,
        entry_target_at: Option<DateTime<Utc>>,
        status: TradeStatus,
    ) -> Option<StoredTrade> {
        let mut handle = signal.persisted.clone();
        let Some(persisted) = handle.wait().await else {
            warn!(news_id = %signal.news_id, "the decision was never stored, no simulated trade");
            return None;
        };
        if persisted.kind != signal.decision {
            info!(news_id = %signal.news_id, handed = signal.decision.as_str(), stored = persisted.kind.as_str(),
                  "another version of the report settled it, no simulated trade");
            return None;
        }
        let hold_secs = Decimal::from(self.settings.hold.as_secs());
        let new = NewTrade {
            decision_id: persisted.id,
            ticker: signal.ticker.clone(),
            exchange_mic: signal.exchange_mic.clone(),
            decision: signal.decision.as_str().to_owned(),
            published_at: signal.published_at,
            hold_minutes: (hold_secs / Decimal::from(60)).round_dp(3),
            entry_target_at,
            status,
            trace_id: None,
            release_traceparent: None,
        };
        self.db("create_trade", || self.store.create_trade(&new))
            .await
            .flatten()
    }

    async fn enter(
        &self,
        trade: StoredTrade,
        first: Option<Fetched>,
        in_session: bool,
        resumed: bool,
    ) {
        let Some(target) = trade.entry_target_at else {
            self.set_status(&trade, TradeStatus::InvalidEntry).await;
            return;
        };
        if resumed && self.clock.now() > target + chrono(LATE_AFTER) {
            self.set_status(&trade, TradeStatus::NoEntryQuote).await;
            return;
        }
        if first.is_none() {
            self.sleep_until(target).await;
        }
        let deadline = target + chrono(self.settings.entry_max_wait);
        let mut next = first;
        let accepted = loop {
            let (at, result) = match next.take() {
                Some(fetched) => fetched,
                None => self.fetch(&trade.ticker, &trade.exchange_mic).await,
            };
            let wait = match result {
                Ok(quote) => match quote.entry_ask() {
                    Some(ask) => {
                        metrics::quote_request("accepted");
                        break Some((at, quote, ask));
                    }
                    None => {
                        metrics::quote_request("rejected");
                        RETRY_EVERY
                    }
                },
                Err(error) => {
                    metrics::quote_request(error.outcome());
                    match error {
                        QuoteError::Unavailable { retry_after } => retry_after,
                        _ => RETRY_EVERY,
                    }
                }
            };
            if self.clock.now() + chrono(wait) > deadline {
                break None;
            }
            tokio::time::sleep(wait).await;
        };
        let Some((at, quote, ask)) = accepted else {
            self.set_status(&trade, TradeStatus::NoEntryQuote).await;
            return;
        };
        self.record_lateness(target, at);
        if in_session {
            metrics::entry_delay((at - target).to_std().unwrap_or(Duration::ZERO));
        }
        let sessions = self.sessions.get();
        let cutoff = session_cutoff(
            &sessions,
            &trade.exchange_mic,
            at,
            self.settings.close_buffer,
        );
        let Some((cutoff, exit_at)) =
            cutoff.and_then(|c| exit_target(at, self.settings.hold, c).map(|e| (c, e)))
        else {
            self.set_status(&trade, TradeStatus::InvalidEntry).await;
            return;
        };
        let entry = Entry {
            at,
            ask,
            bid: quote.bid,
            saxo_at: quote.saxo_last_updated.clone(),
            currency: quote.currency.clone(),
            release_to_entry_ms: (at - trade.published_at).num_milliseconds(),
            exit_target_at: Some(exit_at),
        };
        let Some(opened) = self
            .db("record_entry", || self.store.record_entry(trade.id, &entry))
            .await
        else {
            return;
        };
        self.emit(&opened).await;
        self.run_open(opened, cutoff, false).await;
    }

    async fn resume_trade(self: Arc<Self>, trade: StoredTrade) {
        match trade.status() {
            TradeStatus::PendingEntry => self.enter(trade, None, false, true).await,
            TradeStatus::Open => {
                let sessions = self.sessions.get();
                let cutoff = trade.entry_at.and_then(|at| {
                    session_cutoff(
                        &sessions,
                        &trade.exchange_mic,
                        at,
                        self.settings.close_buffer,
                    )
                });
                match cutoff {
                    Some(cutoff) => self.run_open(trade, cutoff, true).await,
                    None => {
                        self.set_status(&trade, TradeStatus::InvalidEntry).await;
                    }
                }
            }
            _ => {}
        }
    }

    async fn run_open(&self, mut trade: StoredTrade, cutoff: DateTime<Utc>, resumed: bool) {
        let (Some(entry_at), Some(exit_at)) = (trade.entry_at, trade.exit_target_at) else {
            self.set_status(&trade, TradeStatus::InvalidEntry).await;
            return;
        };
        let have: Vec<String> = self
            .db("observations", || self.store.observations(trade.id))
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|o| o.horizon)
            .collect();
        let mut events: Vec<(DateTime<Utc>, Event)> =
            observation_slots(entry_at, cutoff, &self.settings.horizons)
                .into_iter()
                .filter(|(label, _)| !have.contains(label))
                .map(|(label, slot)| match slot {
                    Slot::At(at) => (at, Event::Observe(label)),
                    Slot::BeyondClose => (entry_at, Event::Beyond(label)),
                })
                .collect();
        if !have.iter().any(|h| h == "hold") && trade.status() == TradeStatus::Open {
            events.push((exit_at, Event::Exit));
        }
        events.sort_by_key(|(at, event)| (*at, matches!(event, Event::Exit)));
        for (at, event) in events {
            match event {
                Event::Beyond(label) => {
                    let scheduled = self.beyond_time(entry_at, &label);
                    let observation = NewObservation {
                        horizon: label,
                        scheduled_at: scheduled,
                        fetched_at: None,
                        quote: None,
                        late: false,
                        status: ObservationStatus::BeyondClose,
                    };
                    if let Some(updated) = self.store_observation(&trade, &observation).await {
                        trade = updated;
                    }
                }
                Event::Observe(label) => {
                    if resumed && self.clock.now() > at + chrono(LATE_AFTER) {
                        let observation = NewObservation {
                            horizon: label,
                            scheduled_at: at,
                            fetched_at: None,
                            quote: None,
                            late: false,
                            status: ObservationStatus::Missed,
                        };
                        if let Some(updated) = self.store_observation(&trade, &observation).await {
                            trade = updated;
                        }
                        continue;
                    }
                    self.sleep_until(at).await;
                    let (fetched, result) = self.fetch(&trade.ticker, &trade.exchange_mic).await;
                    let late = self.record_lateness(at, fetched);
                    let (quote, status) = match result {
                        Ok(quote) => {
                            metrics::quote_request("ok");
                            (Some(quote), ObservationStatus::Ok)
                        }
                        Err(error) => {
                            metrics::quote_request(error.outcome());
                            (None, ObservationStatus::NoQuote)
                        }
                    };
                    let observation = NewObservation {
                        horizon: label,
                        scheduled_at: at,
                        fetched_at: Some(fetched),
                        quote,
                        late,
                        status,
                    };
                    if let Some(updated) = self.store_observation(&trade, &observation).await {
                        trade = updated;
                    }
                }
                Event::Exit => {
                    if let Some(updated) = self.exit(&trade, at, cutoff, resumed).await {
                        trade = updated;
                    }
                }
            }
        }
    }

    fn beyond_time(&self, entry_at: DateTime<Utc>, label: &str) -> DateTime<Utc> {
        self.settings
            .horizons
            .iter()
            .find(|h| h.label() == label)
            .and_then(|h| match h {
                Horizon::After(d) => Some(entry_at + chrono(*d)),
                Horizon::Close => None,
            })
            .unwrap_or(entry_at)
    }

    async fn store_observation(
        &self,
        trade: &StoredTrade,
        observation: &NewObservation,
    ) -> Option<StoredTrade> {
        let updated = self
            .db("record_observation", || {
                self.store.record_observation(trade.id, observation)
            })
            .await?;
        self.emit(&updated).await;
        Some(updated)
    }

    async fn exit(
        &self,
        trade: &StoredTrade,
        target: DateTime<Utc>,
        cutoff: DateTime<Utc>,
        resumed: bool,
    ) -> Option<StoredTrade> {
        if resumed && self.clock.now() > target + chrono(LATE_AFTER) {
            let observation = NewObservation {
                horizon: "hold".to_owned(),
                scheduled_at: target,
                fetched_at: None,
                quote: None,
                late: false,
                status: ObservationStatus::Missed,
            };
            let updated = self.store_observation(trade, &observation).await?;
            return self.set_status(&updated, TradeStatus::ExitMissed).await;
        }
        self.sleep_until(target).await;
        let deadline = (target + chrono(EXIT_RETRY_FOR)).min(cutoff);
        let mut last: Option<Fetched> = None;
        let accepted = loop {
            let (at, result) = self.fetch(&trade.ticker, &trade.exchange_mic).await;
            let wait = match &result {
                Ok(quote) => match quote.exit_bid() {
                    Some(bid) => {
                        metrics::quote_request("accepted");
                        break Some((at, quote.clone(), bid));
                    }
                    None => {
                        metrics::quote_request("rejected");
                        RETRY_EVERY
                    }
                },
                Err(error) => {
                    metrics::quote_request(error.outcome());
                    match error {
                        QuoteError::Unavailable { retry_after } => *retry_after,
                        _ => RETRY_EVERY,
                    }
                }
            };
            last = Some((at, result));
            if self.clock.now() + chrono(wait) > deadline {
                break None;
            }
            tokio::time::sleep(wait).await;
        };
        match accepted {
            None => {
                let (fetched_at, quote) = match last {
                    Some((at, Ok(quote))) => (Some(at), Some(quote)),
                    Some((at, Err(_))) => (Some(at), None),
                    None => (None, None),
                };
                let observation = NewObservation {
                    horizon: "hold".to_owned(),
                    scheduled_at: target,
                    fetched_at,
                    quote,
                    late: false,
                    status: ObservationStatus::NoQuote,
                };
                let updated = self.store_observation(trade, &observation).await?;
                self.set_status(&updated, TradeStatus::NoExitQuote).await
            }
            Some((at, quote, bid)) => {
                let late = self.record_lateness(target, at);
                let observation = NewObservation {
                    horizon: "hold".to_owned(),
                    scheduled_at: target,
                    fetched_at: Some(at),
                    quote: Some(quote.clone()),
                    late,
                    status: ObservationStatus::Ok,
                };
                let updated = self.store_observation(trade, &observation).await?;
                let (pnl, status) = match updated.entry_ask {
                    Some(entry_ask) => match compute(
                        entry_ask,
                        bid,
                        updated.currency.as_deref(),
                        quote.currency.as_deref(),
                    ) {
                        Ok(pnl) => (Some(pnl), TradeStatus::Closed),
                        Err(PnlProblem::CurrencyMismatch) => (None, TradeStatus::CurrencyMismatch),
                        Err(PnlProblem::NoEntryPrice) => (None, TradeStatus::InvalidEntry),
                    },
                    None => (None, TradeStatus::InvalidEntry),
                };
                let exit = Exit {
                    at: Some(at),
                    bid: Some(bid),
                    ask: quote.ask,
                    pnl,
                    status,
                };
                let closed = self
                    .db("record_exit", || self.store.record_exit(trade.id, &exit))
                    .await?;
                self.finish(&closed).await;
                Some(closed)
            }
        }
    }

    async fn set_status(&self, trade: &StoredTrade, status: TradeStatus) -> Option<StoredTrade> {
        let updated = self
            .db("set_trade_status", || {
                self.store.set_trade_status(trade.id, status)
            })
            .await?;
        self.finish(&updated).await;
        Some(updated)
    }

    async fn finish(&self, trade: &StoredTrade) {
        let status = trade.status();
        metrics::sim_trade(status.as_str(), trade.shadow);
        info!(
            ticker = %trade.ticker,
            exchange_mic = %trade.exchange_mic,
            decision = %trade.decision,
            shadow = trade.shadow,
            status = status.as_str(),
            entry_ask = trade.entry_ask.map(|d| d.to_string()).unwrap_or_default(),
            exit_bid = trade.exit_bid.map(|d| d.to_string()).unwrap_or_default(),
            pnl_pct = trade.pnl_pct.map(|d| d.to_string()).unwrap_or_default(),
            release_to_entry_ms = trade.release_to_entry_ms.unwrap_or(0),
            "simulated trade"
        );
        self.emit(trade).await;
    }

    async fn emit(&self, trade: &StoredTrade) {
        let observations = self
            .db("observations", || self.store.observations(trade.id))
            .await
            .unwrap_or_default();
        let message = to_proto(trade, &observations);
        let key = format!("{}:{}", trade.ticker, trade.exchange_mic);
        let sink = Arc::clone(&self.sink);
        tokio::spawn(async move {
            for attempt in 1..=EMIT_ATTEMPTS {
                match sink.emit(&key, &message).await {
                    Ok(()) => return,
                    Err(error) => {
                        warn!(attempt, %error, "emitting the simulated trade failed");
                        tokio::time::sleep(EMIT_PAUSE).await;
                    }
                }
            }
            error!(
                key,
                version = message.version,
                "the simulated trade could not be emitted"
            );
        });
    }
}
