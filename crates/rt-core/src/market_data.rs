//! Market data snapshots used by the risk checklist and execution layer.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderBookLevel {
    pub price: Decimal,
    pub quantity: Decimal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderBookSnapshot {
    /// Best bids sorted descending by price.
    pub bids: Vec<OrderBookLevel>,
    /// Best asks sorted ascending by price.
    pub asks: Vec<OrderBookLevel>,
    pub timestamp: DateTime<Utc>,
}

impl OrderBookSnapshot {
    pub fn best_bid(&self) -> Option<&OrderBookLevel> {
        self.bids.first()
    }

    pub fn best_ask(&self) -> Option<&OrderBookLevel> {
        self.asks.first()
    }

    /// Absolute spread in quote-currency units.
    pub fn spread(&self) -> Option<Decimal> {
        Some(self.best_ask()?.price - self.best_bid()?.price)
    }

    /// Spread in basis points of the mid price.
    pub fn spread_bps(&self) -> Option<Decimal> {
        let bid = self.best_bid()?.price;
        let ask = self.best_ask()?.price;
        let mid = (bid + ask) / Decimal::from(2);
        if mid.is_zero() {
            return None;
        }
        Some((ask - bid) / mid * Decimal::from(10_000))
    }

    /// Total quantity available on the bid side within `depth` levels.
    pub fn bid_depth(&self, depth: usize) -> Decimal {
        self.bids
            .iter()
            .take(depth)
            .map(|l| l.quantity)
            .sum()
    }

    /// Total quantity available on the ask side within `depth` levels.
    pub fn ask_depth(&self, depth: usize) -> Decimal {
        self.asks
            .iter()
            .take(depth)
            .map(|l| l.quantity)
            .sum()
    }
}

/// A point-in-time snapshot of market state needed for a pre-trade decision.
///
/// # FX handling
///
/// Crypto instruments on Kraken are priced in USD. ETFs on IBKR can be in
/// USD, EUR, or CHF depending on the instrument. Our notional amounts are
/// always in CHF. Every `MarketSnapshot` therefore carries an explicit
/// [`Self::fx_quote_per_chf`] field so the risk layer can convert between
/// the two without any ambient currency assumption.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketSnapshot {
    pub instrument_symbol: String,
    pub order_book: OrderBookSnapshot,
    /// Last trade price, in the instrument's quote currency.
    pub last_price: Decimal,
    /// Last trade timestamp; used for staleness detection.
    pub last_trade_at: DateTime<Utc>,
    /// Average True Range over a strategy-defined window, in quote currency.
    /// Computed by the Python layer and passed through.
    pub atr_absolute: Decimal,
    /// ATR expressed as a percentage of the last price.
    pub atr_pct: Decimal,
    /// Current funding rate for perpetuals, as a decimal per 8-hour period.
    /// `None` for spot instruments.
    pub funding_rate_per_8h: Option<Decimal>,
    /// FX rate expressed as "units of the instrument's quote currency per
    /// 1 CHF". Example: if the instrument is priced in USD and today's
    /// USD/CHF rate is 0.90 (1 USD = 0.90 CHF), then `fx_quote_per_chf`
    /// is `1 / 0.90 ≈ 1.1111`, meaning 1 CHF buys 1.1111 USD.
    ///
    /// For CHF-denominated instruments (unusual but possible on IBKR),
    /// this value is exactly `1`. Must be positive and non-zero.
    pub fx_quote_per_chf: Decimal,
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::prelude::FromPrimitive;

    fn make_book(bid: f64, ask: f64) -> OrderBookSnapshot {
        OrderBookSnapshot {
            bids: vec![OrderBookLevel {
                price: Decimal::from_f64(bid).unwrap(),
                quantity: Decimal::from(1),
            }],
            asks: vec![OrderBookLevel {
                price: Decimal::from_f64(ask).unwrap(),
                quantity: Decimal::from(1),
            }],
            timestamp: Utc::now(),
        }
    }

    #[test]
    fn spread_bps_computed_correctly() {
        let book = make_book(100.0, 101.0);
        // mid = 100.5, spread = 1.0, 1.0 / 100.5 * 10_000 ≈ 99.5 bps
        let bps = book.spread_bps().unwrap();
        assert!(bps > Decimal::from(99) && bps < Decimal::from(100));
    }

    #[test]
    fn empty_book_returns_none() {
        let empty = OrderBookSnapshot {
            bids: vec![],
            asks: vec![],
            timestamp: Utc::now(),
        };
        assert!(empty.spread().is_none());
        assert!(empty.spread_bps().is_none());
    }
}
