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
    /// Nasdaq Stockholm (Sweden), the main market.
    pub const XSTO: &str = "XSTO";
    /// Nasdaq Copenhagen (Denmark), the main market.
    pub const XCSE: &str = "XCSE";
    /// Nasdaq First North Denmark (Denmark).
    /// Segment MIC operating under Nasdaq Copenhagen (XCSE).
    pub const FNDK: &str = "FNDK";
    /// Nasdaq Helsinki (Finland), the main market.
    pub const XHEL: &str = "XHEL";
    /// Nasdaq First North Finland (Finland).
    /// Segment MIC operating under Nasdaq Helsinki (XHEL).
    pub const FNFI: &str = "FNFI";

    /// Every Nordic venue we cover, in one list: Sweden, Denmark and Finland.
    /// The CLI uses it to decide which listings get MFN/ISIN mappings.
    pub const NORDIC: &[&str] = &[FNSE, XSTO, XCSE, FNDK, XHEL, FNFI];

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
        ("CPH", XCSE), // Nasdaq Copenhagen
        // ── Finland ───────────────────────────────────────────────────────
        ("HEL", XHEL), // Nasdaq Helsinki
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

    /// Whether `exchange_mic` is a US venue. Polygon only covers US listings,
    /// so callers use this to avoid Polygon lookups for other markets.
    pub fn is_us(exchange_mic: &str) -> bool {
        matches!(exchange_mic, XNAS | XNYS | "XASE" | "ARCX" | "XBTS")
    }

    /// ISO 3166-1 alpha-2 country of a known MIC, `None` for unknown ones.
    pub fn country(exchange_mic: &str) -> Option<&'static str> {
        match exchange_mic {
            m if is_us(m) => Some("US"),
            FNSE | XSTO => Some("SE"),
            XCSE | FNDK => Some("DK"),
            XHEL | FNFI => Some("FI"),
            _ => None,
        }
    }

    /// Applies a user's `--mic` override to the MIC Yahoo implied.
    ///
    /// Yahoo cannot tell a main market from its growth market (`CPH` is both
    /// `XCSE` and `FNDK`, `STO` both `XSTO` and `FNSE`). The override must be a
    /// known MIC in the same country as `yahoo_mic`.
    pub fn apply_override(yahoo_mic: &str, mic_override: Option<&str>) -> Result<String, String> {
        let Some(o) = mic_override else {
            return Ok(yahoo_mic.to_string());
        };
        let o = o.trim().to_uppercase();
        let Some(o_country) = country(&o) else {
            return Err(format!("--mic {o} is not a known MIC"));
        };
        match country(yahoo_mic) {
            Some(c) if c == o_country => Ok(o),
            _ => Err(format!(
                "--mic {o} ({o_country}) does not match the exchange Yahoo reports ({yahoo_mic})"
            )),
        }
    }

    /// Returns the ISO 4217 currency code for the primary trading currency of
    /// the given exchange MIC. Providers should use this to populate
    /// `Bar.currency` rather than hardcoding a currency string.
    ///
    /// US venues give `"USD"`, Nordic venues their local currency. Any other
    /// MIC returns `None`: a currency is never guessed, so callers must handle
    /// it (or take the currency from the data source).
    pub fn currency(exchange_mic: &str) -> Option<&'static str> {
        match exchange_mic {
            m if is_us(m) => Some("USD"),
            FNSE | XSTO => Some("SEK"),
            XCSE | FNDK => Some("DKK"),
            // Finland uses the euro: prices stay in EUR, nothing is converted.
            XHEL | FNFI => Some("EUR"),
            _ => None,
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

#[cfg(test)]
mod currency_tests {
    use super::mic;

    #[test]
    fn nordic_mics_have_local_currency() {
        assert_eq!(mic::currency(mic::XSTO), Some("SEK"));
        assert_eq!(mic::currency(mic::FNSE), Some("SEK"));
        assert_eq!(mic::currency(mic::XCSE), Some("DKK"));
        assert_eq!(mic::currency(mic::FNDK), Some("DKK"));
        assert_eq!(mic::currency(mic::XHEL), Some("EUR"));
        assert_eq!(mic::currency(mic::FNFI), Some("EUR"));
    }

    #[test]
    fn nordic_list_has_all_six_venues_and_every_one_has_a_country_and_currency() {
        assert_eq!(
            mic::NORDIC,
            [
                mic::FNSE,
                mic::XSTO,
                mic::XCSE,
                mic::FNDK,
                mic::XHEL,
                mic::FNFI
            ]
        );
        for m in mic::NORDIC {
            assert!(mic::country(m).is_some(), "{m}");
            assert!(mic::currency(m).is_some(), "{m}");
        }
    }

    #[test]
    fn finnish_mics_are_finnish_euro_and_never_usd() {
        for m in [mic::XHEL, mic::FNFI] {
            assert_eq!(mic::country(m), Some("FI"), "{m}");
            assert_eq!(mic::currency(m), Some("EUR"), "{m}");
            assert_ne!(mic::currency(m), Some("USD"), "{m}");
            assert!(!mic::is_us(m), "{m}");
        }
        assert_eq!(mic::mic_for_yahoo_exchange("HEL"), Some(mic::XHEL));
        assert_eq!(
            mic::apply_override(mic::XHEL, Some("FNFI")).unwrap(),
            "FNFI"
        );
        assert!(mic::apply_override(mic::XHEL, Some("FNDK")).is_err());
    }

    #[test]
    fn mic_override_stays_in_the_yahoo_country() {
        assert_eq!(mic::apply_override(mic::XCSE, None).unwrap(), "XCSE");
        assert_eq!(
            mic::apply_override(mic::XCSE, Some("FNDK")).unwrap(),
            "FNDK"
        );
        assert_eq!(
            mic::apply_override(mic::FNSE, Some("xsto")).unwrap(),
            "XSTO"
        );
        assert!(mic::apply_override(mic::FNSE, Some("FNDK")).is_err());
        assert!(mic::apply_override(mic::FNSE, Some("NOPE")).is_err());
    }

    #[test]
    fn us_mics_are_usd_and_unknown_is_none() {
        assert_eq!(mic::currency(mic::XNAS), Some("USD"));
        assert_eq!(mic::currency("XOSL"), None);
    }
}
