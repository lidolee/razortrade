//! Open position representation.

use crate::instrument::{Broker, Sleeve};
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// An open position in a single instrument.
///
/// Positions are created by fills and closed by offsetting fills. The
/// `rt-persistence` layer reconstructs positions by replaying the `orders`
/// table; this struct is the canonical representation for risk checks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Position {
    pub id: i64,
    pub instrument_symbol: String,
    pub broker: Broker,
    pub sleeve: Sleeve,
    /// Signed: positive = long, negative = short.
    pub quantity: Decimal,
    pub avg_entry_price: Decimal,
    /// Effective leverage on this position. 1.0 for spot.
    pub leverage: Decimal,
    /// Liquidation price if leveraged. None for spot.
    pub liquidation_price: Option<Decimal>,
    pub opened_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Position {
    pub fn is_long(&self) -> bool {
        self.quantity > Decimal::ZERO
    }

    pub fn is_short(&self) -> bool {
        self.quantity < Decimal::ZERO
    }

    /// Notional exposure in quote currency.
    pub fn notional(&self, mark_price: Decimal) -> Decimal {
        (self.quantity * mark_price).abs()
    }

    /// Unrealized P&L in quote currency.
    pub fn unrealized_pnl(&self, mark_price: Decimal) -> Decimal {
        (mark_price - self.avg_entry_price) * self.quantity
    }

    /// Distance to liquidation in ATRs. Returns `None` for spot positions.
    ///
    /// A value below 1.5 should trigger automatic position reduction per
    /// the architecture contract.
    pub fn distance_to_liquidation_atrs(
        &self,
        mark_price: Decimal,
        atr_absolute: Decimal,
    ) -> Option<Decimal> {
        let liq = self.liquidation_price?;
        if atr_absolute.is_zero() {
            return None;
        }
        let distance = (mark_price - liq).abs();
        Some(distance / atr_absolute)
    }
}
