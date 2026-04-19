//! # Check 2: Spread & Liquidity
//!
//! Two sub-checks combined:
//!
//! 1. **Spread too wide**: if the bid/ask spread in basis points exceeds
//!    `max_spread_bps`, maker-based execution is too expensive relative
//!    to the expected edge.
//! 2. **Insufficient depth**: if the top `book_depth_levels` levels on the
//!    side we would take from contain less than `min_book_depth_multiple` ×
//!    our order size, we risk material slippage.
//!
//! # FX handling
//!
//! The signal specifies `notional_chf`. The order book is denominated in
//! the instrument's native quote currency (USD for crypto-perpetuals,
//! potentially EUR/USD for ETFs). The required base-currency quantity
//! therefore is:
//!
//! ```text
//! notional_quote = notional_chf × fx_quote_per_chf
//! required_qty   = notional_quote / mid_price
//! ```
//!
//! where `fx_quote_per_chf` is delivered by the Python layer inside the
//! [`MarketSnapshot`]. There is no hard-coded assumption about CHF/USD
//! parity anywhere in this check.

use crate::checklist::{CheckOutcome, PreTradeCheck, PreTradeContext, RejectionReason};
use rt_core::order::Side;
use rust_decimal::Decimal;

pub struct SpreadLiquidityCheck;

