//! Row types that mirror the SQLite schema.
//!
//! These are intentionally separate from the domain types in `rt-core`
//! because the schema uses TEXT-encoded decimals and ISO-8601 strings,
//! and we want the conversion layer to be explicit.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct SignalRow {
    pub id: i64,
    pub created_at: String,
    pub instrument_symbol: String,
    pub side: String,
    pub signal_type: String,
    pub sleeve: String,
    pub notional_chf: String,
    pub leverage: String,
    pub metadata_json: Option<String>,
    pub status: String,
    pub processed_at: Option<String>,
    pub rejection_reason: Option<String>,
    /// OF-2: optional hard expiry (ISO-8601 UTC).
    #[sqlx(default)]
    pub expires_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct OrderRow {
    pub id: i64,
    pub signal_id: Option<i64>,
    pub broker: String,
    pub broker_order_id: Option<String>,
    pub instrument_symbol: String,
    pub side: String,
    pub order_type: String,
    pub time_in_force: String,
    pub quantity: String,
    pub limit_price: Option<String>,
    pub stop_price: Option<String>,
    pub status: String,
    pub filled_quantity: String,
    pub avg_fill_price: Option<String>,
    pub fees_paid: String,
    pub created_at: String,
    pub updated_at: String,
    pub error_message: Option<String>,
    /// CV-1: client-generated order id, format `rt-s<signal_id>`.
    #[sqlx(default)]
    pub cli_ord_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct PositionRow {
    pub id: i64,
    pub instrument_symbol: String,
    pub broker: String,
    pub sleeve: String,
    pub quantity: String,
    pub avg_entry_price: String,
    pub leverage: String,
    pub liquidation_price: Option<String>,
    pub opened_at: String,
    pub updated_at: String,
    pub closed_at: Option<String>,
    pub realized_pnl_chf: Option<String>,
    pub realized_pnl_btc: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct EquitySnapshotRow {
    pub id: i64,
    pub timestamp: String,
    pub total_equity_chf: String,
    pub crypto_spot_value_chf: String,
    pub etf_value_chf: String,
    pub crypto_leverage_value_chf: String,
    pub cash_chf: String,
    pub realized_pnl_leverage_lifetime: String,
    pub drawdown_fraction: String,
    // Added by migration 20260419000002_nav_tracking.sql.
    pub unrealized_pnl_leverage_chf: String,
    pub nav_per_unit: String,
    pub nav_hwm_per_unit: String,
    pub total_units: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct CapitalFlowRow {
    pub id: i64,
    pub timestamp: String,
    pub amount_chf: String,
    pub flow_type: String,
    pub source: Option<String>,
    pub units_minted: Option<String>,
    pub nav_at_flow: Option<String>,
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct KillSwitchEventRow {
    pub id: i64,
    pub triggered_at: String,
    pub reason_kind: String,
    pub reason_detail_json: String,
    pub portfolio_snapshot: String,
    pub resolved_at: Option<String>,
    pub resolved_by: Option<String>,
    pub resolved_note: Option<String>,
}

/// Result of applying a single fill to a tracked order.
///
/// Returned by `Database::apply_fill_to_order` so callers can log the
/// post-update state and drive downstream side effects (the position
/// reconciler in Drop 6b uses `sleeve` and `is_buy` to attribute and
/// sign the fill when updating the positions table).
#[derive(Debug, Clone)]
pub struct OrderFillOutcome {
    /// Primary key of the local `orders` row that was updated.
    pub order_id: i64,
    /// Sleeve attribution from the originating signal, if any.
    /// `None` only for orders inserted without a signal_id (should not
    /// happen in the current pipeline but we degrade gracefully).
    pub sleeve: Option<String>,
    /// Broker identifier from the originating order row. Used to key
    /// positions by broker + instrument.
    pub broker: String,
    /// Instrument symbol from the orders row (e.g. "PI_XBTUSD").
    pub instrument_symbol: String,
    /// True if the original order was a Buy (long-direction fill).
    pub is_buy: bool,
    /// Sum of all fills applied to this order so far (cumulative).
    pub new_filled_quantity: rust_decimal::Decimal,
    /// Quantity-weighted average fill price across all fills.
    pub new_avg_fill_price: rust_decimal::Decimal,
    /// Accumulated fees paid on this order.
    pub new_fees_paid: rust_decimal::Decimal,
    /// New textual status, one of: "partially_filled", "filled".
    /// Status can only progress forward; an order that was already
    /// "filled" stays "filled" even if a stale smaller fill arrives.
    pub new_status: String,
    /// Whether this fill fully completed the order.
    pub is_fully_filled: bool,
}

/// State change applied to the open position for an instrument when a
/// fill is processed. Returned by `Database::apply_fill_to_position`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PositionTransition {
    /// No open position existed; a new row was inserted.
    Opened,
    /// Existing position on the same side; size increased and the
    /// average entry price was updated (quantity-weighted).
    Added,
    /// Existing position on the opposite side; size decreased. Entry
    /// price is unchanged. A realised PnL contribution is recorded.
    Reduced,
    /// Fill exactly closed the existing position. `closed_at` is now
    /// set and `quantity` is zero.
    Closed,
    /// Fill exceeded the existing opposite-side position: the old
    /// position was closed and a new one in the opposite direction
    /// was opened for the residual quantity. Realised PnL is recorded
    /// on the closed portion only.
    Flipped,
}

/// Result of applying a fill to the positions table.
#[derive(Debug, Clone)]
pub struct PositionFillOutcome {
    /// PK of the row that was inserted or updated. For a Flipped
    /// transition, this is the new (residual) position row.
    pub position_id: i64,
    /// New signed quantity after this fill (positive = long,
    /// negative = short, zero = flat).
    pub new_quantity: rust_decimal::Decimal,
    /// Entry price of the surviving position. `None` iff `Closed`.
    pub new_avg_entry_price: Option<rust_decimal::Decimal>,
    /// PnL realised *by this fill* in the contract's settlement
    /// currency (BTC for inverse perpetuals like PI_XBTUSD).
    /// Zero for Opened and Added transitions.
    pub realized_pnl_btc: rust_decimal::Decimal,
    /// The kind of change that was applied.
    pub transition: PositionTransition,
}
