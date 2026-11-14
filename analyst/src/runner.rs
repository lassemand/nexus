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
use telemetry::trace::opentelemetry::trace::{SpanContext, TraceContextExt};
use telemetry::trace::opentelemetry::{Context, KeyValue};
use telemetry::trace::OpenTelemetrySpanExt;
use tokio::sync::Semaphore;
use tokio::time::Instant;
use tracing::{error, info, info_span, warn, Instrument, Span};

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
use crate::spans::{
    clock_span, context_from_traceparent, context_trace_id, end_span, fail_span,
    span_context_from_traceparent, traceparent_of,
};
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
    /// The topic simulated trades are produced on, for the producer span.
    pub trades_topic: String,
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
            trades_topic: config.trades_topic.clone(),
        }
    }
}

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

/// What a quote request is for, in `analyst.quote_purpose`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Purpose {
    Entry,
    Exit,
    Observation,
}

impl Purpose {
    fn as_str(self) -> &'static str {
        match self {
            Purpose::Entry => "entry",
            Purpose::Exit => "exit",
            Purpose::Observation => "observation",
        }
    }
}

/// A fetched quote and the `analyst.quote` span still open around it.
struct Traced {
    at: DateTime<Utc>,
    result: Result<Quote, QuoteError>,
    span: Span,
}

fn status_code(result: &Result<Quote, QuoteError>) -> i64 {
    match result {
        Ok(_) => 200,
        Err(QuoteError::Unavailable { .. }) => 503,
        Err(QuoteError::NotFound) => 404,
        Err(QuoteError::Status(code)) => i64::from(*code),
        Err(_) => 0,
    }
}

/// Where the spans of a trade's entry hang: the entry span, and the release's
/// span context to link the holding trace to.
struct Release {
    entry: Span,
    link: Option<SpanContext>,
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

    async fn fetch(
        &self,
        parent: &Span,
        purpose: Purpose,
        attempt: u32,
        ticker: &str,
        mic: &str,
    ) -> Traced {
        let span = clock_span(
            "analyst.quote",
            self.clock.now(),
            Some(&parent.context()),
            vec![
                KeyValue::new("analyst.quote_purpose", purpose.as_str()),
                KeyValue::new("analyst.attempt", i64::from(attempt)),
            ],
        );
        let _permit = self.permits.acquire().await;
        let result = self.quotes.quote(ticker, mic).await;
        Traced {
            at: self.clock.now(),
            result,
            span,
        }
    }

    fn close_quote(&self, traced: &Traced, accepted: bool, purpose: Purpose) {
        let span = &traced.span;
        span.set_attribute("http.response.status_code", status_code(&traced.result));
        if let Ok(quote) = &traced.result {
            let kind = match purpose {
                Purpose::Entry => quote.ask_type.clone(),
                _ => quote.bid_type.clone(),
            };
            let key = if purpose == Purpose::Entry {
                "analyst.ask_type"
            } else {
                "analyst.bid_type"
            };
            span.set_attribute(key, kind.unwrap_or_default());
        }
        if let Err(error) = &traced.result {
            fail_span(span, &error.to_string());
        }
        span.set_attribute("analyst.accepted", accepted);
        end_span(span, traced.at);
    }

    fn record_lateness(&self, scheduled: DateTime<Utc>, fetched: DateTime<Utc>) -> bool {
        let late = fetched - scheduled;
        metrics::quote_lateness(late.num_milliseconds() as f64 / 1000.0);
        late > chrono(LATE_AFTER)
    }

