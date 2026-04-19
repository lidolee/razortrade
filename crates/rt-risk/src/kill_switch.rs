//! Kill-switch evaluator.
//!
//! This is a separate concern from the pre-trade checklist. The checklist
//! asks "should we take this specific trade?" at the moment a signal
//! arrives. The kill-switch evaluator asks "should leveraged trading be
//! disabled globally, regardless of incoming signals?" and runs on a
//! timer (e.g. every 60 seconds) in the daemon's safety supervisor.
//!
//! # Why realized-only here, but effective in the checklist?
//!
//! The kill-switch guard uses `leverage_sleeve_realized_pnl` (realised
//! losses only), while [`crate::checks::HardLimitCheck`] uses
//! `leverage_sleeve_effective_pnl` (realised + adverse unrealised).
//!
//! These two behaviours are deliberately different:
//!
//! * The **pre-trade check** is a one-shot decision at signal time. It
//!   must be conservative about *admitting new risk*. An open losing
//!   position could hit its stop before the next decision, so we must
//!   assume the worst about already-open exposure.
//!
//! * The **kill-switch supervisor** determines whether to set a
//!   *permanent* flag that disables leveraged trading until an operator
//!   manually resets it. Permanent stops must not be triggered by
//!   transient mark-to-market swings: a sharp dip that fully reverses
//!   within the hour should not permanently lock us out.
//!
//! The drawdown guard below uses NAV-per-unit drawdown, which already
//! incorporates mark-to-market losses correctly and does not depend on
//! realisation.

use crate::RiskConfig;
use chrono::{DateTime, Utc};
use rt_core::portfolio::PortfolioState;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum KillSwitchDecision {
    /// No change needed.
    NoChange,
    /// Leverage sleeve must be disabled. The daemon will persist this
    /// decision and refuse new leveraged trades until manually reset.
    Disable { reason: KillSwitchReason, at: DateTime<Utc> },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum KillSwitchReason {
    /// Realised lifetime loss on the CryptoLeverage sleeve is at or below
    /// the configured budget. This is a **permanent stop**.
    LifetimeRealisedLossBudgetExhausted {
        realized_pnl_chf: Decimal,
        budget_chf: Decimal,
    },
    /// NAV-per-unit drawdown exceeded the configured threshold.
    /// Mark-to-market driven; resets when NAV recovers (but the flag
    /// itself requires manual resolution).
    PortfolioDrawdown {
        drawdown_fraction: Decimal,
        limit_fraction: Decimal,
    },
    ManualTrigger {
        operator: String,
    },
}

impl KillSwitchReason {
    pub fn summary(&self) -> String {
        match self {
            Self::LifetimeRealisedLossBudgetExhausted { realized_pnl_chf, budget_chf } =>
                format!("realised lifetime loss {realized_pnl_chf} CHF reached budget {budget_chf} CHF"),
            Self::PortfolioDrawdown { drawdown_fraction, limit_fraction } =>
                format!("NAV drawdown {drawdown_fraction} exceeded limit {limit_fraction}"),
            Self::ManualTrigger { operator } =>
                format!("manual trigger by {operator}"),
        }
    }
}

pub struct KillSwitchEvaluator;

impl KillSwitchEvaluator {
    pub fn evaluate(
        portfolio: &PortfolioState,
        config: &RiskConfig,
        now: DateTime<Utc>,
    ) -> KillSwitchDecision {
        // If it's already active, don't emit another Disable.
        if portfolio.kill_switch_active {
            return KillSwitchDecision::NoChange;
        }

        // Guard 1: lifetime *realized* loss budget.
        // See module docs for why we use realized-only here.
        let realized = portfolio.leverage_sleeve_realized_pnl();
        if realized <= config.leverage_kill_switch_budget_chf {
            return KillSwitchDecision::Disable {
                reason: KillSwitchReason::LifetimeRealisedLossBudgetExhausted {
                    realized_pnl_chf: realized,
                    budget_chf: config.leverage_kill_switch_budget_chf,
                },
                at: now,
            };
        }

        // Guard 2: NAV-per-unit drawdown.
        let dd = portfolio.drawdown_fraction();
        if dd > config.max_portfolio_drawdown {
            return KillSwitchDecision::Disable {
                reason: KillSwitchReason::PortfolioDrawdown {
                    drawdown_fraction: dd,
                    limit_fraction: config.max_portfolio_drawdown,
                },
                at: now,
            };
        }

        KillSwitchDecision::NoChange
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rt_core::portfolio::SleeveState;
    use rt_core::Sleeve;

    fn portfolio(
        realized: Decimal,
        unrealized: Decimal,
        nav: Decimal,
        nav_hwm: Decimal,
        active: bool,
    ) -> PortfolioState {
        PortfolioState {
            snapshot_at: Utc::now(),
            sleeves: vec![SleeveState {
                sleeve: Sleeve::CryptoLeverage,
                market_value_chf: Decimal::ZERO,
                cash_chf: Decimal::ZERO,
                realized_pnl_lifetime_chf: realized,
                unrealized_pnl_chf: unrealized,
            }],
            nav_per_unit: nav,
            nav_hwm_per_unit: nav_hwm,
            total_units: Decimal::from(10_000),
            kill_switch_active: active,
        }
    }

    #[test]
    fn no_change_when_healthy() {
        let p = portfolio(Decimal::ZERO, Decimal::ZERO, Decimal::from(1), Decimal::from(1), false);
        assert_eq!(
            KillSwitchEvaluator::evaluate(&p, &RiskConfig::default(), Utc::now()),
            KillSwitchDecision::NoChange
        );
    }

    #[test]
    fn disables_on_realised_budget_exhausted() {
        let p = portfolio(Decimal::from(-1000), Decimal::ZERO, Decimal::from(1), Decimal::from(1), false);
        let decision = KillSwitchEvaluator::evaluate(&p, &RiskConfig::default(), Utc::now());
        assert!(matches!(
            decision,
            KillSwitchDecision::Disable {
                reason: KillSwitchReason::LifetimeRealisedLossBudgetExhausted { .. },
                ..
            }
        ));
    }

    /// Important: unrealised loss alone must NOT trigger the permanent stop.
    /// This is the counterpart to the hard_limit.rs change: the pre-trade
    /// check rejects new trades when effective P&L breaches budget, but
    /// the supervisor only permanently disables on realised losses.
    #[test]
    fn does_not_disable_on_unrealised_alone() {
        let p = portfolio(Decimal::ZERO, Decimal::from(-1500), Decimal::from(1), Decimal::from(1), false);
        assert_eq!(
            KillSwitchEvaluator::evaluate(&p, &RiskConfig::default(), Utc::now()),
            KillSwitchDecision::NoChange
        );
    }

    #[test]
    fn disables_on_nav_drawdown() {
        // NAV 0.75 vs HWM 1.00 → 25% drawdown.
        let p = portfolio(Decimal::ZERO, Decimal::ZERO, Decimal::new(75, 2), Decimal::from(1), false);
        let decision = KillSwitchEvaluator::evaluate(&p, &RiskConfig::default(), Utc::now());
        assert!(matches!(
            decision,
            KillSwitchDecision::Disable {
                reason: KillSwitchReason::PortfolioDrawdown { .. },
                ..
            }
        ));
    }

    #[test]
    fn no_duplicate_trigger_when_already_active() {
        let p = portfolio(Decimal::from(-1000), Decimal::ZERO, Decimal::from(1), Decimal::from(1), true);
        assert_eq!(
            KillSwitchEvaluator::evaluate(&p, &RiskConfig::default(), Utc::now()),
            KillSwitchDecision::NoChange
        );
    }
}
