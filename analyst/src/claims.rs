use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// In-memory claims on reports, so that two language versions of one report that
/// arrive at the same moment cannot both be traded.
///
/// The database settles a report only after the decision is written, which is
/// after it has been handed to the trade scheduler. The claim closes that gap:
/// the first version to claim a report decides; the other is a twin.
#[derive(Debug, Default)]
pub struct Claims {
    state: Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
    targets: HashSet<i64>,
    companies: HashMap<(String, String), Instant>,
}

impl Claims {
    /// No claims.
    pub fn new() -> Claims {
        Claims::default()
    }

    /// Claims a report target. False when it is already claimed.
    pub fn claim_target(&self, target_id: i64) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .targets
            .insert(target_id)
    }

    /// Claims "a release of this company" for `window`, for a release that has
    /// no target. False when an earlier claim is still within the window.
    pub fn claim_company(
        &self,
        ticker: &str,
        exchange_mic: &str,
        now: Instant,
        window: Duration,
    ) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state
            .companies
            .retain(|_, claimed| now.saturating_duration_since(*claimed) < window);
        let key = (ticker.to_owned(), exchange_mic.to_owned());
        if state.companies.contains_key(&key) {
            return false;
        }
        state.companies.insert(key, now);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_target_is_claimed_once() {
        let claims = Claims::new();
        assert!(claims.claim_target(1));
        assert!(!claims.claim_target(1));
        assert!(claims.claim_target(2));
    }

    #[test]
    fn a_company_claim_lasts_for_the_window() {
        let claims = Claims::new();
        let start = Instant::now();
        let window = Duration::from_secs(600);
        assert!(claims.claim_company("AAA", "XSTO", start, window));
        assert!(!claims.claim_company("AAA", "XSTO", start + Duration::from_secs(25), window));
        assert!(
            claims.claim_company("BBB", "XSTO", start, window),
            "another company"
        );
        assert!(
            claims.claim_company("AAA", "XHEL", start, window),
            "another exchange"
        );
        assert!(
            claims.claim_company("AAA", "XSTO", start + Duration::from_secs(601), window),
            "the window has passed"
        );
    }
}
