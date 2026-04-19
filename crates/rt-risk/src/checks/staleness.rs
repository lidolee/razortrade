//! # Check 1: Staleness
//!
//! Rejects signals if the last market-data tick is older than
//! `RiskConfig::max_tick_age_ms`. Operates on the order-book timestamp
//! (not the `last_trade_at`): a stale order book means our price
//! assumptions are wrong.

use crate::checklist::{CheckOutcome, PreTradeCheck, PreTradeContext, RejectionReason};

pub struct StalenessCheck;

impl PreTradeCheck for StalenessCheck {
    fn id(&self) -> &'static str {
        "staleness"
    }

    fn description(&self) -> &'static str {
        "Reject if last order-book update is older than the configured tick-age limit"
    }

    fn evaluate(&self, ctx: &PreTradeContext<'_>) -> CheckOutcome {
        let age = ctx.now - ctx.market.order_book.timestamp;
        let age_ms = age.num_milliseconds().max(0) as u64;
        let limit = ctx.config.max_tick_age_ms;

        if age_ms > limit {
            CheckOutcome::Rejected(RejectionReason::StaleMarketData {
                last_tick_age_ms: age_ms,
                limit_ms: limit,
            })
        } else {
            CheckOutcome::Approved
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RiskConfig;
    use chrono::{Duration, Utc};
    use rt_core::{
        instrument::{Broker, Sleeve},
        market_data::{MarketSnapshot, OrderBookLevel, OrderBookSnapshot},
        order::Side,
        portfolio::{PortfolioState, SleeveState},
        signal::{Signal, SignalStatus, SignalType},
    };
    use rust_decimal::Decimal;

    fn make_ctx(book_age_ms: i64) -> (Signal, MarketSnapshot, PortfolioState, RiskConfig, chrono::DateTime<Utc>) {
        let now = Utc::now();
        let book_ts = now - Duration::milliseconds(book_age_ms);

        let signal = Signal {
            id: 1,
            created_at: now,
            instrument_symbol: "XBT/USD".to_string(),
            side: Side::Buy,
            signal_type: SignalType::SpotAccumulation,
            sleeve: Sleeve::CryptoSpot,
            notional_chf: Decimal::from(100),
            leverage: Decimal::from(1),
            metadata: None,
            status: SignalStatus::Pending,
            processed_at: None,
            rejection_reason: None,
        };

        let market = MarketSnapshot {
            instrument_symbol: "XBT/USD".to_string(),
            order_book: OrderBookSnapshot {
                bids: vec![OrderBookLevel { price: Decimal::from(50_000), quantity: Decimal::from(1) }],
                asks: vec![OrderBookLevel { price: Decimal::from(50_010), quantity: Decimal::from(1) }],
                timestamp: book_ts,
            },
            last_price: Decimal::from(50_000),
            last_trade_at: book_ts,
            atr_absolute: Decimal::from(1500),
            atr_pct: Decimal::from(3),
            funding_rate_per_8h: None,
            fx_quote_per_chf: Decimal::from(1),
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
    fn approves_fresh_data() {
        let (signal, market, portfolio, config, now) = make_ctx(100); // 100ms old
        let ctx = PreTradeContext { signal: &signal, market: &market, portfolio: &portfolio, config: &config, now };
        assert_eq!(StalenessCheck.evaluate(&ctx), CheckOutcome::Approved);
    }

    #[test]
    fn rejects_stale_data() {
        let (signal, market, portfolio, config, now) = make_ctx(1000); // 1s old
        let ctx = PreTradeContext { signal: &signal, market: &market, portfolio: &portfolio, config: &config, now };
        match StalenessCheck.evaluate(&ctx) {
            CheckOutcome::Rejected(RejectionReason::StaleMarketData { last_tick_age_ms, limit_ms }) => {
                assert!(last_tick_age_ms >= 1000);
                assert_eq!(limit_ms, 500);
            }
            other => panic!("expected staleness rejection, got {:?}", other),
        }
    }

    #[test]
    fn boundary_exactly_at_limit_is_approved() {
        // Limit is exclusive: > limit fails, = limit passes.
        let (signal, market, portfolio, config, now) = make_ctx(500);
        let ctx = PreTradeContext { signal: &signal, market: &market, portfolio: &portfolio, config: &config, now };
        assert_eq!(StalenessCheck.evaluate(&ctx), CheckOutcome::Approved);
    }
}
