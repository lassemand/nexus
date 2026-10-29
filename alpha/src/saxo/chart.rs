//! Historical **daily** bars from Saxo's chart API (`GET /chart/v1/charts`,
//! `Horizon=1440`).
//!
//! # Token constraint
//!
//! Saxo refresh tokens rotate on every use (ADR-0003); `saxo-stream` owns the
//! chain. [`ChartClient`] therefore holds no [`super::SaxoAuth`] and never
//! talks to the token endpoint: it only *reads* the stream's [`SharedToken`]
//! for each request.
//!
//! # Prices are decimals
//!
//! Sample prices are parsed from the raw JSON number text straight into
//! [`Decimal`]; they never pass through `f64`.
//!
//! # Dates
//!
//! A sample's `Time` is an instant. The trading date is its calendar date in the
//! **exchange's own time zone** ([`model::exchange`], never a hard-coded city):
//! a bar stamped `2026-01-15T23:00:00Z` is the 16th in Stockholm and in
//! Copenhagen alike.
//!
//! # Unverified against the live API
//!
//! Saxo's reference docs do not say whether daily samples are adjusted for
//! splits / corporate actions, and `/chart/v1` returns 404 on SIM (ADR-0001), so
//! neither could be verified without live credentials. Treat bars as
//! **unadjusted** until confirmed (see NEX-162).

use super::auth::SharedToken;
use chrono::{DateTime, NaiveDate, Utc};
use model::exchange::local_date;
use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::value::RawValue;
use std::str::FromStr;
use std::time::Duration;
use thiserror::Error;
use tokio::time::Instant;
use tracing::warn;

/// `Horizon` for daily samples (minutes).
pub const DAILY_HORIZON: u32 = 1440;
/// Saxo's cap on samples per request.
pub const MAX_COUNT: u32 = 1200;

const MAX_ATTEMPTS: u32 = 4;
const DEFAULT_RETRY_AFTER: Duration = Duration::from_secs(5);
const MAX_RETRY_AFTER: Duration = Duration::from_secs(120);

/// Errors from the chart API.
#[derive(Debug, Error)]
pub enum ChartError {
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("chart API returned {0}")]
    Status(reqwest::StatusCode),
    #[error("chart API rate limited the request {0} times")]
    RateLimited(u32),
    #[error("invalid chart URL: {0}")]
    Url(#[from] url::ParseError),
    #[error("no time zone known for exchange {0}")]
    UnknownExchange(String),
}

impl ChartError {
    /// Whether retrying can help. A 4xx other than 408/429, an undecodable
    /// body (reported as 422) or a bad URL give the same result every time.
    pub fn is_transient(&self) -> bool {
        match self {
            ChartError::Http(e) => super::is_transient_reqwest(e),
            ChartError::Status(s) => super::is_transient_status(*s),
            ChartError::RateLimited(_) => true,
            ChartError::Url(_) | ChartError::UnknownExchange(_) => false,
        }
    }
}

