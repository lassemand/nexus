//! Real-time emission of `company_announcement` events from `nordic-live`
//! (NEX-241).
//!
//! The WebSocket reader never waits for Kafka: events go into a bounded queue
//! ([`QUEUE_CAPACITY`]) with [`Emitter::offer`], which drops (and counts) when
//! the queue is full, and one producer task ([`run_producer`]) drains it in
//! order, retrying a failed send with exponential backoff. Anything dropped or
//! given up on is recovered by the 22:00 `nordic-reports` poll: it builds the
//! same event and signal dedupes by `(source, source_id)`.

use super::events::EventSink;
use chrono::{DateTime, Utc};
use metrics::{counter, gauge, histogram};
use model::generated::SpecialEvent;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tracing::{error, warn};

/// Capacity of the emit queue.
pub const QUEUE_CAPACITY: usize = 10_000;

/// Histogram buckets (seconds) for `nordic_live_emit_latency_seconds`.
pub const EMIT_LATENCY_BUCKETS: &[f64] = &[
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 30.0,
];

/// One event to produce.
#[derive(Debug, Clone, PartialEq)]
pub struct EmitJob {
    /// Kafka key, `ticker:exchange_mic`.
    pub key: String,
    /// The event.
    pub event: SpecialEvent,
    /// When the item was read from the socket.
    pub received_at: DateTime<Utc>,
}

/// The queue's sending side, cheap to clone.
#[derive(Debug, Clone)]
pub struct Emitter {
    tx: mpsc::Sender<EmitJob>,
}

impl Emitter {
    /// A queue of `capacity` jobs: the emitter and the receiver for
    /// [`run_producer`].
    pub fn channel(capacity: usize) -> (Self, mpsc::Receiver<EmitJob>) {
        let (tx, rx) = mpsc::channel(capacity);
        (Self { tx }, rx)
    }

    /// Queues `job` without waiting. Returns `"queued"`, or `"dropped"` (logged
    /// at `warn` and counted) when the queue is full or the producer is gone.
    pub fn offer(&self, job: EmitJob) -> &'static str {
        let news_id = job.event.source_id.clone();
        match self.tx.try_send(job) {
            Ok(()) => {
                gauge!("nordic_live_emit_queue_depth")
                    .set((self.tx.max_capacity() - self.tx.capacity()) as f64);
                "queued"
            }
            Err(e) => {
                counter!("nordic_live_events_total", "result" => "dropped").increment(1);
                warn!(news_id = %news_id, error = %e,
                      "emit queue unavailable, announcement dropped; the poll will publish it");
                "dropped"
            }
        }
    }
}

/// How a failed send is retried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    /// First delay; doubled after each failure.
    pub initial: Duration,
    /// Largest delay.
    pub max_delay: Duration,
    /// Total time after which the event is given up on.
    pub give_up_after: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            initial: Duration::from_millis(100),
            max_delay: Duration::from_secs(30),
            give_up_after: Duration::from_secs(5 * 60),
        }
    }
}

/// Sets every `nordic_live_events_total` result and the queue gauge to 0.
pub fn describe_emit_metrics() {
    for result in ["sent", "dropped", "failed"] {
        counter!("nordic_live_events_total", "result" => result).absolute(0);
    }
    gauge!("nordic_live_emit_queue_depth").set(0.0);
}

async fn send_with_retry(
    sink: &(dyn EventSink + Send + Sync),
    job: &EmitJob,
    policy: RetryPolicy,
) -> anyhow::Result<()> {
    let started = Instant::now();
    let mut delay = policy.initial;
    loop {
        match sink.send(&job.key, &job.event).await {
            Ok(()) => return Ok(()),
            Err(e) => {
                if started.elapsed() + delay > policy.give_up_after {
                    return Err(e);
                }
                warn!(key = %job.key, news_id = %job.event.source_id, error = format!("{e:#}"),
                      retry_in = ?delay, "producing announcement failed, retrying");
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(policy.max_delay);
            }
        }
    }
}

