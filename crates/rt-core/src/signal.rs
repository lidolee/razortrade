//! Signal: the Python brain's recommendation to the Rust execution layer.
//!
//! Signals are written to SQLite by Python and consumed by Rust. This type
//! defines the shared contract.

use crate::instrument::Sleeve;
use crate::order::Side;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignalType {
    /// Antizyklischer Spot-Kauf (Sammler / VCA logic).
    SpotAccumulation,
    /// Momentum-Breakout auf Spot oder Perpetual (Jäger).
    TrendEntry,
    /// Trailing-Stop oder Time-Stop auf bestehender Jäger-Position.
    TrendExit,
    /// Rebalancing-Transfer zwischen Sleeves.
    Rebalance,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignalStatus {
    /// Awaiting processing by Rust execution layer.
    Pending,
    /// Accepted by checklist, order submitted.
    Processed,
    /// Rejected by pre-trade checklist.
    Rejected,
    /// Processed but order ultimately failed at broker.
    Failed,
    /// Manually invalidated (e.g. operator cancelled).
    Cancelled,
}

/// A strategy signal from the Python layer.
///
/// # Invariants
///
/// - `notional_chf > 0`
/// - `leverage >= 1.0`; for non-leveraged signals exactly `1.0`.
/// - For `CryptoLeverage` sleeve: `leverage <= 2.0` (hard cap, enforced in checklist).
/// - For all other sleeves: `leverage == 1.0`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Signal {
    pub id: i64,
    pub created_at: DateTime<Utc>,
    pub instrument_symbol: String,
    pub side: Side,
    pub signal_type: SignalType,
    pub sleeve: Sleeve,
    /// Nominal trade size in CHF (before leverage).
    pub notional_chf: Decimal,
    /// Leverage multiplier. 1.0 = spot / no leverage.
    pub leverage: Decimal,
    /// Free-form metadata from the strategy (ATR value, breakout level,
    /// regime label, etc.). Stored as JSON string for forward compatibility.
    pub metadata: Option<serde_json::Value>,
    pub status: SignalStatus,
    pub processed_at: Option<DateTime<Utc>>,
    pub rejection_reason: Option<String>,
}
