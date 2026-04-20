//! Portfolio-state loader.
//!
//! Reads the latest `equity_snapshot` row from SQLite and materialises it
//! into a `PortfolioState` suitable for the pre-trade checklist. On the
//! very first daemon run (no snapshots in the database), writes a
//! synthetic bootstrap snapshot so the pipeline is never blocked by a
//! cold database.
//!
//! # Design
//!
//! The snapshot table is the *source of truth* for the daemon's view of
//! the portfolio. It is intended to be updated by:
//!
//! 1. A periodic reconciliation task (not yet implemented) that consumes
//!    broker fill messages and recomputes each sleeve's market value.
//! 2. Deposit / withdrawal scripts that also write to `capital_flows` and
//!    update NAV-per-unit accounting.
//!
//! Until those are wired up, the daemon reads whatever is in the latest
//! snapshot and treats it as authoritative. Operators must therefore
//! maintain that table out-of-band; the warning from
//! `write_bootstrap_equity_snapshot` is designed to make this obvious.
//!
//! # Sleeve value mapping
//!
//! The snapshot schema stores per-sleeve market values directly. Cash is
//! stored as a single pooled number and assigned to `Sleeve::CryptoSpot`
//! by convention so the risk layer has somewhere to find it. This mapping
//! is cosmetic for the current set of checks but should be reviewed when
//! per-sleeve cash allocation becomes a real constraint.

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Utc};
use rt_core::instrument::Sleeve;
use rt_core::portfolio::{OpenPositionSummary, PortfolioState, SleeveState};
use rt_persistence::models::EquitySnapshotRow;
use rt_persistence::Database;
use rust_decimal::Decimal;
use std::str::FromStr;
use std::sync::Arc;
use tracing::debug;

/// Load the current portfolio state from the database. On first run,
/// writes a bootstrap snapshot and returns the synthetic baseline.
pub async fn load_portfolio_state(
    db: &Arc<Database>,
    snapshot_at: DateTime<Utc>,
) -> Result<PortfolioState> {
    let latest = db
        .latest_equity_snapshot()
        .await
        .context("fetching latest equity snapshot")?;

    let snap = match latest {
        Some(s) => s,
        None => {
            db.write_bootstrap_equity_snapshot(&rt_core::time::canonical_iso(snapshot_at))
                .await
                .context("writing bootstrap equity snapshot")?;
            db.latest_equity_snapshot()
                .await?
                .ok_or_else(|| anyhow!("bootstrap snapshot not readable after insert"))?
        }
    };

    let kill_switch_active = db
        .kill_switch_active()
        .await
        .context("checking kill-switch state")?;

    // Drop 19 — CV-A1d: load open positions for the duplicate-position
    // pre-trade check. We reuse the existing list_open_leverage_positions
    // helper; it returns (symbol, broker, sleeve, qty_str) tuples for
    // every row in `positions` with `closed_at IS NULL`.
    let raw_positions = db
        .list_open_leverage_positions()
        .await
        .context("listing open positions")?;
    let mut open_positions: Vec<OpenPositionSummary> =
        Vec::with_capacity(raw_positions.len());
    for (symbol, _broker, sleeve_str, qty_str) in raw_positions {
        let qty = Decimal::from_str(&qty_str).map_err(|e| {
            anyhow!("parsing position quantity {:?}: {}", qty_str, e)
        })?;
        let sleeve = match sleeve_str.as_str() {
            "crypto_leverage" => Sleeve::CryptoLeverage,
            "crypto_spot" => Sleeve::CryptoSpot,
            "etf" => Sleeve::Etf,
            other => {
                return Err(anyhow!(
                    "unknown sleeve {:?} on open position for {}",
                    other,
                    symbol
                ))
            }
        };
        open_positions.push(OpenPositionSummary {
            instrument_symbol: symbol,
            sleeve,
            quantity: qty,
            // Drop 19 LF-A1: diese Felder werden noch nicht aus der
            // positions-Tabelle + Kraken-Account gelesen. Feeding
            // kommt in Drop 20 (neuer equity_writer-Pfad). Bis dahin
            // fällt LiquidationDistanceCheck auf NotApplicable.
            avg_entry_price: None,
            liquidation_price: None,
            leverage: None,
        });
    }

    let state = materialise(&snap, snapshot_at, kill_switch_active, open_positions)?;
    debug!(
        nav_per_unit = %state.nav_per_unit,
        nav_hwm = %state.nav_hwm_per_unit,
        total_units = %state.total_units,
        kill_switch = kill_switch_active,
        open_positions = state.open_positions.len(),
        "portfolio state loaded from snapshot"
    );
    Ok(state)
}

