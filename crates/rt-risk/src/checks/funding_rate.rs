//! # Check 5: Funding Rate (Perpetuals only)
//!
//! For crypto-perpetual futures, holders pay (or receive) a funding rate
//! every funding period (Kraken: every 4h; we conservatively model per-8h
//! to match the Binance-style convention used in research literature).
//!
//! When we go long and funding is positive, we PAY. When funding gets
//! extreme (e.g. 0.05% per 4h → ~55% annualized), the funding cost alone
//! can exceed any reasonable expected edge from a momentum trade.
//!
//! This check rejects entries where the funding rate works against our
//! position and exceeds the configured threshold. It is not-applicable
//! for spot instruments (which have no funding rate).

use crate::checklist::{CheckOutcome, PreTradeCheck, PreTradeContext, RejectionReason};
use rt_core::order::Side;
use rust_decimal::Decimal;

pub struct FundingRateCheck;

impl PreTradeCheck for FundingRateCheck {
    fn id(&self) -> &'static str {
        "funding_rate"
    }

    fn description(&self) -> &'static str {
        "Reject perpetual trades when adverse funding exceeds the configured threshold"
    }

    fn evaluate(&self, ctx: &PreTradeContext<'_>) -> CheckOutcome {
        let funding = match ctx.market.funding_rate_per_8h {
            Some(f) => f,
            // Spot instruments have no funding; check does not apply.
            None => return CheckOutcome::NotApplicable,
        };

        // Convention: positive funding = longs pay shorts.
        // Adverse funding for a long position = positive funding.
        // Adverse funding for a short position = negative funding.
        let adverse_funding = match ctx.signal.side {
            Side::Buy => funding,
            Side::Sell => -funding,
        };

        let limit = ctx.config.max_funding_rate_per_8h;
        if adverse_funding > limit {
            return CheckOutcome::Rejected(RejectionReason::FundingRateTooExpensive {
                rate_per_8h: adverse_funding,
                limit_per_8h: limit,
            });
        }

        // Funding is either in our favour (<= 0) or within limits.
        // Note: we don't reward favourable funding — it's just tolerated.
        let _ = Decimal::ZERO; // silence unused import on some toolchains
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

    fn build_ctx(
        side: Side,
        funding: Option<Decimal>,
    ) -> (Signal, MarketSnapshot, PortfolioState, RiskConfig, chrono::DateTime<Utc>) {
        let now = Utc::now();
        let signal = Signal {
            id: 1,
            created_at: now,
            instrument_symbol: "PI_XBTUSD".to_string(),
            side,
            signal_type: SignalType::TrendEntry,
            sleeve: Sleeve::CryptoLeverage,
            notional_chf: Decimal::from(100),
            leverage: Decimal::from(2),
            metadata: None,
            status: SignalStatus::Pending,
            processed_at: None,
            rejection_reason: None,
            expires_at: None,
        };
        let market = MarketSnapshot {
            instrument_symbol: "PI_XBTUSD".to_string(),
            order_book: OrderBookSnapshot {
                bids: vec![OrderBookLevel { price: Decimal::from(50_000), quantity: Decimal::from(1) }],
                asks: vec![OrderBookLevel { price: Decimal::from(50_010), quantity: Decimal::from(1) }],
                timestamp: now,
            },
            last_price: Decimal::from(50_000),
            last_trade_at: now,
            atr_absolute: Decimal::from(1500),
            atr_pct: Decimal::from(3),
            funding_rate_per_8h: funding,
            fx_quote_per_chf: Decimal::from(1),
        };
        let portfolio = PortfolioState {
            snapshot_at: now,
            sleeves: vec![SleeveState {
                sleeve: Sleeve::CryptoLeverage,
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
    fn not_applicable_for_spot_instruments() {
        let (s, m, p, c, n) = build_ctx(Side::Buy, None);
        let ctx = PreTradeContext { signal: &s, market: &m, portfolio: &p, config: &c, now: n };
        assert_eq!(FundingRateCheck.evaluate(&ctx), CheckOutcome::NotApplicable);
    }

    #[test]
    fn rejects_long_when_funding_very_positive() {
        // 0.0005 per 8h > 0.0002 default limit. Long pays → adverse.
        let (s, m, p, c, n) = build_ctx(Side::Buy, Some(Decimal::new(5, 4)));
        let ctx = PreTradeContext { signal: &s, market: &m, portfolio: &p, config: &c, now: n };
        match FundingRateCheck.evaluate(&ctx) {
            CheckOutcome::Rejected(RejectionReason::FundingRateTooExpensive { .. }) => {}
            other => panic!("expected funding rejection, got {:?}", other),
        }
    }

    #[test]
    fn approves_long_when_funding_negative() {
        // Negative funding is a payment TO longs — favourable.
        let (s, m, p, c, n) = build_ctx(Side::Buy, Some(Decimal::new(-5, 4)));
        let ctx = PreTradeContext { signal: &s, market: &m, portfolio: &p, config: &c, now: n };
        assert_eq!(FundingRateCheck.evaluate(&ctx), CheckOutcome::Approved);
    }

    #[test]
    fn rejects_short_when_funding_very_negative() {
        // Negative funding is adverse for shorts.
        let (s, m, p, c, n) = build_ctx(Side::Sell, Some(Decimal::new(-5, 4)));
        let ctx = PreTradeContext { signal: &s, market: &m, portfolio: &p, config: &c, now: n };
        match FundingRateCheck.evaluate(&ctx) {
            CheckOutcome::Rejected(RejectionReason::FundingRateTooExpensive { .. }) => {}
            other => panic!("expected funding rejection, got {:?}", other),
        }
    }
}
