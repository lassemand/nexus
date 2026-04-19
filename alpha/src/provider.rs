use std::time::SystemTime;

use async_trait::async_trait;
use model::{asset::Asset, price::Price};

#[async_trait]
pub trait PriceProvider {
    type Error;

    /// Returns the price of `asset` at the given point in time.
    async fn price_at(&self, asset: &Asset, at: SystemTime) -> Result<Price, Self::Error>;
}
