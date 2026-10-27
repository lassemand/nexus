use chronicle_lib::registry;
use tracing::{info, warn};

/// Returns the sorted tickers registered on the `company.tickers` topic,
/// after replaying every registration and unregistration from the start
/// ([`registry::replay`]).
///
/// Reads without a consumer group, so each call starts from offset 0. Returns
/// an empty list (logged) when the topic cannot be read.
// Used by chronicle, market, earnings, filings binaries — not all in the same compilation unit.
#[allow(dead_code)]
pub fn load_tickers(brokers: &str, topic: &str) -> Vec<String> {
    match registry::read_registrations(brokers, topic) {
        Ok(registrations) => {
            let tickers = registrations.tickers();
            info!(
                count = tickers.len(),
                unregistered = registrations.withdrawn.len(),
                topic,
                "loaded tickers from kafka topic"
            );
            tickers
        }
        Err(e) => {
            warn!(error = %e, topic, "could not read ticker registrations");
            Vec::new()
        }
    }
}
