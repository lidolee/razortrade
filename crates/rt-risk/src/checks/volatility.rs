//! # Check 3: Volatility Regime
//!
//! Two-tier volatility filter:
//!
//! 1. **Leverage veto**: if ATR% exceeds `max_atr_pct_for_leverage`,
//!    leveraged trades are rejected. Spot trades still pass — this is
//!    aligned with the architectural rule that VCA/spot accumulation
//!    operates independently of the leverage filter.
//! 2. **Absolute halt**: if ATR% exceeds `max_atr_pct_absolute`, ALL
//!    trades are rejected. This is the flash-crash / market-halt detector.
//!    At 15% intraday ATR the spread and order book are unreliable enough
//!    that our depth estimates in Check 2 cannot be trusted either.

use crate::checklist::{CheckOutcome, PreTradeCheck, PreTradeContext, RejectionReason};
use rt_core::instrument::Sleeve;

pub struct VolatilityRegimeCheck;

impl PreTradeCheck for VolatilityRegimeCheck {
    fn id(&self) -> &'static str {
        "volatility_regime"
    }

    fn description(&self) -> &'static str {
        "Reject leveraged trades in high-vol regimes; reject all trades in extreme-vol regimes"
    }

    fn evaluate(&self, ctx: &PreTradeContext<'_>) -> CheckOutcome {
        let atr_pct = ctx.market.atr_pct;

        // Tier 2: absolute halt applies to every sleeve.
        if atr_pct > ctx.config.max_atr_pct_absolute {
            return CheckOutcome::Rejected(RejectionReason::VolatilityAbsoluteHalt {
                atr_pct,
                limit_pct: ctx.config.max_atr_pct_absolute,
            });
        }

        // Tier 1: leverage veto only for CryptoLeverage sleeve.
        if ctx.signal.sleeve == Sleeve::CryptoLeverage
            && atr_pct > ctx.config.max_atr_pct_for_leverage
        {
            return CheckOutcome::Rejected(RejectionReason::VolatilityTooHighForLeverage {
                atr_pct,
                limit_pct: ctx.config.max_atr_pct_for_leverage,
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
        market_data::{MarketSnapshot, OrderBookLevel, OrderBookSnapshot},
        order::Side,
        portfolio::{PortfolioState, SleeveState},
        signal::{Signal, SignalStatus, SignalType},
    };
    use rust_decimal::Decimal;

    fn ctx(sleeve: Sleeve, atr_pct: Decimal) -> (Signal, MarketSnapshot, PortfolioState, RiskConfig, chrono::DateTime<Utc>) {
        let now = Utc::now();
        let signal = Signal {
            id: 1,
            created_at: now,
            instrument_symbol: "XBT/USD".to_string(),
            side: Side::Buy,
            signal_type: SignalType::TrendEntry,
            sleeve,
            notional_chf: Decimal::from(100),
            leverage: if sleeve == Sleeve::CryptoLeverage { Decimal::from(2) } else { Decimal::from(1) },
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
                timestamp: now,
            },
            last_price: Decimal::from(50_000),
            last_trade_at: now,
            atr_absolute: Decimal::from(1500),
            atr_pct,
            funding_rate_per_8h: None,
            fx_quote_per_chf: Decimal::from(1),
        };
        let portfolio = PortfolioState {
            snapshot_at: now,
            sleeves: vec![SleeveState {
                sleeve,
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
    fn approves_normal_vol_spot() {
        let (s, m, p, c, n) = ctx(Sleeve::CryptoSpot, Decimal::from(4));
        let x = PreTradeContext { signal: &s, market: &m, portfolio: &p, config: &c, now: n };
        assert_eq!(VolatilityRegimeCheck.evaluate(&x), CheckOutcome::Approved);
    }

    #[test]
    fn approves_normal_vol_leverage() {
        let (s, m, p, c, n) = ctx(Sleeve::CryptoLeverage, Decimal::from(4));
        let x = PreTradeContext { signal: &s, market: &m, portfolio: &p, config: &c, now: n };
        assert_eq!(VolatilityRegimeCheck.evaluate(&x), CheckOutcome::Approved);
    }

    #[test]
    fn rejects_high_vol_only_for_leverage() {
        // ATR 8% > 6% leverage limit, but < 15% absolute limit.
        let (s, m, p, c, n) = ctx(Sleeve::CryptoLeverage, Decimal::from(8));
        let x = PreTradeContext { signal: &s, market: &m, portfolio: &p, config: &c, now: n };
        match VolatilityRegimeCheck.evaluate(&x) {
            CheckOutcome::Rejected(RejectionReason::VolatilityTooHighForLeverage { .. }) => {}
            other => panic!("expected leverage-veto rejection, got {:?}", other),
        }

        // Same ATR on spot: should be approved.
        let (s, m, p, c, n) = ctx(Sleeve::CryptoSpot, Decimal::from(8));
        let x = PreTradeContext { signal: &s, market: &m, portfolio: &p, config: &c, now: n };
        assert_eq!(VolatilityRegimeCheck.evaluate(&x), CheckOutcome::Approved);
    }

    #[test]
    fn rejects_extreme_vol_for_every_sleeve() {
        for sleeve in [Sleeve::CryptoSpot, Sleeve::Etf, Sleeve::CryptoLeverage] {
            let (s, m, p, c, n) = ctx(sleeve, Decimal::from(20));
            let x = PreTradeContext { signal: &s, market: &m, portfolio: &p, config: &c, now: n };
            match VolatilityRegimeCheck.evaluate(&x) {
                CheckOutcome::Rejected(RejectionReason::VolatilityAbsoluteHalt { .. }) => {}
                other => panic!("expected absolute-halt for {:?}, got {:?}", sleeve, other),
            }
        }
    }
}
