use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, TimeZone, Utc};
use model::generated::SpecialEvent;
use telemetry::trace::OpenTelemetrySpanExt;
use tracing::field::Empty;
use tracing::{error, info, info_span, warn, Instrument, Span};
use verdict_sdk::Lang;

use crate::claims::Claims;
use crate::clock::Clock;
use crate::decision::{DecisionKind, NewDecision, Recorded};
use crate::handoff::{decision_channel, DecisionSlot, Persisted, TradeHandOff, TradeSignal};
use crate::matching::Release;
use crate::metrics;
use crate::registry::{KernelRegistry, KERNEL_RETENTION};
use crate::runtime::{run_blocking, RunOutcome};
use crate::sink::DecisionSink;
use crate::spans::{record_failure, trace_id_of};
use crate::store::{Store, Target};
use crate::types::{ReportKind, TargetStatus};
use crate::verdict::KernelMetric;

/// Releases of one company this close in time are language versions of the
/// same report, for a release that has no target.
pub const TWIN_WINDOW: Duration = Duration::from_secs(10 * 60);

/// A release event that passed the filter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseInput {
    /// MFN's news id (`source_id`), what makes processing idempotent.
    pub news_id: String,
    /// The company's ticker.
    pub ticker: String,
    /// The company's exchange.
    pub exchange_mic: String,
    /// The release's language tag.
    pub lang: String,
    /// Interim or annual.
    pub kind: ReportKind,
    /// The release's fiscal period, when MFN gave one.
    pub fiscal_period: Option<String>,
    /// When MFN published it.
    pub published_at: DateTime<Utc>,
    /// The release text: the title, a blank line, MFN's text.
    pub text: String,
}

/// Whether the analyst acts on an event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Classified {
    /// A released report to decide on.
    Process(Box<ReleaseInput>),
    /// Not for the analyst; the reason labels `analyst_release_events_total` as
    /// `ignored_<reason>`.
    Ignore(&'static str),
}

/// Decides whether an event is a report release the analyst acts on: a released
/// quarterly or annual report from MFN that is not a correction and has text.
pub fn classify(event: &SpecialEvent) -> Classified {
    let kind = match event.event_type.as_str() {
        "quarterly_report" => ReportKind::Interim,
        "annual_report" => ReportKind::Annual,
        _ => return Classified::Ignore("event_type"),
    };
    if event.source != "mfn" {
        return Classified::Ignore("source");
    }
    if event.is_correction {
        return Classified::Ignore("correction");
    }
    if event.text.trim().is_empty() {
        return Classified::Ignore("no_text");
    }
    let news_id = event.source_id.trim();
    if news_id.is_empty() {
        return Classified::Ignore("no_news_id");
    }
    if event.ticker.trim().is_empty() || event.exchange_mic.trim().is_empty() {
        return Classified::Ignore("invalid");
    }
    let Some(published_at) = Utc
        .timestamp_opt(event.occurred_at_unix_secs, 0)
        .single()
        .filter(|_| event.occurred_at_unix_secs > 0)
    else {
        return Classified::Ignore("invalid");
    };
    let period = event.fiscal_period.trim();
    Classified::Process(Box::new(ReleaseInput {
        news_id: news_id.to_owned(),
        ticker: event.ticker.trim().to_owned(),
        exchange_mic: event.exchange_mic.trim().to_owned(),
        lang: event.lang.trim().to_owned(),
        kind,
        fiscal_period: (!period.is_empty()).then(|| period.to_owned()),
        published_at,
        text: event.text.clone(),
    }))
}

/// When a release was consumed, on the wall clock (for the record) and on the
/// monotonic one (to measure how long the decision took).
#[derive(Debug, Clone, Copy)]
pub struct Receipt {
    /// The wall-clock instant, from the [`Clock`].
    pub at: DateTime<Utc>,
    /// The monotonic instant.
    pub started: Instant,
}

impl Receipt {
    /// The receipt of a release consumed now.
    pub fn now(clock: &dyn Clock) -> Receipt {
        Receipt {
            at: clock.now(),
            started: Instant::now(),
        }
    }
}

