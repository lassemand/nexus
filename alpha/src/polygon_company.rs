use async_trait::async_trait;
use model::{asset::Asset, sector::Sector};
use serde::Deserialize;
use tracing::warn;

use crate::company::CompanyProvider;
use crate::polygon::PolygonError;

const BASE_URL: &str = "https://api.polygon.io";

pub struct PolygonSectorProvider {
    api_key: String,
    client: reqwest::Client,
}

impl PolygonSectorProvider {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            client: reqwest::Client::new(),
        }
    }
}

#[derive(Deserialize)]
struct TickerDetailsResponse {
    results: Option<TickerDetails>,
}

#[derive(Deserialize)]
struct TickerDetails {
    sic_code: Option<String>,
}

#[async_trait]
impl CompanyProvider for PolygonSectorProvider {
    type Error = PolygonError;

    async fn sector(&self, asset: &Asset) -> Result<Sector, PolygonError> {
        let url = format!("{BASE_URL}/v3/reference/tickers/{}", asset.ticker);

        let resp: TickerDetailsResponse = self
            .client
            .get(&url)
            .query(&[("apiKey", &self.api_key)])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        let sector = resp
            .results
            .and_then(|r| r.sic_code)
            .and_then(|s| s.parse::<u32>().ok())
            .map(Sector::from_sic);

        let sector = match sector {
            Some(s) => s,
            None => {
                warn!(
                    ticker = %asset.ticker,
                    "ticker defaulted to Industrials — SIC lookup failed"
                );
                Sector::Industrials
            }
        };

        Ok(sector)
    }
}
