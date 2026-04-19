//! Shared, concurrently-readable stores for private-feed data.
//!
//! Lives alongside [`crate::ws::BookStore`] and [`crate::ws::TickerStore`].
//! The daemon reads these during state reconciliation (on reconnect,
//! after a signal execution, or before issuing a new order).

use crate::messages::{FillEntry, OpenOrderVerboseEntry};
use chrono::{DateTime, Utc};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use tokio::sync::RwLock;

/// Maximum fills kept in memory. At 40 trades/month this is ~4 months
/// of history; beyond that we drop the oldest. The SQLite `orders`
/// table remains the durable source of truth.
pub const FILLS_RING_CAPACITY: usize = 200;

pub type OpenOrdersStore = Arc<RwLock<OpenOrdersState>>;
pub type FillsStore = Arc<RwLock<FillsRing>>;

/// Materialised view of the current open-order set, keyed by broker order id.
#[derive(Debug, Default)]
pub struct OpenOrdersState {
    orders: HashMap<String, OpenOrderVerboseEntry>,
    /// Highest per-account fill sequence number observed. Used to detect
    /// gaps in the fills stream.
    last_seen_at: Option<DateTime<Utc>>,
    is_synced: bool,
}

impl OpenOrdersState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace the entire snapshot. Called on initial subscription and
    /// on resubscription after reconnect.
    pub fn apply_snapshot(&mut self, orders: Vec<OpenOrderVerboseEntry>) {
        self.orders.clear();
        for o in orders {
            self.orders.insert(o.order_id.clone(), o);
        }
        self.last_seen_at = Some(Utc::now());
        self.is_synced = true;
    }

    /// Apply an incremental update.
    ///
    /// If `is_cancel` is true, remove the order (if present).
    /// Otherwise, insert or replace. Unknown order ids on cancel are
    /// logged but not treated as errors — they can legitimately arrive
    /// for orders that were cancelled just before we subscribed.
    pub fn apply_update(&mut self, update: crate::messages::OpenOrdersUpdateMessage) {
        self.last_seen_at = Some(Utc::now());

        if update.is_cancel {
            if let Some(order_id) = update
                .order
                .as_ref()
                .map(|o| o.order_id.clone())
                .or(update.order_id.clone())
            {
                self.orders.remove(&order_id);
            }
            return;
        }

        if let Some(order) = update.order {
            self.orders.insert(order.order_id.clone(), order);
        }
    }

    /// Invalidate on WebSocket disconnect. Subsequent reads will fail
    /// until a fresh snapshot arrives.
    pub fn invalidate(&mut self) {
        self.orders.clear();
        self.is_synced = false;
    }

    pub fn is_synced(&self) -> bool {
        self.is_synced
    }

    pub fn get(&self, order_id: &str) -> Option<&OpenOrderVerboseEntry> {
        self.orders.get(order_id)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &OpenOrderVerboseEntry)> {
        self.orders.iter()
    }

    pub fn len(&self) -> usize {
        self.orders.len()
    }

    pub fn is_empty(&self) -> bool {
        self.orders.is_empty()
    }

    pub fn last_seen_at(&self) -> Option<DateTime<Utc>> {
        self.last_seen_at
    }
}

/// Bounded ring buffer of recent fill events. The daemon consumes these
/// to update realised P&L and position state in SQLite.
#[derive(Debug)]
pub struct FillsRing {
    fills: VecDeque<FillEntry>,
    capacity: usize,
    highest_seq: u64,
    is_synced: bool,
}

impl FillsRing {
    pub fn new(capacity: usize) -> Self {
        Self {
            fills: VecDeque::with_capacity(capacity),
            capacity,
            highest_seq: 0,
            is_synced: false,
        }
    }

    pub fn apply_snapshot(&mut self, fills: Vec<FillEntry>) {
        self.fills.clear();
        self.highest_seq = 0;
        for f in fills {
            self.highest_seq = self.highest_seq.max(f.seq);
            self.push(f);
        }
        self.is_synced = true;
    }

