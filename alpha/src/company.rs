use async_trait::async_trait;
use model::{asset::Asset, sector::Sector};

#[async_trait]
pub trait CompanyProvider {
    type Error: std::error::Error + Send + Sync + 'static;

    /// Returns the primary sector for the given asset.
    async fn sector(&self, asset: &Asset) -> Result<Sector, Self::Error>;
}
