//! Finnish managers' transactions from `nordic-live` (NEX-279).
//!
//! The live path of [`super::insiders`]: an item of a company whose country takes
//! its insider transactions from MAR releases ([`insiders::uses_mar_releases`])
//! that [`insiders::is_managers_transaction`] selects is queued here as it is
//! read, next to its `company_announcement` event. The WebSocket reader only
//! selects and queues ([`InsiderQueue::offer`], never waits, drops and counts on a
//! full queue); one producer task ([`run_insider_producer`]) parses the release
//! with the code `nordic-reports` uses ([`insiders::build`] on the one item) and
//! publishes each trade to `insider.transactions` with the same key
//! ([`insiders::insider_key`]) and message, retrying a failed send.
//!
//! Language twins: each release comes in Finnish and English with the same MAR
//! reference number. Both copies are published as they arrive; they carry the
//! same key, person, dates, volume, price and type, so signal stores one row.
//! Only the role and nature texts, `lang`, `mfn_news_id` and `url` differ;
//! signal keeps the English role whichever copy arrived first.
//!
//! Anything dropped, unparsable or given up on is recovered by the 22:00
//! `nordic-reports` poll, which publishes the same messages.

use super::db::Company;
use super::feed::NewsItem;
use super::insiders::{self, insider_key, InsiderSink, InsiderStats};
use super::live::{fail_span, send_span, ItemSpan};
use backon::{ExponentialBuilder, Retryable};
use chrono::{DateTime, Utc};
use metrics::{counter, gauge, histogram};
use model::generated::UnifiedInsiderTransaction;
use std::sync::Arc;
use std::time::SystemTime;
use telemetry::trace::span_between;
use tokio::sync::mpsc;
use tracing::{error, info, info_span, warn, Instrument};

/// Capacity of the insider queue, in releases.
pub const QUEUE_CAPACITY: usize = 1_000;

/// Histogram buckets (seconds) for `nordic_live_insider_latency_seconds`.
pub const INSIDER_LATENCY_BUCKETS: &[f64] = &[
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 30.0,
];

/// Every `outcome` of `nordic_live_insider_transactions_total`.
pub const OUTCOMES: &[&str] = &["published", "parse_error", "dropped", "skipped", "failed"];

/// True if `item`, authored by a company listed on `exchange_mic`, goes the
/// insider path: its country takes insiders from MAR releases and the item is a
/// managers' transaction.
pub fn is_insider_release(exchange_mic: &str, item: &NewsItem) -> bool {
    insiders::uses_mar_releases(exchange_mic) && insiders::is_managers_transaction(item)
}

/// One release to parse and publish.
#[derive(Debug, Clone)]
pub struct InsiderJob {
    /// The registered company that authored the release.
    pub company: Company,
    /// The release as read.
    pub item: NewsItem,
    /// When the item was read.
    pub received_at: DateTime<Utc>,
    /// The item's trace; the producer continues it.
    pub span: ItemSpan,
    /// When the job was queued, for the `nordic_live.insider_queue` span.
    pub queued_at: SystemTime,
}

impl InsiderJob {
    /// A job queued now, carrying `span` (the item's root span).
    pub fn new(
        company: Company,
        item: NewsItem,
        received_at: DateTime<Utc>,
        span: ItemSpan,
    ) -> Self {
        Self {
            company,
            item,
            received_at,
            span,
            queued_at: SystemTime::now(),
        }
    }
}

/// The insider queue's sending side, cheap to clone.
#[derive(Debug, Clone)]
pub struct InsiderQueue {
    tx: mpsc::Sender<InsiderJob>,
}

impl InsiderQueue {
    /// A queue of `capacity` releases: the queue and the receiver for
    /// [`run_insider_producer`].
    pub fn channel(capacity: usize) -> (Self, mpsc::Receiver<InsiderJob>) {
        let (tx, rx) = mpsc::channel(capacity);
        (Self { tx }, rx)
    }

    /// Queues `job` without waiting. Returns `"queued"`, or `"dropped"` (logged
    /// at `warn`, counted as `outcome="dropped"`) when the queue is full or the
    /// producer is gone.
    pub fn offer(&self, job: InsiderJob) -> &'static str {
        let news_id = job.item.news_id;
        let ticker = job.company.ticker.clone();
        match self.tx.try_send(job) {
            Ok(()) => {
                gauge!("nordic_live_insider_queue_depth")
                    .set((self.tx.max_capacity() - self.tx.capacity()) as f64);
                "queued"
            }
            Err(e) => {
                counter!("nordic_live_insider_transactions_total", "outcome" => "dropped")
                    .increment(1);
                warn!(%news_id, %ticker, error = %e,
                      "insider queue unavailable, managers' transaction dropped; the poll will publish it");
                "dropped"
            }
        }
    }
}

