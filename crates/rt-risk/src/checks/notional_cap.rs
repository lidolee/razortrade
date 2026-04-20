//! # Notional Cap
//!
//! Rejects signals whose `notional_chf` exceeds
//! `RiskConfig::max_notional_chf_per_order`. This is a defensive
//! per-order cap against signal-generator bugs, not a sizing policy.
//! The leverage sleeve's P&L budget (Check 4: HardLimit) and the
//! in-build integer contract cap (Drop 5.5a in the daemon) operate
//! on different axes and complement this one.
//!
//! Boundary: the limit is inclusive-max — a signal with
//! `notional_chf == max_notional_chf_per_order` is approved. A
//! signal strictly larger is rejected.

use crate::checklist::{CheckOutcome, PreTradeCheck, PreTradeContext, RejectionReason};

pub struct NotionalCapCheck;

impl PreTradeCheck for NotionalCapCheck {
    fn id(&self) -> &'static str {
        "notional_cap"
    }

    fn description(&self) -> &'static str {
        "Reject if signal notional exceeds the configured per-order cap"
    }

    fn evaluate(&self, ctx: &PreTradeContext<'_>) -> CheckOutcome {
        let notional = ctx.signal.notional_chf;
        let limit = ctx.config.max_notional_chf_per_order;

        if notional > limit {
            CheckOutcome::Rejected(RejectionReason::NotionalTooLarge {
                notional_chf: notional,
                limit_chf: limit,
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
    use chrono::Utc;
    use rt_core::{
        instrument::Sleeve,
        market_data::{MarketSnapshot, OrderBookLevel, OrderBookSnapshot},
        order::Side,
        portfolio::{PortfolioState, SleeveState},
        signal::{Signal, SignalStatus, SignalType},
    };
    use rust_decimal::Decimal;

    fn make_ctx(
        notional_chf: Decimal,
    ) -> (Signal, MarketSnapshot, PortfolioState, RiskConfig, chrono::DateTime<Utc>) {
        let now = Utc::now();

        let signal = Signal {
            id: 1,
            created_at: now,
            instrument_symbol: "XBT/USD".to_string(),
            side: Side::Buy,
            signal_type: SignalType::TrendEntry,
            sleeve: Sleeve::CryptoLeverage,
            notional_chf,
            leverage: Decimal::from(1),
            metadata: None,
            status: SignalStatus::Pending,
            processed_at: None,
            rejection_reason: None,
            expires_at: None,
        };

        let market = MarketSnapshot {
            instrument_symbol: "XBT/USD".to_string(),
            order_book: OrderBookSnapshot {
                bids: vec![OrderBookLevel {
                    price: Decimal::from(50_000),
                    quantity: Decimal::from(1),
                }],
                asks: vec![OrderBookLevel {
                    price: Decimal::from(50_010),
                    quantity: Decimal::from(1),
                }],
                timestamp: now,
            },
            last_price: Decimal::from(50_000),
            last_trade_at: now,
            atr_absolute: Decimal::from(1500),
            atr_pct: Decimal::from(3),
            funding_rate_per_8h: None,
            fx_quote_per_chf: Decimal::from(1),
        };

        let portfolio = PortfolioState {
            snapshot_at: now,
            sleeves: vec![SleeveState {
                sleeve: Sleeve::CryptoLeverage,
                market_value_chf: Decimal::ZERO,
                cash_chf: Decimal::from(10_000),
                realized_pnl_lifetime_chf: Decimal::ZERO,
                unrealized_pnl_chf: Decimal::ZERO,
            }],
            nav_per_unit: Decimal::from(1),
            nav_hwm_per_unit: Decimal::from(1),
            total_units: Decimal::from(1000),
            kill_switch_active: false,
            open_positions: Vec::new(),
        };

        (signal, market, portfolio, RiskConfig::default(), now)
    }

    #[test]
    fn approves_notional_below_limit() {
        let (signal, market, portfolio, config, now) = make_ctx(Decimal::from(500));
        let ctx = PreTradeContext {
            signal: &signal,
            market: &market,
            portfolio: &portfolio,
            config: &config,
            now,
        };
        assert_eq!(NotionalCapCheck.evaluate(&ctx), CheckOutcome::Approved);
    }

    #[test]
    fn approves_notional_exactly_at_limit() {
        // Default is 1000; a signal exactly at 1000 CHF is approved.
        let (signal, market, portfolio, config, now) = make_ctx(Decimal::from(1000));
        let ctx = PreTradeContext {
            signal: &signal,
            market: &market,
            portfolio: &portfolio,
            config: &config,
            now,
        };
        assert_eq!(NotionalCapCheck.evaluate(&ctx), CheckOutcome::Approved);
    }

    #[test]
    fn rejects_notional_above_limit() {
        let (signal, market, portfolio, config, now) = make_ctx(Decimal::from(2000));
        let ctx = PreTradeContext {
            signal: &signal,
            market: &market,
            portfolio: &portfolio,
            config: &config,
            now,
        };
        match NotionalCapCheck.evaluate(&ctx) {
            CheckOutcome::Rejected(RejectionReason::NotionalTooLarge {
                notional_chf,
                limit_chf,
            }) => {
                assert_eq!(notional_chf, Decimal::from(2000));
                assert_eq!(limit_chf, Decimal::from(1000));
            }
            other => panic!("expected NotionalTooLarge rejection, got {other:?}"),
        }
    }

    #[test]
    fn respects_custom_limit_from_config() {
        // Operator-tuned 200 CHF cap for an extra-conservative setup.
        let (signal, market, portfolio, mut config, now) = make_ctx(Decimal::from(500));
        config.max_notional_chf_per_order = Decimal::from(200);
        let ctx = PreTradeContext {
            signal: &signal,
            market: &market,
            portfolio: &portfolio,
            config: &config,
            now,
        };
        match NotionalCapCheck.evaluate(&ctx) {
            CheckOutcome::Rejected(RejectionReason::NotionalTooLarge { limit_chf, .. }) => {
                assert_eq!(limit_chf, Decimal::from(200));
            }
            other => panic!("expected rejection at custom 200 CHF limit, got {other:?}"),
        }
    }
}
