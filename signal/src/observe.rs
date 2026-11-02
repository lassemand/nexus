//! Prometheus metrics for the consumer (NEX-227). Conventions: `docs/guides/metrics.md`.
//!
//! Every label value is bounded: topic names come from the configuration,
//! outcomes and results from the fixed sets below.

use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// How handling one Kafka message ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Stored (or applied) successfully.
    Stored,
    /// The message had no payload.
    SkippedEmpty,
    /// The payload did not decode.
    SkippedMalformed,
    /// Decoded, but a field is invalid or the message does not apply (unregistered ticker, unknown topic).
    SkippedInvalid,
    /// A database error was logged and the offset committed anyway: data lost.
    DbErrorCommitted,
    /// A database error made the store retry; counted once per failed attempt.
    DbErrorRetried,
}

impl Outcome {
    /// The `outcome` label value.
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Stored => "stored",
            Outcome::SkippedEmpty => "skipped_empty",
            Outcome::SkippedMalformed => "skipped_malformed",
            Outcome::SkippedInvalid => "skipped_invalid",
            Outcome::DbErrorCommitted => "db_error_committed",
            Outcome::DbErrorRetried => "db_error_retried",
        }
    }
}

/// Registers help text and units for every metric. Call after the recorder is installed.
pub fn describe() {
    use metrics::{describe_counter, describe_gauge, describe_histogram, Unit};
    describe_counter!(
        "signal_messages_total",
        "Kafka messages handled, by topic and outcome (db_error_retried counts failed attempts)"
    );
    describe_histogram!(
        "signal_store_duration_seconds",
        Unit::Seconds,
        "Time from receipt to commit of a message, by topic"
    );
    describe_counter!(
        "signal_store_retries_total",
        "Failed store attempts that were retried, by topic"
    );
    describe_gauge!(
        "signal_store_blocked",
        "1 while the Nordic store retry loop is waiting, otherwise 0"
    );
    describe_gauge!(
        "signal_last_processed_timestamp_seconds",
        Unit::Seconds,
        "Unix time of the last message processed, by topic"
    );
    describe_counter!(
        "signal_nordic_events_total",
        "Nordic events stored, by event type and result"
    );
    describe_counter!(
        "signal_reactions_computed_total",
        "Report reactions computed, by data_status"
    );
    describe_histogram!(
        "signal_reaction_refresh_duration_seconds",
        Unit::Seconds,
        "Duration of a report reaction refresh"
    );
    describe_gauge!(
        "signal_reaction_queue_depth",
        "Instruments waiting for a report reaction recompute"
    );
    describe_counter!(
        "signal_insider_db_errors_total",
        "Insider transactions rejected by the database and committed without being stored"
    );
}

fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

/// One message being handled; created when it is received.
pub struct Handling {
    topic: String,
    started: Instant,
}

impl Handling {
    /// Starts timing a message from `topic`.
    pub fn start(topic: &str) -> Self {
        Self {
            topic: topic.to_string(),
            started: Instant::now(),
        }
    }

    /// Records the final outcome: the count, the duration and the time of the last message.
    pub fn finish(&self, outcome: Outcome) {
        metrics::counter!("signal_messages_total", "topic" => self.topic.clone(), "outcome" => outcome.as_str())
            .increment(1);
        metrics::histogram!("signal_store_duration_seconds", "topic" => self.topic.clone())
            .record(self.started.elapsed().as_secs_f64());
        metrics::gauge!("signal_last_processed_timestamp_seconds", "topic" => self.topic.clone())
            .set(now_secs());
    }

    /// Records one failed store attempt that will be retried.
    pub fn retried(&self) {
        metrics::counter!("signal_messages_total", "topic" => self.topic.clone(),
                          "outcome" => Outcome::DbErrorRetried.as_str())
        .increment(1);
        metrics::counter!("signal_store_retries_total", "topic" => self.topic.clone()).increment(1);
    }
}

