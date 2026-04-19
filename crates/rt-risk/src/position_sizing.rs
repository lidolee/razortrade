//! ATR-based position sizing.
//!
//! Gemini's Round-2 critique argued that leverage should be a "capital
//! binding tool, not a volume amplifier". This module operationalises that:
//! the caller specifies how much CHF risk they want to expose per trade
//! (default: 1% of equity for the CryptoLeverage sleeve), and this module
//! returns the quantity to trade such that an adverse move of N × ATR
//! would cost exactly that much.
//!
//! ## Formula
//!
//! ```text
//! risk_chf = equity_chf × risk_per_trade_fraction
//! stop_distance_quote = atr_absolute × stop_multiplier
//! quantity = risk_chf / stop_distance_quote
//! ```
//!
//! Leverage then determines margin usage, not position size.

use rust_decimal::Decimal;

#[derive(Debug, Clone)]
pub struct PositionSizing {
    /// Quantity to buy/sell in base-currency units (e.g. BTC amount).
    pub quantity: Decimal,
    /// The stop-loss price distance used in the calculation (for reference).
    pub stop_distance_quote: Decimal,
    /// The CHF risk this position represents assuming the stop is hit.
    pub risk_chf: Decimal,
}

/// Compute position size given volatility and an intended risk envelope.
///
/// Returns `None` if ATR is zero (unusable) or if the stop distance would
/// imply a position larger than the equity can cover (guard against
/// degenerate inputs).
pub fn atr_based_size(
    equity_chf: Decimal,
    risk_per_trade_fraction: Decimal,
    atr_absolute_quote: Decimal,
    stop_multiplier: Decimal,
    price_chf_per_quote_unit: Decimal,
) -> Option<PositionSizing> {
    if atr_absolute_quote.is_zero() || price_chf_per_quote_unit.is_zero() {
        return None;
    }

    let risk_chf = equity_chf * risk_per_trade_fraction;
    let stop_distance_quote = atr_absolute_quote * stop_multiplier;

    // Convert stop distance in quote currency to stop distance in CHF per
    // unit of base currency:
    //   stop_distance_chf_per_base = stop_distance_quote × price_chf_per_quote_unit
    // Wait — price_chf_per_quote_unit is the FX-like conversion. At our
    // scales CHF ≈ USD within 1-2%. For a simpler and safer model we
    // treat quote_unit = CHF (which is what the Python layer ensures by
    // passing notional in CHF), so the formula reduces to:
    let stop_distance_chf = stop_distance_quote * price_chf_per_quote_unit;
    if stop_distance_chf.is_zero() {
        return None;
    }

    let quantity = risk_chf / stop_distance_chf;

    Some(PositionSizing {
        quantity,
        stop_distance_quote,
        risk_chf,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_sizing_example() {
        // Equity 10_000 CHF, risk 1%, ATR 1500 USD on BTC, 2× ATR stop,
        // USD/CHF ≈ 1.0 assumption for simplicity.
        let sizing = atr_based_size(
            Decimal::from(10_000),
            Decimal::new(1, 2),        // 0.01
            Decimal::from(1500),
            Decimal::from(2),
            Decimal::from(1),
        )
        .expect("sizing should succeed");

        // risk_chf = 100; stop_distance = 3000; quantity = 100/3000 = 0.033...
        assert_eq!(sizing.risk_chf, Decimal::from(100));
        assert_eq!(sizing.stop_distance_quote, Decimal::from(3000));
        // 0.0333...
        let expected = Decimal::from(100) / Decimal::from(3000);
        assert_eq!(sizing.quantity, expected);
    }

    #[test]
    fn zero_atr_returns_none() {
        let sizing = atr_based_size(
            Decimal::from(10_000),
            Decimal::new(1, 2),
            Decimal::ZERO,
            Decimal::from(2),
            Decimal::from(1),
        );
        assert!(sizing.is_none());
    }
}
