//! Local Level-2 order book.
//!
//! # Data structure
//!
//! `BTreeMap<Decimal, Decimal>` for each side. For bids we iterate in
//! reverse (highest price first); for asks, natural order. At our scale
//! (25 levels per side), `BTreeMap` gives us:
//!
//! - O(log n) insert / update / delete.
//! - O(1) best bid/ask via `last_key_value` (bids) or `first_key_value` (asks).
//! - O(n) ordered iteration for depth queries — fine at 25 levels.
//!
//! Memory footprint: ~5 KB per instrument. We track 1–5 instruments in
//! production, so aggregate < 25 KB.
//!
//! # Sequence validation
//!
//! Kraken publishes a monotonic `seq` field on both snapshot and delta
//! messages, per product. Any gap means we missed an update and the local
//! book is now inconsistent with the exchange — we MUST resynchronise by
//! unsubscribing and resubscribing to get a fresh snapshot. Until then,
//! the book is marked `OutOfSync` and all reads return an error.
//!
//! # Cross-book detection
//!
//! If the top bid is ever ≥ the top ask, the book is corrupt. This can
//! happen if we apply updates out of order or if Kraken sends inconsistent
//! data (extremely rare but observed during partial outages). We detect
//! this in `sanity_check` and mark the book out-of-sync.