/// What processing a release came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Processed {
    /// A decision was recorded.
    Decided {
        /// The decision as stored.
        kind: DecisionKind,
        /// Time from consuming the release to handing the decision on.
        to_hand_off: Duration,
    },
    /// The release already had a decision; nothing was done.
    Duplicate,
    /// The database could not be read or written and the decision was not
    /// recorded (it is in the log).
    Failed,
}

/// How a failed database call is retried.
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    /// The first pause.
    pub start: Duration,
    /// The longest pause; the pause doubles up to it.
    pub max: Duration,
    /// How many times a failure that is not transient is tried before giving up.
    /// Transient failures (the database is unreachable) are retried until they pass.
    pub non_transient_attempts: u32,
}

impl Default for RetryPolicy {
    fn default() -> RetryPolicy {
        RetryPolicy {
            start: Duration::from_millis(50),
            max: Duration::from_secs(5),
            non_transient_attempts: 5,
        }
    }
}

/// True for a database error that goes away by itself: the connection, the pool,
/// a resource, a restart or a conflict.
pub fn is_transient(error: &sqlx::Error) -> bool {
    match error {
        sqlx::Error::Io(_)
        | sqlx::Error::Tls(_)
        | sqlx::Error::PoolTimedOut
        | sqlx::Error::PoolClosed
        | sqlx::Error::WorkerCrashed => true,
        sqlx::Error::Database(db) => db
            .code()
            .is_some_and(|code| matches!(code.get(..2), Some("08" | "40" | "53" | "57" | "58"))),
        _ => false,
    }
}

/// Runs `operation` until it succeeds, pausing between attempts with a growing
/// delay. Transient errors are retried without end; others give up after
/// `policy.non_transient_attempts`.
pub async fn with_retry<T, F, Fut>(
    step: &'static str,
    policy: RetryPolicy,
    operation: F,
) -> Result<T, sqlx::Error>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, sqlx::Error>>,
{
    with_retry_counted(step, policy, operation)
        .await
        .map(|(value, _)| value)
}

/// [`with_retry`] that also returns how many attempts it took, 1 when the first
/// one worked.
pub async fn with_retry_counted<T, F, Fut>(
    step: &'static str,
    policy: RetryPolicy,
    mut operation: F,
) -> Result<(T, u32), sqlx::Error>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, sqlx::Error>>,
{
    let mut delay = policy.start;
    let mut attempts = 0u32;
    loop {
        attempts += 1;
        match operation().await {
            Ok(value) => return Ok((value, attempts)),
            Err(error) => {
                if !is_transient(&error) && attempts >= policy.non_transient_attempts {
                    return Err(error);
                }
                metrics::decision_retry(step);
                warn!(step, attempts, %error, retry_in = ?delay, "database call failed, retrying");
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(policy.max);
            }
        }
    }
}

/// Decides on report releases: matches the release to its target, runs the
/// target's kernel, hands the decision to the trade scheduler, records it and
/// emits it.
pub struct ReleaseProcessor {
    store: Store,
    registry: Arc<KernelRegistry>,
    clock: Arc<dyn Clock>,
    sink: Arc<dyn DecisionSink>,
    hand_off: Arc<dyn TradeHandOff>,
    claims: Claims,
    max_age: Duration,
    retry: RetryPolicy,
    decisions_topic: String,
}

impl ReleaseProcessor {
    /// A processor over the analyst's database, the loaded kernels, a clock, the
    /// sink decisions are emitted to and the trade scheduler's hand-off. A
    /// release published more than `max_age` before it is consumed is `stale`.
    pub fn new(
        store: Store,
        registry: Arc<KernelRegistry>,
        clock: Arc<dyn Clock>,
        sink: Arc<dyn DecisionSink>,
        hand_off: Arc<dyn TradeHandOff>,
        max_age: Duration,
    ) -> ReleaseProcessor {
        ReleaseProcessor {
            store,
            registry,
            clock,
            sink,
            hand_off,
            claims: Claims::new(),
            max_age,
            retry: RetryPolicy::default(),
            decisions_topic: "analyst.decisions".to_owned(),
        }
    }

