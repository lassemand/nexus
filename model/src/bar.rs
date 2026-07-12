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
    /// ISO 4217 currency code for the prices in this bar (e.g. `"USD"`, `"SEK"`).
    ///
    /// Use [`crate::asset::mic::currency`] to derive this from `asset.exchange_mic`
    /// when constructing a bar from exchange data.
    pub currency: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asset::{mic, Asset};

    fn make_bar(exchange_mic: &str) -> Bar {
        Bar {
            asset: Asset::with_mic("TEST", exchange_mic),
            open: 1.0,
            high: 2.0,
            low: 0.5,
            close: 1.5,
            volume: 1000.0,
            timestamp: SystemTime::UNIX_EPOCH,
            currency: mic::currency(exchange_mic).to_string(),
        }
    }

    #[test]
    fn usd_bar_has_usd_currency() {
        let bar = make_bar(mic::XNAS);
        assert_eq!(bar.currency, "USD");
    }

    #[test]
    fn sek_bar_has_sek_currency() {
        let bar = make_bar(mic::FNSE);
        assert_eq!(bar.currency, "SEK");
    }

    #[test]
    fn nyse_bar_has_usd_currency() {
        let bar = make_bar(mic::XNYS);
        assert_eq!(bar.currency, "USD");
    }
}