/// Sets `signal_store_blocked`: 1 while the Nordic retry loop waits, otherwise 0.
pub fn store_blocked(blocked: bool) {
    metrics::gauge!("signal_store_blocked").set(if blocked { 1.0 } else { 0.0 });
}

/// Counts an insider transaction the database rejected, which is then skipped (data loss).
pub fn insider_db_error() {
    metrics::counter!("signal_insider_db_errors_total").increment(1);
}

/// Counts one stored Nordic event; `result` is one of a fixed set.
pub fn nordic_event(event_type: &'static str, result: &'static str) {
    metrics::counter!("signal_nordic_events_total", "event_type" => event_type, "result" => result)
        .increment(1);
}

/// Records a finished reaction refresh: its duration and the computed rows by `data_status`.
pub fn reactions_refreshed(counts: &crate::reactions::Counts, started: Instant) {
    metrics::histogram!("signal_reaction_refresh_duration_seconds")
        .record(started.elapsed().as_secs_f64());
    for (status, n) in &counts.by_status {
        metrics::counter!("signal_reactions_computed_total", "data_status" => *status)
            .increment(*n as u64);
    }
}

/// Sets `signal_reaction_queue_depth`.
pub fn reaction_queue_depth(depth: usize) {
    metrics::gauge!("signal_reaction_queue_depth").set(depth as f64);
}

#[cfg(test)]
mod tests {
    use super::*;
    use metrics_exporter_prometheus::PrometheusBuilder;

    /// Runs `f` against a local recorder and returns the rendered exposition text.
    fn render(f: impl FnOnce()) -> String {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, f);
        handle.render()
    }

    #[test]
    fn each_outcome_increments_its_own_counter() {
        let all = [
            Outcome::Stored,
            Outcome::SkippedEmpty,
            Outcome::SkippedMalformed,
            Outcome::SkippedInvalid,
            Outcome::DbErrorCommitted,
        ];
        let text = render(|| {
            for o in all {
                Handling::start("t").finish(o);
            }
            Handling::start("t").retried();
        });
        for o in all {
            let line = format!(
                "signal_messages_total{{topic=\"t\",outcome=\"{}\"}} 1",
                o.as_str()
            );
            assert!(text.contains(&line), "missing {line} in\n{text}");
        }
        assert!(text.contains("signal_messages_total{topic=\"t\",outcome=\"db_error_retried\"} 1"));
        assert!(text.contains("signal_store_retries_total{topic=\"t\"} 1"));
        assert!(text.contains("signal_store_duration_seconds_count{topic=\"t\"} 5"));
        assert!(text.contains("signal_last_processed_timestamp_seconds{topic=\"t\"}"));
    }

    #[test]
    fn store_blocked_and_insider_errors() {
        let text = render(|| {
            store_blocked(true);
            insider_db_error();
        });
        assert!(text.contains("signal_store_blocked 1"));
        assert!(text.contains("signal_insider_db_errors_total 1"));
        let text = render(|| {
            store_blocked(true);
            store_blocked(false);
        });
        assert!(text.contains("signal_store_blocked 0"));
    }

    #[test]
    fn reaction_metrics() {
        let mut counts = crate::reactions::Counts::default();
        counts.by_status.insert("ok", 2);
        counts.by_status.insert("missing_base_bar", 1);
        let text = render(|| {
            reactions_refreshed(&counts, Instant::now());
            reaction_queue_depth(3);
            nordic_event("quarterly", "new");
        });
        assert!(text.contains("signal_reactions_computed_total{data_status=\"ok\"} 2"));
        assert!(
            text.contains("signal_reactions_computed_total{data_status=\"missing_base_bar\"} 1")
        );
        assert!(text.contains("signal_reaction_refresh_duration_seconds_count 1"));
        assert!(text.contains("signal_reaction_queue_depth 3"));
        assert!(
            text.contains("signal_nordic_events_total{event_type=\"quarterly\",result=\"new\"} 1")
        );
    }
}
