//! Portfolio-level state: aggregated across sleeves and brokers.
//!
//! # Drawdown methodology (revised 2026-04)
//!
//! Earlier revisions of this file computed drawdown as
//! `(ATH_chf - current_chf) / ATH_chf`, using absolute CHF totals.
//! That formulation is mathematically wrong for a portfolio with ongoing
//! capital flows: a deposit inflates both `current` and the recorded ATH,
//! which masks adverse performance. A 50% crash followed by a deposit
//! would look "recovered" in the drawdown metric when in fact the
//! algorithm lost half the pre-deposit capital.
//!
//! We now use a **NAV-per-unit** scheme identical to the one used by
//! mutual funds and regulated collective investment schemes:
//!
//! * The portfolio has `total_units` outstanding units (dimensionless).
//! * On each deposit of D CHF at current `nav_per_unit = NAV`, we mint
//!   `D / NAV` new units. The per-unit NAV is unchanged by the deposit.
//! * On each withdrawal of W CHF, we burn `W / NAV` units.
//! * `nav_per_unit = total_value_chf / total_units`.
//! * The drawdown is `1 - (nav_per_unit / nav_hwm_per_unit)`.
//!
//! The arithmetic for this lives in the Python layer (or a yet-to-be-written
//! reconciliation tool); the Rust side consumes the precomputed values.
//!
//! # Unrealised P&L for new-trade admission
//!
//! The pre-trade checklist's hard-limit guard must consider adverse open
//! positions when deciding whether a new leveraged trade is admissible.
//! See [`PortfolioState::leverage_sleeve_effective_pnl`].

use crate::instrument::Sleeve;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// Drop 19 — CV-A1d: minimal summary of an open leverage position,
/// used by the `duplicate_position` pre-trade check to reject a second
/// entry on the same instrument + side while a position is live.
///
/// Intentionally narrow: symbol + signed quantity is enough to derive
/// side. Full entry price / unrealised PnL / leverage are already
/// accounted for by the sleeve-level aggregates elsewhere in
/// `PortfolioState`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OpenPositionSummary {
    pub instrument_symbol: String,
    pub sleeve: Sleeve,
    /// Signed quantity: positive = long, negative = short.
    /// Stored as a string to match the rest of the domain types which
    /// round-trip through SQLite's TEXT-decimal encoding without loss.
    pub quantity: Decimal,
}

impl OpenPositionSummary {
    pub fn is_long(&self) -> bool {
        self.quantity.is_sign_positive() && !self.quantity.is_zero()
    }
    pub fn is_short(&self) -> bool {
        self.quantity.is_sign_negative()
    }
}

/// Aggregated state of a single sleeve.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SleeveState {
    pub sleeve: Sleeve,
    /// Current market value of all positions in this sleeve, in CHF.
    pub market_value_chf: Decimal,
    /// Available cash allocated to this sleeve, in CHF.
    pub cash_chf: Decimal,
    /// Realized P&L lifetime for this sleeve, in CHF.
    /// Increases on profitable exits, decreases on losses.
    pub realized_pnl_lifetime_chf: Decimal,
    /// Unrealized P&L on currently open positions in this sleeve, in CHF.
    /// Positive when open positions are in profit, negative when in loss.
    pub unrealized_pnl_chf: Decimal,
}

impl SleeveState {
    pub fn total_value_chf(&self) -> Decimal {
        self.market_value_chf + self.cash_chf
    }
}

/// Consolidated portfolio state used by pre-trade risk checks.
///
/// This is a snapshot — it's computed once at the start of each risk
/// evaluation and not updated mid-evaluation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortfolioState {
    pub snapshot_at: DateTime<Utc>,
    pub sleeves: Vec<SleeveState>,

    /// Current NAV per unit, in CHF. Initialised at 1.0 on portfolio
    /// creation. Unchanged by capital flows; only moves with market value
    /// and realised P&L.
    pub nav_per_unit: Decimal,

    /// All-time high of `nav_per_unit`, updated whenever a new peak is
    /// reached. Used as the denominator for drawdown calculations.
    pub nav_hwm_per_unit: Decimal,

    /// Total outstanding units. Grows on deposits, shrinks on withdrawals.
    pub total_units: Decimal,

    /// Whether the kill-switch has been tripped. When true, no new orders
    /// for the leverage sleeve will pass the checklist.
    pub kill_switch_active: bool,

    /// Drop 19 — CV-A1d: open positions across sleeves, used by the
    /// `duplicate_position` pre-trade check. `Vec::new()` is a valid
    /// cold-start value (no open positions).
    #[serde(default)]
    pub open_positions: Vec<OpenPositionSummary>,
}

impl PortfolioState {
    pub fn total_value_chf(&self) -> Decimal {
        self.sleeves.iter().map(|s| s.total_value_chf()).sum()
    }

