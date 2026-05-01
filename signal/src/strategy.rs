use std::time::{Duration, SystemTime, UNIX_EPOCH};

use alpha::PriceProvider;
use chrono::NaiveDate;
use model::{asset::Asset, generated::EarningsEvent};
use thiserror::Error;

use crate::db::TradeResult;

#[derive(Debug, Error)]
pub enum StrategyError {
    #[error("price fetch failed: {0}")]
    Price(String),
}

/// Simulates buying the day before the earnings report and selling on the report day.
pub async fn evaluate<P>(provider: &P, event: &EarningsEvent) -> Result<TradeResult, StrategyError>
where
    P: PriceProvider,
    P::Error: std::fmt::Display,
{
    if event.announced_at_unix_secs <= 0 {
        return Err(StrategyError::Price(format!(
            "invalid timestamp for {}: announced_at_unix_secs must be > 0",
            event.ticker
        )));
    }

    let asset = Asset::new(&event.ticker);

    let report_ts = UNIX_EPOCH + Duration::from_secs(event.announced_at_unix_secs as u64);
    let buy_ts = report_ts - Duration::from_secs(24 * 60 * 60);
    // Sell the day after the announcement — earnings are typically after market close,
    // so the market reaction is priced in on the following trading day.
    let sell_ts = report_ts + Duration::from_secs(24 * 60 * 60);

    let buy_price = provider
        .price_at(&asset, buy_ts)
        .await
        .map_err(|e| StrategyError::Price(e.to_string()))?
        .value;

    let sell_price = provider
        .price_at(&asset, sell_ts)
        .await
        .map_err(|e| StrategyError::Price(e.to_string()))?
        .value;

    let pnl = sell_price - buy_price;
    let pnl_pct = pnl / buy_price * 100.0;

    let earnings_date = to_naive_date(report_ts);
    let buy_date = to_naive_date(buy_ts);
    let sell_date = to_naive_date(sell_ts);

    Ok(TradeResult {
        ticker: event.ticker.clone(),
        earnings_date,
        buy_date,
        sell_date,
        buy_price,
        sell_price,
        pnl,
        pnl_pct,
    })
}

fn to_naive_date(t: SystemTime) -> NaiveDate {
    let secs = t.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    let dt = chrono::DateTime::from_timestamp(secs as i64, 0).unwrap_or_default();
    dt.date_naive()
}
