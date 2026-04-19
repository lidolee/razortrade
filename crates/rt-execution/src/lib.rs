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

    /// List all open orders. Used for reconciliation after reconnect or restart.
    async fn open_orders(&self) -> Result<Vec<OpenOrderSummary>, ExecutionError>;

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
}