    pub fn sleeve(&self, sleeve: Sleeve) -> Option<&SleeveState> {
        self.sleeves.iter().find(|s| s.sleeve == sleeve)
    }

    /// Current drawdown as a fraction (0.0 to 1.0) of the all-time-high
    /// NAV per unit.
    ///
    /// Cash flows (deposits / withdrawals) do not affect this metric.
    /// Returns `Decimal::ZERO` on cold-start (HWM is zero or NAV is at HWM).
    pub fn drawdown_fraction(&self) -> Decimal {
        if self.nav_hwm_per_unit.is_zero() {
            return Decimal::ZERO;
        }
        if self.nav_per_unit >= self.nav_hwm_per_unit {
            return Decimal::ZERO;
        }
        (self.nav_hwm_per_unit - self.nav_per_unit) / self.nav_hwm_per_unit
    }

    /// Lifetime realised P&L of the leverage sleeve (CHF).
    ///
    /// Used by the kill-switch supervisor for the permanent-stop decision.
    /// We intentionally use realised-only here so that the permanent stop
    /// is not triggered by transient mark-to-market swings.
    pub fn leverage_sleeve_realized_pnl(&self) -> Decimal {
        self.sleeve(Sleeve::CryptoLeverage)
            .map(|s| s.realized_pnl_lifetime_chf)
            .unwrap_or(Decimal::ZERO)
    }

    /// Unrealised P&L on open positions in the leverage sleeve (CHF).
    pub fn leverage_sleeve_unrealized_pnl(&self) -> Decimal {
        self.sleeve(Sleeve::CryptoLeverage)
            .map(|s| s.unrealized_pnl_chf)
            .unwrap_or(Decimal::ZERO)
    }

    /// Effective P&L used for **new-trade** admission: realised plus any
    /// adverse unrealised position. Favourable unrealised profit is
    /// deliberately ignored — we do not count paper gains as fresh budget.
    ///
    /// This is the correct input for the pre-trade checklist's budget
    /// guard, because an open losing position could realise its loss at
    /// any time (e.g. if the trailing stop triggers between this check
    /// and the next).
    pub fn leverage_sleeve_effective_pnl(&self) -> Decimal {
        let realized = self.leverage_sleeve_realized_pnl();
        let unrealized = self.leverage_sleeve_unrealized_pnl();
        // Only adverse unrealised counts.
        let adverse = unrealized.min(Decimal::ZERO);
        realized + adverse
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(
        nav_per_unit: Decimal,
        nav_hwm: Decimal,
        realized: Decimal,
        unrealized: Decimal,
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
            nav_per_unit,
            nav_hwm_per_unit: nav_hwm,
            total_units: Decimal::from(1_000),
            kill_switch_active: false,
            open_positions: Vec::new(),
        }
    }

    #[test]
    fn drawdown_zero_at_hwm() {
        let s = state(Decimal::new(10_500, 4), Decimal::new(10_500, 4), Decimal::ZERO, Decimal::ZERO);
        assert_eq!(s.drawdown_fraction(), Decimal::ZERO);
    }

    #[test]
    fn drawdown_twenty_percent_nav_based() {
        // HWM 1.25, current 1.00. Drawdown = 0.25 / 1.25 = 0.20.
        let s = state(Decimal::new(10_000, 4), Decimal::new(12_500, 4), Decimal::ZERO, Decimal::ZERO);
        assert_eq!(s.drawdown_fraction(), Decimal::new(2, 1));
    }

    #[test]
    fn drawdown_independent_of_units_outstanding() {
        // The key property: drawdown depends on NAV-per-unit only, not on
        // the nominal amount of capital in the portfolio.
        let s1 = state(Decimal::new(11_000, 4), Decimal::new(12_500, 4), Decimal::ZERO, Decimal::ZERO);
        let s2 = PortfolioState {
            total_units: Decimal::from(10_000_000),
            ..s1.clone()
        };
        assert_eq!(s1.drawdown_fraction(), s2.drawdown_fraction());
    }

    #[test]
    fn effective_pnl_includes_adverse_unrealized() {
        let s = state(
            Decimal::from(1), Decimal::from(1),
            Decimal::from(-500),   // realized
            Decimal::from(-900),   // adverse unrealized
        );
        assert_eq!(s.leverage_sleeve_effective_pnl(), Decimal::from(-1400));
    }

    #[test]
    fn effective_pnl_ignores_favourable_unrealized() {
        let s = state(
            Decimal::from(1), Decimal::from(1),
            Decimal::from(-500),   // realized
            Decimal::from(300),    // favourable unrealized
        );
        // The +300 paper gain is NOT counted as fresh budget.
        assert_eq!(s.leverage_sleeve_effective_pnl(), Decimal::from(-500));
    }
}
