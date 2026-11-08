//! Helpers for the run metrics of the CronJob binaries (see `docs/guides/metrics.md`).
//!
//! The values are pushed to the Pushgateway when a run ends and describe that run
//! only: read them as gauges, never `rate()` them.

use metrics::counter;

/// Registers `<job>_items_fetched_total`, `<job>_events_published_total` and the
/// `<job>_api_errors_total` classes at zero, so a run with no errors pushes an
/// explicit `0` instead of a missing series.
pub fn register(job: &'static str) {
    counter!(format!("{job}_items_fetched_total")).absolute(0);
    counter!(format!("{job}_events_published_total")).absolute(0);
    for class in STATUS_CLASSES {
        counter!(format!("{job}_api_errors_total"), "status_class" => class).absolute(0);
    }
}

/// The `status_class` label values.
pub const STATUS_CLASSES: [&str; 4] = ["4xx", "5xx", "network", "other"];

/// Adds `n` to `<job>_items_fetched_total` (the job's own unit: bars, filings, ...).
pub fn items_fetched(job: &'static str, n: usize) {
    counter!(format!("{job}_items_fetched_total")).increment(n as u64);
}

/// Adds `n` to `<job>_events_published_total`.
pub fn events_published(job: &'static str, n: usize) {
    counter!(format!("{job}_events_published_total")).increment(n as u64);
}

/// Counts one failed API call in `<job>_api_errors_total{status_class}`.
pub fn api_error(job: &'static str, status_class: &'static str) {
    counter!(format!("{job}_api_errors_total"), "status_class" => status_class).increment(1);
}

/// `4xx` or `5xx` for an HTTP status, `other` for anything else.
pub fn class_of_status(status: u16) -> &'static str {
    match status {
        400..=499 => "4xx",
        500..=599 => "5xx",
        _ => "other",
    }
}

/// The class of a `reqwest` failure: its status, else `network` (connect, timeout,
/// TLS, body or decode error with no status).
pub fn class_of_reqwest(e: &reqwest::Error) -> &'static str {
    e.status()
        .map_or("network", |s| class_of_status(s.as_u16()))
}

/// [`class_of_reqwest`] for an `anyhow` chain that wraps a `reqwest::Error`;
/// `other` when it does not.
pub fn class_of_anyhow(e: &anyhow::Error) -> &'static str {
    e.chain()
        .find_map(|c| c.downcast_ref::<reqwest::Error>())
        .map_or("other", class_of_reqwest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses_are_classed() {
        assert_eq!(class_of_status(404), "4xx");
        assert_eq!(class_of_status(429), "4xx");
        assert_eq!(class_of_status(503), "5xx");
        assert_eq!(class_of_status(302), "other");
    }

    #[test]
    fn a_non_http_error_is_other() {
        assert_eq!(class_of_anyhow(&anyhow::anyhow!("boom")), "other");
    }

    #[test]
    fn counters_are_named_per_job_and_registered_at_zero() {
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            register("market");
            items_fetched("market", 3);
            events_published("market", 2);
            api_error("market", "5xx");
        });
        let text = handle.render();
        assert!(text.contains("market_items_fetched_total 3"), "{text}");
        assert!(text.contains("market_events_published_total 2"), "{text}");
        assert!(
            text.contains("market_api_errors_total{status_class=\"5xx\"} 1"),
            "{text}"
        );
        assert!(
            text.contains("market_api_errors_total{status_class=\"4xx\"} 0"),
            "{text}"
        );
    }
}