/// What one release gave.
#[derive(Debug, Default)]
pub struct ParsedRelease {
    /// `(Kafka key, message)` per trade, exactly what `nordic-reports` sends
    /// for this release.
    pub messages: Vec<(String, UnifiedInsiderTransaction)>,
    /// What [`insiders::build`] saw and dropped.
    pub stats: InsiderStats,
}

impl ParsedRelease {
    /// `published` when there is something to send, `parse_error` when the
    /// release did not parse, `skipped` for a release that is never published
    /// (before MAR, another language, a zero price on a buy or sell).
    pub fn outcome(&self) -> &'static str {
        if !self.messages.is_empty() {
            "published"
        } else if self.stats.parse_errors > 0 {
            "parse_error"
        } else {
            "skipped"
        }
    }
}

/// Parses one release with the code of `nordic-reports`: [`insiders::build`] on
/// this one item, each message keyed by [`insider_key`]. Pure.
pub fn parse_release(company: &Company, item: &NewsItem) -> ParsedRelease {
    let batch = insiders::build(company, &[item]);
    ParsedRelease {
        messages: batch
            .messages
            .into_iter()
            .map(|(_, m)| (insider_key(&m), m))
            .collect(),
        stats: batch.stats,
    }
}

/// Sets every `nordic_live_insider_transactions_total` outcome and the queue
/// gauge to 0.
pub fn describe_insider_metrics() {
    for outcome in OUTCOMES {
        counter!("nordic_live_insider_transactions_total", "outcome" => *outcome).absolute(0);
    }
    gauge!("nordic_live_insider_queue_depth").set(0.0);
}

/// Drains the queue until every [`InsiderQueue`] is dropped, one release at a
/// time and in order. Each release is parsed (`nordic_live.insider_parse`), and
/// each of its trades is sent as `send <topic>` under the item's root span,
/// retried on `backoff`; one that keeps failing is counted as `failed` and the
/// next goes on normally. A release that does not parse never stops the queue.
pub async fn run_insider_producer(
    sink: Arc<dyn InsiderSink>,
    mut rx: mpsc::Receiver<InsiderJob>,
    backoff: ExponentialBuilder,
) {
    while let Some(job) = rx.recv().await {
        gauge!("nordic_live_insider_queue_depth").set(rx.len() as f64);
        span_between(
            "nordic_live.insider_queue",
            job.queued_at,
            SystemTime::now(),
            Some(&job.span.context()),
            [],
        )
        .in_scope(|| ());
        let step = info_span!(
            parent: job.span.span(),
            "nordic_live.insider_parse",
            nordic_live.insider_outcome = tracing::field::Empty,
            nordic_live.insider_messages = tracing::field::Empty,
        );
        let release = step.in_scope(|| parse_release(&job.company, &job.item));
        let outcome = release.outcome();
        step.record("nordic_live.insider_outcome", outcome);
        step.record(
            "nordic_live.insider_messages",
            release.messages.len() as i64,
        );
        drop(step);
        if outcome != "published" {
            counter!("nordic_live_insider_transactions_total", "outcome" => outcome).increment(1);
            info!(news_id = %job.item.news_id, ticker = %job.company.ticker, outcome,
                  stats = ?release.stats, "managers' transaction not published");
            continue;
        }
        for (key, msg) in &release.messages {
            send_one(sink.as_ref(), &job, key, msg, backoff).await;
        }
    }
}

async fn send_one(
    sink: &dyn InsiderSink,
    job: &InsiderJob,
    key: &str,
    msg: &UnifiedInsiderTransaction,
    backoff: ExponentialBuilder,
) {
    let sent = (|| async {
        let step = send_span(job.span.span(), sink.destination(), key);
        let result = sink.send(key, msg).instrument(step.clone()).await;
        if let Err(e) = &result {
            fail_span(&step, &format!("{e:#}"));
        }
        result
    })
    .retry(backoff)
    .notify(|e: &anyhow::Error, retry_in| {
        warn!(key, news_id = %job.item.news_id, error = format!("{e:#}"), ?retry_in,
              "producing insider transaction failed, retrying");
    })
    .await;
    match sent {
        Ok(()) => {
            let latency = (Utc::now() - job.received_at).num_milliseconds().max(0) as f64 / 1000.0;
            histogram!("nordic_live_insider_latency_seconds").record(latency);
            counter!("nordic_live_insider_transactions_total", "outcome" => "published")
                .increment(1);
            info!(key, news_id = %job.item.news_id, "published to insider.transactions");
        }
        Err(e) => {
            counter!("nordic_live_insider_transactions_total", "outcome" => "failed").increment(1);
            job.span.fail(&format!("{e:#}"));
            error!(key, news_id = %job.item.news_id, error = format!("{e:#}"),
                   "insider transaction not produced after retrying; the poll will publish it");
        }
    }
}
