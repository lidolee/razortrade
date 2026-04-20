//! # rt-execution
//!
//! Broker-agnostic execution layer.
//!
//! This crate defines the `Broker` trait that every venue-specific client
//! implements. The daemon holds a collection of `Box<dyn Broker>` instances
//! keyed by `rt_core::Broker`, and routes orders based on instrument metadata.
//!
//! Concrete implementations (`rt-kraken-futures`, `rt-ibkr`) will live in
//! their own crates and depend on this one.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rt_core::{Order, OrderStatus};
use rust_decimal::Decimal;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ExecutionError {
    #[error("broker rejected order: {0}")]
    Rejected(String),

    #[error("broker returned invalid data: {0}")]
    InvalidResponse(String),

    #[error("network error: {0}")]
    Network(String),

    #[error("timeout after {seconds}s")]
    Timeout { seconds: u64 },

    #[error("broker rate-limited us: retry after {retry_after_ms} ms")]
    RateLimited { retry_after_ms: u64 },

    #[error("authentication failed: {0}")]
    Authentication(String),

    #[error("insufficient margin: {0}")]
    InsufficientMargin(String),

    #[error("operation not supported by this broker: {0}")]
    Unsupported(String),
}

/// The result of asking the broker to submit an order.
#[derive(Debug, Clone)]
pub struct SubmissionResult {
    pub broker_order_id: String,
    pub status: OrderStatus,
    pub submitted_at: DateTime<Utc>,
}

/// Fill event pushed from the broker (WebSocket) or polled.
#[derive(Debug, Clone)]
pub struct FillEvent {
    pub broker_order_id: String,
    pub fill_quantity: Decimal,
    pub fill_price: Decimal,
    pub fee: Decimal,
    pub is_maker: bool,
    pub occurred_at: DateTime<Utc>,
}

/// The trait every broker client must implement.
///
/// Implementations must be cancel-safe: if the caller drops the future,
/// the in-flight HTTP request is aborted and the broker-side state may be
/// unknown. Callers should use `reconcile()` on startup or after timeouts.
#[async_trait]
pub trait Broker: Send + Sync {
    /// Identifier used for routing and logging.
    fn id(&self) -> rt_core::Broker;

    /// Submit a new order. Returns the broker's order id and initial status.
    async fn submit_order(&self, order: &Order) -> Result<SubmissionResult, ExecutionError>;

    /// Cancel an existing order by broker order id.
    async fn cancel_order(&self, broker_order_id: &str) -> Result<(), ExecutionError>;

    /// Fetch the current state of a specific order.
    async fn get_order(&self, broker_order_id: &str) -> Result<OrderStatus, ExecutionError>;

    /// Drop 19 — CV-A1a: Look up an order by its client-generated id.
    ///
    /// Used by the uncertain-resubmit path in `signal_processor`:
    /// after a REST-submit timeout we do not know whether the broker
    /// accepted the order. Before re-submitting we ask the broker via
    /// its open-orders listing (filtered by our cli_ord_id). If the
    /// broker has it, we adopt the broker_order_id and mark the local
    /// order acknowledged. If it is absent, we re-submit with the same
    /// cli_ord_id — venue-side uniqueness of cli_ord_id prevents a
    /// genuine double-submit.
    ///
    /// Returns `None` when the broker has no open order with this
    /// cli_ord_id. An `Err` means the lookup itself failed (network,
    /// auth, etc.) — the caller must extend the cooldown and retry
    /// later.
    async fn get_order_by_cli_ord_id(
        &self,
        cli_ord_id: &str,
    ) -> Result<Option<OpenOrderSummary>, ExecutionError>;

    /// List all open orders. Used for reconciliation after reconnect or restart.
    async fn open_orders(&self) -> Result<Vec<OpenOrderSummary>, ExecutionError>;

    /// Drop 19 CV-6: List all open leverage positions at the broker.
    /// Used by the periodic reconciler to detect drift between our
    /// local `positions` table and Kraken reality (e.g. a position
    /// was manually closed in the Kraken UI, or a fill never reached
    /// our WebSocket feed).
    ///
    /// Default implementation returns empty so brokers that don't yet
    /// expose positions (paper, mocks) do not need to implement it.
    async fn open_positions(&self) -> Result<Vec<OpenPositionEntry>, ExecutionError> {
        Ok(Vec::new())
    }

    /// Best-effort health check: a cheap API call that confirms we can
    /// still talk to the venue and our auth is valid.
    async fn health_check(&self) -> Result<(), ExecutionError>;
}

#[derive(Debug, Clone)]
pub struct OpenOrderSummary {
    pub broker_order_id: String,
    pub instrument_symbol: String,
    pub status: OrderStatus,
    pub quantity: Decimal,
    pub filled_quantity: Decimal,
    pub limit_price: Option<Decimal>,
    /// Drop 19 — CV-A1a: echo of the client-generated id so the
    /// reconciliation logic can match back to the local order row.
    pub cli_ord_id: Option<String>,
}

/// Drop 19 CV-6: minimales Open-Position-Schema für die Reconciler.
/// `side` folgt der Kraken-Konvention long/short; `signed_quantity`
/// ist positiv bei long, negativ bei short.
#[derive(Debug, Clone)]
pub struct OpenPositionEntry {
    pub instrument_symbol: String,
    /// Positive = long, negative = short.
    pub signed_quantity: Decimal,
    pub avg_entry_price: Option<Decimal>,
    pub liquidation_price: Option<Decimal>,
    pub leverage: Option<Decimal>,
}