    async fn run_signal(self: Arc<Self>, signal: TradeSignal) {
        let started = self.clock.now();
        let sessions = self.sessions.get();
        let plan = plan_entry(
            &sessions,
            &signal.exchange_mic,
            signal.decided_at,
            self.settings.close_buffer,
        );
        let release_context = signal.context.clone();
        let link = {
            let span_context = release_context.span().span_context().clone();
            span_context.is_valid().then_some(span_context)
        };
        let entry_span = clock_span(
            "analyst.trade_entry",
            started,
            Some(&release_context),
            vec![
                KeyValue::new("nexus.ticker", signal.ticker.clone()),
                KeyValue::new("nexus.exchange_mic", signal.exchange_mic.clone()),
                KeyValue::new("nexus.decision", signal.decision.as_str()),
                KeyValue::new("analyst.shadow", signal.decision != DecisionKind::Buy),
            ],
        );
        let release = Release {
            entry: entry_span,
            link,
        };
        let first = match plan {
            EntryPlan::Now => Some(
                self.fetch(
                    &release.entry,
                    Purpose::Entry,
                    1,
                    &signal.ticker,
                    &signal.exchange_mic,
                )
                .await,
            ),
            _ => None,
        };
        let target = plan.target(signal.decided_at);
        if let Some(target) = target {
            release
                .entry
                .set_attribute("analyst.entry_target_at", target.to_rfc3339());
        }
        let status = match plan {
            EntryPlan::CalendarUnknown => TradeStatus::CalendarUnknown,
            _ => TradeStatus::PendingEntry,
        };
        let Some(trade) = self.create(&signal, target, status).await else {
            if let Some(first) = &first {
                self.close_quote(first, false, Purpose::Entry);
            }
            fail_span(&release.entry, "no trade");
            end_span(&release.entry, self.clock.now());
            return;
        };
        if status == TradeStatus::CalendarUnknown {
            release
                .entry
                .set_attribute("analyst.trade_status", status.as_str());
            end_span(&release.entry, self.clock.now());
            self.finish(&trade, &release.entry).await;
            return;
        }
        self.enter(trade, first, matches!(plan, EntryPlan::Now), false, release)
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
            trace_id: context_trace_id(&signal.context),
            release_traceparent: traceparent_of(&signal.context),
        };
        self.db("create_trade", || self.store.create_trade(&new))
            .await
            .flatten()
    }

    async fn enter(
        &self,
        trade: StoredTrade,
        first: Option<Traced>,
        in_session: bool,
        resumed: bool,
        release: Release,
    ) {
        let Release {
            entry: entry_span,
            link,
        } = release;
        entry_span.set_attribute("analyst.resumed", resumed);
        let Some(target) = trade.entry_target_at else {
            entry_span.set_attribute("analyst.trade_status", TradeStatus::InvalidEntry.as_str());
            end_span(&entry_span, self.clock.now());
            self.set_status(&trade, TradeStatus::InvalidEntry, &entry_span)
                .await;
            return;
        };
        if resumed && self.clock.now() > target + chrono(LATE_AFTER) {
            entry_span.set_attribute("analyst.trade_status", TradeStatus::NoEntryQuote.as_str());
            end_span(&entry_span, self.clock.now());
            self.set_status(&trade, TradeStatus::NoEntryQuote, &entry_span)
                .await;
            return;
        }
        if first.is_none() {
            let now = self.clock.now();
            if target > now {
                let wait = clock_span(
                    "analyst.wait_for_session",
                    now,
                    Some(&entry_span.context()),
                    vec![KeyValue::new(
                        "analyst.entry_target_at",
                        target.to_rfc3339(),
                    )],
                );
                self.sleep_until(target).await;
                end_span(&wait, self.clock.now());
            }
        }
        let deadline = target + chrono(self.settings.entry_max_wait);
        let mut next = first;
        let mut attempt = 0u32;
        let accepted = loop {
            attempt += 1;
            let traced = match next.take() {
                Some(traced) => traced,
                None => {
                    self.fetch(
                        &entry_span,
                        Purpose::Entry,
                        attempt,
                        &trade.ticker,
                        &trade.exchange_mic,
                    )
                    .await
                }
            };
            let wait = match &traced.result {
                Ok(quote) => match quote.entry_ask() {
                    Some(ask) => {
                        metrics::quote_request("accepted");
                        self.close_quote(&traced, true, Purpose::Entry);
                        break Some((traced.at, quote.clone(), ask));
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
            self.close_quote(&traced, false, Purpose::Entry);
            if self.clock.now() + chrono(wait) > deadline {
                break None;
            }
            tokio::time::sleep(wait).await;
        };
        let Some((at, quote, ask)) = accepted else {
            entry_span.set_attribute("analyst.trade_status", TradeStatus::NoEntryQuote.as_str());
            fail_span(&entry_span, "no_entry_quote");
            end_span(&entry_span, self.clock.now());
            self.set_status(&trade, TradeStatus::NoEntryQuote, &entry_span)
                .await;
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
            entry_span.set_attribute("analyst.trade_status", TradeStatus::InvalidEntry.as_str());
            end_span(&entry_span, at);
            self.set_status(&trade, TradeStatus::InvalidEntry, &entry_span)
                .await;
            return;
        };
        let release_to_entry_ms = (at - trade.published_at).num_milliseconds();
        let entry = Entry {
            at,
            ask,
            bid: quote.bid,
            saxo_at: quote.saxo_last_updated.clone(),
            currency: quote.currency.clone(),
            release_to_entry_ms,
            exit_target_at: Some(exit_at),
        };
        entry_span.set_attribute("analyst.release_to_entry_ms", release_to_entry_ms);
        entry_span.set_attribute("analyst.trade_status", TradeStatus::Open.as_str());
        end_span(&entry_span, at);
        let Some(opened) = self
            .db("record_entry", || self.store.record_entry(trade.id, &entry))
            .await
        else {
            return;
        };
        self.emit(&opened, &entry_span).await;
        drop(entry_span);
        self.run_open(opened, cutoff, false, link).await;
    }

    async fn resume_trade(self: Arc<Self>, trade: StoredTrade) {
        let release_context = trade
            .release_traceparent
            .as_deref()
            .and_then(context_from_traceparent);
        let link = trade
            .release_traceparent
            .as_deref()
            .and_then(span_context_from_traceparent);
        match trade.status() {
            TradeStatus::PendingEntry => {
                let entry_span = clock_span(
                    "analyst.trade_entry",
                    self.clock.now(),
                    release_context.as_ref(),
                    vec![
                        KeyValue::new("nexus.ticker", trade.ticker.clone()),
                        KeyValue::new("nexus.exchange_mic", trade.exchange_mic.clone()),
                        KeyValue::new("nexus.decision", trade.decision.clone()),
                        KeyValue::new("analyst.shadow", trade.shadow),
                    ],
                );
                let release = Release {
                    entry: entry_span,
                    link,
                };
                self.enter(trade, None, false, true, release).await;
            }
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
                    Some(cutoff) => self.run_open(trade, cutoff, true, link).await,
                    None => {
                        let span = clock_span("analyst.trade", self.clock.now(), None, Vec::new());
                        self.set_status(&trade, TradeStatus::InvalidEntry, &span)
                            .await;
                        end_span(&span, self.clock.now());
                    }
                }
            }
            _ => {}
        }
    }

    async fn run_open(
        &self,
        mut trade: StoredTrade,
        cutoff: DateTime<Utc>,
        resumed: bool,
        release: Option<SpanContext>,
    ) {
        let started = if resumed {
            self.clock.now()
        } else {
            trade.entry_at.unwrap_or_else(|| self.clock.now())
        };
        let trade_span = clock_span(
            "analyst.trade",
            started,
            None,
            vec![
                KeyValue::new("nexus.ticker", trade.ticker.clone()),
                KeyValue::new("nexus.exchange_mic", trade.exchange_mic.clone()),
                KeyValue::new("nexus.decision", trade.decision.clone()),
                KeyValue::new("analyst.shadow", trade.shadow),
                KeyValue::new("analyst.resumed", resumed),
            ],
        );
        if let Some(link) = release {
            trade_span.set_attribute("nexus.release_trace_id", link.trace_id().to_string());
            trade_span.add_link(link);
        }
        let trade_context = trade_span.context();
        if let Some(trace_id) = context_trace_id(&trade_context) {
            self.db("set_trade_trace", || {
                self.store.set_trade_trace(trade.id, &trace_id)
            })
            .await;
        }
        let (Some(entry_at), Some(exit_at)) = (trade.entry_at, trade.exit_target_at) else {
            self.set_status(&trade, TradeStatus::InvalidEntry, &trade_span)
                .await;
            end_span(&trade_span, self.clock.now());
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
                    let span = self.observation_span(&trade_context, &label, scheduled);
                    span.set_attribute("analyst.observation_status", "beyond_close");
                    end_span(&span, scheduled);
                    let observation = NewObservation {
                        horizon: label,
                        scheduled_at: scheduled,
                        fetched_at: None,
                        quote: None,
                        late: false,
                        status: ObservationStatus::BeyondClose,
                    };
                    if let Some(updated) = self.store_observation(&trade, &observation, &span).await
                    {
                        trade = updated;
                    }
                }
                Event::Observe(label) => {
                    if resumed && self.clock.now() > at + chrono(LATE_AFTER) {
                        let span = self.observation_span(&trade_context, &label, at);
                        span.set_attribute("analyst.observation_status", "missed");
                        fail_span(&span, "missed");
                        end_span(&span, at);
                        let observation = NewObservation {
                            horizon: label,
                            scheduled_at: at,
                            fetched_at: None,
                            quote: None,
                            late: false,
                            status: ObservationStatus::Missed,
                        };
                        if let Some(updated) =
                            self.store_observation(&trade, &observation, &span).await
                        {
                            trade = updated;
                        }
                        continue;
                    }
                    self.sleep_until(at).await;
                    let span = self.observation_span(&trade_context, &label, at);
                    let traced = self
                        .fetch(
                            &span,
                            Purpose::Observation,
                            1,
                            &trade.ticker,
                            &trade.exchange_mic,
                        )
                        .await;
                    let late = self.record_lateness(at, traced.at);
                    let (quote, status) = match &traced.result {
                        Ok(quote) => {
                            metrics::quote_request("ok");
                            (Some(quote.clone()), ObservationStatus::Ok)
                        }
                        Err(error) => {
                            metrics::quote_request(error.outcome());
                            (None, ObservationStatus::NoQuote)
                        }
                    };
                    self.close_quote(
                        &traced,
                        status == ObservationStatus::Ok,
                        Purpose::Observation,
                    );
                    span.set_attribute("analyst.late", late);
                    span.set_attribute("analyst.observation_status", status.as_str());
                    end_span(&span, traced.at);
                    let observation = NewObservation {
                        horizon: label,
                        scheduled_at: at,
                        fetched_at: Some(traced.at),
                        quote,
                        late,
                        status,
                    };
                    if let Some(updated) = self.store_observation(&trade, &observation, &span).await
                    {
                        trade = updated;
                    }
                }
                Event::Exit => {
                    if let Some(updated) =
                        self.exit(&trade, at, cutoff, resumed, &trade_context).await
                    {
                        trade = updated;
                    }
                }
            }
        }
        end_span(&trade_span, self.clock.now());
    }

    fn observation_span(&self, trade: &Context, horizon: &str, start: DateTime<Utc>) -> Span {
        clock_span(
            "analyst.observation",
            start,
            Some(trade),
            vec![KeyValue::new("analyst.horizon", horizon.to_owned())],
        )
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
        cause: &Span,
    ) -> Option<StoredTrade> {
        let updated = self
            .db("record_observation", || {
                self.store.record_observation(trade.id, observation)
            })
            .await?;
        self.emit(&updated, cause).await;
        Some(updated)
    }

    async fn exit(
        &self,
        trade: &StoredTrade,
        target: DateTime<Utc>,
        cutoff: DateTime<Utc>,
        resumed: bool,
        trade_context: &Context,
    ) -> Option<StoredTrade> {
        let span = clock_span(
            "analyst.exit",
            target,
            Some(trade_context),
            vec![KeyValue::new("analyst.horizon", "hold")],
        );
        if resumed && self.clock.now() > target + chrono(LATE_AFTER) {
            span.set_attribute("analyst.observation_status", "missed");
            fail_span(&span, "missed");
            end_span(&span, target);
            let observation = NewObservation {
                horizon: "hold".to_owned(),
                scheduled_at: target,
                fetched_at: None,
                quote: None,
                late: false,
                status: ObservationStatus::Missed,
            };
            let updated = self.store_observation(trade, &observation, &span).await?;
            return self
                .set_status(&updated, TradeStatus::ExitMissed, &span)
                .await;
        }
        self.sleep_until(target).await;
        let deadline = (target + chrono(EXIT_RETRY_FOR)).min(cutoff);
        let mut last: Option<(DateTime<Utc>, Result<Quote, QuoteError>)> = None;
        let mut attempt = 0u32;
        let accepted = loop {
            attempt += 1;
            let traced = self
                .fetch(
                    &span,
                    Purpose::Exit,
                    attempt,
                    &trade.ticker,
                    &trade.exchange_mic,
                )
                .await;
            let wait = match &traced.result {
                Ok(quote) => match quote.exit_bid() {
                    Some(bid) => {
                        metrics::quote_request("accepted");
                        self.close_quote(&traced, true, Purpose::Exit);
                        break Some((traced.at, quote.clone(), bid));
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
            self.close_quote(&traced, false, Purpose::Exit);
            last = Some((traced.at, traced.result));
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
                span.set_attribute("analyst.observation_status", "no_quote");
                fail_span(&span, "no_exit_quote");
                end_span(&span, self.clock.now());
                let observation = NewObservation {
                    horizon: "hold".to_owned(),
                    scheduled_at: target,
                    fetched_at,
                    quote,
                    late: false,
                    status: ObservationStatus::NoQuote,
                };
                let updated = self.store_observation(trade, &observation, &span).await?;
                self.set_status(&updated, TradeStatus::NoExitQuote, &span)
                    .await
            }
            Some((at, quote, bid)) => {
                let late = self.record_lateness(target, at);
                span.set_attribute("analyst.late", late);
                span.set_attribute("analyst.observation_status", "ok");
                end_span(&span, at);
                let observation = NewObservation {
                    horizon: "hold".to_owned(),
                    scheduled_at: target,
                    fetched_at: Some(at),
                    quote: Some(quote.clone()),
                    late,
                    status: ObservationStatus::Ok,
                };
                let updated = self.store_observation(trade, &observation, &span).await?;
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
                self.finish(&closed, &span).await;
                Some(closed)
            }
        }
    }

    async fn set_status(
        &self,
        trade: &StoredTrade,
        status: TradeStatus,
        cause: &Span,
    ) -> Option<StoredTrade> {
        let updated = self
            .db("set_trade_status", || {
                self.store.set_trade_status(trade.id, status)
            })
            .await?;
        self.finish(&updated, cause).await;
        Some(updated)
    }

    async fn finish(&self, trade: &StoredTrade, cause: &Span) {
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
        self.emit(trade, cause).await;
    }

    async fn emit(&self, trade: &StoredTrade, cause: &Span) {
        let observations = self
            .db("observations", || self.store.observations(trade.id))
            .await
            .unwrap_or_default();
        let message = to_proto(trade, &observations);
        let key = format!("{}:{}", trade.ticker, trade.exchange_mic);
        let send = info_span!(
            parent: cause,
            "send analyst.trades",
            otel.kind = "producer",
            messaging.system = "kafka",
            messaging.destination.name = self.settings.trades_topic.as_str(),
            messaging.kafka.message.key = key.as_str(),
            analyst.trade_version = message.version
        );
        let sink = Arc::clone(&self.sink);
        tokio::spawn(
            async move {
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
            }
            .instrument(send),
        );
    }
}
