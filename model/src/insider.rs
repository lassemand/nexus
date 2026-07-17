/// Helpers for the unified `UnifiedInsiderTransaction` proto message.
///
/// # Transaction ID scheme
///
/// A stable, deterministic ID for use in `amended_transaction_id`:
///
/// ```text
/// Format: "{SOURCE}:{TICKER}:{PERSON_NAME}:{TRANSACTION_DATE}"
/// Example: "SEC:AAPL:TIM COOK:2026-01-15"
///          "FI:GOMX:LARS KROGH ALMINDE:2026-03-12"
/// ```
///
/// All components are upper-cased and trimmed. Stable across re-ingestion
/// of the same underlying record.
///
/// The ID is intentionally date-level (not per-transaction) because:
/// - SEC Form 4 has no stable per-transaction sequence number in the public feed
/// - FI PDMR has no native ID either
/// - A person legitimately files multiple transactions on the same day
///   (e.g. exercising options and selling) — these are related but distinct;
///   the amendment links to the date bucket and consumers correlate on content
///
/// Compute the stable transaction ID for a record.
///
/// `source` should be the string representation of `SourceRegistry`
/// (e.g. `"SEC"` or `"FI"`).
pub fn transaction_id(
    source: &str,
    ticker: &str,
    person_name: &str,
    transaction_date: &str,
) -> String {
    format!(
        "{}:{}:{}:{}",
        source.trim().to_uppercase(),
        ticker.trim().to_uppercase(),
        person_name.trim().to_uppercase(),
        transaction_date.trim(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sec_transaction_id_is_stable() {
        let id1 = transaction_id("SEC", "AAPL", "Tim Cook", "2026-01-15");
        let id2 = transaction_id("SEC", "aapl", "TIM COOK", "2026-01-15");
        assert_eq!(id1, id2, "IDs must be case-insensitive for ticker and name");
        assert_eq!(id1, "SEC:AAPL:TIM COOK:2026-01-15");
    }

    #[test]
    fn fi_transaction_id_is_stable() {
        let id = transaction_id("FI", "GOMX", "Lars Krogh Alminde", "2026-03-12");
        assert_eq!(id, "FI:GOMX:LARS KROGH ALMINDE:2026-03-12");
    }

    #[test]
    fn different_dates_produce_different_ids() {
        let id1 = transaction_id("FI", "GOMX", "Lars Krogh Alminde", "2026-03-12");
        let id2 = transaction_id("FI", "GOMX", "Lars Krogh Alminde", "2026-03-13");
        assert_ne!(id1, id2);
    }

    #[test]
    fn different_sources_produce_different_ids() {
        let id1 = transaction_id("SEC", "AAPL", "Tim Cook", "2026-01-15");
        let id2 = transaction_id("FI", "AAPL", "Tim Cook", "2026-01-15");
        assert_ne!(id1, id2);
    }
}