/// One daily bar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DailyBarRow {
    /// Trading date in the exchange's own time zone.
    pub date: NaiveDate,
    /// The sample's `Time` as returned by Saxo.
    pub time: DateTime<Utc>,
    pub open: Decimal,
    pub high: Decimal,
    pub low: Decimal,
    pub close: Decimal,
    pub volume: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ChartResponse {
    #[serde(default)]
    data: Vec<RawSample>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct RawSample {
    time: String,
    open: Option<Box<RawValue>>,
    high: Option<Box<RawValue>>,
    low: Option<Box<RawValue>>,
    close: Option<Box<RawValue>>,
    volume: Option<Box<RawValue>>,
}

fn decimal(raw: &Option<Box<RawValue>>) -> Option<Decimal> {
    let text = raw.as_ref()?.get().trim();
    Decimal::from_str(text)
        .or_else(|_| Decimal::from_scientific(text))
        .ok()
}

impl RawSample {
    fn into_row(self, exchange_mic: &str) -> Option<DailyBarRow> {
        let time = DateTime::parse_from_rfc3339(&self.time)
            .ok()?
            .with_timezone(&Utc);
        let volume = match &self.volume {
            None => 0,
            Some(_) => {
                let v = decimal(&self.volume)?;
                if v.is_sign_negative() {
                    return None;
                }
                u64::try_from(v.trunc()).ok()?
            }
        };
        Some(DailyBarRow {
            date: local_date(exchange_mic, time)?,
            time,
            open: decimal(&self.open)?,
            high: decimal(&self.high)?,
            low: decimal(&self.low)?,
            close: decimal(&self.close)?,
            volume,
        })
    }
}

/// Parses a chart response body into bars, oldest first. Samples that cannot be
/// read (missing or non-numeric price, bad time) are skipped with a warning.
/// Days Saxo omits are simply absent: nothing is ever filled forward.
pub fn parse_daily_bars(
    body: &str,
    exchange_mic: &str,
) -> Result<Vec<DailyBarRow>, serde_json::Error> {
    let resp: ChartResponse = serde_json::from_str(body)?;
    let mut rows: Vec<DailyBarRow> = resp
        .data
        .into_iter()
        .filter_map(|s| {
            let time = s.time.clone();
            let row = s.into_row(exchange_mic);
            if row.is_none() {
                warn!(time, "skipping unreadable chart sample");
            }
            row
        })
        .collect();
    rows.sort_by_key(|r| r.time);
    Ok(rows)
}

/// Rate-limited reader of Saxo daily bars. See the module docs for why it
/// borrows the stream's token instead of authenticating.
pub struct ChartClient {
    http: reqwest::Client,
    api_base: String,
    version: String,
    token: SharedToken,
    min_interval: Duration,
    last_request: Option<Instant>,
}

impl ChartClient {
    /// `api_base` is e.g. `https://gateway.saxobank.com/openapi`; `min_interval`
    /// the minimum gap between chart requests (they share Saxo's per-session
    /// limits with the stream).
    pub fn new(
        http: reqwest::Client,
        api_base: impl Into<String>,
        token: SharedToken,
        min_interval: Duration,
    ) -> Self {
        Self {
            http,
            api_base: api_base.into(),
            version: "v1".to_string(),
            token,
            min_interval,
            last_request: None,
        }
    }

    /// Use another chart API version (`v1` by default; `v3` is the successor).
    pub fn with_version(mut self, version: impl Into<String>) -> Self {
        self.version = version.into();
        self
    }

    async fn throttle(&mut self) {
        if let Some(last) = self.last_request {
            let elapsed = last.elapsed();
            if elapsed < self.min_interval {
                tokio::time::sleep(self.min_interval - elapsed).await;
            }
        }
        self.last_request = Some(Instant::now());
    }

    /// Up to `count` (≤ 1200) daily bars ending at `up_to` (inclusive), oldest
    /// first, dated in `exchange_mic`'s time zone. Transient failures (no response, 408, 429, 5xx) are retried up to
    /// `MAX_ATTEMPTS` times, after `Retry-After` when given, else after 1 s,
    /// 2 s, 4 s. Any other status fails at once.
    pub async fn fetch_up_to(
        &mut self,
        uic: u64,
        exchange_mic: &str,
        up_to: DateTime<Utc>,
        count: u32,
    ) -> Result<Vec<DailyBarRow>, ChartError> {
        if model::exchange::timezone(exchange_mic).is_none() {
            return Err(ChartError::UnknownExchange(exchange_mic.to_string()));
        }
        let mut url = url::Url::parse(&format!(
            "{}/chart/{}/charts",
            self.api_base.trim_end_matches('/'),
            self.version
        ))?;
        url.query_pairs_mut()
            .append_pair("AssetType", "Stock")
            .append_pair("Uic", &uic.to_string())
            .append_pair("Horizon", &DAILY_HORIZON.to_string())
            .append_pair("Mode", "UpTo")
            .append_pair("Time", &up_to.format("%Y-%m-%dT%H:%M:%S%.6fZ").to_string())
            .append_pair("Count", &count.clamp(1, MAX_COUNT).to_string());

        for attempt in 1..=MAX_ATTEMPTS {
            self.throttle().await;
            // Read the stream's current token; never refresh it.
            let access_token = self.token.lock().unwrap().access_token.clone();
            let backoff = Duration::from_secs(1 << (attempt - 1));
            let resp = match self
                .http
                .get(url.clone())
                .bearer_auth(access_token)
                .send()
                .await
            {
                Ok(r) => r,
                // No response at all: worth another try.
                Err(e) if attempt < MAX_ATTEMPTS => {
                    warn!(uic, error = %e, ?backoff, "Saxo chart request failed, retrying");
                    tokio::time::sleep(backoff).await;
                    continue;
                }
                Err(e) => return Err(e.into()),
            };
            let status = resp.status();
            if status.is_success() {
                let body = resp.text().await?;
                return parse_daily_bars(&body, exchange_mic).map_err(|e| {
                    warn!(error = %e, "undecodable chart response");
                    ChartError::Status(reqwest::StatusCode::UNPROCESSABLE_ENTITY)
                });
            }
            if !super::is_transient_status(status) {
                // Saxo explains a 4xx in the body (`ErrorCode`, `Message`).
                let body = resp.text().await.unwrap_or_default();
                let body: String = body.chars().take(300).collect();
                warn!(uic, %status, body = %body, url = %url.path(), "Saxo chart API rejected the request");
                return Err(ChartError::Status(status));
            }
            if attempt == MAX_ATTEMPTS {
                return Err(if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                    ChartError::RateLimited(attempt)
                } else {
                    ChartError::Status(status)
                });
            }
            let retry_after = resp
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse::<u64>().ok())
                .map(Duration::from_secs);
            let wait = if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                retry_after.unwrap_or(DEFAULT_RETRY_AFTER)
            } else {
                retry_after.unwrap_or(backoff)
            }
            .min(MAX_RETRY_AFTER);
            warn!(uic, %status, ?wait, "Saxo chart API returned a transient error, waiting");
            tokio::time::sleep(wait).await;
        }
        Err(ChartError::RateLimited(MAX_ATTEMPTS))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::saxo::SaxoToken;
    use chrono::TimeZone;
    use std::sync::{Arc, Mutex};
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn token() -> SharedToken {
        Arc::new(Mutex::new(SaxoToken {
            access_token: "stream-token".into(),
            expires_at: Utc::now() + chrono::Duration::hours(1),
        }))
    }

    fn client(server: &MockServer) -> ChartClient {
        ChartClient::new(
            reqwest::Client::new(),
            server.uri(),
            token(),
            Duration::ZERO,
        )
    }

    fn at(y: i32, m: u32, d: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, m, d, 12, 0, 0).unwrap()
    }

    #[test]
    fn prices_keep_every_digit_without_float_round_trip() {
        let body = r#"{"Data":[{"Time":"2026-08-26T00:00:00.000000Z","Open":12.345678,
            "High":13.000001,"Low":12.0,"Close":12.345678,"Volume":1234567.0}]}"#;
        let rows = parse_daily_bars(body, "FNSE").unwrap();
        assert_eq!(rows[0].close.to_string(), "12.345678");
        assert_eq!(rows[0].high.to_string(), "13.000001");
        assert_eq!(rows[0].low.to_string(), "12.0");
        assert_eq!(rows[0].volume, 1_234_567);
        // 0.1 + 0.2 style values that an f64 would corrupt.
        let body = r#"{"Data":[{"Time":"2026-08-26T00:00:00Z","Open":0.30000000000000004,
            "High":1,"Low":1,"Close":1e-5,"Volume":0}]}"#;
        let rows = parse_daily_bars(body, "FNSE").unwrap();
        assert_eq!(rows[0].open.to_string(), "0.30000000000000004");
        assert_eq!(rows[0].close.to_string(), "0.00001");
        assert_eq!(rows[0].volume, 0);
    }

    #[test]
    fn dates_are_stockholm_dates_in_winter_and_summer() {
        let body = r#"{"Data":[
          {"Time":"2026-01-15T23:00:00Z","Open":1,"High":1,"Low":1,"Close":1,"Volume":1},
          {"Time":"2026-01-15T00:00:00Z","Open":1,"High":1,"Low":1,"Close":1,"Volume":1},
          {"Time":"2026-07-15T22:00:00Z","Open":1,"High":1,"Low":1,"Close":1,"Volume":1},
          {"Time":"2026-07-15T00:00:00Z","Open":1,"High":1,"Low":1,"Close":1,"Volume":1}]}"#;
        let dates: Vec<String> = parse_daily_bars(body, "FNSE")
            .unwrap()
            .iter()
            .map(|r| r.date.to_string())
            .collect();
        // Sorted oldest first; 23:00Z (winter) and 22:00Z (summer) are the next day.
        assert_eq!(
            dates,
            vec!["2026-01-15", "2026-01-16", "2026-07-15", "2026-07-16"]
        );
    }

    #[test]
    fn danish_bars_are_dated_in_the_danish_zone() {
        let body = r#"{"Data":[
          {"Time":"2026-01-15T23:00:00Z","Open":1,"High":1,"Low":1,"Close":1,"Volume":1},
          {"Time":"2026-07-15T22:00:00Z","Open":1,"High":1,"Low":1,"Close":1,"Volume":1}]}"#;
        for mic in ["XCSE", "FNDK"] {
            let dates: Vec<String> = parse_daily_bars(body, mic)
                .unwrap()
                .iter()
                .map(|r| r.date.to_string())
                .collect();
            assert_eq!(dates, vec!["2026-01-16", "2026-07-16"], "{mic}");
        }
        // A venue without a known zone yields no bars rather than a guessed date.
        assert!(parse_daily_bars(body, "XNAS").unwrap().is_empty());
    }

    #[test]
    fn finnish_bars_are_dated_in_the_helsinki_zone() {
        // 21:30Z on 21 Oct 2026 is 00:30 on the 22nd in Helsinki (EEST, UTC+3)
        // but 23:30 on the 21st in Stockholm (CEST, UTC+2): the zones differ here.
        let body = r#"{"Data":[
          {"Time":"2026-10-21T21:30:00Z","Open":1,"High":1,"Low":1,"Close":1,"Volume":1},
          {"Time":"2026-10-21T22:00:00Z","Open":1,"High":1,"Low":1,"Close":1,"Volume":1},
          {"Time":"2026-01-15T22:00:00Z","Open":1,"High":1,"Low":1,"Close":1,"Volume":1},
          {"Time":"2026-01-15T21:59:00Z","Open":1,"High":1,"Low":1,"Close":1,"Volume":1}]}"#;
        let dates = |mic: &str| -> Vec<String> {
            parse_daily_bars(body, mic)
                .unwrap()
                .iter()
                .map(|r| r.date.to_string())
                .collect()
        };
        for mic in ["XHEL", "FNFI"] {
            assert_eq!(
                dates(mic),
                vec!["2026-01-15", "2026-01-16", "2026-10-22", "2026-10-22"], // parsed in time order,
                "{mic}"
            );
        }
        assert_eq!(
            dates("XSTO"),
            vec!["2026-01-15", "2026-01-15", "2026-10-21", "2026-10-22"]
        );
    }

    #[tokio::test]
    async fn an_unknown_exchange_is_refused_before_any_request() {
        let server = MockServer::start().await;
        let err = client(&server)
            .fetch_up_to(1, "XNAS", at(2026, 8, 26), 10)
            .await
            .unwrap_err();
        assert!(matches!(err, ChartError::UnknownExchange(_)));
        assert!(!err.is_transient());
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[test]
    fn missing_days_are_not_filled_and_bad_samples_are_skipped() {
        let body = r#"{"Data":[
          {"Time":"2026-08-24T00:00:00Z","Open":1,"High":2,"Low":1,"Close":2,"Volume":10},
          {"Time":"2026-08-26T00:00:00Z","Open":2,"High":3,"Low":2,"Close":3,"Volume":0},
          {"Time":"2026-08-27T00:00:00Z","Open":2,"High":3,"Low":2,"Volume":5},
          {"Time":"garbage","Open":2,"High":3,"Low":2,"Close":3,"Volume":5}]}"#;
        let rows = parse_daily_bars(body, "FNSE").unwrap();
        let dates: Vec<String> = rows.iter().map(|r| r.date.to_string()).collect();
        assert_eq!(dates, vec!["2026-08-24", "2026-08-26"]); // the 25th is absent, not filled
        assert_eq!(rows[1].volume, 0); // zero-volume bar kept as is
        assert_eq!(parse_daily_bars(r#"{"Data":[]}"#, "FNSE").unwrap(), vec![]);
        assert_eq!(parse_daily_bars("{}", "FNSE").unwrap(), vec![]);
    }

    #[tokio::test]
    async fn requests_daily_up_to_with_the_streams_token_and_never_touches_the_token_endpoint() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/chart/v1/charts"))
            .and(query_param("AssetType", "Stock"))
            .and(query_param("Uic", "4711"))
            .and(query_param("Horizon", "1440"))
            .and(query_param("Mode", "UpTo"))
            .and(query_param("Count", "1200"))
            .and(query_param("Time", "2026-08-26T12:00:00.000000Z"))
            .and(header("Authorization", "Bearer stream-token"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"Data":[{"Time":"2026-08-26T00:00:00Z","Open":1,"High":2,"Low":1,"Close":2,"Volume":3}]}"#,
            ))
            .expect(1)
            .mount(&server)
            .await;
        let mut c = client(&server);
        let rows = c
            .fetch_up_to(4711, "FNSE", at(2026, 8, 26), 5000)
            .await
            .unwrap(); // count clamped
        assert_eq!(rows.len(), 1);
        let token_calls = server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.url.path().contains("token") || r.url.path().contains("authorize"))
            .count();
        assert_eq!(token_calls, 0);
    }

    #[tokio::test]
    async fn retries_429_after_retry_after_and_fails_on_other_errors() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "0"))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"Data":[]}"#))
            .mount(&server)
            .await;
        let mut c = client(&server);
        assert!(c
            .fetch_up_to(1, "FNSE", at(2026, 8, 26), 10)
            .await
            .unwrap()
            .is_empty());

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(404))
            .expect(1)
            .mount(&server)
            .await;
        let err = client(&server)
            .fetch_up_to(1, "FNSE", at(2026, 8, 26), 10)
            .await
            .unwrap_err();
        assert!(matches!(err, ChartError::Status(s) if s.as_u16() == 404));
        assert!(!err.is_transient());
    }

    #[tokio::test]
    async fn a_server_error_is_retried_and_a_bad_request_is_not() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(503).insert_header("Retry-After", "0"))
            .up_to_n_times(2)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"Data":[]}"#))
            .mount(&server)
            .await;
        assert!(client(&server)
            .fetch_up_to(1, "FNSE", at(2026, 8, 26), 10)
            .await
            .unwrap()
            .is_empty());
        assert_eq!(server.received_requests().await.unwrap().len(), 3);

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(400))
            .expect(1)
            .mount(&server)
            .await;
        let err = client(&server)
            .fetch_up_to(1, "FNSE", at(2026, 8, 26), 10)
            .await
            .unwrap_err();
        assert!(matches!(err, ChartError::Status(s) if s.as_u16() == 400));
        assert!(!err.is_transient());
    }

    #[tokio::test]
    async fn a_server_error_that_persists_is_reported_as_transient() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(502).insert_header("Retry-After", "0"))
            .expect(u64::from(MAX_ATTEMPTS))
            .mount(&server)
            .await;
        let err = client(&server)
            .fetch_up_to(1, "FNSE", at(2026, 8, 26), 10)
            .await
            .unwrap_err();
        assert!(matches!(err, ChartError::Status(s) if s.as_u16() == 502));
        assert!(err.is_transient());
    }

    #[tokio::test]
    async fn chart_version_is_configurable() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/chart/v3/charts"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"Data":[]}"#))
            .expect(1)
            .mount(&server)
            .await;
        let mut c = client(&server).with_version("v3");
        c.fetch_up_to(1, "FNSE", at(2026, 8, 26), 10).await.unwrap();
    }
}
