//! Instrument identification and classification.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Which execution venue the instrument trades on.
///
/// This affects which client code talks to which broker and is material for
/// routing decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Broker {
    /// Kraken Spot (kraken.com).
    KrakenSpot,
    /// Kraken Futures / Perpetuals (futures.kraken.com). Separate account, separate API.
    KrakenFutures,
    /// Interactive Brokers Client Portal Web API.
    Ibkr,
}

impl fmt::Display for Broker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Broker::KrakenSpot => write!(f, "kraken_spot"),
            Broker::KrakenFutures => write!(f, "kraken_futures"),
            Broker::Ibkr => write!(f, "ibkr"),
        }
    }
}

/// Which portfolio bucket ("sleeve") a position or signal belongs to.
///
/// The sleeve determines which risk rules apply. In particular:
/// - `CryptoSpot` and `Etf` accept paper drawdowns without triggering the kill-switch.
/// - `CryptoLeverage` is the only sleeve where realized losses count toward
///   the hard 1_000 CHF kill-switch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Sleeve {
    /// Antizyklisches Krypto-Sammeln auf Spot. 40% Zielgewichtung.
    CryptoSpot,
    /// Welt-ETF (z.B. VT/VWRL) via IBKR als Stabilitätsanker. 30% Zielgewichtung.
    Etf,
    /// Momentum-Trend-Following auf Krypto-Perpetuals mit max 2x Hebel. 30% Zielgewichtung.
    CryptoLeverage,
    /// Cash-Parking in T-Bill-ETFs (SGOV) bei IBKR, um Cash Drag zu minimieren.
    CashYield,
}

impl Sleeve {
    /// Is this sleeve subject to the hard kill-switch on realized losses?
    pub fn counts_toward_kill_switch(&self) -> bool {
        matches!(self, Sleeve::CryptoLeverage)
    }

    /// Target portfolio weight for this sleeve (for rebalancing logic).
    pub fn target_weight_pct(&self) -> f64 {
        match self {
            Sleeve::CryptoSpot => 40.0,
            Sleeve::Etf => 30.0,
            Sleeve::CryptoLeverage => 30.0,
            Sleeve::CashYield => 0.0, // reserve, opportunistic
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssetClass {
    CryptoSpot,
    CryptoPerp,
    EquityEtf,
    BondEtf,
    Fiat,
}

/// A tradeable instrument on a specific broker.
///
/// The `symbol` field uses the broker's native notation:
/// - Kraken Spot: "XBT/USD", "ETH/USD"
/// - Kraken Futures: "PI_XBTUSD", "PI_ETHUSD"
/// - IBKR: "VT", "VWRL", "SGOV"
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Instrument {
    pub symbol: String,
    pub broker: Broker,
    pub asset_class: AssetClass,
    /// Minimum order size in base-currency units (e.g. 0.0001 BTC).
    pub min_order_size: rust_decimal::Decimal,
    /// Price tick (minimum price increment).
    pub tick_size: rust_decimal::Decimal,
}

impl Instrument {
    pub fn is_leveraged(&self) -> bool {
        matches!(self.asset_class, AssetClass::CryptoPerp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kill_switch_only_applies_to_leverage_sleeve() {
        assert!(Sleeve::CryptoLeverage.counts_toward_kill_switch());
        assert!(!Sleeve::CryptoSpot.counts_toward_kill_switch());
        assert!(!Sleeve::Etf.counts_toward_kill_switch());
        assert!(!Sleeve::CashYield.counts_toward_kill_switch());
    }

    #[test]
    fn target_weights_sum_to_100_excluding_cash() {
        let total = Sleeve::CryptoSpot.target_weight_pct()
            + Sleeve::Etf.target_weight_pct()
            + Sleeve::CryptoLeverage.target_weight_pct();
        assert_eq!(total, 100.0);
    }

    #[test]
    fn broker_serialization_is_stable() {
        let json = serde_json::to_string(&Broker::KrakenFutures).unwrap();
        assert_eq!(json, "\"kraken_futures\"");
    }
}
