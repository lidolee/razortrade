//! # Check 4: Hard Limit & Drawdown
//!
//! The most important check. Four independent guards, in declaration order:
//!
//! 1. **Leverage cap** (applies to all signals): reject any signal that
//!    requests `leverage > max_leverage`.
//!
//! 2. **Leverage sleeve effective-budget exhausted**: if the leverage
//!    sleeve's *effective* lifetime P&L (realised + adverse unrealised)
//!    has fallen below the configured budget (default -1000 CHF), reject
//!    all further leveraged entries. The Sammler (spot) sleeves continue.
//!
//!    We use effective P&L here (not just realised) because an open
//!    adverse position could realise its loss at any moment. Ignoring
//!    unrealised losses would allow the system to pile on new trades
//!    while an existing position is quietly dragging the portfolio toward
//!    the hard stop.
//!
//! 3. **Portfolio drawdown exceeded**: if total portfolio NAV-per-unit
//!    has dropped more than `max_portfolio_drawdown` (default 20%) from
//!    its all-time high, reject any leveraged trade. Spot accumulation
//!    is still allowed.
//!
//! 4. **Worst-case loss would exhaust budget**: simulate an instantaneous
//!    adverse move of `adverse_move_for_worst_case` on the new trade,
//!    combine with the current effective P&L, and reject if the result
//!    would cross the budget.

use crate::checklist::{CheckOutcome, PreTradeCheck, PreTradeContext, RejectionReason};
use rt_core::instrument::Sleeve;
use rust_decimal::Decimal;

pub struct HardLimitCheck;

