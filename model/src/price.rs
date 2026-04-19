use std::time::SystemTime;

use crate::asset::Asset;

/// The price of an asset at a specific point in time.
#[derive(Debug, Clone)]
pub struct Price {
    pub asset: Asset,
    pub value: f64,
    pub currency: String,
    pub timestamp: SystemTime,
}