    /// Sets the topic decisions are emitted to; it is the
    /// `messaging.destination.name` of the `send analyst.decisions` span.
    pub fn with_decisions_topic(mut self, topic: impl Into<String>) -> ReleaseProcessor {
        self.decisions_topic = topic.into();
        self
    }

    /// Replaces the retry policy (tests use short pauses).
    pub fn with_retry_policy(mut self, retry: RetryPolicy) -> ReleaseProcessor {
        self.retry = retry;
        self
    }

    /// The clock the processor reads.
    pub fn clock(&self) -> &Arc<dyn Clock> {
        &self.clock
    }

    /// Processes one release. Meant to run in its own task, one per release, so
    /// that a slow database write for one company never holds another's.
    ///
    /// The order of operations is the point: the decision is computed, handed to
    /// the trade scheduler (so the entry quote never waits for the database),
    /// written, emitted. Nothing before the hand-off leaves the process but two
    /// indexed reads of the analyst's own database.
    ///
    /// Run it inside the release's `process special.events` span (see
    /// [`crate::releases::consumer_span`]): every step is a child span of the
    /// current span, the trace id is stored with the decision, and the decision
    /// is recorded on it as `nexus.decision`. Without a span nothing is traced.
    pub async fn process(&self, mut input: ReleaseInput, receipt: Receipt) -> Processed {
        let retry = self.retry;
        let store = &self.store;
        let consumer = Span::current();
        let trace_id = trace_id_of(&consumer);
        match with_retry("decision_read", retry, || {
            store.decision_by_news_id(&input.news_id)
        })
        .await
        {
            Ok(Some(_)) => {
                metrics::release_event("duplicate");
                return Processed::Duplicate;
            }
            Ok(None) => {}
            Err(error) => return self.failed(&input, "reading the decision", &error),
        }
        let release = Release {
            ticker: input.ticker.clone(),
            exchange_mic: input.exchange_mic.clone(),
            kind: input.kind,
            fiscal_period: input.fiscal_period.clone(),
            published_at: input.published_at,
        };
        let match_span = info_span!(
            "analyst.match_target",
            analyst.target_id = Empty,
            analyst.match = Empty
        );
        let located = async {
            let matched =
                with_retry("target_match", retry, || store.match_target(&release)).await?;
            let twin = self.twin_reason(&input, matched.target.as_ref()).await?;
            Ok::<_, sqlx::Error>((matched, twin))
        }
        .instrument(match_span.clone())
        .await;
        let (matched, twin) = match located {
            Ok(located) => located,
            Err(error) => return self.failed(&input, "matching the release", &error),
        };
        if let Some(target) = &matched.target {
            match_span.record("analyst.target_id", target.id);
        }
        match_span.record("analyst.match", matched.how.as_str());
        let target = matched.target;

        let mut kernel_id = None;
        let mut kernel_micros = None;
        let mut metrics_found: Vec<KernelMetric> = Vec::new();
        let mut error_kind = None;
        let mut used_kernel = None;
        let (kind, reason) = if let Some(reason) = twin {
            (DecisionKind::TwinIgnored, reason)
        } else if self.is_stale(receipt.at, input.published_at) {
            if let Ok(kernel) = self.kernel_for(target.as_ref()) {
                kernel_id = i64::try_from(kernel.id()).ok();
                used_kernel = Some(kernel);
            }
            (
                DecisionKind::Stale,
                format!(
                    "published {} s before it was consumed, over the {} s limit",
                    (receipt.at - input.published_at).num_seconds(),
                    self.max_age.as_secs()
                ),
            )
        } else {
            match self.kernel_for(target.as_ref()) {
                Err(why) => (DecisionKind::SkippedNoKernel, why),
                Ok(kernel) => {
                    kernel_id = i64::try_from(kernel.id()).ok();
                    let lang = Lang::from_tag(&input.lang);
                    let text = std::mem::take(&mut input.text).into_bytes();
                    used_kernel = Some(Arc::clone(&kernel));
                    let run_span = info_span!(
                        "analyst.kernel_run",
                        analyst.kernel_id = kernel_id,
                        analyst.kernel_micros = Empty,
                        nexus.decision = Empty,
                        otel.status_code = Empty,
                        otel.status_description = Empty
                    );
                    let outcome = run_blocking(kernel, text, lang)
                        .instrument(run_span.clone())
                        .await;
                    match outcome {
                        RunOutcome::Ok { verdict, micros } => {
                            kernel_micros = i64::try_from(micros).ok();
                            let decision: DecisionKind = verdict.decision.into();
                            run_span.record("analyst.kernel_micros", micros);
                            run_span.record("nexus.decision", decision.as_str());
                            metrics_found = verdict.metrics;
                            (decision, verdict.reason)
                        }
                        RunOutcome::Err(failure) => {
                            error_kind = Some(failure.kind().to_owned());
                            record_failure(&run_span, failure.kind(), &failure.to_string());
                            (DecisionKind::KernelError, failure.to_string())
                        }
                    }
                }
            }
        };

        let decided_at = self.clock.now();
        let new = NewDecision {
            target_id: target.as_ref().map(|t| t.id),
            kernel_id,
            news_id: input.news_id.clone(),
            ticker: input.ticker.clone(),
            exchange_mic: input.exchange_mic.clone(),
            lang: input.lang.clone(),
            fiscal_period: input.fiscal_period.clone(),
            published_at: input.published_at,
            received_at: receipt.at,
            decided_at,
            decision: kind,
            reason: Some(reason),
            metrics: metrics_found,
            kernel_micros,
            error: error_kind,
            trace_id,
        };
        let mut decision_slot: Option<DecisionSlot> = None;
        if matches!(
            kind,
            DecisionKind::Buy
                | DecisionKind::NoBuy
                | DecisionKind::Abstain
                | DecisionKind::SkippedNoKernel
        ) {
            let hand_off_span = info_span!("analyst.hand_off", nexus.decision = kind.as_str());
            let (slot, handle) = decision_channel();
            decision_slot = Some(slot);
            hand_off_span.in_scope(|| {
                self.hand_off.hand_off(TradeSignal {
                    news_id: new.news_id.clone(),
                    ticker: new.ticker.clone(),
                    exchange_mic: new.exchange_mic.clone(),
                    lang: new.lang.clone(),
                    fiscal_period: new.fiscal_period.clone(),
                    decision: kind,
                    baseline: kind == DecisionKind::SkippedNoKernel,
                    published_at: new.published_at,
                    received_at: new.received_at,
                    decided_at,
                    context: Span::current().context(),
                    persisted: handle,
                });
            });
        }
        let to_hand_off = receipt.started.elapsed();
        metrics::release_to_decision(to_hand_off);
        metrics::publish_to_decision(
            (decided_at - input.published_at).num_milliseconds() as f64 / 1000.0,
        );

        let write_span = info_span!("analyst.decision_write", analyst.write_attempts = Empty);
        let written = with_retry_counted("decision_write", retry, || store.record_decision(&new))
            .instrument(write_span.clone())
            .await;
        let recorded = match written {
            Ok((recorded, attempts)) => {
                write_span.record("analyst.write_attempts", attempts);
                recorded
            }
            Err(error) => {
                record_failure(&write_span, "database", &error.to_string());
                error!(decision = ?new, %error, "the decision could not be written");
                return Processed::Failed;
            }
        };
        let stored_kind = match recorded {
            Recorded::Duplicate => {
                metrics::release_event("duplicate");
                return Processed::Duplicate;
            }
            Recorded::Inserted { id, kind: stored } => {
                if let Some(slot) = &decision_slot {
                    slot.set(Persisted { id, kind: stored });
                }
                stored
            }
        };
        if stored_kind != kind {
            warn!(news_id = %new.news_id, decided = kind.as_str(), stored = stored_kind.as_str(),
                  "another version of the report settled it first");
        }
        let key = format!("{}:{}", new.ticker, new.exchange_mic);
        let send_span = info_span!(
            "send analyst.decisions",
            otel.kind = "producer",
            messaging.system = "kafka",
            messaging.destination.name = self.decisions_topic.as_str(),
            messaging.kafka.message.key = key.as_str()
        );
        self.emit(&new, stored_kind).instrument(send_span).await;
        consumer.record("nexus.decision", stored_kind.as_str());
        if let (Some(target), Some(kernel)) = (&target, &used_kernel) {
            self.registry
                .schedule_drop(target.id, kernel, KERNEL_RETENTION);
        }
        metrics::decision(stored_kind.as_str());
        metrics::release_event("decided");
        info!(
            ticker = %new.ticker,
            exchange_mic = %new.exchange_mic,
            fiscal_period = new.fiscal_period.as_deref().unwrap_or(""),
            decision = stored_kind.as_str(),
            reason = new.reason.as_deref().unwrap_or(""),
            news_id = %new.news_id,
            published_at = %new.published_at,
            received_at = %new.received_at,
            decided_at = %new.decided_at,
            kernel_micros = new.kernel_micros.unwrap_or(0),
            "decision"
        );
        Processed::Decided {
            kind: stored_kind,
            to_hand_off,
        }
    }

