use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use model::{asset::Asset, price::Price};
use time::OffsetDateTime;
use yahoo_finance_api::YahooConnector;

use crate::provider::PriceProvider;

pub struct YahooPriceProvider {
    connector: YahooConnector,
}

#[derive(Debug, thiserror::Error)]
pub enum YahooError {
    #[error("yahoo finance error: {0}")]
    Api(#[from] yahoo_finance_api::YahooError),
    #[error("no quotes returned for {ticker} around {timestamp:?}")]
    NoData { ticker: String, timestamp: SystemTime },
}

impl YahooPriceProvider {
    pub fn new() -> Self {
        Self {
            connector: YahooConnector::new().expect("failed to create YahooConnector"),
        }
    }
}

impl Default for YahooPriceProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl PriceProvider for YahooPriceProvider {
    type Error = YahooError;

    async fn price_at(&self, asset: &Asset, at: SystemTime) -> Result<Price, Self::Error> {
        let end = to_offset(at);
        // Fetch a 5-day window ending at `at` to guarantee at least one trading day.
        let start = to_offset(at - Duration::from_secs(5 * 24 * 60 * 60));

        let resp = self
            .connector
            .get_quote_history(&asset.ticker, start, end)
            .await?;

        let quotes = resp.quotes()?;
        let quote = quotes.last().ok_or_else(|| YahooError::NoData {
            ticker: asset.ticker.clone(),
            timestamp: at,
        })?;

        Ok(Price {
            asset: asset.clone(),
            value: quote.close,
            currency: "USD".to_string(),
            timestamp: at,
        })
    }
}

fn to_offset(t: SystemTime) -> OffsetDateTime {
    let secs = t
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    OffsetDateTime::from_unix_timestamp(secs as i64).expect("invalid timestamp")
}
