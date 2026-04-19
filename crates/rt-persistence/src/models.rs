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
