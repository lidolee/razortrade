//! Pre-trade checklist orchestration.
//!
//! This is the entrypoint. The daemon calls [`PreTradeChecklist::evaluate`]
//! with a context struct; the checklist returns a [`ChecklistResult`]
//! enumerating every check's outcome. The daemon then either submits the
//! order (if all checks pass) or writes a rejection record.

use crate::RiskConfig;
use chrono::{DateTime, Utc};
use rt_core::{MarketSnapshot, PortfolioState, Signal, Sleeve};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// Everything a check needs to make a decision. Constructed by the daemon
/// and passed to each check in turn.
#[derive(Debug)]
pub struct PreTradeContext<'a> {
    pub signal: &'a Signal,
    pub market: &'a MarketSnapshot,
    pub portfolio: &'a PortfolioState,
    pub config: &'a RiskConfig,
    pub now: DateTime<Utc>,
}

/// Outcome of a single check.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum CheckOutcome {
    Approved,
    Rejected(RejectionReason),
    /// The check is not applicable to this signal (e.g. funding-rate check
    /// on a spot instrument). Not a failure, not an approval — logged for
    /// audit but does not count toward go/no-go.
    NotApplicable,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RejectionReason {
    StaleMarketData {
        last_tick_age_ms: u64,
        limit_ms: u64,
    },
    SpreadTooWide {
        spread_bps: Decimal,
        limit_bps: Decimal,
    },
    InsufficientLiquidity {
        required_qty: Decimal,
        available_qty: Decimal,
        side: String,
    },
    VolatilityTooHighForLeverage {
        atr_pct: Decimal,
        limit_pct: Decimal,
    },
    VolatilityAbsoluteHalt {
        atr_pct: Decimal,
        limit_pct: Decimal,
    },
    LeverageEffectivePnlBudgetExhausted {
        effective_pnl_chf: Decimal,
        realized_pnl_chf: Decimal,
        unrealized_pnl_chf: Decimal,
        budget_chf: Decimal,
    },
    PortfolioDrawdownExceeded {
        drawdown_fraction: Decimal,
        limit_fraction: Decimal,
    },
    WorstCaseLossWouldExhaustBudget {
        projected_loss_chf: Decimal,
        remaining_budget_chf: Decimal,
    },
    FundingRateTooExpensive {
        rate_per_8h: Decimal,
        limit_per_8h: Decimal,
    },
    LeverageCapExceeded {
        requested: Decimal,
        cap: Decimal,
    },
    NotionalTooLarge {
        notional_chf: Decimal,
        limit_chf: Decimal,
    },
    KillSwitchActive,
    InvalidSignal {
        detail: String,
    },
}

impl RejectionReason {
    /// Human-readable, short description for operator logs and alerts.
    pub fn summary(&self) -> String {
        match self {
            Self::StaleMarketData { last_tick_age_ms, limit_ms } =>
                format!("stale market data ({last_tick_age_ms}ms > {limit_ms}ms)"),
            Self::SpreadTooWide { spread_bps, limit_bps } =>
                format!("spread too wide ({spread_bps} > {limit_bps} bps)"),
            Self::InsufficientLiquidity { required_qty, available_qty, side } =>
                format!("insufficient {side} liquidity (need {required_qty}, have {available_qty})"),
            Self::VolatilityTooHighForLeverage { atr_pct, limit_pct } =>
                format!("ATR% {atr_pct} exceeds leverage limit {limit_pct}"),
            Self::VolatilityAbsoluteHalt { atr_pct, limit_pct } =>
                format!("ATR% {atr_pct} exceeds absolute market-halt limit {limit_pct}"),
            Self::LeverageEffectivePnlBudgetExhausted { effective_pnl_chf, realized_pnl_chf, unrealized_pnl_chf, budget_chf } =>
                format!("leverage effective P&L {effective_pnl_chf} CHF (realised {realized_pnl_chf} + unrealised {unrealized_pnl_chf}) has exhausted budget {budget_chf} CHF"),
            Self::PortfolioDrawdownExceeded { drawdown_fraction, limit_fraction } =>
                format!("portfolio drawdown {drawdown_fraction} exceeds limit {limit_fraction}"),
            Self::WorstCaseLossWouldExhaustBudget { projected_loss_chf, remaining_budget_chf } =>
                format!("worst-case loss {projected_loss_chf} CHF would exceed remaining budget {remaining_budget_chf} CHF"),
            Self::FundingRateTooExpensive { rate_per_8h, limit_per_8h } =>
                format!("funding rate {rate_per_8h}/8h exceeds limit {limit_per_8h}/8h"),
            Self::LeverageCapExceeded { requested, cap } =>
                format!("requested leverage {requested}x exceeds cap {cap}x"),
            Self::NotionalTooLarge { notional_chf, limit_chf } =>
                format!("notional {notional_chf} CHF exceeds per-order cap {limit_chf} CHF"),
            Self::KillSwitchActive =>
                "kill-switch is active; no leveraged trades accepted".to_string(),
            Self::InvalidSignal { detail } =>
                format!("invalid signal: {detail}"),
        }
    }
}