/// Drains the queue until every [`Emitter`] is dropped. One event at a time, in
/// order, so a company's events stay in order. A send that keeps failing for
/// `policy.give_up_after` is counted as failed and logged at `error`; the next
/// event goes on normally.
pub async fn run_producer(
    sink: Arc<dyn EventSink + Send + Sync>,
    mut rx: mpsc::Receiver<EmitJob>,
    policy: RetryPolicy,
) {
    while let Some(job) = rx.recv().await {
        gauge!("nordic_live_emit_queue_depth").set(rx.len() as f64);
        match send_with_retry(sink.as_ref(), &job, policy).await {
            Ok(()) => {
                let latency =
                    (Utc::now() - job.received_at).num_milliseconds().max(0) as f64 / 1000.0;
                histogram!("nordic_live_emit_latency_seconds").record(latency);
                counter!("nordic_live_events_total", "result" => "sent").increment(1);
            }
            Err(e) => {
                counter!("nordic_live_events_total", "result" => "failed").increment(1);
                error!(key = %job.key, news_id = %job.event.source_id, error = format!("{e:#}"),
                       "announcement not produced after retrying; the poll will publish it");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    fn job(n: u32) -> EmitJob {
        EmitJob {
            key: "GOMX:FNSE".into(),
            event: SpecialEvent {
                source_id: n.to_string(),
                ..Default::default()
            },
            received_at: Utc::now(),
        }
    }

    #[derive(Default)]
    struct FlakySink {
        fail_first: AtomicUsize,
        attempts: AtomicUsize,
        sent: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl EventSink for FlakySink {
        async fn send(&self, _key: &str, event: &SpecialEvent) -> anyhow::Result<()> {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            if event.source_id == "bad" {
                anyhow::bail!("broker down");
            }
            let left = self.fail_first.load(Ordering::SeqCst);
            if left > 0 {
                self.fail_first.store(left - 1, Ordering::SeqCst);
                anyhow::bail!("transient");
            }
            self.sent.lock().unwrap().push(event.source_id.clone());
            Ok(())
        }
    }

    fn fast() -> RetryPolicy {
        RetryPolicy {
            initial: Duration::from_millis(1),
            max_delay: Duration::from_millis(4),
            give_up_after: Duration::from_millis(60),
        }
    }

    #[test]
    fn a_full_queue_drops_without_waiting_and_later_jobs_still_go() {
        let (emitter, mut rx) = Emitter::channel(2);
        assert_eq!(emitter.offer(job(1)), "queued");
        assert_eq!(emitter.offer(job(2)), "queued");
        assert_eq!(emitter.offer(job(3)), "dropped");
        assert_eq!(rx.try_recv().unwrap().event.source_id, "1");
        assert_eq!(emitter.offer(job(4)), "queued");
        assert_eq!(rx.try_recv().unwrap().event.source_id, "2");
        assert_eq!(rx.try_recv().unwrap().event.source_id, "4");
    }

    #[tokio::test]
    async fn a_closed_queue_drops() {
        let (emitter, rx) = Emitter::channel(2);
        drop(rx);
        assert_eq!(emitter.offer(job(1)), "dropped");
    }

    #[tokio::test]
    async fn transient_failures_are_retried_then_the_event_is_sent() {
        let sink = Arc::new(FlakySink::default());
        sink.fail_first.store(2, Ordering::SeqCst);
        let (emitter, rx) = Emitter::channel(10);
        emitter.offer(job(1));
        drop(emitter);
        run_producer(sink.clone(), rx, fast()).await;
        assert_eq!(sink.attempts.load(Ordering::SeqCst), 3);
        assert_eq!(*sink.sent.lock().unwrap(), vec!["1".to_string()]);
    }

    #[tokio::test]
    async fn a_hopeless_event_is_given_up_on_and_the_next_one_is_sent() {
        let sink = Arc::new(FlakySink::default());
        let (emitter, rx) = Emitter::channel(10);
        emitter.offer(job(1));
        let mut bad = job(2);
        bad.event.source_id = "bad".into();
        emitter.offer(bad);
        emitter.offer(job(3));
        drop(emitter);
        run_producer(sink.clone(), rx, fast()).await;
        assert!(
            sink.attempts.load(Ordering::SeqCst) > 3,
            "the bad one was retried"
        );
        assert_eq!(
            *sink.sent.lock().unwrap(),
            vec!["1".to_string(), "3".to_string()]
        );
    }
}
