use async_trait::async_trait;
use model::{asset::Asset, bar::Bar};
use time::Date;

#[async_trait]
pub trait BarProvider {
    type Error;

    /// Returns daily OHLCV bars for `asset` over the given date range, inclusive.
    async fn bars(&self, asset: &Asset, from: Date, to: Date) -> Result<Vec<Bar>, Self::Error>;
}
