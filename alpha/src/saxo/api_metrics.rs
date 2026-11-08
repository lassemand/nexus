//! Metrics of the calls to Saxo's HTTP APIs (`saxo_api_*`, see `docs/guides/metrics.md`).

use metrics::{counter, histogram};
use reqwest::StatusCode;
use std::time::Instant;

/// The Saxo APIs saxo-stream talks to. A fixed set, so the `endpoint` label stays bounded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Endpoint {
    /// The OAuth token endpoint (refresh).
    Token,
    /// The chart API (daily bars).
    Chart,
    /// The reference-data instrument search (Uic resolution).
    Instruments,
}

impl Endpoint {
    /// The `endpoint` label value.
    pub fn label(self) -> &'static str {
        match self {
            Endpoint::Token => "token",
            Endpoint::Chart => "chart",
            Endpoint::Instruments => "instruments",
        }
    }
}

/// The `status_class` label: `2xx`, `4xx`, `5xx`, or `error` for no response at
/// all (connect, TLS, timeout) and for any other status (1xx/3xx, which a
/// client that follows redirects should not see).
pub fn status_class(status: Option<StatusCode>) -> &'static str {
    match status {
        Some(s) if s.is_success() => "2xx",
        Some(s) if s.is_client_error() => "4xx",
        Some(s) if s.is_server_error() => "5xx",
        _ => "error",
    }
}

/// Records one request that started at `started`: `saxo_api_requests_total`,
/// `saxo_api_request_duration_seconds` and, for a 429, `saxo_api_rate_limited_total`.
/// `status` is `None` when no response came back.
pub fn record(endpoint: Endpoint, started: Instant, status: Option<StatusCode>) {
    let endpoint = endpoint.label();
    counter!("saxo_api_requests_total", "endpoint" => endpoint, "status_class" => status_class(status))
        .increment(1);
    histogram!("saxo_api_request_duration_seconds", "endpoint" => endpoint)
        .record(started.elapsed().as_secs_f64());
    if status == Some(StatusCode::TOO_MANY_REQUESTS) {
        counter!("saxo_api_rate_limited_total").increment(1);
    }
}

/// Registers every series at zero so `rate()` and alerts see them from the start.
pub fn describe() {
    for endpoint in [Endpoint::Token, Endpoint::Chart, Endpoint::Instruments] {
        for class in ["2xx", "4xx", "5xx", "error"] {
            counter!("saxo_api_requests_total", "endpoint" => endpoint.label(), "status_class" => class)
                .absolute(0);
        }
    }
    counter!("saxo_api_rate_limited_total").absolute(0);
}

/// Histogram buckets (seconds) for `saxo_api_request_duration_seconds`.
pub const DURATION_BUCKETS: &[f64] = &[0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses_map_to_their_class() {
        assert_eq!(status_class(Some(StatusCode::OK)), "2xx");
        assert_eq!(status_class(Some(StatusCode::NOT_FOUND)), "4xx");
        assert_eq!(status_class(Some(StatusCode::TOO_MANY_REQUESTS)), "4xx");
        assert_eq!(status_class(Some(StatusCode::BAD_GATEWAY)), "5xx");
        assert_eq!(status_class(Some(StatusCode::FOUND)), "error");
        assert_eq!(status_class(None), "error");
    }

    #[test]
    fn a_429_is_counted_as_a_4xx_and_as_rate_limited() {
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            record(
                Endpoint::Chart,
                Instant::now(),
                Some(StatusCode::TOO_MANY_REQUESTS),
            );
            record(Endpoint::Chart, Instant::now(), Some(StatusCode::OK));
            record(Endpoint::Token, Instant::now(), None);
        });
        let text = handle.render();
        assert!(
            text.contains("saxo_api_requests_total{endpoint=\"chart\",status_class=\"4xx\"} 1"),
            "{text}"
        );
        assert!(
            text.contains("saxo_api_requests_total{endpoint=\"chart\",status_class=\"2xx\"} 1"),
            "{text}"
        );
        assert!(
            text.contains("saxo_api_requests_total{endpoint=\"token\",status_class=\"error\"} 1"),
            "{text}"
        );
        assert!(text.contains("saxo_api_rate_limited_total 1"), "{text}");
        assert!(text.contains("saxo_api_request_duration_seconds"), "{text}");
    }
}
