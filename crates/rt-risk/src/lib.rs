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

    /// LF-1 hard kill-switch threshold, in CHF. Triggers a **hard**
    /// stop (panic-close all open leverage positions) when realised
    /// plus unrealised P&L on the leverage sleeve falls to or below
    /// this value. Must be strictly lower than
    /// `leverage_kill_switch_budget_chf` so the soft (block-new-trades)
    /// trigger fires first and the hard trigger only activates when
    /// the market keeps moving against us. Default: -1200 CHF.
    ///
    /// `#[serde(default)]` ensures configs predating this field load
    /// with the default rather than failing to parse.
    #[serde(default = "default_kill_switch_effective_threshold_chf")]
    pub kill_switch_effective_threshold_chf: Decimal,

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

    /// Fallback USD-per-CHF FX rate used by the equity snapshot
    /// writer when no fresh signal is available to supply
    /// `fx_quote_per_chf` via metadata. Signals remain the preferred
    /// source; this default only kicks in for background equity
    /// tracking that must not stall just because the signal generator
    /// is between runs.
    ///
    /// Intended to be refreshed manually (config reload) during
    /// periods of significant FX moves. Default 1.10 (CHF slightly
    /// weaker than USD, a rough mid-2020s mid).
    #[serde(default = "default_usd_per_chf_fallback")]
    pub usd_per_chf_fallback: Decimal,

    /// Drop 19 LF-A1: Mindestabstand zum Liquidationspreis, gemessen
    /// in ATRs. Unterhalb dieses Wertes wird ein neuer Leverage-Entry
    /// abgelehnt, weil ein einziger gewöhnlicher ATR-Move die Position
    /// wegliquidieren würde. Default 2.0 (konservativ: 2 ATR Puffer).
    /// Bei 2x Leverage und 300 CHF ist dieser Check praktisch immer
    /// `Approved` — er wird erst relevant wenn höhere Hebel oder
    /// mehrere korrelierte Positionen ins Spiel kommen. `#[serde(default)]`
    /// damit alte Configs nicht brechen.
    #[serde(default = "default_min_liq_distance_atrs")]
    pub min_liq_distance_atrs: Decimal,

    /// Drop 19 LF-A2: erwartete Round-Trip-Gebühren auf einem
    /// Leverage-Trade, als Bruchteil des Notionals. Deckt Open + Close
    /// als Taker ab. Kraken Futures aktuell ca. 0.05% Taker pro Leg
    /// → 0.001 = 0.10% round-trip. Wird im Hard-Limit-Check zum
    /// worst_case_loss addiert, damit ein Trade der "gerade eben noch"
    /// ins Budget passen würde nicht durch Gebühren allein das
    /// Kill-Switch-Budget reissen kann. Default 0.001 (0.10%).
    #[serde(default = "default_expected_round_trip_fee_fraction")]
    pub expected_round_trip_fee_fraction: Decimal,

    /// Drop 19 LF-A2: erwartete Funding-Kosten auf einem typischen
    /// Halt eines Leverage-Trades, als Bruchteil des Notionals.
    /// Kraken-Futures Funding akkumuliert alle 8h. Bei einer geplanten
    /// Haltedauer von ~24h sind das 3 Funding-Windows. Bei typischen
    /// 2-3 bps pro 8h + Adverse-Drift-Annahme ergibt sich grob 0.002
    /// = 0.20% reserviertes Funding. Wird wie die Fee-Reserve zum
    /// worst_case_loss addiert. Default 0.002 (0.20%).
    #[serde(default = "default_expected_funding_reserve_fraction")]
    pub expected_funding_reserve_fraction: Decimal,
}

fn default_min_liq_distance_atrs() -> Decimal {
    Decimal::from(2)
}

fn default_expected_round_trip_fee_fraction() -> Decimal {
    // 0.001 = 0.10% round-trip (ca. 0.05% × 2 legs, Taker)
    Decimal::new(1, 3)
}

fn default_expected_funding_reserve_fraction() -> Decimal {
    // 0.002 = 0.20% pro erwartetem ~24h-Halt
    Decimal::new(2, 3)
}

fn default_max_notional_chf_per_order() -> Decimal {
    Decimal::from(1000)
}

fn default_usd_per_chf_fallback() -> Decimal {
    // Decimal::new(mantissa, scale) → 110 / 10^1 = 1.10
    Decimal::new(110, 2)
}

fn default_kill_switch_effective_threshold_chf() -> Decimal {
    // -1200 CHF — 200 CHF below the -1000 realised budget, so the
    // soft (block-new-trades) trigger always fires first and the hard
    // (panic-close) trigger only catches a runaway mark-to-market
    // drop after realised losses already breached budget.
    Decimal::from(-1200)
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
            usd_per_chf_fallback: default_usd_per_chf_fallback(),
            kill_switch_effective_threshold_chf:
                default_kill_switch_effective_threshold_chf(),
            min_liq_distance_atrs: default_min_liq_distance_atrs(),
            expected_round_trip_fee_fraction: default_expected_round_trip_fee_fraction(),
            expected_funding_reserve_fraction: default_expected_funding_reserve_fraction(),
        }
    }
}
