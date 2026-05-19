use async_trait::async_trait;
use chrono::NaiveDate;
use model::asset::Asset;

/// A single quarter of reported earnings data for an asset.
#[derive(Debug, Clone)]
pub struct QuarterlyEarnings {
    /// The ticker symbol of the asset.
    pub ticker: String,
    /// The fiscal year the quarter belongs to.
    pub fiscal_year: u32,
    /// The fiscal quarter (1–4). Invariant: value is in range 1..=4; not enforced at runtime.
    pub fiscal_quarter: u8,
    /// The last day of the fiscal reporting period.
    pub period_end: NaiveDate,
    /// The date the earnings were filed / reported.
    pub filing_date: NaiveDate,
    // NOTE: f64 used to match EarningsEvent protobuf; do not use for monetary arithmetic
    /// Actual earnings per share for the quarter, if reported.
    pub eps_actual: Option<f64>,
    // NOTE: f64 used to match EarningsEvent protobuf; do not use for monetary arithmetic
    /// Actual revenue for the quarter, if reported.
    pub revenue_actual: Option<f64>,
}

/// Async trait for fetching historical quarterly earnings data.
#[async_trait]
pub trait EarningsProvider {
    type Error: std::error::Error + Send + Sync + 'static;

    /// Returns all quarterly earnings for `asset` with a `filing_date` on or after `from`.
    async fn quarterly_earnings(
        &self,
        asset: &Asset,
        from: NaiveDate,
    ) -> Result<Vec<QuarterlyEarnings>, Self::Error>;
}