    /// Append a new fill. Returns `false` if the fill was a duplicate
    /// (seq ≤ highest_seq); the daemon should treat duplicates as
    /// harmless no-ops.
    pub fn apply_fills(&mut self, fills: Vec<FillEntry>) -> FillApplyResult {
        let mut appended = 0_usize;
        let mut duplicates = 0_usize;
        let mut gap_detected = false;

        for f in fills {
            if f.seq <= self.highest_seq {
                duplicates += 1;
                continue;
            }

            if self.highest_seq != 0 && f.seq > self.highest_seq + 1 {
                // We missed at least one fill between highest_seq and f.seq.
                // This should only happen after a disconnect; the daemon's
                // reconciliation step covers it by calling the REST fills
                // endpoint.
                gap_detected = true;
            }

            self.highest_seq = f.seq;
            self.push(f);
            appended += 1;
        }

        FillApplyResult {
            appended,
            duplicates,
            gap_detected,
        }
    }

    fn push(&mut self, fill: FillEntry) {
        if self.fills.len() == self.capacity {
            self.fills.pop_front();
        }
        self.fills.push_back(fill);
    }

    pub fn invalidate(&mut self) {
        self.fills.clear();
        self.highest_seq = 0;
        self.is_synced = false;
    }

    pub fn is_synced(&self) -> bool {
        self.is_synced
    }

    pub fn highest_seq(&self) -> u64 {
        self.highest_seq
    }

    pub fn iter(&self) -> impl Iterator<Item = &FillEntry> {
        self.fills.iter()
    }

    pub fn len(&self) -> usize {
        self.fills.len()
    }

    pub fn is_empty(&self) -> bool {
        self.fills.is_empty()
    }
}

impl Default for FillsRing {
    fn default() -> Self {
        Self::new(FILLS_RING_CAPACITY)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct FillApplyResult {
    pub appended: usize,
    pub duplicates: usize,
    pub gap_detected: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::Decimal;

    fn fill(seq: u64) -> FillEntry {
        FillEntry {
            instrument: "PI_XBTUSD".to_string(),
            time: 0,
            price: Decimal::from(50_000),
            seq,
            buy: true,
            qty: Decimal::from(1),
            remaining_order_qty: None,
            order_id: format!("order-{seq}"),
            fill_id: format!("fill-{seq}"),
            fill_type: "maker".to_string(),
            fee_paid: Decimal::ZERO,
            fee_currency: "BTC".to_string(),
            cli_ord_id: None,
            order_type: Some("limit".to_string()),
            taker_order_type: None,
        }
    }

    #[test]
    fn fills_ring_rejects_duplicates() {
        let mut ring = FillsRing::new(10);
        ring.apply_snapshot(vec![fill(1), fill(2), fill(3)]);
        let r = ring.apply_fills(vec![fill(2), fill(3), fill(4)]);
        assert_eq!(r.duplicates, 2);
        assert_eq!(r.appended, 1);
        assert!(!r.gap_detected);
        assert_eq!(ring.highest_seq(), 4);
    }

    #[test]
    fn fills_ring_detects_gap() {
        let mut ring = FillsRing::new(10);
        ring.apply_snapshot(vec![fill(1)]);
        let r = ring.apply_fills(vec![fill(5)]);
        assert!(r.gap_detected);
        assert_eq!(r.appended, 1);
        assert_eq!(ring.highest_seq(), 5);
    }

    #[test]
    fn fills_ring_respects_capacity() {
        let mut ring = FillsRing::new(3);
        for i in 1..=5 {
            ring.apply_fills(vec![fill(i)]);
        }
        assert_eq!(ring.len(), 3);
        let last_seqs: Vec<u64> = ring.iter().map(|f| f.seq).collect();
        assert_eq!(last_seqs, vec![3, 4, 5]);
    }
}
