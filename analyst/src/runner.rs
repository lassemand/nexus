//! Runs simulated trades: from a decision handed over by the release path to
//! the entry quote, the exit and every observation, as one task per trade.
//!
//! Prices come from the [`QuoteBook`], the latest quote `saxo-stream` streamed
//! for each watched instrument. The entry price of a decision taken during
//! trading is read from the book inside [`TradeHandOff::hand_off`], in the
//! decision's own task and before anything is spawned, awaited, logged or
//! traced, so it is the quote held at the decision instant. If it is not
//! tradable the trade waits for the next tradable update; if the stream is not
//! healthy, or the instrument was not on the watch list (an unscheduled report),
//! the price is requested with `GET /quote` instead, and that fallback quote is
//! accepted only if Saxo's timestamp on it is at most `QUOTE_MAX_AGE` old. While
//! the stream is healthy a held quote is current whatever its age: Saxo pushes
//! only on change.
//!
//! The trade row is created once the decision's row is known, and a trade
//! waiting for the session is stored before it sleeps, so a restart finds it.
//! Every exit and observation is taken at a deadline computed from the wall clock
//! when it is scheduled and slept to with `tokio::time::sleep_until`: there is no
//! loop that checks whether it is time. Fallback requests of different trades run
//! concurrently, at most `QUOTE_CONCURRENCY` at once. After every state change the
//! full trade is emitted on `analyst.trades` without waiting for the broker.
//!
//! Only streamed quotes and the read-only quote endpoint are used. Nothing is
//! ever traded.

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
use crate::quote_book::{key, Held, Origin, Pair, Pin, QuoteBook};
use crate::quotes::{Quote, QuoteError, QuoteSource};
use crate::release::{with_retry, RetryPolicy};
use crate::sink::TradeSink;
use crate::spans::{
    clock_span, context_from_traceparent, context_trace_id, end_span, fail_span,
    span_context_from_traceparent, traceparent_of,
};
use crate::trades::{
    to_proto, Entry, Exit, NewObservation, NewTrade, ObservationStatus, QuoteInfo, StoredTrade,
    TradeStatus, TradeStore,
};
use crate::trading::{exit_target, observation_slots, plan_entry, session_cutoff, EntryPlan, Slot};

/// A quote fetched later than this after its target is `late`; after a restart
/// a target that passed longer ago than this is `missed`, never fetched.
pub const LATE_AFTER: Duration = Duration::from_secs(2);
/// Between two requests for the same quote.
pub const RETRY_EVERY: Duration = Duration::from_secs(1);
/// How long the exit quote is retried, never past the session cutoff.
pub const EXIT_RETRY_FOR: Duration = Duration::from_secs(30);
/// How long an exit or observation waits for a tradable bid update from the
/// stream, never past the session cutoff.
pub const BID_WAIT: Duration = Duration::from_secs(30);
/// How often a wait for a streamed update looks at the stream's health.
const HEALTH_RECHECK: Duration = Duration::from_millis(250);
/// A price whose pair has no quote in the book this long after its target is
/// requested instead of waited for.
pub const NO_QUOTE_FALLBACK: Duration = Duration::from_secs(2);
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
    /// A quote fetched by request is used only if Saxo's timestamp on it is at
    /// most this old.
    pub quote_max_age: Duration,
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
            quote_max_age: config.quote_max_age,
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
    book: Arc<QuoteBook>,
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
        let book = QuoteBook::disabled(clock.clone());
        TradeRunner {
            inner: Arc::new(Inner {
                store,
                sessions,
                quotes,
                book,
                sink,
                clock,
                settings,
                permits,
                retry: RetryPolicy::default(),
            }),
        }
    }

    /// Reads prices from `book` instead of the default book, which nothing feeds
    /// and which is therefore never healthy (every price is requested).
    pub fn with_quote_book(self, book: Arc<QuoteBook>) -> TradeRunner {
        let inner = Arc::try_unwrap(self.inner)
            .unwrap_or_else(|_| panic!("the quote book is set before the runner is shared"));
        TradeRunner {
            inner: Arc::new(Inner { book, ..inner }),
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
        let pre = inner.pre_read(&signal);
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move { inner.run_signal(signal, pre).await });
            }
            Err(_) => error!("no runtime to simulate the trade on"),
        }
    }
}

