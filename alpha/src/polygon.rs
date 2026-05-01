use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use model::{asset::Asset, bar::Bar};
use serde::Deserialize;
use time::Date;
use time::format_description::well_known::Iso8601;

use crate::bar_provider::BarProvider;

const BASE_URL: &str = "https://api.polygon.io";

pub struct PolygonBarProvider {
    api_key: String,
    client: reqwest::Client,
}

#[derive(Debug, thiserror::Error)]
pub enum PolygonError {
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("polygon returned status {status}: {message}")]
    Api { status: String, message: String },
}

#[derive(Deserialize)]
struct AggResponse {
    status: String,
    #[serde(default)]
    results: Vec<AggResult>,
}

#[derive(Deserialize)]
struct AggResult {
    o: f64,
    h: f64,
    l: f64,
    c: f64,
    v: f64,
    /// Unix milliseconds
    t: u64,
}

impl PolygonBarProvider {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            client: reqwest::Client::new(),
        }
    }
}

#[async_trait]
impl BarProvider for PolygonBarProvider {
    type Error = PolygonError;

    async fn bars(&self, asset: &Asset, from: Date, to: Date) -> Result<Vec<Bar>, PolygonError> {
        let from_str = from.format(&Iso8601::DATE).expect("date format");
        let to_str = to.format(&Iso8601::DATE).expect("date format");

        let url = format!(
            "{BASE_URL}/v2/aggs/ticker/{ticker}/range/1/day/{from}/{to}",
            ticker = asset.ticker,
            from = from_str,
            to = to_str,
        );

        let resp: AggResponse = self
            .client
            .get(&url)
            .query(&[("adjusted", "true"), ("sort", "asc"), ("limit", "50000")])
            .query(&[("apiKey", &self.api_key)])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        if resp.status != "OK" && resp.status != "DELAYED" {
            return Err(PolygonError::Api {
                status: resp.status.clone(),
                message: format!("no results for {}", asset.ticker),
            });
        }

        let bars = resp
            .results
            .into_iter()
            .map(|r| {
                let timestamp =
                    SystemTime::UNIX_EPOCH + Duration::from_millis(r.t);
                Bar {
                    asset: asset.clone(),
                    open: r.o,
                    high: r.h,
                    low: r.l,
                    close: r.c,
                    volume: r.v,
                    timestamp,
                }
            })
            .collect();

        Ok(bars)
    }
}