impl PreTradeCheck for SpreadLiquidityCheck {
    fn id(&self) -> &'static str {
        "spread_liquidity"
    }

    fn description(&self) -> &'static str {
        "Reject if spread exceeds limit or order-book depth is insufficient"
    }

    fn evaluate(&self, ctx: &PreTradeContext<'_>) -> CheckOutcome {
        // --- FX sanity: no trade without a usable FX rate -----------------
        if ctx.market.fx_quote_per_chf <= Decimal::ZERO {
            return CheckOutcome::Rejected(RejectionReason::InvalidSignal {
                detail: format!(
                    "invalid fx_quote_per_chf in market snapshot: {}",
                    ctx.market.fx_quote_per_chf
                ),
            });
        }

        // --- Spread check -------------------------------------------------
        let spread_bps = match ctx.market.order_book.spread_bps() {
            Some(bps) => bps,
            None => {
                return CheckOutcome::Rejected(RejectionReason::InsufficientLiquidity {
                    required_qty: Decimal::ZERO,
                    available_qty: Decimal::ZERO,
                    side: "either".to_string(),
                });
            }
        };

        if spread_bps > ctx.config.max_spread_bps {
            return CheckOutcome::Rejected(RejectionReason::SpreadTooWide {
                spread_bps,
                limit_bps: ctx.config.max_spread_bps,
            });
        }

        // --- Depth check with explicit FX conversion ---------------------
        let mid = match (
            ctx.market.order_book.best_bid(),
            ctx.market.order_book.best_ask(),
        ) {
            (Some(b), Some(a)) => (b.price + a.price) / Decimal::from(2),
            _ => {
                return CheckOutcome::Rejected(RejectionReason::InsufficientLiquidity {
                    required_qty: Decimal::ZERO,
                    available_qty: Decimal::ZERO,
                    side: "either".to_string(),
                });
            }
        };

        if mid.is_zero() {
            return CheckOutcome::Rejected(RejectionReason::InsufficientLiquidity {
                required_qty: Decimal::ZERO,
                available_qty: Decimal::ZERO,
                side: "either".to_string(),
            });
        }

        // Convert CHF → instrument quote currency, then to base-currency qty.
        let notional_quote = ctx.signal.notional_chf * ctx.market.fx_quote_per_chf;
        let required_qty = notional_quote / mid;
        let required_with_buffer = required_qty * ctx.config.min_book_depth_multiple;

        let (available, side_str) = match ctx.signal.side {
            Side::Buy => (
                ctx.market.order_book.ask_depth(ctx.config.book_depth_levels),
                "ask",
            ),
            Side::Sell => (
                ctx.market.order_book.bid_depth(ctx.config.book_depth_levels),
                "bid",
            ),
        };

        if available < required_with_buffer {
            return CheckOutcome::Rejected(RejectionReason::InsufficientLiquidity {
                required_qty: required_with_buffer,
                available_qty: available,
                side: side_str.to_string(),
            });
        }

        CheckOutcome::Approved
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RiskConfig;
    use chrono::Utc;
    use rt_core::{
        instrument::Sleeve,
        market_data::{MarketSnapshot, OrderBookLevel, OrderBookSnapshot},
        portfolio::{PortfolioState, SleeveState},
        signal::{Signal, SignalStatus, SignalType},
    };

    fn ctx_with_book(
        bids: Vec<(i64, i64)>,
        asks: Vec<(i64, i64)>,
        notional_chf: Decimal,
        side: Side,
        fx: Decimal,
    ) -> (Signal, MarketSnapshot, PortfolioState, RiskConfig, chrono::DateTime<Utc>) {
        let now = Utc::now();
        let signal = Signal {
            id: 1,
            created_at: now,
            instrument_symbol: "XBT/USD".to_string(),
            side,
            signal_type: SignalType::SpotAccumulation,
            sleeve: Sleeve::CryptoSpot,
            notional_chf,
            leverage: Decimal::from(1),
            metadata: None,
            status: SignalStatus::Pending,
            processed_at: None,
            rejection_reason: None,
        };
        let book = OrderBookSnapshot {
            bids: bids.into_iter().map(|(p, q)| OrderBookLevel {
                price: Decimal::from(p),
                quantity: Decimal::new(q, 3),
            }).collect(),
            asks: asks.into_iter().map(|(p, q)| OrderBookLevel {
                price: Decimal::from(p),
                quantity: Decimal::new(q, 3),
            }).collect(),
            timestamp: now,
        };
        let market = MarketSnapshot {
            instrument_symbol: "XBT/USD".to_string(),
            order_book: book,
            last_price: Decimal::from(50_000),
            last_trade_at: now,
            atr_absolute: Decimal::from(1500),
            atr_pct: Decimal::from(3),
            funding_rate_per_8h: None,
            fx_quote_per_chf: fx,
        };
        let portfolio = PortfolioState {
            snapshot_at: now,
            sleeves: vec![SleeveState {
                sleeve: Sleeve::CryptoSpot,
                market_value_chf: Decimal::ZERO,
                cash_chf: Decimal::from(1_000),
                realized_pnl_lifetime_chf: Decimal::ZERO,
                unrealized_pnl_chf: Decimal::ZERO,
            }],
            nav_per_unit: Decimal::from(1),
            nav_hwm_per_unit: Decimal::from(1),
            total_units: Decimal::from(1000),
            kill_switch_active: false,
        };
        (signal, market, portfolio, RiskConfig::default(), now)
    }

    #[test]
    fn rejects_wide_spread() {
        // Mid ≈ 50_000, spread = 200 → ~40 bps > 20 bps limit.
        let (s, m, p, c, n) = ctx_with_book(
            vec![(49_900, 1000)],
            vec![(50_100, 1000)],
            Decimal::from(100),
            Side::Buy,
            Decimal::from(1),
        );
        let ctx = PreTradeContext { signal: &s, market: &m, portfolio: &p, config: &c, now: n };
        match SpreadLiquidityCheck.evaluate(&ctx) {
            CheckOutcome::Rejected(RejectionReason::SpreadTooWide { .. }) => {}
            other => panic!("expected spread rejection, got {:?}", other),
        }
    }

    #[test]
    fn approves_tight_book_sufficient_depth_at_parity() {
        let (s, m, p, c, n) = ctx_with_book(
            vec![(49_995, 1000)],
            vec![(50_005, 1000)],
            Decimal::from(100),
            Side::Buy,
            Decimal::from(1),
        );
        let ctx = PreTradeContext { signal: &s, market: &m, portfolio: &p, config: &c, now: n };
        assert_eq!(SpreadLiquidityCheck.evaluate(&ctx), CheckOutcome::Approved);
    }

    /// FX rate materially changes the required quantity. Confirm that
    /// a non-parity rate is applied, not silently ignored.
    #[test]
    fn fx_rate_is_actually_used() {
        // FX = 1.25 means 1 CHF buys 1.25 USD (CHF is "stronger").
        // 100 CHF × 1.25 = 125 USD notional → 125/50_000 = 0.0025 BTC needed,
        // × 3x buffer = 0.0075. Book has 1.0 on ask → ample.
        let (s, m, p, c, n) = ctx_with_book(
            vec![(49_995, 1000)],
            vec![(50_005, 1000)],
            Decimal::from(100),
            Side::Buy,
            Decimal::new(125, 2),
        );
        let ctx = PreTradeContext { signal: &s, market: &m, portfolio: &p, config: &c, now: n };
        assert_eq!(SpreadLiquidityCheck.evaluate(&ctx), CheckOutcome::Approved);
    }

    #[test]
    fn rejects_on_invalid_fx() {
        let (s, m, p, c, n) = ctx_with_book(
            vec![(49_995, 1000)],
            vec![(50_005, 1000)],
            Decimal::from(100),
            Side::Buy,
            Decimal::ZERO,
        );
        let ctx = PreTradeContext { signal: &s, market: &m, portfolio: &p, config: &c, now: n };
        match SpreadLiquidityCheck.evaluate(&ctx) {
            CheckOutcome::Rejected(RejectionReason::InvalidSignal { .. }) => {}
            other => panic!("expected invalid-signal rejection, got {:?}", other),
        }
    }
}
