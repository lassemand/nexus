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

    /// Mapping from Yahoo Finance `exchangeName` codes to ISO 10383 MIC codes.
    ///
    /// Yahoo's `exchangeName` field (from `chart/meta.exchangeName`) uses
    /// abbreviated codes that differ from ISO MICs. This table maps known
    /// codes to their canonical MIC. An unmapped code must fail loudly —
    /// never silently default to a US exchange.
    ///
    /// Sources:
    /// - Yahoo Finance chart API `meta.exchangeName` observed values
    /// - SWIFT ISO 10383 MIC registry
    pub const YAHOO_EXCHANGE_TO_MIC: &[(&str, &str)] = &[
        // ── United States ─────────────────────────────────────────────────
        ("NMS", XNAS),   // Nasdaq Global Select Market
        ("NGM", XNAS),   // Nasdaq Global Market
        ("NCM", XNAS),   // Nasdaq Capital Market
        ("NYQ", XNYS),   // New York Stock Exchange
        ("ASE", "XASE"), // NYSE American (formerly AMEX)
        ("PCX", "ARCX"), // NYSE Arca
        ("BTS", "XBTS"), // CBOE BZX Exchange
        // ── Sweden ────────────────────────────────────────────────────────
        ("STO", FNSE), // Nasdaq Stockholm / First North
        // ── Denmark ───────────────────────────────────────────────────────
        ("CPH", "XCSE"), // Nasdaq Copenhagen
        // ── Finland ───────────────────────────────────────────────────────
        ("HEL", "XHEL"), // Nasdaq Helsinki
        // ── Norway ────────────────────────────────────────────────────────
        ("OSL", "XOSL"), // Oslo Børs
        // ── Germany ───────────────────────────────────────────────────────
        ("GER", "XFRA"), // Frankfurt Stock Exchange / Deutsche Börse
        ("FRA", "XFRA"),
        // ── United Kingdom ────────────────────────────────────────────────
        ("LSE", "XLON"), // London Stock Exchange
        // ── Canada ────────────────────────────────────────────────────────
        ("TOR", "XTSE"), // Toronto Stock Exchange
        ("VAN", "XTSX"), // TSX Venture Exchange
    ];

    /// Resolve a Yahoo Finance `exchangeName` code to an ISO 10383 MIC.
    ///
    /// Returns `None` if the code is not in the mapping table — callers
    /// must fail loudly on `None` rather than silently defaulting.
    pub fn mic_for_yahoo_exchange(yahoo_code: &str) -> Option<&'static str> {
        YAHOO_EXCHANGE_TO_MIC
            .iter()
            .find(|(code, _)| *code == yahoo_code)
            .map(|(_, mic)| *mic)
    }

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
