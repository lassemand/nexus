/// Well-known ISO 10383 Market Identifier Codes used in this codebase.
///
/// Confirmed against the official SWIFT ISO 10383 MIC registry.
pub mod mic {
    /// Nasdaq Global Select / Global Market / Capital Market (US).
    pub const XNAS: &str = "XNAS";
    /// New York Stock Exchange (US).
    pub const XNYS: &str = "XNYS";
    /// Nasdaq First North Growth Market Stockholm (Sweden).
    /// Segment MIC operating under Nasdaq Stockholm (XSTO).
    pub const FNSE: &str = "FNSE";

    /// Returns the ISO 4217 currency code for the primary trading currency of
    /// the given exchange MIC. Providers should use this to populate
    /// `Bar.currency` rather than hardcoding a currency string.
    ///
    /// Returns `"USD"` for unknown MICs as a safe default for US-centric usage,
    /// but callers constructing bars for non-US instruments should always pass
    /// the currency explicitly rather than relying on this fallback.
    pub fn currency(exchange_mic: &str) -> &'static str {
        match exchange_mic {
            XNAS | XNYS => "USD",
            FNSE => "SEK",
            _ => "USD",
        }
    }
}

/// A tradeable asset identified by its ticker symbol and the exchange it
/// trades on, expressed as an ISO 10383 Market Identifier Code (MIC).
///
/// Two assets with the same ticker but different `exchange_mic` values
/// are treated as distinct instruments (e.g. `GOMX` on `FNSE` is not
/// the same asset as a hypothetical US ticker `GOMX` on `XNAS`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Asset {
    pub ticker: String,
    /// ISO 10383 Market Identifier Code identifying the exchange or
    /// trading venue. Use the constants in [`mic`] for well-known values.
    pub exchange_mic: String,
}

impl Asset {
    /// Construct an asset traded on Nasdaq US (`XNAS`).
    ///
    /// This is the backward-compatible constructor for call sites that
    /// previously used `Asset::new(ticker)` for US-listed instruments.
    pub fn new(ticker: impl Into<String>) -> Self {
        Self {
            ticker: ticker.into(),
            exchange_mic: mic::XNAS.to_string(),
        }
    }

    /// Construct an asset with an explicit exchange MIC.
    pub fn with_mic(ticker: impl Into<String>, exchange_mic: impl Into<String>) -> Self {
        Self {
            ticker: ticker.into(),
            exchange_mic: exchange_mic.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_ticker_different_mic_are_not_equal() {
        let us = Asset::new("GOMX");
        let se = Asset::with_mic("GOMX", mic::FNSE);
        assert_ne!(us, se, "assets on different exchanges must be distinct");
    }

    #[test]
    fn same_ticker_same_mic_are_equal() {
        let a = Asset::new("AAPL");
        let b = Asset::new("AAPL");
        assert_eq!(a, b);
    }

    #[test]
    fn asset_new_defaults_to_xnas() {
        let asset = Asset::new("AAPL");
        assert_eq!(asset.exchange_mic, mic::XNAS);
    }

    #[test]
    fn with_mic_sets_exchange_correctly() {
        let asset = Asset::with_mic("GOMX", mic::FNSE);
        assert_eq!(asset.ticker, "GOMX");
        assert_eq!(asset.exchange_mic, mic::FNSE);
    }

    #[test]
    fn assets_hash_consistently_with_mic() {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        set.insert(Asset::new("AAPL"));
        set.insert(Asset::with_mic("AAPL", mic::FNSE));
        assert_eq!(
            set.len(),
            2,
            "different MICs must produce different hash keys"
        );
    }
}
