//! The result of a simulated trade, per share, in exact decimals.

use rust_decimal::{Decimal, RoundingStrategy};

/// What a closed trade made, per share. No fees, no position size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pnl {
    /// `exit_bid - entry_ask`.
    pub per_share: Decimal,
    /// `(exit_bid - entry_ask) / entry_ask * 100`, 4 decimals.
    pub pct: Decimal,
}

/// Why there is no result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PnlProblem {
    /// Entry and exit were quoted in different currencies.
    CurrencyMismatch,
    /// The entry ask is not above zero: no division.
    NoEntryPrice,
}

/// The result of buying at `entry_ask` and selling at `exit_bid`, when both
/// were quoted in the same currency.
pub fn compute(
    entry_ask: Decimal,
    exit_bid: Decimal,
    entry_currency: Option<&str>,
    exit_currency: Option<&str>,
) -> Result<Pnl, PnlProblem> {
    if entry_currency != exit_currency {
        return Err(PnlProblem::CurrencyMismatch);
    }
    if entry_ask <= Decimal::ZERO {
        return Err(PnlProblem::NoEntryPrice);
    }
    let per_share = exit_bid - entry_ask;
    let mut pct = (per_share / entry_ask * Decimal::ONE_HUNDRED)
        .round_dp_with_strategy(4, RoundingStrategy::MidpointAwayFromZero);
    pct.rescale(4);
    Ok(Pnl { per_share, pct })
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;

    fn dec(text: &str) -> Decimal {
        Decimal::from_str(text).expect("decimal")
    }

    #[test]
    fn a_gain_is_exact_to_four_decimals() {
        let pnl = compute(dec("100.00"), dec("101.50"), Some("SEK"), Some("SEK")).expect("pnl");
        assert_eq!(pnl.per_share, dec("1.50"));
        assert_eq!(pnl.pct.to_string(), "1.5000");
    }

    #[test]
    fn a_loss_and_rounding_away_from_zero() {
        let pnl = compute(dec("3"), dec("2"), None, None).expect("pnl");
        assert_eq!(pnl.pct.to_string(), "-33.3333");
        let pnl = compute(dec("200000"), dec("200001"), None, None).expect("pnl");
        assert_eq!(pnl.pct.to_string(), "0.0005");
    }

    #[test]
    fn a_zero_entry_price_is_not_divided_by() {
        assert_eq!(
            compute(Decimal::ZERO, dec("1"), Some("SEK"), Some("SEK")),
            Err(PnlProblem::NoEntryPrice)
        );
    }

    #[test]
    fn different_currencies_give_no_result() {
        assert_eq!(
            compute(dec("100"), dec("101"), Some("SEK"), Some("EUR")),
            Err(PnlProblem::CurrencyMismatch)
        );
        assert_eq!(
            compute(dec("100"), dec("101"), Some("SEK"), None),
            Err(PnlProblem::CurrencyMismatch)
        );
    }
}
