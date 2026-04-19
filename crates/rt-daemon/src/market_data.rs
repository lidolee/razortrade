//! Market data service.
//!
//! Composes Kraken WS book and ticker stores into a `MarketSnapshot`
//! suitable for the risk checklist. This is the adapter between the
//! broker-specific data plane (`rt-kraken-futures`) and the broker-agnostic
//! risk plane (`rt-risk`).
//!
//! # Where the fields come from
//!
//! | Field                    | Source                                                 |
//! |--------------------------|--------------------------------------------------------|
//! | `order_book`             | Kraken WS book feed, maintained in `BookStore`         |
//! | `last_price`             | Kraken WS ticker feed (`last` field)                   |
//! | `last_trade_at`          | Kraken WS ticker feed (`time` field)                   |
//! | `funding_rate_per_8h`    | Kraken WS ticker feed, converted from per-second       |
//! | `atr_absolute`, `atr_pct`| Python signal metadata (`signal.metadata.atr_*`)       |
//! | `fx_quote_per_chf`       | Python signal metadata (`signal.metadata.fx_*`)        |
//!
//! The ATR and FX rate come with the signal rather than the market feed
//! because they are computed by the Python strategy layer and are logically
//! inputs to the decision, not live market data.

use chrono::{DateTime, TimeZone, Utc};
use rt_core::market_data::MarketSnapshot;
use rt_kraken_futures::ws::{BookStore, TickerStore};
use rust_decimal::Decimal;
use serde_json::Value;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum MarketDataError {
    #[error("no book available for {symbol}")]
    NoBook { symbol: String },

    #[error("book for {symbol} is out of sync")]
    OutOfSync { symbol: String },

    #[error("no ticker available for {symbol}")]
    NoTicker { symbol: String },

    #[error("signal metadata missing required field: {field}")]
    MissingMetadata { field: &'static str },

    #[error("signal metadata field {field} has invalid value: {reason}")]
    InvalidMetadata { field: &'static str, reason: String },

    #[error("orderbook error: {0}")]
    Book(#[from] rt_kraken_futures::orderbook::BookError),
}

pub struct MarketDataService {
    books: BookStore,
    tickers: TickerStore,
    /// Depth to pass to `LocalOrderBook::to_rf_core_snapshot`. 5 by default
    /// matches the checklist's `book_depth_levels`.
    depth: usize,
}

impl MarketDataService {
    pub fn new(books: BookStore, tickers: TickerStore) -> Self {
        Self {
            books,
            tickers,
            depth: 5,
        }
    }

    pub fn with_depth(mut self, depth: usize) -> Self {
        self.depth = depth;
        self
    }

    /// Build a `MarketSnapshot` for the given Kraken product id using the
    /// current book + ticker state, plus ATR and FX rate from the signal
    /// metadata.
    ///
    /// Fails if any of the four data sources (book, ticker, ATR meta, FX
    /// meta) is missing. The caller should treat all errors as
    /// `CheckOutcome::Rejected` with an `InvalidSignal` reason.
    pub async fn snapshot_for_signal(
        &self,
        product_id: &str,
        signal_metadata: Option<&Value>,
    ) -> Result<MarketSnapshot, MarketDataError> {
        let (atr_abs, atr_pct) = extract_atr(signal_metadata)?;
        let fx = extract_fx(signal_metadata)?;

        let books = self.books.read().await;
        let book = books
            .get(product_id)
            .ok_or_else(|| MarketDataError::NoBook {
                symbol: product_id.to_string(),
            })?;

        let tickers = self.tickers.read().await;
        let ticker = tickers
            .get(product_id)
            .ok_or_else(|| MarketDataError::NoTicker {
                symbol: product_id.to_string(),
            })?;

        let last_price = ticker
            .last
            .ok_or_else(|| MarketDataError::MissingMetadata {
                field: "ticker.last",
            })?;

        let last_trade_at: DateTime<Utc> = ticker
            .time
            .and_then(|ms| Utc.timestamp_millis_opt(ms as i64).single())
            .unwrap_or_else(Utc::now);

        let funding_rate_per_8h = ticker.funding_rate_per_8h();

        let snapshot = book.to_market_snapshot(
            self.depth,
            last_price,
            last_trade_at,
            atr_abs,
            atr_pct,
            funding_rate_per_8h,
            fx,
        )?;
        Ok(snapshot)
    }
}

/// Extract `atr_absolute` and `atr_pct` from signal metadata.
///
/// Both are required for the risk checklist (Volatility check and
/// Position Sizing). Missing → reject.
fn extract_atr(meta: Option<&Value>) -> Result<(Decimal, Decimal), MarketDataError> {
    let obj = meta.ok_or(MarketDataError::MissingMetadata {
        field: "metadata",
    })?;

    let atr_abs = obj
        .get("atr_absolute")
        .ok_or(MarketDataError::MissingMetadata {
            field: "atr_absolute",
        })?;
    let atr_pct = obj
        .get("atr_pct")
        .ok_or(MarketDataError::MissingMetadata {
            field: "atr_pct",
        })?;

    let parse = |field: &'static str, v: &Value| -> Result<Decimal, MarketDataError> {
        match v {
            Value::String(s) => s.parse::<Decimal>().map_err(|e| {
                MarketDataError::InvalidMetadata {
                    field,
                    reason: format!("parse decimal: {e}"),
                }
            }),
            Value::Number(n) => n.to_string().parse::<Decimal>().map_err(|e| {
                MarketDataError::InvalidMetadata {
                    field,
                    reason: format!("parse decimal from number: {e}"),
                }
            }),
            _ => Err(MarketDataError::InvalidMetadata {
                field,
                reason: "expected string or number".to_string(),
            }),
        }
    };

    Ok((parse("atr_absolute", atr_abs)?, parse("atr_pct", atr_pct)?))
}

fn extract_fx(meta: Option<&Value>) -> Result<Decimal, MarketDataError> {
    let obj = meta.ok_or(MarketDataError::MissingMetadata {
        field: "metadata",
    })?;
    let fx = obj
        .get("fx_quote_per_chf")
        .ok_or(MarketDataError::MissingMetadata {
            field: "fx_quote_per_chf",
        })?;
    match fx {
        Value::String(s) => s.parse::<Decimal>().map_err(|e| {
            MarketDataError::InvalidMetadata {
                field: "fx_quote_per_chf",
                reason: format!("parse decimal: {e}"),
            }
        }),
        Value::Number(n) => n.to_string().parse::<Decimal>().map_err(|e| {
            MarketDataError::InvalidMetadata {
                field: "fx_quote_per_chf",
                reason: format!("parse decimal from number: {e}"),
            }
        }),
        _ => Err(MarketDataError::InvalidMetadata {
            field: "fx_quote_per_chf",
            reason: "expected string or number".to_string(),
        }),
    }
}