/// Where a trade's entry price comes from.
enum Feed {
    /// The stream was healthy and the instrument watched when the decision was
    /// taken: this is the quote the book held at that instant, read before
    /// anything else was done.
    Stream(Option<Held>),
    /// The price is requested from the first moment (stream not healthy, or an
    /// unscheduled instrument).
    Request,
    /// The market is closed (or the trade was resumed): the source is chosen at
    /// the session open.
    Deferred,
}

/// Everything read at the decision instant, before a task is spawned.
struct PreRead {
    started: DateTime<Utc>,
    plan: EntryPlan,
    feed: Feed,
    pin: Pin,
    read_elapsed: Duration,
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

/// Which side of the quote a price is taken from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Side {
    /// The ask, paid to buy.
    Ask,
    /// The bid, received to sell.
    Bid,
}

fn price_of(quote: &Quote, side: Side) -> Option<Decimal> {
    match side {
        Side::Ask => quote.entry_ask(),
        Side::Bid => quote.exit_bid(),
    }
}

/// The tradable price of a held quote on `side`, if it may be used: Saxo's
/// timestamp must not be before `min_time`, and a quote without a readable one
/// never qualifies when a minimum is set.
fn acceptable(held: &Held, side: Side, min_time: Option<DateTime<Utc>>) -> Option<Decimal> {
    let price = price_of(&held.quote, side)?;
    if let Some(min) = min_time {
        if held.saxo_time.is_none_or(|time| time < min) {
            return None;
        }
    }
    Some(price)
}

/// What a price is looked for: where, which side, and until when.
struct Wanted {
    pair: Pair,
    side: Side,
    purpose: Purpose,
    min_time: Option<DateTime<Utc>>,
    target: DateTime<Utc>,
    deadline: DateTime<Utc>,
}

/// How waiting for a streamed price ended.
enum StreamResult {
    Taken {
        held: Held,
        price: Decimal,
        at: DateTime<Utc>,
        waited: bool,
        immediate: bool,
    },
    TimedOut {
        last: Option<Held>,
    },
    /// The book cannot answer: the stream is unhealthy, `saxo-stream` reported
    /// the pair as failing, or no quote arrived for it. The price is requested.
    Unavailable,
}

/// How a price was found, from the stream or by request.
enum Priced {
    Taken {
        at: DateTime<Utc>,
        quote: Quote,
        price: Decimal,
        info: QuoteInfo,
    },
    TooOld {
        at: DateTime<Utc>,
        quote: Quote,
        info: QuoteInfo,
    },
    Failed {
        last: Option<(DateTime<Utc>, Result<Quote, QuoteError>)>,
        info: Option<QuoteInfo>,
    },
}

/// How an observation went.
struct Observed {
    fetched_at: Option<DateTime<Utc>>,
    quote: Option<Quote>,
    status: ObservationStatus,
    info: Option<QuoteInfo>,
}

/// A fetched quote and when its request started and was answered; its span is
/// made afterwards, so building it never sits in front of the request.
struct Raw {
    start: DateTime<Utc>,
    at: DateTime<Utc>,
    result: Result<Quote, QuoteError>,
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

    async fn fetch_raw(&self, ticker: &str, mic: &str) -> Raw {
        let start = self.clock.now();
        let _permit = self.permits.acquire().await;
        let result = self.quotes.quote(ticker, mic).await;
        Raw {
            start,
            at: self.clock.now(),
            result,
        }
    }

