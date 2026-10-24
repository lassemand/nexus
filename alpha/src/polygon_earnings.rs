use async_trait::async_trait;
use chrono::NaiveDate;
use model::asset::Asset;
use serde::Deserialize;
use tracing::debug;

use crate::earnings::{EarningsProvider, QuarterlyEarnings};

const DEFAULT_BASE_URL: &str = "https://api.polygon.io";

/// Polygon-backed implementation of [`EarningsProvider`].
///
/// Calls `/vx/reference/financials` and pages through all results via
/// `next_url` until the field is absent from the response.
pub struct PolygonEarningsProvider {
    api_key: String,
    client: reqwest::Client,
    base_url: String,
}

/// Errors produced by [`PolygonEarningsProvider`].
#[derive(Debug, thiserror::Error)]
pub enum PolygonEarningsError {
    /// An underlying HTTP transport error. Built through `From`, which strips
    /// the request URL so the `apiKey` query parameter never reaches a log.
    #[error("http error: {0}")]
    Http(reqwest::Error),
    /// The Polygon API returned HTTP 429 (rate limited).
    #[error("polygon rate limited (429)")]
    RateLimited,
    /// The Polygon API returned a non-2xx status other than 429.
    #[error("polygon api error {status}: {message}")]
    Api { status: u16, message: String },
    /// A date string from the API could not be parsed.
    #[error("failed to parse date: {0}")]
    DateParse(String),
}

impl From<reqwest::Error> for PolygonEarningsError {
    fn from(e: reqwest::Error) -> Self {
        Self::Http(e.without_url())
    }
}

// ---------- Serde response shapes ----------

#[derive(Deserialize)]
struct FinancialsResponse {
    #[serde(default)]
    results: Vec<FinancialResult>,
    next_url: Option<String>,
}

#[derive(Deserialize)]
struct FinancialResult {
    fiscal_period: Option<String>,
    fiscal_year: Option<u32>,
    end_date: Option<String>,
    filing_date: Option<String>,
    financials: Option<Financials>,
}

#[derive(Deserialize)]
struct Financials {
    income_statement: Option<IncomeStatement>,
}

#[derive(Deserialize)]
struct IncomeStatement {
    basic_earnings_per_share: Option<MoneyValue>,
    revenues: Option<MoneyValue>,
}

#[derive(Deserialize)]
struct MoneyValue {
    value: Option<f64>,
}

// ---------- Constructors ----------

impl PolygonEarningsProvider {
    /// Creates a new [`PolygonEarningsProvider`] using the default Polygon base URL.
    pub fn new(api_key: impl Into<String>) -> Self {
        Self::with_base_url(api_key, DEFAULT_BASE_URL)
    }

    /// Creates a new [`PolygonEarningsProvider`] using a custom base URL.
    ///
    /// Useful for pointing at a test server or a recorded-response fixture.
    pub fn with_base_url(api_key: impl Into<String>, base_url: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            client: reqwest::Client::new(),
            base_url: base_url.into(),
        }
    }
}

// ---------- Helper ----------

/// Converts a Polygon fiscal_period string to a quarter number.
///
/// Returns `None` for non-quarterly periods ("FY", "H1", "H2", "TTM") so they
/// are silently skipped.
fn fiscal_period_to_quarter(period: &str) -> Option<u8> {
    match period {
        "Q1" => Some(1),
        "Q2" => Some(2),
        "Q3" => Some(3),
        "Q4" => Some(4),
        _ => None,
    }
}

/// Fetches and deserialises a single page from `url`, appending the API key.
async fn fetch_page(
    client: &reqwest::Client,
    url: &str,
    api_key: &str,
) -> Result<FinancialsResponse, PolygonEarningsError> {
    let response = client
        .get(url)
        .query(&[("apiKey", api_key)])
        .send()
        .await
        .map_err(PolygonEarningsError::from)?;

    let status = response.status();

    if status.as_u16() == 429 {
        return Err(PolygonEarningsError::RateLimited);
    }

    if !status.is_success() {
        let message = response
            .text()
            .await
            .unwrap_or_else(|_| "<no body>".to_owned());
        return Err(PolygonEarningsError::Api {
            status: status.as_u16(),
            message,
        });
    }

    response.json().await.map_err(PolygonEarningsError::from)
}