impl PreTradeCheck for HardLimitCheck {
    fn id(&self) -> &'static str {
        "hard_limit"
    }

    fn description(&self) -> &'static str {
        "Enforce leverage cap, effective-P&L budget, portfolio drawdown, and worst-case projection"
    }

    fn evaluate(&self, ctx: &PreTradeContext<'_>) -> CheckOutcome {
        // --- Guard 1: leverage cap applies to every signal ---------------
        if ctx.signal.leverage > ctx.config.max_leverage {
            return CheckOutcome::Rejected(RejectionReason::LeverageCapExceeded {
                requested: ctx.signal.leverage,
                cap: ctx.config.max_leverage,
            });
        }

        // Non-leverage sleeves: Check 4 stops here. Spot sleeves are
        // deliberately allowed to accumulate through drawdowns.
        if ctx.signal.sleeve != Sleeve::CryptoLeverage {
            return CheckOutcome::Approved;
        }

        // --- Guard 2: effective P&L vs. budget ---------------------------
        let effective = ctx.portfolio.leverage_sleeve_effective_pnl();
        let budget = ctx.config.leverage_kill_switch_budget_chf;

        if effective <= budget {
            return CheckOutcome::Rejected(
                RejectionReason::LeverageEffectivePnlBudgetExhausted {
                    effective_pnl_chf: effective,
                    realized_pnl_chf: ctx.portfolio.leverage_sleeve_realized_pnl(),
                    unrealized_pnl_chf: ctx.portfolio.leverage_sleeve_unrealized_pnl(),
                    budget_chf: budget,
                },
            );
        }

        // --- Guard 3: portfolio drawdown ---------------------------------
        let dd = ctx.portfolio.drawdown_fraction();
        if dd > ctx.config.max_portfolio_drawdown {
            return CheckOutcome::Rejected(RejectionReason::PortfolioDrawdownExceeded {
                drawdown_fraction: dd,
                limit_fraction: ctx.config.max_portfolio_drawdown,
            });
        }

        // --- Guard 4: worst-case projection on top of effective P&L -------
        // Position notional = notional_chf × leverage.
        let notional = ctx.signal.notional_chf * ctx.signal.leverage;
        let worst_case_loss = notional * ctx.config.adverse_move_for_worst_case;
        // Subtract (loss is a positive number representing CHF lost).
        let projected = effective - worst_case_loss;

        if projected <= budget {
            let remaining = (effective - budget).max(Decimal::ZERO);
            return CheckOutcome::Rejected(
                RejectionReason::WorstCaseLossWouldExhaustBudget {
                    projected_loss_chf: worst_case_loss,
                    remaining_budget_chf: remaining,
                },
            );
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

    fn build_ctx(
        sleeve: Sleeve,
        notional: Decimal,
        leverage: Decimal,
        realized_pnl: Decimal,
        unrealized_pnl: Decimal,
        nav_per_unit: Decimal,
        nav_hwm: Decimal,
    ) -> (Signal, MarketSnapshot, PortfolioState, RiskConfig, chrono::DateTime<Utc>) {
        let now = Utc::now();
        let signal = Signal {
            id: 1,
            created_at: now,
            instrument_symbol: "PI_XBTUSD".to_string(),
            side: Side::Buy,
            signal_type: SignalType::TrendEntry,
            sleeve,
            notional_chf: notional,
            leverage,
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
            funding_rate_per_8h: None,
            fx_quote_per_chf: Decimal::from(1),
        };
        let portfolio = PortfolioState {
            snapshot_at: now,
            sleeves: vec![SleeveState {
                sleeve,
                market_value_chf: Decimal::ZERO,
                cash_chf: Decimal::from(10_000),
                realized_pnl_lifetime_chf: realized_pnl,
                unrealized_pnl_chf: unrealized_pnl,
            }],
            nav_per_unit,
            nav_hwm_per_unit: nav_hwm,
            total_units: Decimal::from(10_000),
            kill_switch_active: false,
            open_positions: Vec::new(),
        };
        (signal, market, portfolio, RiskConfig::default(), now)
    }

    fn par() -> Decimal { Decimal::from(1) }

    #[test]
    fn rejects_over_leverage() {
        let (s, m, p, c, n) = build_ctx(
            Sleeve::CryptoLeverage, Decimal::from(100), Decimal::from(3),
            Decimal::ZERO, Decimal::ZERO, par(), par(),
        );
        let ctx = PreTradeContext { signal: &s, market: &m, portfolio: &p, config: &c, now: n };
        match HardLimitCheck.evaluate(&ctx) {
            CheckOutcome::Rejected(RejectionReason::LeverageCapExceeded { .. }) => {}
            other => panic!("expected leverage cap rejection, got {:?}", other),
        }
    }

    #[test]
    fn rejects_when_realized_alone_exceeds_budget() {
        let (s, m, p, c, n) = build_ctx(
            Sleeve::CryptoLeverage, Decimal::from(100), Decimal::from(2),
            Decimal::from(-1000), Decimal::ZERO, par(), par(),
        );
        let ctx = PreTradeContext { signal: &s, market: &m, portfolio: &p, config: &c, now: n };
        match HardLimitCheck.evaluate(&ctx) {
            CheckOutcome::Rejected(
                RejectionReason::LeverageEffectivePnlBudgetExhausted { .. }
            ) => {}
            other => panic!("expected effective-budget rejection, got {:?}", other),
        }
    }

    /// The Gemini-identified bug: realized small, unrealized large and adverse.
    /// Must now reject instead of admitting new trades.
    #[test]
    fn rejects_when_unrealized_loss_pushes_effective_over_budget() {
        let (s, m, p, c, n) = build_ctx(
            Sleeve::CryptoLeverage, Decimal::from(100), Decimal::from(2),
            Decimal::from(-200),  // realized only -200
            Decimal::from(-900),  // but unrealized -900 → effective = -1100 ≤ -1000 budget
            par(), par(),
        );
        let ctx = PreTradeContext { signal: &s, market: &m, portfolio: &p, config: &c, now: n };
        match HardLimitCheck.evaluate(&ctx) {
            CheckOutcome::Rejected(
                RejectionReason::LeverageEffectivePnlBudgetExhausted {
                    effective_pnl_chf,
                    realized_pnl_chf,
                    unrealized_pnl_chf,
                    ..
                }
            ) => {
                assert_eq!(effective_pnl_chf, Decimal::from(-1100));
                assert_eq!(realized_pnl_chf, Decimal::from(-200));
                assert_eq!(unrealized_pnl_chf, Decimal::from(-900));
            }
            other => panic!("expected effective-budget rejection, got {:?}", other),
        }
    }

    /// The key contrast: old bug scenario (realized=0, unrealized=-900)
    /// would previously have been admitted. Now rejected by Guard 4
    /// (worst-case projection from effective=-900).
    #[test]
    fn rejects_new_trade_when_open_position_close_to_budget() {
        // effective = 0 + (-900) = -900. budget = -1000. headroom = 100.
        // New trade notional 1000 × 2x = 2000. 10% adverse = -200.
        // projected = -900 - 200 = -1100 ≤ -1000 → reject.
        let (s, m, p, c, n) = build_ctx(
            Sleeve::CryptoLeverage, Decimal::from(1000), Decimal::from(2),
            Decimal::ZERO, Decimal::from(-900), par(), par(),
        );
        let ctx = PreTradeContext { signal: &s, market: &m, portfolio: &p, config: &c, now: n };
        match HardLimitCheck.evaluate(&ctx) {
            CheckOutcome::Rejected(RejectionReason::WorstCaseLossWouldExhaustBudget { .. }) => {}
            other => panic!("expected worst-case rejection, got {:?}", other),
        }
    }

    #[test]
    fn favourable_unrealized_does_not_inflate_budget() {
        // Realized -950, unrealized +200. Effective must NOT be -750, it must be -950.
        // Budget -1000. Remaining = 50. Trade notional 1000 × 2 = 2000; 10% = -200. → reject.
        let (s, m, p, c, n) = build_ctx(
            Sleeve::CryptoLeverage, Decimal::from(1000), Decimal::from(2),
            Decimal::from(-950), Decimal::from(200), par(), par(),
        );
        let ctx = PreTradeContext { signal: &s, market: &m, portfolio: &p, config: &c, now: n };
        match HardLimitCheck.evaluate(&ctx) {
            CheckOutcome::Rejected(RejectionReason::WorstCaseLossWouldExhaustBudget { .. }) => {}
            other => panic!("expected worst-case rejection, got {:?}", other),
        }
    }

    #[test]
    fn rejects_when_nav_drawdown_exceeded() {
        let (s, m, p, c, n) = build_ctx(
            Sleeve::CryptoLeverage, Decimal::from(100), Decimal::from(2),
            Decimal::ZERO, Decimal::ZERO,
            Decimal::new(75, 2),   // NAV 0.75
            Decimal::from(1),      // HWM 1.00 → DD 25%
        );
        let ctx = PreTradeContext { signal: &s, market: &m, portfolio: &p, config: &c, now: n };
        match HardLimitCheck.evaluate(&ctx) {
            CheckOutcome::Rejected(RejectionReason::PortfolioDrawdownExceeded { .. }) => {}
            other => panic!("expected drawdown rejection, got {:?}", other),
        }
    }

    #[test]
    fn approves_healthy_leverage_trade() {
        let (s, m, p, c, n) = build_ctx(
            Sleeve::CryptoLeverage, Decimal::from(100), Decimal::from(2),
            Decimal::ZERO, Decimal::ZERO, par(), par(),
        );
        let ctx = PreTradeContext { signal: &s, market: &m, portfolio: &p, config: &c, now: n };
        assert_eq!(HardLimitCheck.evaluate(&ctx), CheckOutcome::Approved);
    }

    #[test]
    fn spot_trades_skip_leverage_specific_guards() {
        // Budget exhausted, drawdown high, unrealised huge loss → spot still accepted.
        let (s, m, p, c, n) = build_ctx(
            Sleeve::CryptoSpot, Decimal::from(100), Decimal::from(1),
            Decimal::from(-5000), Decimal::from(-1000),
            Decimal::new(50, 2), Decimal::from(1),
        );
        let ctx = PreTradeContext { signal: &s, market: &m, portfolio: &p, config: &c, now: n };
        assert_eq!(HardLimitCheck.evaluate(&ctx), CheckOutcome::Approved);
    }
}