    fn failed(&self, input: &ReleaseInput, step: &str, error: &sqlx::Error) -> Processed {
        error!(news_id = %input.news_id, ticker = %input.ticker, step, %error,
               "the release could not be processed");
        Processed::Failed
    }

    fn is_stale(&self, received_at: DateTime<Utc>, published_at: DateTime<Utc>) -> bool {
        (received_at - published_at)
            .to_std()
            .is_ok_and(|age| age > self.max_age)
    }

    fn kernel_for(
        &self,
        target: Option<&Target>,
    ) -> Result<Arc<crate::runtime::LoadedKernel>, String> {
        let Some(target) = target else {
            return Err("no report target for this release".to_owned());
        };
        match target.status() {
            Ok(TargetStatus::Ready) => self
                .registry
                .get(target.id)
                .ok_or_else(|| "no kernel is loaded for the target".to_owned()),
            Ok(status) => Err(format!("the target is {}, not ready", status.as_str())),
            Err(error) => Err(error),
        }
    }

    async fn twin_reason(
        &self,
        input: &ReleaseInput,
        target: Option<&Target>,
    ) -> Result<Option<String>, sqlx::Error> {
        match target {
            Some(target) => {
                if let Ok(status @ (TargetStatus::Decided | TargetStatus::Skipped)) =
                    target.status()
                {
                    return Ok(Some(format!("the report is already {}", status.as_str())));
                }
                if !self.claims.claim_target(target.id) {
                    return Ok(Some(
                        "another version of the report is being decided".to_owned(),
                    ));
                }
                Ok(None)
            }
            None => {
                if !self.claims.claim_company(
                    &input.ticker,
                    &input.exchange_mic,
                    Instant::now(),
                    TWIN_WINDOW,
                ) {
                    return Ok(Some(
                        "another version of the release is being decided".to_owned(),
                    ));
                }
                let earlier = with_retry("twin_lookup", self.retry, || {
                    self.store.earlier_decision(
                        &input.ticker,
                        &input.exchange_mic,
                        None,
                        input.published_at,
                        &input.news_id,
                    )
                })
                .await?;
                Ok(earlier.map(|earlier| {
                    format!(
                        "another version of the release was decided: {}",
                        earlier.news_id
                    )
                }))
            }
        }
    }

    async fn emit(&self, new: &NewDecision, stored_kind: DecisionKind) {
        let proto = new.to_proto(stored_kind);
        let key = format!("{}:{}", new.ticker, new.exchange_mic);
        let mut delay = self.retry.start;
        let mut attempts = 0u32;
        loop {
            match self.sink.emit(&key, &proto).await {
                Ok(()) => return,
                Err(error) => {
                    attempts += 1;
                    metrics::decision_retry("decision_emit");
                    warn!(news_id = %new.news_id, attempts, %error, retry_in = ?delay,
                          "emitting the decision failed, retrying");
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(self.retry.max);
                }
            }
        }
    }
}
