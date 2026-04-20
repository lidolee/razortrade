//! # rt-core
//!
//! Domain types for the razortrade trading system. This crate has ZERO business
//! logic and ZERO I/O. It defines the vocabulary that every other crate speaks.
//!
//! ## Design principles
//!
//! 1. All monetary amounts use `rust_decimal::Decimal`, never `f64`. Floating
//!    point arithmetic in financial code is a well-known source of silent
//!    rounding errors that become material over thousands of trades.
//! 2. All timestamps are `chrono::DateTime<Utc>`. No local time, ever.
//! 3. All enums that cross the Python/Rust boundary have stable string
//!    representations via `serde(rename_all = "snake_case")`.
//! 4. Invalid states must be unrepresentable. We prefer making the compiler
//!    enforce invariants over runtime assertions.

pub mod execution_mode;
pub mod instrument;
pub mod market_data;
pub mod order;
pub mod portfolio;
pub mod position;
pub mod signal;
pub mod time;

pub use execution_mode::ExecutionMode;
pub use instrument::{AssetClass, Broker, Instrument, Sleeve};
pub use market_data::{MarketSnapshot, OrderBookLevel, OrderBookSnapshot};
pub use order::{Order, OrderStatus, OrderType, Side, TimeInForce};
pub use portfolio::{PortfolioState, SleeveState};
pub use position::Position;
pub use signal::{Signal, SignalType};

/// Re-export Decimal so downstream crates don't need to depend on rust_decimal directly.
pub use rust_decimal::Decimal;