fn materialise(
    snap: &EquitySnapshotRow,
    snapshot_at: DateTime<Utc>,
    kill_switch_active: bool,
    open_positions: Vec<rt_core::portfolio::OpenPositionSummary>,
) -> Result<PortfolioState> {
    let crypto_spot_value = decimal(&snap.crypto_spot_value_chf, "crypto_spot_value_chf")?;
    let etf_value = decimal(&snap.etf_value_chf, "etf_value_chf")?;
    let leverage_value = decimal(
        &snap.crypto_leverage_value_chf,
        "crypto_leverage_value_chf",
    )?;
    let cash = decimal(&snap.cash_chf, "cash_chf")?;
    let realized_leverage = decimal(
        &snap.realized_pnl_leverage_lifetime,
        "realized_pnl_leverage_lifetime",
    )?;
    let unrealized_leverage = decimal(
        &snap.unrealized_pnl_leverage_chf,
        "unrealized_pnl_leverage_chf",
    )?;
    let nav_per_unit = decimal(&snap.nav_per_unit, "nav_per_unit")?;
    let nav_hwm_per_unit = decimal(&snap.nav_hwm_per_unit, "nav_hwm_per_unit")?;
    let total_units = decimal(&snap.total_units, "total_units")?;

    Ok(PortfolioState {
        snapshot_at,
        sleeves: vec![
            SleeveState {
                sleeve: Sleeve::CryptoSpot,
                market_value_chf: crypto_spot_value,
                // All cash is assigned to CryptoSpot by convention; the
                // risk checks read total portfolio cash, not per-sleeve
                // cash, so this grouping is cosmetic.
                cash_chf: cash,
                realized_pnl_lifetime_chf: Decimal::ZERO,
                unrealized_pnl_chf: Decimal::ZERO,
            },
            SleeveState {
                sleeve: Sleeve::Etf,
                market_value_chf: etf_value,
                cash_chf: Decimal::ZERO,
                realized_pnl_lifetime_chf: Decimal::ZERO,
                unrealized_pnl_chf: Decimal::ZERO,
            },
            SleeveState {
                sleeve: Sleeve::CryptoLeverage,
                market_value_chf: leverage_value,
                cash_chf: Decimal::ZERO,
                realized_pnl_lifetime_chf: realized_leverage,
                unrealized_pnl_chf: unrealized_leverage,
            },
        ],
        nav_per_unit,
        nav_hwm_per_unit,
        total_units,
        kill_switch_active,
        open_positions,
    })
}

fn decimal(raw: &str, field: &'static str) -> Result<Decimal> {
    Decimal::from_str(raw)
        .map_err(|e| anyhow!("parsing {} as decimal: {} (raw: {:?})", field, e, raw))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_row() -> EquitySnapshotRow {
        EquitySnapshotRow {
            id: 1,
            timestamp: "2026-04-19T10:00:00+00:00".to_string(),
            total_equity_chf: "1500".to_string(),
            crypto_spot_value_chf: "400".to_string(),
            etf_value_chf: "300".to_string(),
            crypto_leverage_value_chf: "500".to_string(),
            cash_chf: "300".to_string(),
            realized_pnl_leverage_lifetime: "-50".to_string(),
            drawdown_fraction: "0.05".to_string(),
            unrealized_pnl_leverage_chf: "-30".to_string(),
            nav_per_unit: "1.5".to_string(),
            nav_hwm_per_unit: "1.6".to_string(),
            total_units: "1000".to_string(),
        }
    }

    #[test]
    fn materialise_maps_fields_correctly() {
        let snap = sample_row();
        let now = Utc::now();
        let state = materialise(&snap, now, false, Vec::new()).unwrap();

        assert_eq!(state.nav_per_unit, Decimal::new(15, 1));
        assert_eq!(state.nav_hwm_per_unit, Decimal::new(16, 1));
        assert_eq!(state.total_units, Decimal::from(1000));
        assert!(!state.kill_switch_active);
        assert_eq!(state.sleeves.len(), 3);

        let spot = &state.sleeves[0];
        assert_eq!(spot.sleeve, Sleeve::CryptoSpot);
        assert_eq!(spot.market_value_chf, Decimal::from(400));
        assert_eq!(spot.cash_chf, Decimal::from(300));

        let leverage = &state.sleeves[2];
        assert_eq!(leverage.sleeve, Sleeve::CryptoLeverage);
        assert_eq!(leverage.realized_pnl_lifetime_chf, Decimal::from(-50));
        assert_eq!(leverage.unrealized_pnl_chf, Decimal::from(-30));
    }

    #[test]
    fn materialise_propagates_kill_switch() {
        let snap = sample_row();
        let state = materialise(&snap, Utc::now(), true, Vec::new()).unwrap();
        assert!(state.kill_switch_active);
    }

    #[test]
    fn malformed_decimal_is_hard_error() {
        let mut snap = sample_row();
        snap.nav_per_unit = "not-a-number".to_string();
        let err = materialise(&snap, Utc::now(), false, Vec::new()).unwrap_err();
        assert!(err.to_string().contains("nav_per_unit"));
    }
}
