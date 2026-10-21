//! Client for the Nasdaq Nordic company-news API.
//!
//! The request asks for `timeZone=UTC`, so `releaseTime` is already an instant
//! in UTC and no Stockholm DST handling is needed (verified against MFN:
//! 2026-09-18 08:00 local == 06:00Z, and 2026-04-01 13:30 local == 11:30Z).
//! `company=` filters by exact issuer name; a partial name returns nothing.

use anyhow::{anyhow, Context};
use chrono::{DateTime, NaiveDateTime, Utc};
use serde::Deserialize;
use std::time::Duration;
use tokio::time::Instant;
use tracing::warn;

/// Production endpoint.
pub const NASDAQ_NEWS_URL: &str = "https://api.news.eu.nasdaq.com/news/query.action";
/// Page size requested from the API.
pub const PAGE_SIZE: usize = 200;

const MAX_ATTEMPTS: u32 = 4;

/// One news item as returned by the API (only the fields we use).
#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct NewsItem {
    pub disclosure_id: i64,
    pub headline: String,
    #[serde(default)]
    pub cns_category: String,
    pub message_url: Option<String>,
    /// `yyyy-MM-dd HH:mm:ss` in UTC (we request `timeZone=UTC`).
    pub release_time: String,
    #[serde(default)]
    pub company: String,
}

impl NewsItem {
    /// Parse `releaseTime` as a UTC instant.
    pub fn released_at(&self) -> anyhow::Result<DateTime<Utc>> {
        let naive = NaiveDateTime::parse_from_str(&self.release_time, "%Y-%m-%d %H:%M:%S")
            .with_context(|| format!("bad releaseTime {:?}", self.release_time))?;
        Ok(naive.and_utc())
    }
}

#[derive(Debug, Deserialize)]
struct Results {
    #[serde(default)]
    item: Vec<NewsItem>,
}

#[derive(Debug, Deserialize)]
struct Response {
    results: Results,
    #[serde(default)]
    count: usize,
}

/// One page of results.
#[derive(Debug)]
pub struct Page {
    pub items: Vec<NewsItem>,
    /// Total number of items available for the query.
    pub total: usize,
}

/// Rate-limited, retrying HTTP client for the news API.
pub struct NewsClient {
    http: reqwest::Client,
    base_url: String,
    min_interval: Duration,
    backoff_base: Duration,
    last_request: Option<Instant>,
}

impl NewsClient {
    /// Build a client. `min_interval` is the minimum gap between requests and
    /// `backoff_base` the first retry delay (doubled per attempt).
    pub fn new(
        base_url: impl Into<String>,
        min_interval: Duration,
        backoff_base: Duration,
    ) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent("nexus-chronicle/nordic-reports (lasse.alm@gsfleet.io)")
            .timeout(Duration::from_secs(30))
            .build()?;
        Ok(Self {
            http,
            base_url: base_url.into(),
            min_interval,
            backoff_base,
            last_request: None,
        })
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

