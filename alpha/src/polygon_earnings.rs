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
    /// An underlying HTTP transport error.
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
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
        .map_err(PolygonEarningsError::Http)?;

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

    response.json().await.map_err(PolygonEarningsError::Http)
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
