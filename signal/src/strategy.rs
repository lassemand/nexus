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
    /// Returned when the buy-side and sell-side prices are denominated in
    /// different currencies. Computing PnL across currencies produces a
    /// meaningless number — the platform never converts; it fails loud instead.
    #[error(
        "currency mismatch for {ticker}: buy currency is {buy_currency}, \
         sell currency is {sell_currency} — PnL not computed"
    )]
    CurrencyMismatch {
        ticker: String,
        buy_currency: String,
        sell_currency: String,
    },
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

    let buy = provider
        .price_at(&asset, buy_ts)
        .await
        .map_err(|e| StrategyError::Price(e.to_string()))?;

    let sell = provider
        .price_at(&asset, sell_ts)
        .await
        .map_err(|e| StrategyError::Price(e.to_string()))?;

    // Guard: reject cross-currency PnL computation explicitly.
    // This platform never converts currencies — mixing them silently would
    // produce a meaningless number that looks like a valid PnL.
    if buy.currency != sell.currency {
        return Err(StrategyError::CurrencyMismatch {
            ticker: event.ticker.clone(),
            buy_currency: buy.currency,
            sell_currency: sell.currency,
        });
    }

    let pnl = sell.value - buy.value;
    let pnl_pct = pnl / buy.value * 100.0;

    let earnings_date = to_naive_date(report_ts);
    let buy_date = to_naive_date(buy_ts);
    let sell_date = to_naive_date(sell_ts);

    Ok(TradeResult {
        ticker: event.ticker.clone(),
        earnings_date,
        buy_date,
        sell_date,
        buy_price: buy.value,
        sell_price: sell.value,
        pnl,
        pnl_pct,
        currency: buy.currency,
    })
}

fn to_naive_date(t: SystemTime) -> NaiveDate {
    let secs = t.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    let dt = chrono::DateTime::from_timestamp(secs as i64, 0).unwrap_or_default();
    dt.date_naive()
}

#[cfg(test)]
mod tests {
    use super::*;
    use alpha::PriceProvider;
    use async_trait::async_trait;
    use model::{asset::Asset, generated::EarningsEvent, price::Price};
    use std::time::{SystemTime, UNIX_EPOCH};

    struct MockPriceProvider {
        buy_price: f64,
        buy_currency: String,
        sell_price: f64,
        sell_currency: String,
    }

    impl MockPriceProvider {
        fn same_currency(buy: f64, sell: f64, currency: &str) -> Self {
            Self {
                buy_price: buy,
                sell_price: sell,
                buy_currency: currency.to_string(),
                sell_currency: currency.to_string(),
            }
        }

        fn mixed_currency(buy: f64, sell: f64, buy_cur: &str, sell_cur: &str) -> Self {
            Self {
                buy_price: buy,
                sell_price: sell,
                buy_currency: buy_cur.to_string(),
                sell_currency: sell_cur.to_string(),
            }
        }
    }

    #[async_trait]
    impl PriceProvider for MockPriceProvider {
        type Error = String;

        async fn price_at(&self, asset: &Asset, at: SystemTime) -> Result<Price, String> {
            // First call (buy) is earlier in time.
            let at_secs = at.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
            let report_secs = 1_000_000u64;
            let (value, currency) = if at_secs < report_secs {
                (self.buy_price, &self.buy_currency)
            } else {
                (self.sell_price, &self.sell_currency)
            };
            Ok(Price {
                asset: asset.clone(),
                value,
                currency: currency.clone(),
                timestamp: at,
            })
        }
    }

    fn event_at(ticker: &str, ts: i64) -> EarningsEvent {
        EarningsEvent {
            ticker: ticker.to_string(),
            announced_at_unix_secs: ts,
            report_period_unix_secs: ts,
            fiscal_quarter: 1,
            fiscal_year: 2024,
            eps_actual: 0.0,
            eps_estimate: 0.0,
            revenue_actual: 0.0,
            revenue_estimate: 0.0,
            cik: 0,
            filing_url: String::new(),
        }
    }

    #[tokio::test]
    async fn same_currency_usd_computes_pnl_correctly() {
        let provider = MockPriceProvider::same_currency(100.0, 110.0, "USD");
        let event = event_at("AAPL", 1_000_000);
        let result = evaluate(&provider, &event).await.unwrap();

        assert_eq!(result.buy_price, 100.0);
        assert_eq!(result.sell_price, 110.0);
        assert!((result.pnl - 10.0).abs() < 1e-9);
        assert!((result.pnl_pct - 10.0).abs() < 1e-9);
        assert_eq!(result.currency, "USD");
    }

    #[tokio::test]
    async fn same_currency_sek_computes_pnl_correctly() {
        let provider = MockPriceProvider::same_currency(17.0, 18.5, "SEK");
        let event = event_at("GOMX", 1_000_000);
        let result = evaluate(&provider, &event).await.unwrap();

        assert_eq!(result.currency, "SEK");
        assert!((result.pnl - 1.5).abs() < 1e-9);
    }

    #[tokio::test]
    async fn mismatched_currency_returns_error_not_value() {
        let provider = MockPriceProvider::mixed_currency(100.0, 110.0, "USD", "SEK");
        let event = event_at("FAKE", 1_000_000);
        let result = evaluate(&provider, &event).await;

        assert!(result.is_err(), "must not compute PnL across currencies");
        match result.unwrap_err() {
            StrategyError::CurrencyMismatch {
                buy_currency,
                sell_currency,
                ..
            } => {
                assert_eq!(buy_currency, "USD");
                assert_eq!(sell_currency, "SEK");
            }
            e => panic!("expected CurrencyMismatch, got {e}"),
        }
    }
}