// ---------- EarningsProvider impl ----------

#[async_trait]
impl EarningsProvider for PolygonEarningsProvider {
    type Error = PolygonEarningsError;

    /// Returns all quarterly earnings for `asset` with a `filing_date` on or
    /// after `from`, sorted ascending by `period_end`.
    async fn quarterly_earnings(
        &self,
        asset: &Asset,
        from: NaiveDate,
    ) -> Result<Vec<QuarterlyEarnings>, PolygonEarningsError> {
        let first_url = format!(
            "{}/vx/reference/financials?ticker={}&timeframe=quarterly&order=desc&limit=100&filing_date.gte={}",
            self.base_url,
            asset.ticker,
            from,
        );

        let mut all: Vec<QuarterlyEarnings> = Vec::new();
        let mut current_url: Option<String> = Some(first_url);

        while let Some(url) = current_url.take() {
            debug!(url = %url, ticker = %asset.ticker, "fetching earnings page");

            let page = fetch_page(&self.client, &url, &self.api_key).await?;

            for result in page.results {
                // Skip non-quarterly fiscal periods (FY, H1, H2, TTM).
                let fiscal_quarter = match result.fiscal_period.as_deref() {
                    Some(p) => match fiscal_period_to_quarter(p) {
                        Some(q) => q,
                        None => continue,
                    },
                    None => continue,
                };

                let fiscal_year = match result.fiscal_year {
                    Some(y) => y,
                    None => continue,
                };

                let period_end = match result.end_date.as_deref() {
                    Some(s) => NaiveDate::parse_from_str(s, "%Y-%m-%d")
                        .map_err(|_| PolygonEarningsError::DateParse(s.to_owned()))?,
                    None => continue,
                };

                let filing_date = match result.filing_date.as_deref() {
                    Some(s) => NaiveDate::parse_from_str(s, "%Y-%m-%d")
                        .map_err(|_| PolygonEarningsError::DateParse(s.to_owned()))?,
                    None => continue,
                };

                let eps_actual = result
                    .financials
                    .as_ref()
                    .and_then(|f| f.income_statement.as_ref())
                    .and_then(|i| i.basic_earnings_per_share.as_ref())
                    .and_then(|v| v.value);

                let revenue_actual = result
                    .financials
                    .as_ref()
                    .and_then(|f| f.income_statement.as_ref())
                    .and_then(|i| i.revenues.as_ref())
                    .and_then(|v| v.value);

                all.push(QuarterlyEarnings {
                    ticker: asset.ticker.clone(),
                    fiscal_year,
                    fiscal_quarter,
                    period_end,
                    filing_date,
                    eps_actual,
                    revenue_actual,
                });
            }

            // Follow next_url if present.
            current_url = page.next_url;
        }

        // Return sorted ascending by period_end.
        all.sort_by_key(|e| e.period_end);
        Ok(all)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const PATH: &str = "/vx/reference/financials";

    fn result(period: &str, end: &str, filed: &str) -> Value {
        json!({
            "fiscal_period": period,
            "fiscal_year": 2023,
            "end_date": end,
            "filing_date": filed,
            "financials": {"income_statement": {
                "basic_earnings_per_share": {"value": 1.5},
                "revenues": {"value": 1000.0}
            }}
        })
    }

    fn from() -> NaiveDate {
        NaiveDate::from_ymd_opt(2020, 1, 1).unwrap()
    }

    async fn run(server: &MockServer) -> Result<Vec<QuarterlyEarnings>, PolygonEarningsError> {
        PolygonEarningsProvider::with_base_url("key", server.uri())
            .quarterly_earnings(&Asset::new("AAPL"), from())
            .await
    }

    async fn mount_ok(server: &MockServer, body: Value) {
        Mock::given(method("GET"))
            .and(path(PATH))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn http_errors_do_not_carry_the_api_key() {
        // A refused connection is a transport error, which carries the request
        // URL — and with it the `apiKey` query parameter — unless stripped.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        let err = PolygonEarningsProvider::with_base_url("s3cr3t-key", base)
            .quarterly_earnings(&Asset::new("AAPL"), from())
            .await
            .unwrap_err();
        assert!(matches!(err, PolygonEarningsError::Http(_)), "{err:?}");
        assert!(!format!("{err} {err:?}").contains("s3cr3t-key"), "{err:?}");
    }

    #[tokio::test]
    async fn happy_path_sorted_ascending() {
        let server = MockServer::start().await;
        mount_ok(
            &server,
            json!({"results": [
                result("Q3", "2023-09-30", "2023-11-01"),
                result("Q1", "2023-03-31", "2023-05-01"),
                result("Q2", "2023-06-30", "2023-08-01"),
            ]}),
        )
        .await;
        let out = run(&server).await.unwrap();
        assert_eq!(out.len(), 3);
        assert!(out.windows(2).all(|w| w[0].period_end < w[1].period_end));
        assert_eq!(out[0].fiscal_quarter, 1);
        assert_eq!(out[0].eps_actual, Some(1.5));
        assert_eq!(out[0].revenue_actual, Some(1000.0));
    }

    #[tokio::test]
    async fn pagination_combines_pages() {
        let server = MockServer::start().await;
        let next = format!("{}/page2?cursor=abc", server.uri());
        Mock::given(method("GET"))
            .and(path(PATH))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "results": [
                    result("Q3", "2023-09-30", "2023-11-01"),
                    result("Q2", "2023-06-30", "2023-08-01"),
                ],
                "next_url": next,
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/page2"))
            .and(query_param("cursor", "abc"))
            .and(query_param("apiKey", "key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "results": [result("Q1", "2023-03-31", "2023-05-01")]
            })))
            .expect(1)
            .mount(&server)
            .await;
        let out = run(&server).await.unwrap();
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].fiscal_quarter, 1);
        assert_eq!(out[2].fiscal_quarter, 3);
    }

    #[tokio::test]
    async fn filters_non_quarterly_periods() {
        let server = MockServer::start().await;
        mount_ok(
            &server,
            json!({"results": [
                result("Q1", "2023-03-31", "2023-05-01"),
                result("FY", "2023-12-31", "2024-02-01"),
                result("H1", "2023-06-30", "2023-08-01"),
                result("TTM", "2023-09-30", "2023-11-01"),
            ]}),
        )
        .await;
        let out = run(&server).await.unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].fiscal_quarter, 1);
    }

    #[tokio::test]
    async fn missing_eps_is_none() {
        let server = MockServer::start().await;
        let mut r = result("Q1", "2023-03-31", "2023-05-01");
        r["financials"]["income_statement"]
            .as_object_mut()
            .unwrap()
            .remove("basic_earnings_per_share");
        mount_ok(&server, json!({"results": [r]})).await;
        let out = run(&server).await.unwrap();
        assert_eq!(out[0].eps_actual, None);
        assert_eq!(out[0].revenue_actual, Some(1000.0));
    }

    #[tokio::test]
    async fn missing_revenue_is_none() {
        let server = MockServer::start().await;
        let mut r = result("Q1", "2023-03-31", "2023-05-01");
        r["financials"]["income_statement"]
            .as_object_mut()
            .unwrap()
            .remove("revenues");
        mount_ok(&server, json!({"results": [r]})).await;
        let out = run(&server).await.unwrap();
        assert_eq!(out[0].revenue_actual, None);
        assert_eq!(out[0].eps_actual, Some(1.5));
    }

    #[tokio::test]
    async fn http_429_is_rate_limited() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(429))
            .mount(&server)
            .await;
        assert!(matches!(
            run(&server).await,
            Err(PolygonEarningsError::RateLimited)
        ));
    }

    #[tokio::test]
    async fn http_500_is_api_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;
        match run(&server).await {
            Err(PolygonEarningsError::Api { status, message }) => {
                assert_eq!(status, 500);
                assert_eq!(message, "boom");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn sends_filing_date_gte_param() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(PATH))
            .and(query_param("filing_date.gte", "2020-01-01"))
            .and(query_param("ticker", "AAPL"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"results": []})))
            .expect(1)
            .mount(&server)
            .await;
        assert!(run(&server).await.unwrap().is_empty());
    }
}