/// Trait implemented by each individual check. Must be cheap to evaluate
/// (pure arithmetic over the context) and must not panic.
pub trait PreTradeCheck: Send + Sync {
    /// Stable identifier used in logs and database audit records.
    fn id(&self) -> &'static str;

    /// Short human description for operator dashboards.
    fn description(&self) -> &'static str;

    /// Evaluate this check against the provided context.
    fn evaluate(&self, ctx: &PreTradeContext<'_>) -> CheckOutcome;
}

/// Result of running the full checklist.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChecklistResult {
    /// Ordered list of (check_id, outcome) pairs, in declaration order.
    pub outcomes: Vec<(String, CheckOutcome)>,
    /// The time at which the checklist finished evaluating.
    pub evaluated_at: DateTime<Utc>,
    /// True iff every check returned `Approved` or `NotApplicable`.
    pub approved: bool,
}

impl ChecklistResult {
    pub fn rejections(&self) -> Vec<(&str, &RejectionReason)> {
        self.outcomes
            .iter()
            .filter_map(|(id, outcome)| match outcome {
                CheckOutcome::Rejected(reason) => Some((id.as_str(), reason)),
                _ => None,
            })
            .collect()
    }

    pub fn summary(&self) -> String {
        if self.approved {
            format!(
                "APPROVED ({} checks ran at {})",
                self.outcomes.len(),
                self.evaluated_at
            )
        } else {
            let rejections = self.rejections();
            let reasons: Vec<String> = rejections
                .iter()
                .map(|(id, reason)| format!("{}: {}", id, reason.summary()))
                .collect();
            format!("REJECTED ({})", reasons.join("; "))
        }
    }
}

/// The checklist itself. Constructed once at daemon startup and reused for
/// every signal. All checks are evaluated in declaration order.
pub struct PreTradeChecklist {
    checks: Vec<Box<dyn PreTradeCheck>>,
}

impl PreTradeChecklist {
    pub fn new(checks: Vec<Box<dyn PreTradeCheck>>) -> Self {
        Self { checks }
    }

    /// Build the canonical 5-point checklist in the documented order.
    ///
    /// Order matters for human-readable log output but not for correctness:
    /// every check is always evaluated.
    pub fn standard() -> Self {
        use crate::checks::*;
        Self::new(vec![
            Box::new(StalenessCheck),
            Box::new(SpreadLiquidityCheck),
            Box::new(VolatilityRegimeCheck),
            Box::new(HardLimitCheck),
            Box::new(FundingRateCheck),
            Box::new(NotionalCapCheck),
        ])
    }

    pub fn evaluate(&self, ctx: &PreTradeContext<'_>) -> ChecklistResult {
        // First, a trivial guard: if the kill-switch is active and this is a
        // leveraged trade, short-circuit with a dedicated rejection. We still
        // run the full checklist for audit purposes, but we prepend this
        // overarching rejection.
        let mut outcomes: Vec<(String, CheckOutcome)> = Vec::with_capacity(self.checks.len() + 1);

        if ctx.portfolio.kill_switch_active && ctx.signal.sleeve == Sleeve::CryptoLeverage {
            outcomes.push((
                "kill_switch_gate".to_string(),
                CheckOutcome::Rejected(RejectionReason::KillSwitchActive),
            ));
        }

        for check in &self.checks {
            let outcome = check.evaluate(ctx);
            outcomes.push((check.id().to_string(), outcome));
        }

        let approved = outcomes
            .iter()
            .all(|(_, o)| matches!(o, CheckOutcome::Approved | CheckOutcome::NotApplicable));

        ChecklistResult {
            outcomes,
            evaluated_at: ctx.now,
            approved,
        }
    }

    pub fn check_ids(&self) -> Vec<&'static str> {
        self.checks.iter().map(|c| c.id()).collect()
    }
}

impl Default for PreTradeChecklist {
    fn default() -> Self {
        Self::standard()
    }
}
