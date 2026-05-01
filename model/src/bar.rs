use std::time::SystemTime;

use crate::asset::Asset;

/// A single OHLCV bar for one trading day.
#[derive(Debug, Clone)]
pub struct Bar {
    pub asset: Asset,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: f64,
    pub timestamp: SystemTime,
}