    /// Fetch one page (newest first) of news for the exact issuer name.
    /// Retries 5xx, 429 and transport errors with exponential backoff.
    pub async fn fetch_page(&mut self, company: &str, start: usize) -> anyhow::Result<Page> {
        let query = [
            ("type", "json"),
            ("showCompany", "true"),
            ("countResults", "true"),
            ("globalGroup", "exchangeNotice"),
            ("globalName", "NordicAllMarkets"),
            ("displayLanguage", "en"),
            ("language", "en"),
            ("timeZone", "UTC"),
            ("dateMask", "yyyy-MM-dd HH:mm:ss"),
            ("dir", "DESC"),
            ("company", company),
        ];
        let limit = PAGE_SIZE.to_string();
        let start_s = start.to_string();

        let mut delay = self.backoff_base;
        for attempt in 1..=MAX_ATTEMPTS {
            self.throttle().await;
            let result = self
                .http
                .get(&self.base_url)
                .query(&query)
                .query(&[("limit", limit.as_str()), ("start", start_s.as_str())])
                .send()
                .await;
            let retryable = match result {
                Ok(resp) if resp.status().is_success() => {
                    let body: Response = resp.json().await.context("decoding news response")?;
                    return Ok(Page {
                        items: body.results.item,
                        total: body.count,
                    });
                }
                Ok(resp) => {
                    let s = resp.status();
                    if !(s.is_server_error() || s.as_u16() == 429) {
                        return Err(anyhow!("news API returned {s}"));
                    }
                    format!("status {s}")
                }
                Err(e) if e.is_timeout() || e.is_connect() || e.is_request() => e.to_string(),
                Err(e) => return Err(e.into()),
            };
            if attempt == MAX_ATTEMPTS {
                return Err(anyhow!(
                    "news API failed after {attempt} attempts: {retryable}"
                ));
            }
            warn!(attempt, error = %retryable, "news API request failed, retrying");
            tokio::time::sleep(delay).await;
            delay *= 2;
        }
        unreachable!("loop returns on the last attempt")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn item(release_time: &str) -> NewsItem {
        NewsItem {
            disclosure_id: 1,
            headline: String::new(),
            cns_category: String::new(),
            message_url: None,
            release_time: release_time.into(),
            company: String::new(),
        }
    }

    #[test]
    fn release_time_is_utc_in_winter_and_summer() {
        // UTC is taken verbatim: no +1 / +2 shift is applied by us.
        let w = item("2026-02-19 07:00:25").released_at().unwrap();
        assert_eq!(w.to_rfc3339(), "2026-02-19T07:00:25+00:00");
        let s = item("2026-08-26 06:00:00").released_at().unwrap();
        assert_eq!(s.to_rfc3339(), "2026-08-26T06:00:00+00:00");
    }

    #[test]
    fn dst_transition_hours_stay_distinct_instants() {
        // The repeated local hour on 2026-10-25 (02:30 CEST and 02:30 CET) is
        // 00:30Z and 01:30Z in UTC: unambiguous, no earliest-instant guess needed.
        let a = item("2026-10-25 00:30:00").released_at().unwrap();
        let b = item("2026-10-25 01:30:00").released_at().unwrap();
        assert_eq!((b - a).num_minutes(), 60);
        // Spring-forward gap day 2026-03-29 parses fine as well.
        assert!(item("2026-03-29 01:30:00").released_at().is_ok());
    }

    #[test]
    fn bad_release_time_errors() {
        assert!(item("26/08/2026").released_at().is_err());
    }

    fn client(url: String) -> NewsClient {
        NewsClient::new(url, Duration::ZERO, Duration::from_millis(1)).unwrap()
    }

    #[tokio::test]
    async fn fetches_and_parses_fixture() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(query_param("company", "GomSpace Group AB"))
            .and(query_param("timeZone", "UTC"))
            .respond_with(ResponseTemplate::new(200).set_body_string(include_str!(
                "../../tests/fixtures/nasdaq_news_gomspace.json"
            )))
            .mount(&server)
            .await;
        let mut c = client(server.uri());
        let page = c.fetch_page("GomSpace Group AB", 0).await.unwrap();
        assert_eq!(page.total, page.items.len());
        assert!(page.items.len() > 5);
    }

    #[tokio::test]
    async fn retries_5xx_then_succeeds() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(503))
            .up_to_n_times(2)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(r#"{"results":{"item":[]},"count":0}"#),
            )
            .mount(&server)
            .await;
        let page = client(server.uri()).fetch_page("X", 0).await.unwrap();
        assert!(page.items.is_empty());
    }

    #[tokio::test]
    async fn gives_up_after_max_attempts_and_does_not_retry_4xx() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(500))
            .expect(MAX_ATTEMPTS as u64)
            .mount(&server)
            .await;
        assert!(client(server.uri()).fetch_page("X", 0).await.is_err());

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(404))
            .expect(1)
            .mount(&server)
            .await;
        assert!(client(server.uri()).fetch_page("X", 0).await.is_err());
    }

    #[tokio::test]
    async fn requests_are_spaced_by_min_interval() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(r#"{"results":{"item":[]},"count":0}"#),
            )
            .mount(&server)
            .await;
        let mut c =
            NewsClient::new(server.uri(), Duration::from_millis(300), Duration::ZERO).unwrap();
        let t = Instant::now();
        c.fetch_page("X", 0).await.unwrap();
        c.fetch_page("X", 200).await.unwrap();
        assert!(t.elapsed() >= Duration::from_millis(300));
    }
}