use crate::messages::{BookDeltaMessage, BookSide, BookSnapshotMessage};
use chrono::{DateTime, Utc};
use rt_core::market_data::{MarketSnapshot, OrderBookLevel, OrderBookSnapshot};
use rust_decimal::Decimal;
use std::collections::BTreeMap;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum BookError {
    #[error("sequence gap on {product}: expected {expected}, got {got}")]
    SequenceGap {
        product: String,
        expected: u64,
        got: u64,
    },

    #[error("book for {product} is out-of-sync: {reason}")]
    OutOfSync { product: String, reason: String },

    #[error("book for {product} is not yet initialised")]
    NotInitialised { product: String },

    #[error("crossed book detected: bid {bid} >= ask {ask}")]
    Crossed { bid: Decimal, ask: Decimal },

    #[error("product mismatch: expected {expected}, got {got}")]
    ProductMismatch { expected: String, got: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BookState {
    /// No snapshot received yet.
    Uninitialised,
    /// Snapshot received and deltas are being applied in-order.
    Synced,
    /// Lost data or detected corruption. All reads fail until resync.
    OutOfSync,
}

#[derive(Debug)]
pub struct LocalOrderBook {
    product_id: String,
    bids: BTreeMap<Decimal, Decimal>,
    asks: BTreeMap<Decimal, Decimal>,
    /// The sequence number of the most recently applied message.
    /// `None` until the first snapshot arrives.
    last_seq: Option<u64>,
    /// When the book was last updated.
    last_update_at: Option<DateTime<Utc>>,
    state: BookState,
}

impl LocalOrderBook {
    pub fn new(product_id: impl Into<String>) -> Self {
        Self {
            product_id: product_id.into(),
            bids: BTreeMap::new(),
            asks: BTreeMap::new(),
            last_seq: None,
            last_update_at: None,
            state: BookState::Uninitialised,
        }
    }

    pub fn product_id(&self) -> &str {
        &self.product_id
    }

    pub fn state(&self) -> BookState {
        self.state
    }

    pub fn is_synced(&self) -> bool {
        self.state == BookState::Synced
    }

    pub fn last_seq(&self) -> Option<u64> {
        self.last_seq
    }

    /// Apply a snapshot message. Resets the book completely.
    pub fn apply_snapshot(&mut self, snap: &BookSnapshotMessage) -> Result<(), BookError> {
        if snap.product_id != self.product_id {
            return Err(BookError::ProductMismatch {
                expected: self.product_id.clone(),
                got: snap.product_id.clone(),
            });
        }

        self.bids.clear();
        self.asks.clear();
        for level in &snap.bids {
            if level.qty > Decimal::ZERO {
                self.bids.insert(level.price, level.qty);
            }
        }
        for level in &snap.asks {
            if level.qty > Decimal::ZERO {
                self.asks.insert(level.price, level.qty);
            }
        }

        self.last_seq = Some(snap.seq);
        self.last_update_at = Some(Utc::now());
        self.state = BookState::Synced;

        // Verify invariants after the snapshot.
        self.sanity_check()?;
        Ok(())
    }

    /// Apply a delta message. Validates the sequence number strictly.
    pub fn apply_delta(&mut self, delta: &BookDeltaMessage) -> Result<(), BookError> {
        if delta.product_id != self.product_id {
            return Err(BookError::ProductMismatch {
                expected: self.product_id.clone(),
                got: delta.product_id.clone(),
            });
        }

        match self.state {
            BookState::Uninitialised => {
                return Err(BookError::NotInitialised {
                    product: self.product_id.clone(),
                })
            }
            BookState::OutOfSync => {
                return Err(BookError::OutOfSync {
                    product: self.product_id.clone(),
                    reason: "book was previously marked out-of-sync".to_string(),
                })
            }
            BookState::Synced => {}
        }

        let expected = self
            .last_seq
            .expect("Synced implies last_seq is set")
            + 1;
        if delta.seq != expected {
            self.state = BookState::OutOfSync;
            return Err(BookError::SequenceGap {
                product: self.product_id.clone(),
                expected,
                got: delta.seq,
            });
        }

        let side = match delta.side {
            BookSide::Buy => &mut self.bids,
            BookSide::Sell => &mut self.asks,
        };

        if delta.qty.is_zero() {
            side.remove(&delta.price);
        } else {
            side.insert(delta.price, delta.qty);
        }

        self.last_seq = Some(delta.seq);
        self.last_update_at = Some(Utc::now());

        // Cheap post-update sanity check. If the book went inconsistent
        // (crossed), flip to OutOfSync and surface the error.
        if let Err(e) = self.sanity_check() {
            self.state = BookState::OutOfSync;
            return Err(e);
        }

        Ok(())
    }

    /// Mark the book as out-of-sync (e.g. on WebSocket disconnect).
    pub fn invalidate(&mut self, reason: impl Into<String>) {
        tracing::warn!(
            product = %self.product_id,
            reason = %reason.into(),
            "order book invalidated"
        );
        self.state = BookState::OutOfSync;
        self.bids.clear();
        self.asks.clear();
        self.last_seq = None;
    }

    /// Highest bid (price, quantity).
    pub fn best_bid(&self) -> Option<(Decimal, Decimal)> {
        self.bids.last_key_value().map(|(p, q)| (*p, *q))
    }

    /// Lowest ask.
    pub fn best_ask(&self) -> Option<(Decimal, Decimal)> {
        self.asks.first_key_value().map(|(p, q)| (*p, *q))
    }

    /// Verifies book is not crossed. Called internally after every mutation.
    pub fn sanity_check(&self) -> Result<(), BookError> {
        if let (Some((bid, _)), Some((ask, _))) = (self.best_bid(), self.best_ask()) {
            if bid >= ask {
                return Err(BookError::Crossed { bid, ask });
            }
        }
        Ok(())
    }

    /// Build a `rt_core::OrderBookSnapshot` capped at `depth` levels per side.
    ///
    /// Fails if the book is not currently synced — pre-trade checks will
    /// then reject the signal as stale-data rather than act on a stale or
    /// partial book.
    pub fn to_rf_core_snapshot(&self, depth: usize) -> Result<OrderBookSnapshot, BookError> {
        if !self.is_synced() {
            return Err(BookError::OutOfSync {
                product: self.product_id.clone(),
                reason: format!("current state: {:?}", self.state),
            });
        }

        let bids: Vec<OrderBookLevel> = self
            .bids
            .iter()
            .rev()
            .take(depth)
            .map(|(price, qty)| OrderBookLevel {
                price: *price,
                quantity: *qty,
            })
            .collect();

        let asks: Vec<OrderBookLevel> = self
            .asks
            .iter()
            .take(depth)
            .map(|(price, qty)| OrderBookLevel {
                price: *price,
                quantity: *qty,
            })
            .collect();

        Ok(OrderBookSnapshot {
            bids,
            asks,
            timestamp: self.last_update_at.unwrap_or_else(Utc::now),
        })
    }

    /// Convenience: construct a full `MarketSnapshot` for the risk layer.
    /// The caller supplies the per-instrument fields that the book itself
    /// doesn't know about (ATR, funding rate, FX rate, last trade).
    #[allow(clippy::too_many_arguments)]
    pub fn to_market_snapshot(
        &self,
        depth: usize,
        last_price: Decimal,
        last_trade_at: DateTime<Utc>,
        atr_absolute: Decimal,
        atr_pct: Decimal,
        funding_rate_per_8h: Option<Decimal>,
        fx_quote_per_chf: Decimal,
    ) -> Result<MarketSnapshot, BookError> {
        let book = self.to_rf_core_snapshot(depth)?;
        Ok(MarketSnapshot {
            instrument_symbol: self.product_id.clone(),
            order_book: book,
            last_price,
            last_trade_at,
            atr_absolute,
            atr_pct,
            funding_rate_per_8h,
            fx_quote_per_chf,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::messages::PriceLevel;

    fn snap(product: &str, seq: u64, bids: &[(i64, i64)], asks: &[(i64, i64)]) -> BookSnapshotMessage {
        BookSnapshotMessage {
            product_id: product.to_string(),
            timestamp: 1,
            seq,
            bids: bids
                .iter()
                .map(|(p, q)| PriceLevel {
                    price: Decimal::from(*p),
                    qty: Decimal::from(*q),
                })
                .collect(),
            asks: asks
                .iter()
                .map(|(p, q)| PriceLevel {
                    price: Decimal::from(*p),
                    qty: Decimal::from(*q),
                })
                .collect(),
        }
    }

    fn delta(product: &str, seq: u64, side: BookSide, price: i64, qty: i64) -> BookDeltaMessage {
        BookDeltaMessage {
            product_id: product.to_string(),
            timestamp: 1,
            seq,
            side,
            price: Decimal::from(price),
            qty: Decimal::from(qty),
        }
    }

    #[test]
    fn snapshot_initialises_book() {
        let mut b = LocalOrderBook::new("PI_XBTUSD");
        b.apply_snapshot(&snap("PI_XBTUSD", 100, &[(100, 5), (99, 3)], &[(101, 4), (102, 2)]))
            .unwrap();
        assert!(b.is_synced());
        assert_eq!(b.best_bid(), Some((Decimal::from(100), Decimal::from(5))));
        assert_eq!(b.best_ask(), Some((Decimal::from(101), Decimal::from(4))));
    }

    #[test]
    fn delta_in_sequence_updates_book() {
        let mut b = LocalOrderBook::new("PI_XBTUSD");
        b.apply_snapshot(&snap("PI_XBTUSD", 100, &[(100, 5)], &[(101, 4)])).unwrap();
        b.apply_delta(&delta("PI_XBTUSD", 101, BookSide::Buy, 100, 8)).unwrap();
        assert_eq!(b.best_bid(), Some((Decimal::from(100), Decimal::from(8))));
    }

    #[test]
    fn delta_with_gap_invalidates_book() {
        let mut b = LocalOrderBook::new("PI_XBTUSD");
        b.apply_snapshot(&snap("PI_XBTUSD", 100, &[(100, 5)], &[(101, 4)])).unwrap();
        let err = b.apply_delta(&delta("PI_XBTUSD", 103, BookSide::Buy, 100, 8))
            .unwrap_err();
        assert!(matches!(err, BookError::SequenceGap { .. }));
        assert!(!b.is_synced());
    }

    #[test]
    fn delta_with_zero_qty_removes_level() {
        let mut b = LocalOrderBook::new("PI_XBTUSD");
        b.apply_snapshot(&snap("PI_XBTUSD", 100, &[(100, 5), (99, 3)], &[(101, 4)])).unwrap();
        b.apply_delta(&delta("PI_XBTUSD", 101, BookSide::Buy, 100, 0)).unwrap();
        assert_eq!(b.best_bid(), Some((Decimal::from(99), Decimal::from(3))));
    }

    #[test]
    fn crossed_book_sets_out_of_sync() {
        let mut b = LocalOrderBook::new("PI_XBTUSD");
        b.apply_snapshot(&snap("PI_XBTUSD", 100, &[(100, 5)], &[(101, 4)])).unwrap();
        // Force a bid above the best ask → crossed.
        let err = b.apply_delta(&delta("PI_XBTUSD", 101, BookSide::Buy, 105, 1))
            .unwrap_err();
        assert!(matches!(err, BookError::Crossed { .. }));
        assert!(!b.is_synced());
    }

    #[test]
    fn product_mismatch_rejected() {
        let mut b = LocalOrderBook::new("PI_XBTUSD");
        let err = b
            .apply_snapshot(&snap("PI_ETHUSD", 100, &[(100, 5)], &[(101, 4)]))
            .unwrap_err();
        assert!(matches!(err, BookError::ProductMismatch { .. }));
    }

    #[test]
    fn delta_before_snapshot_is_rejected() {
        let mut b = LocalOrderBook::new("PI_XBTUSD");
        let err = b.apply_delta(&delta("PI_XBTUSD", 1, BookSide::Buy, 100, 1))
            .unwrap_err();
        assert!(matches!(err, BookError::NotInitialised { .. }));
    }

    #[test]
    fn to_snapshot_caps_depth() {
        let mut b = LocalOrderBook::new("PI_XBTUSD");
        b.apply_snapshot(&snap("PI_XBTUSD", 100,
            &[(100, 1), (99, 1), (98, 1), (97, 1), (96, 1)],
            &[(101, 1), (102, 1), (103, 1), (104, 1), (105, 1)])).unwrap();
        let s = b.to_rf_core_snapshot(3).unwrap();
        assert_eq!(s.bids.len(), 3);
        assert_eq!(s.asks.len(), 3);
        // Bids must be descending.
        assert_eq!(s.bids[0].price, Decimal::from(100));
        assert_eq!(s.bids[2].price, Decimal::from(98));
        // Asks must be ascending.
        assert_eq!(s.asks[0].price, Decimal::from(101));
        assert_eq!(s.asks[2].price, Decimal::from(103));
    }

    #[test]
    fn to_snapshot_fails_when_out_of_sync() {
        let mut b = LocalOrderBook::new("PI_XBTUSD");
        b.invalidate("test");
        assert!(b.to_rf_core_snapshot(5).is_err());
    }
}
