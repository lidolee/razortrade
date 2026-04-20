//! Order types: what the trader asked the broker to do.

use crate::instrument::{Broker, Instrument};
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Buy,
    Sell,
}

impl Side {
    pub fn opposite(self) -> Self {
        match self {
            Side::Buy => Side::Sell,
            Side::Sell => Side::Buy,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderType {
    /// Submit at current best bid/ask. Always Taker. Used only for emergency exits.
    Market,
    /// Limit order. Default for all strategy entries.
    Limit,
    /// Limit order that REJECTS if it would cross the spread (guaranteed Maker).
    PostOnly,
    /// Stop-loss trigger that becomes a market order when triggered.
    StopMarket,
    /// Stop-loss trigger that becomes a limit order when triggered.
    StopLimit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimeInForce {
    /// Good-Til-Cancelled.
    Gtc,
    /// Immediate-Or-Cancel: fill what you can, cancel the rest.
    Ioc,
    /// Fill-Or-Kill: fill entirely or cancel entirely.
    Fok,
    /// Good for N seconds then auto-cancel.
    GoodForSeconds(u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderStatus {
    /// Created locally, not yet submitted to broker.
    PendingSubmission,
    /// Submitted to broker, awaiting acknowledgement.
    Submitted,
    /// Broker acknowledged, order is live.
    Acknowledged,
    /// Partially filled, still live for remainder.
    PartiallyFilled,
    /// Fully filled.
    Filled,
    /// Cancelled (by us or by broker/market).
    Cancelled,
    /// Rejected by broker (insufficient margin, invalid parameters, etc.).
    Rejected,
    /// Failed locally (network error, timeout). State unknown on broker side;
    /// requires reconciliation.
    Failed,
}

impl OrderStatus {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            OrderStatus::Filled
                | OrderStatus::Cancelled
                | OrderStatus::Rejected
                | OrderStatus::Failed
        )
    }

    pub fn is_live(self) -> bool {
        matches!(
            self,
            OrderStatus::Submitted | OrderStatus::Acknowledged | OrderStatus::PartiallyFilled
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Order {
    pub id: i64,
    pub signal_id: Option<i64>,
    pub broker: Broker,
    /// Set once the broker acknowledges the order.
    pub broker_order_id: Option<String>,
    /// Client-generated order id sent to the broker as `cliOrdId`.
    /// Kraken Futures echoes this back in fills and open-orders updates,
    /// letting the reconciler match fills even when the REST response
    /// carrying `broker_order_id` was lost to a network timeout.
    /// Format: `rt-s<signal_id>` — unique per signal, stable across
    /// submit retries so the broker can deduplicate.
    pub cli_ord_id: Option<String>,
    pub instrument: Instrument,
    pub side: Side,
    pub order_type: OrderType,
    pub time_in_force: TimeInForce,
    pub quantity: Decimal,
    /// Null for market orders.
    pub limit_price: Option<Decimal>,
    /// For stop orders.
    pub stop_price: Option<Decimal>,
    pub status: OrderStatus,
    pub filled_quantity: Decimal,
    pub avg_fill_price: Option<Decimal>,
    pub fees_paid: Decimal,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub error_message: Option<String>,
    /// Drop 19 Part B — G3: Kraken-Futures `reduceOnly` Flag.
    /// True bei Panic-Close-Orders und anderen Exit-Only-Flows:
    /// Kraken lehnt die Order ab wenn ihre Ausführung die Position
    /// in die Gegenrichtung bringen würde. Schützt gegen versehentliches
    /// Short-gehen bei Doppel-Feuer der Close-Logik.
    /// None = konventioneller Entry (kein reduceOnly).
    #[serde(default)]
    pub reduce_only: Option<bool>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_states_are_mutually_exclusive_from_live_states() {
        for status in [
            OrderStatus::PendingSubmission,
            OrderStatus::Submitted,
            OrderStatus::Acknowledged,
            OrderStatus::PartiallyFilled,
            OrderStatus::Filled,
            OrderStatus::Cancelled,
            OrderStatus::Rejected,
            OrderStatus::Failed,
        ] {
            assert!(
                !(status.is_terminal() && status.is_live()),
                "status {:?} claims to be both terminal and live",
                status
            );
        }
    }

    #[test]
    fn side_opposite_is_involutive() {
        assert_eq!(Side::Buy.opposite().opposite(), Side::Buy);
        assert_eq!(Side::Sell.opposite().opposite(), Side::Sell);
    }
}
