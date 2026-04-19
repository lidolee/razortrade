//! # rt-risk
//!
//! The hard pre-trade checklist. This crate implements the "Brawn" half of
//! the Brain & Brawn architecture: every signal from the Python strategy
//! layer MUST pass through [`Checklist::evaluate`] before any broker API is
//! called.
//!
//! ## Architectural contract
//!
//! - Checks are **deterministic**. Given identical inputs, they produce
//!   identical outputs. No I/O, no wall-clock reads (`now` is an input).
//! - Checks are **independent**. Failing one check does not short-circuit
//!   the others; all checks are always evaluated and their results logged.
//!   This is deliberate: when we reject a trade, we want the full picture
//!   of which checks failed, not just the first one.
//! - Checks are **hardcoded**. They are not loaded from config at runtime.
//!   Changing a check requires a code change, a PR, and a re-deploy.
//! - Checks are **boring**. They do not use ML, statistics, or heuristics
//!   beyond simple threshold comparisons. The cleverness lives in the
//!   Python strategy layer; the risk layer is deliberately dumb.

pub mod checklist;
pub mod checks;
pub mod kill_switch;
pub mod position_sizing;

pub use checklist::{
    CheckOutcome, ChecklistResult, PreTradeCheck, PreTradeChecklist, PreTradeContext,
    RejectionReason,
};
pub use kill_switch::{KillSwitchDecision, KillSwitchEvaluator, KillSwitchReason};

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// Runtime-configurable thresholds for the checklist.
///
/// These values are loaded from a TOML config file at daemon startup and
/// passed by reference to every check. They can be tuned without recompiling,
/// but not changed at runtime — reload requires a daemon restart.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RiskConfig {
    /// Check 1: Max age of last market-data tick before a signal is considered
    /// to be operating on stale data. Default: 500 ms.
    pub max_tick_age_ms: u64,

    /// Check 2: Max spread as a fraction of mid price (in basis points)
    /// before the market is considered too illiquid to trade.
    /// Default: 20 bps (0.2%).
    pub max_spread_bps: Decimal,

    /// Check 2: Required order-book depth within top-N levels expressed as
    /// a multiple of the order quantity. A value of 3.0 means the top N
    /// levels must contain at least 3x our order size, so we are not more
    /// than 1/3 of near-liquidity.
    pub min_book_depth_multiple: Decimal,

    /// Check 2: How many levels deep to look when summing `bid_depth`/`ask_depth`.
    pub book_depth_levels: usize,

    /// Check 3: Volatility threshold for the leverage sleeve. If realized ATR%
    /// exceeds this value, leveraged entries are rejected. Default: 6.0 (%).
    pub max_atr_pct_for_leverage: Decimal,

    /// Check 3: Volatility threshold for all sleeves as a hard market-halt
    /// indicator (e.g. flash-crash detection). Default: 15.0 (%).
    pub max_atr_pct_absolute: Decimal,

    /// Check 4: Hard kill-switch budget for the leverage sleeve, in CHF.
    /// Default: -1000 CHF (lifetime realized loss).
    pub leverage_kill_switch_budget_chf: Decimal,

    /// Check 4: Portfolio-wide drawdown threshold. When exceeded, the
    /// leverage sleeve is force-disabled. Default: 0.20 (20%).
    pub max_portfolio_drawdown: Decimal,

    /// Check 4: Assumed instantaneous adverse move used to simulate
    /// worst-case loss for Check 4. Default: 0.10 (10%).
    pub adverse_move_for_worst_case: Decimal,

    /// Check 5: Maximum acceptable funding rate (per 8h) against our
    /// position direction before rejecting a perpetual trade.
    /// Default: 0.0002 (2 bps per 8h, ≈22% annualized).
    pub max_funding_rate_per_8h: Decimal,

    /// Hard cap on leverage for the CryptoLeverage sleeve.
    /// Default: 2.0. Checklist rejects any signal with leverage > this value.
    pub max_leverage: Decimal,

    /// Absolute cap on the notional size of a single order, in CHF.
    /// Protects against signal-generator bugs that produce absurd
    /// position sizes. This is a per-order cap and is evaluated
    /// independently of the leverage sleeve's P&L budget. Default:
    /// 1000 CHF — well above the 300-500 CHF MVP sleeves but well
    /// below anything that would cause meaningful harm.
    ///
    /// `#[serde(default)]` means configs predating this field will
    /// load with the default value rather than failing to parse.
    /// Operators tuning this should explicitly set it in daemon.toml.
    #[serde(default = "default_max_notional_chf_per_order")]
    pub max_notional_chf_per_order: Decimal,
}

fn default_max_notional_chf_per_order() -> Decimal {
    Decimal::from(1000)
}

impl Default for RiskConfig {
    fn default() -> Self {
        Self {
            max_tick_age_ms: 500,
            max_spread_bps: Decimal::from(20),
            min_book_depth_multiple: Decimal::from(3),
            book_depth_levels: 5,
            max_atr_pct_for_leverage: Decimal::from(6),
            max_atr_pct_absolute: Decimal::from(15),
            // -1000 CHF
            leverage_kill_switch_budget_chf: Decimal::from(-1000),
            // 0.20
            max_portfolio_drawdown: Decimal::new(20, 2),
            // 0.10
            adverse_move_for_worst_case: Decimal::new(10, 2),
            // 0.0002
            max_funding_rate_per_8h: Decimal::new(2, 4),
            max_leverage: Decimal::from(2),
            max_notional_chf_per_order: default_max_notional_chf_per_order(),
        }
    }
}