    fn trace_quote(&self, parent: &Span, purpose: Purpose, attempt: u32, raw: Raw) -> Traced {
        let span = clock_span(
            "analyst.quote",
            raw.start,
            Some(&parent.context()),
            vec![
                KeyValue::new("analyst.quote_purpose", purpose.as_str()),
                KeyValue::new("analyst.attempt", i64::from(attempt)),
            ],
        );
        Traced {
            at: raw.at,
            result: raw.result,
            span,
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
        let raw = self.fetch_raw(ticker, mic).await;
        self.trace_quote(parent, purpose, attempt, raw)
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

    /// Everything that must be read at the decision instant, in the decision's
    /// own task: the clock, the entry plan and, when the stream is healthy and the
    /// instrument watched, the quote the book holds now. Nothing is awaited,
    /// logged or traced before the read.
    fn pre_read(&self, signal: &TradeSignal) -> PreRead {
        let started = self.clock.now();
        let real = std::time::Instant::now();
        let pair = key(&signal.ticker, &signal.exchange_mic);
        let sessions = self.sessions.get();
        let plan = plan_entry(
            &sessions,
            &signal.exchange_mic,
            signal.decided_at,
            self.settings.close_buffer,
        );
        let watched = self.book.is_watched(&pair);
        let healthy = self.book.is_healthy();
        let feed = match plan {
            EntryPlan::Now if watched && healthy => Feed::Stream(self.book.current(&pair)),
            EntryPlan::Now => Feed::Request,
            _ => Feed::Deferred,
        };
        let read_elapsed = real.elapsed();
        let pin = self.book.pin(pair);
        PreRead {
            started,
            plan,
            feed,
            pin,
            read_elapsed,
        }
    }

    async fn run_signal(self: Arc<Self>, signal: TradeSignal, pre: PreRead) {
        let PreRead {
            started,
            plan,
            feed,
            pin,
            read_elapsed,
        } = pre;
        let first_raw = match (&feed, plan) {
            (Feed::Request, EntryPlan::Now) => {
                Some(self.fetch_raw(&signal.ticker, &signal.exchange_mic).await)
            }
            _ => None,
        };
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
        let first = first_raw.map(|raw| self.trace_quote(&release.entry, Purpose::Entry, 1, raw));
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
        self.enter(
            trade,
            first,
            feed,
            matches!(plan, EntryPlan::Now),
            false,
            release,
            pin,
            read_elapsed,
        )
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

    fn annotate(span: &Span, info: &QuoteInfo) {
        span.set_attribute("analyst.quote_source", info.source.as_str());
        if let Some(age) = info.age_ms {
            span.set_attribute("analyst.quote_age_ms", age);
        }
        if let Some(time) = info.saxo_time {
            span.set_attribute("analyst.quote_saxo_time", time.to_rfc3339());
        }
    }

    fn fresh(&self, info: &QuoteInfo) -> bool {
        let limit = i64::try_from(self.settings.quote_max_age.as_millis()).unwrap_or(i64::MAX);
        info.age_ms.is_some_and(|age| age <= limit)
    }

    fn quote_taken(info: &QuoteInfo, purpose: Purpose) {
        if let Some(age) = info.age_ms {
            metrics::quote_age(info.source.as_str(), age as f64 / 1000.0);
        }
        metrics::quote(info.source.as_str(), purpose.as_str(), "ok");
    }

    /// Waits for a tradable quote on `want.side` from the stream. Reads the held
    /// quote first (`held` when the caller read it already, at the decision
    /// instant); if that is not acceptable, waits for updates until
    /// `want.deadline`. `want.min_time` is the earliest acceptable Saxo
    /// timestamp. Gives up on an unhealthy stream, on a pair `saxo-stream`
    /// reported as failing, and on a pair with no quote [`NO_QUOTE_FALLBACK`]
    /// after the target, so the caller can request instead.
    async fn stream_price(
        &self,
        parent: &Span,
        want: &Wanted,
        held: Option<Option<Held>>,
    ) -> StreamResult {
        let side = want.side;
        let min_time = want.min_time;
        let immediate = held.is_some();
        let mut last = self
            .book
            .live(held.unwrap_or_else(|| self.book.current(&want.pair)));
        if let Some(h) = &last {
            if let Some(price) = acceptable(h, side, min_time) {
                return StreamResult::Taken {
                    held: h.clone(),
                    price,
                    at: self.clock.now(),
                    waited: false,
                    immediate,
                };
            }
        }
        if !self.book.is_healthy() {
            return StreamResult::Unavailable;
        }
        let mut rx = self.book.subscribe(&want.pair);
        let current = self.book.live(rx.borrow_and_update().clone());
        if let Some(h) = &current {
            if let Some(price) = acceptable(h, side, min_time) {
                return StreamResult::Taken {
                    held: h.clone(),
                    price,
                    at: self.clock.now(),
                    waited: false,
                    immediate: false,
                };
            }
            last = current;
        }
        let started = self.clock.now();
        let wait = clock_span(
            "analyst.wait_for_quote",
            started,
            Some(&parent.context()),
            vec![KeyValue::new(
                "analyst.quote_purpose",
                want.purpose.as_str(),
            )],
        );
        let result = loop {
            let now = self.clock.now();
            if !self.book.is_healthy() {
                break StreamResult::Unavailable;
            }
            if self.book.pair_failing(&want.pair)
                || (last.is_none() && now >= want.target + chrono(NO_QUOTE_FALLBACK))
            {
                break StreamResult::Unavailable;
            }
            if now >= want.deadline {
                break StreamResult::TimedOut { last };
            }
            let remaining = (want.deadline - now).to_std().unwrap_or(Duration::ZERO);
            match tokio::time::timeout(remaining.min(HEALTH_RECHECK), rx.changed()).await {
                Ok(Ok(())) => {
                    let update = self.book.live(rx.borrow_and_update().clone());
                    if let Some(h) = update {
                        if let Some(price) = acceptable(&h, side, min_time) {
                            if self.book.is_healthy() {
                                break StreamResult::Taken {
                                    held: h,
                                    price,
                                    at: self.clock.now(),
                                    waited: true,
                                    immediate: false,
                                };
                            }
                            break StreamResult::Unavailable;
                        }
                        last = Some(h);
                    }
                }
                Ok(Err(_)) => break StreamResult::TimedOut { last },
                Err(_) => {}
            }
        };
        match &result {
            StreamResult::TimedOut { last } => {
                metrics::quote(
                    "stream",
                    want.purpose.as_str(),
                    if last.is_some() {
                        "not_tradable"
                    } else {
                        "no_quote"
                    },
                );
                fail_span(&wait, "no tradable update");
            }
            StreamResult::Unavailable => fail_span(&wait, "stream cannot answer"),
            StreamResult::Taken { .. } => {}
        }
        end_span(&wait, self.clock.now());
        result
    }

    /// The entry price: from the stream when it is the source, requested
    /// otherwise or when the stream cannot answer. At a session open
    /// (`Feed::Deferred`) a quote stamped before the open is no opening price yet.
    #[allow(clippy::too_many_arguments)]
    async fn entry_price(
        &self,
        trade: &StoredTrade,
        entry_span: &Span,
        feed: Feed,
        first: Option<Traced>,
        target: DateTime<Utc>,
        deadline: DateTime<Utc>,
        read_elapsed: Duration,
        in_session: bool,
    ) -> Priced {
        let at_open = matches!(feed, Feed::Deferred).then_some(target);
        let (held, min_time) = match feed {
            Feed::Stream(held) => (Some(held), None),
            Feed::Deferred if self.book.is_healthy() => (None, Some(target)),
            Feed::Deferred | Feed::Request => {
                return self
                    .request_price(
                        entry_span,
                        Purpose::Entry,
                        &trade.ticker,
                        &trade.exchange_mic,
                        first,
                        at_open,
                        deadline,
                    )
                    .await
            }
        };
        let want = Wanted {
            pair: key(&trade.ticker, &trade.exchange_mic),
            side: Side::Ask,
            purpose: Purpose::Entry,
            min_time,
            target,
            deadline,
        };
        match self.stream_price(entry_span, &want, held).await {
            StreamResult::Taken {
                held,
                price,
                at,
                waited,
                immediate,
            } => {
                let at = if immediate && !waited { target } else { at };
                let info = QuoteInfo::of(&held.quote, Origin::Stream, at);
                Self::quote_taken(&info, Purpose::Entry);
                if in_session {
                    metrics::entry_delay(if immediate && !waited {
                        read_elapsed
                    } else {
                        (at - target).to_std().unwrap_or(Duration::ZERO)
                    });
                }
                Priced::Taken {
                    at,
                    quote: held.quote,
                    price,
                    info,
                }
            }
            StreamResult::TimedOut { .. } => Priced::Failed {
                last: None,
                info: None,
            },
            StreamResult::Unavailable => {
                self.request_price(
                    entry_span,
                    Purpose::Entry,
                    &trade.ticker,
                    &trade.exchange_mic,
                    None,
                    at_open,
                    deadline,
                )
                .await
            }
        }
    }

    /// Requests a price with `GET /quote` for `purpose`, retrying a quote that is
    /// not tradable (or, with `min_time`, not stamped at or after it) every
    /// [`RETRY_EVERY`] until `deadline`, and a failed request after its own pause.
    /// The first tradable quote is used if Saxo's timestamp on it is at most
    /// `QUOTE_MAX_AGE` old, else the price is `TooOld` and the search ends. With
    /// `min_time` the quotes stamped too early are only kept: if the wait ends
    /// and the newest of them is older than `QUOTE_MAX_AGE` the price is `TooOld`,
    /// else it `Failed`. One attempt is made when `deadline` has passed.
    #[allow(clippy::too_many_arguments)]
    async fn request_price(
        &self,
        span: &Span,
        purpose: Purpose,
        ticker: &str,
        mic: &str,
        first: Option<Traced>,
        min_time: Option<DateTime<Utc>>,
        deadline: DateTime<Utc>,
    ) -> Priced {
        let side = match purpose {
            Purpose::Entry => Side::Ask,
            _ => Side::Bid,
        };
        let label = purpose.as_str();
        let mut next = first;
        let mut attempt = 0u32;
        let mut early: Option<(DateTime<Utc>, Quote, QuoteInfo)> = None;
        let mut last: Option<(DateTime<Utc>, Result<Quote, QuoteError>)>;
        loop {
            attempt += 1;
            let traced = match next.take() {
                Some(traced) => traced,
                None => self.fetch(span, purpose, attempt, ticker, mic).await,
            };
            let wait = match &traced.result {
                Ok(quote) => match price_of(quote, side) {
                    Some(price) => {
                        let info = QuoteInfo::of(quote, Origin::Request, traced.at);
                        if min_time.is_some_and(|min| info.saxo_time.is_none_or(|t| t < min)) {
                            metrics::quote("request", label, "rejected");
                            let newer = early
                                .as_ref()
                                .is_none_or(|(_, _, old)| info.saxo_time > old.saxo_time);
                            if newer {
                                early = Some((traced.at, quote.clone(), info));
                            }
                            RETRY_EVERY
                        } else if !self.fresh(&info) {
                            metrics::quote("request", label, "too_old");
                            self.close_quote(&traced, false, purpose);
                            return Priced::TooOld {
                                at: traced.at,
                                quote: quote.clone(),
                                info,
                            };
                        } else {
                            metrics::quote("request", label, "accepted");
                            metrics::quote_age("request", info.age_ms.unwrap_or(0) as f64 / 1000.0);
                            self.close_quote(&traced, true, purpose);
                            return Priced::Taken {
                                at: traced.at,
                                quote: quote.clone(),
                                price,
                                info,
                            };
                        }
                    }
                    None => {
                        metrics::quote("request", label, "rejected");
                        RETRY_EVERY
                    }
                },
                Err(error) => {
                    metrics::quote("request", label, error.outcome());
                    match error {
                        QuoteError::Unavailable { retry_after } => *retry_after,
                        _ => RETRY_EVERY,
                    }
                }
            };
            self.close_quote(&traced, false, purpose);
            last = Some((traced.at, traced.result));
            if self.clock.now() + chrono(wait) > deadline {
                if let Some((at, quote, info)) = early {
                    if !self.fresh(&info) {
                        metrics::quote("request", label, "too_old");
                        return Priced::TooOld { at, quote, info };
                    }
                }
                let info = match &last {
                    Some((at, Ok(quote))) => Some(QuoteInfo::of(quote, Origin::Request, *at)),
                    _ => None,
                };
                return Priced::Failed { last, info };
            }
            tokio::time::sleep(wait).await;
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn enter(
        &self,
        trade: StoredTrade,
        first: Option<Traced>,
        feed: Feed,
        in_session: bool,
        resumed: bool,
        release: Release,
        pin: Pin,
        read_elapsed: Duration,
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
        if matches!(feed, Feed::Deferred) {
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
        let outcome = self
            .entry_price(
                &trade,
                &entry_span,
                feed,
                first,
                target,
                deadline,
                read_elapsed,
                in_session,
            )
            .await;
        let (at, quote, ask, info) = match outcome {
            Priced::Taken {
                at,
                quote,
                price,
                info,
            } => (at, quote, price, info),
            Priced::TooOld { info, .. } => {
                Self::annotate(&entry_span, &info);
                entry_span.set_attribute("analyst.trade_status", TradeStatus::QuoteTooOld.as_str());
                fail_span(&entry_span, "quote_too_old");
                end_span(&entry_span, self.clock.now());
                if let Some(rejected) = self
                    .db("reject_entry", || {
                        self.store
                            .reject_entry(trade.id, TradeStatus::QuoteTooOld, &info)
                    })
                    .await
                {
                    self.finish(&rejected, &entry_span).await;
                }
                return;
            }
            Priced::Failed { .. } => {
                entry_span
                    .set_attribute("analyst.trade_status", TradeStatus::NoEntryQuote.as_str());
                fail_span(&entry_span, "no_entry_quote");
                end_span(&entry_span, self.clock.now());
                self.set_status(&trade, TradeStatus::NoEntryQuote, &entry_span)
                    .await;
                return;
            }
        };
        self.record_lateness(target, at);
        if in_session && info.source == Origin::Request {
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
            info,
        };
        Self::annotate(&entry_span, &info);
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
        self.run_open(opened, cutoff, false, link, pin).await;
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
        let pin = self.book.pin(key(&trade.ticker, &trade.exchange_mic));
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
                self.enter(
                    trade,
                    None,
                    Feed::Deferred,
                    false,
                    true,
                    release,
                    pin,
                    Duration::ZERO,
                )
                .await;
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
                    Some(cutoff) => self.run_open(trade, cutoff, true, link, pin).await,
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
        _pin: Pin,
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
            let store = Arc::clone(&self.store);
            let retry = self.retry;
            let id = trade.id;
            tokio::spawn(async move {
                if let Err(error) = with_retry("set_trade_trace", retry, || {
                    store.set_trade_trace(id, &trace_id)
                })
                .await
                {
                    error!(%error, "the trace id of a simulated trade could not be stored");
                }
            });
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
        let targets: Vec<DateTime<Utc>> = events.iter().map(|(at, _)| *at).collect();
        for (index, (at, event)) in events.into_iter().enumerate() {
            let next = targets.get(index + 1).copied();
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
                        info: None,
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
                            info: None,
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
                    let observed = self
                        .observe(&span, &trade, at, cutoff, next, label == "close")
                        .await;
                    let taken_at = observed.fetched_at.unwrap_or_else(|| self.clock.now());
                    let late = self.record_lateness(at, taken_at);
                    if let Some(info) = &observed.info {
                        Self::annotate(&span, info);
                    }
                    span.set_attribute("analyst.late", late);
                    span.set_attribute("analyst.observation_status", observed.status.as_str());
                    end_span(&span, taken_at);
                    let observation = NewObservation {
                        horizon: label,
                        scheduled_at: at,
                        fetched_at: observed.fetched_at,
                        quote: observed.quote,
                        late,
                        status: observed.status,
                        info: observed.info,
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
                info: None,
            };
            let updated = self.store_observation(trade, &observation, &span).await?;
            return self
                .set_status(&updated, TradeStatus::ExitMissed, &span)
                .await;
        }
        self.sleep_until(target).await;
        let outcome = self.exit_price(&span, trade, target, cutoff).await;
        match outcome {
            Priced::Failed { last, info } => {
                let (fetched_at, quote) = match last {
                    Some((at, Ok(quote))) => (Some(at), Some(quote)),
                    Some((at, Err(_))) => (Some(at), None),
                    None => (None, None),
                };
                if let Some(info) = &info {
                    Self::annotate(&span, info);
                }
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
                    info,
                };
                let updated = self.store_observation(trade, &observation, &span).await?;
                self.set_status(&updated, TradeStatus::NoExitQuote, &span)
                    .await
            }
            Priced::TooOld { at, quote, info } => {
                Self::annotate(&span, &info);
                span.set_attribute("analyst.observation_status", "quote_too_old");
                fail_span(&span, "exit_quote_too_old");
                end_span(&span, at);
                let observation = NewObservation {
                    horizon: "hold".to_owned(),
                    scheduled_at: target,
                    fetched_at: Some(at),
                    quote: Some(quote),
                    late: false,
                    status: ObservationStatus::QuoteTooOld,
                    info: Some(info),
                };
                let updated = self.store_observation(trade, &observation, &span).await?;
                let exit = Exit {
                    at: None,
                    bid: None,
                    ask: None,
                    pnl: None,
                    status: TradeStatus::ExitQuoteTooOld,
                    info: Some(info),
                };
                let closed = self
                    .db("record_exit", || self.store.record_exit(updated.id, &exit))
                    .await?;
                self.finish(&closed, &span).await;
                Some(closed)
            }
            Priced::Taken {
                at,
                quote,
                price: bid,
                info,
            } => {
                let late = self.record_lateness(target, at);
                Self::annotate(&span, &info);
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
                    info: Some(info),
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
                    info: Some(info),
                };
                let closed = self
                    .db("record_exit", || self.store.record_exit(trade.id, &exit))
                    .await?;
                self.finish(&closed, &span).await;
                Some(closed)
            }
        }
    }

    /// The bid at the exit target: from the stream while it can answer, else
    /// requested (retried for [`EXIT_RETRY_FOR`], never past the cutoff) with the
    /// age limit. A stream that times out gets one request: the exit never ends
    /// on a stream timeout alone.
    async fn exit_price(
        &self,
        span: &Span,
        trade: &StoredTrade,
        target: DateTime<Utc>,
        cutoff: DateTime<Utc>,
    ) -> Priced {
        if self.book.is_healthy() {
            let want = Wanted {
                pair: key(&trade.ticker, &trade.exchange_mic),
                side: Side::Bid,
                purpose: Purpose::Exit,
                min_time: None,
                target,
                deadline: (target + chrono(BID_WAIT)).min(cutoff),
            };
            if let StreamResult::Taken {
                held, price, at, ..
            } = self.stream_price(span, &want, None).await
            {
                let info = QuoteInfo::of(&held.quote, Origin::Stream, at);
                Self::quote_taken(&info, Purpose::Exit);
                return Priced::Taken {
                    at,
                    quote: held.quote,
                    price,
                    info,
                };
            }
        }
        self.request_price(
            span,
            Purpose::Exit,
            &trade.ticker,
            &trade.exchange_mic,
            None,
            None,
            (target + chrono(EXIT_RETRY_FOR)).min(cutoff),
        )
        .await
    }

    /// The bid of one observation: from the stream while it can answer, else one
    /// request, with the age limit. The wait never passes `next`, the target of
    /// the next scheduled event, so an observation cannot delay the exit. The
    /// `close` observation (`last_chance`) gets one request when the stream times
    /// out. `Ok` means a tradable, real-time, positive bid, whatever the source.
    async fn observe(
        &self,
        span: &Span,
        trade: &StoredTrade,
        target: DateTime<Utc>,
        cutoff: DateTime<Utc>,
        next: Option<DateTime<Utc>>,
        last_chance: bool,
    ) -> Observed {
        if self.book.is_healthy() {
            let mut deadline = (target + chrono(BID_WAIT)).min(cutoff);
            if let Some(next) = next {
                deadline = deadline.min(next);
            }
            let want = Wanted {
                pair: key(&trade.ticker, &trade.exchange_mic),
                side: Side::Bid,
                purpose: Purpose::Observation,
                min_time: None,
                target,
                deadline,
            };
            match self.stream_price(span, &want, None).await {
                StreamResult::Taken { held, at, .. } => {
                    let info = QuoteInfo::of(&held.quote, Origin::Stream, at);
                    Self::quote_taken(&info, Purpose::Observation);
                    return Observed {
                        fetched_at: Some(at),
                        quote: Some(held.quote),
                        status: ObservationStatus::Ok,
                        info: Some(info),
                    };
                }
                StreamResult::TimedOut { last } if !last_chance => {
                    let now = self.clock.now();
                    let info = last
                        .as_ref()
                        .map(|h| QuoteInfo::of(&h.quote, Origin::Stream, now));
                    return Observed {
                        fetched_at: Some(now),
                        quote: last.map(|h| h.quote),
                        status: ObservationStatus::NoQuote,
                        info,
                    };
                }
                StreamResult::TimedOut { .. } | StreamResult::Unavailable => {}
            }
        }
        let priced = self
            .request_price(
                span,
                Purpose::Observation,
                &trade.ticker,
                &trade.exchange_mic,
                None,
                None,
                self.clock.now(),
            )
            .await;
        match priced {
            Priced::Taken {
                at, quote, info, ..
            } => Observed {
                fetched_at: Some(at),
                quote: Some(quote),
                status: ObservationStatus::Ok,
                info: Some(info),
            },
            Priced::TooOld { at, quote, info } => Observed {
                fetched_at: Some(at),
                quote: Some(quote),
                status: ObservationStatus::QuoteTooOld,
                info: Some(info),
            },
            Priced::Failed { last, info } => Observed {
                fetched_at: last.as_ref().map(|(at, _)| *at),
                quote: last.and_then(|(_, result)| result.ok()),
                status: ObservationStatus::NoQuote,
                info,
            },
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
