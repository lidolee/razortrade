//! Signal processor: the beating heart of the daemon.
//!
//! Polls the `signals` table at 1 Hz, and for each pending signal:
//!
//! 1. Builds a `MarketSnapshot` via [`MarketDataService`].
//! 2. Builds a `PortfolioState` (currently stubbed; next drop will read
//!    it from SQLite equity snapshots).
//! 3. Evaluates the pre-trade checklist.
//! 4. Records the outcome in `checklist_evaluations` and updates the
//!    `signals` row to `processed` or `rejected`.
//! 5. On approval: (next drop) routes to the appropriate Broker client.
//!
//! This skeleton performs steps 1–4 end-to-end and leaves a clear TODO
//! for step 5 so the integration surface is obvious.

use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use rt_core::instrument::Sleeve;
use rt_core::order::Side;
use rt_core::portfolio::{PortfolioState, SleeveState};
use rt_core::signal::{Signal, SignalStatus, SignalType};
use rt_persistence::models::SignalRow;
use rt_persistence::Database;
use rt_risk::{PreTradeChecklist, PreTradeContext, RiskConfig};
use rust_decimal::Decimal;
use tokio::sync::watch;
use tracing::{error, info, instrument, warn};

use crate::market_data::{MarketDataError, MarketDataService};

pub struct SignalProcessor {
    db: Arc<Database>,
    checklist: Arc<PreTradeChecklist>,
    risk_config: Arc<RiskConfig>,
    market_data: Arc<MarketDataService>,
}

impl SignalProcessor {
    pub fn new(
        db: Arc<Database>,
        checklist: Arc<PreTradeChecklist>,
        risk_config: Arc<RiskConfig>,
        market_data: Arc<MarketDataService>,
    ) -> Self {
        Self {
            db,
            checklist,
            risk_config,
            market_data,
        }
    }

    pub async fn run(&self, interval: Duration, mut shutdown: watch::Receiver<bool>) {
        info!(
            interval_ms = interval.as_millis() as u64,
            "signal processor started"
        );

        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    if let Err(e) = self.poll_once().await {
                        error!(error = %e, "signal poll iteration failed");
                    }
                }
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        info!("signal processor received shutdown signal");
                        return;
                    }
                }
            }
        }
    }

    async fn poll_once(&self) -> anyhow::Result<()> {
        let pending = self.db.pending_signals(64).await?;
        if pending.is_empty() {
            return Ok(());
        }
        info!(count = pending.len(), "processing pending signals");

        for row in pending {
            if let Err(e) = self.process_signal(row).await {
                error!(error = %e, "signal processing failed");
            }
        }
        Ok(())
    }

    #[instrument(skip(self, row), fields(signal_id = row.id, instrument = %row.instrument_symbol))]
    async fn process_signal(&self, row: SignalRow) -> anyhow::Result<()> {
        let signal = match row_to_signal(&row) {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "signal row failed validation");
                let reason = serde_json::json!({
                    "kind": "invalid_signal",
                    "detail": format!("row parsing: {e}")
                });
                self.db
                    .mark_signal_rejected(row.id, &Utc::now().to_rfc3339(), &reason.to_string())
                    .await?;
                return Ok(());
            }
        };

        // --- Build MarketSnapshot --------------------------------------
        let market = match self
            .market_data
            .snapshot_for_signal(&signal.instrument_symbol, signal.metadata.as_ref())
            .await
        {
            Ok(m) => m,
            Err(e) => {
                warn!(error = %e, "market snapshot unavailable; signal rejected");
                self.record_rejection(&signal, &market_data_error_to_json(e))
                    .await?;
                return Ok(());
            }
        };

        // --- Build PortfolioState (stubbed) ----------------------------
        // TODO(next-drop): load real state from `equity_snapshots` +
        // open positions. For now, we use a safe placeholder so the
        // checklist can still run and exercise the full pipeline.
        let portfolio = stub_portfolio_state();

        // --- Evaluate checklist ----------------------------------------
        let ctx = PreTradeContext {
            signal: &signal,
            market: &market,
            portfolio: &portfolio,
            config: self.risk_config.as_ref(),
            now: Utc::now(),
        };
        let result = self.checklist.evaluate(&ctx);

        // --- Persist audit record --------------------------------------
        let outcomes_json = serde_json::to_string(&result.outcomes)?;
        self.db
            .record_checklist_evaluation(
                signal.id,
                &result.evaluated_at.to_rfc3339(),
                result.approved,
                &outcomes_json,
            )
            .await?;

        if result.approved {
            info!(summary = %result.summary(), "signal approved; submission TODO");
            // TODO(next-drop): route via rt-execution::Broker based on
            // signal.sleeve → Kraken Futures or IBKR.
            // Until then, we mark processed without actually trading.
            self.db
                .mark_signal_processed(signal.id, &Utc::now().to_rfc3339())
                .await?;
        } else {
            info!(summary = %result.summary(), "signal rejected by checklist");
            let rejection_json = serde_json::json!({
                "kind": "checklist_rejected",
                "summary": result.summary(),
                "outcomes": result.outcomes,
            });
            self.db
                .mark_signal_rejected(
                    signal.id,
                    &Utc::now().to_rfc3339(),
                    &rejection_json.to_string(),
                )
                .await?;
        }
        Ok(())
    }

    async fn record_rejection(
        &self,
        signal: &Signal,
        reason_json: &serde_json::Value,
    ) -> anyhow::Result<()> {
        self.db
            .mark_signal_rejected(
                signal.id,
                &Utc::now().to_rfc3339(),
                &reason_json.to_string(),
            )
            .await?;
        Ok(())
    }
}

fn row_to_signal(row: &SignalRow) -> anyhow::Result<Signal> {
    use anyhow::anyhow;

    let side = match row.side.as_str() {
        "buy" => Side::Buy,
        "sell" => Side::Sell,
        other => return Err(anyhow!("invalid side: {other}")),
    };

    let signal_type: SignalType = serde_json::from_value(serde_json::Value::String(
        row.signal_type.clone(),
    ))
    .map_err(|e| anyhow!("invalid signal_type {}: {e}", row.signal_type))?;

    let sleeve: Sleeve = serde_json::from_value(serde_json::Value::String(row.sleeve.clone()))
        .map_err(|e| anyhow!("invalid sleeve {}: {e}", row.sleeve))?;

    let notional_chf = Decimal::from_str(&row.notional_chf)
        .map_err(|e| anyhow!("invalid notional: {e}"))?;
    let leverage =
        Decimal::from_str(&row.leverage).map_err(|e| anyhow!("invalid leverage: {e}"))?;

    let created_at = chrono::DateTime::parse_from_rfc3339(&row.created_at)
        .map_err(|e| anyhow!("invalid created_at: {e}"))?
        .with_timezone(&Utc);

    let metadata = row
        .metadata_json
        .as_deref()
        .map(|s| serde_json::from_str::<serde_json::Value>(s))
        .transpose()
        .map_err(|e| anyhow!("invalid metadata json: {e}"))?;

    let status: SignalStatus = serde_json::from_value(serde_json::Value::String(
        row.status.clone(),
    ))
    .map_err(|e| anyhow!("invalid status {}: {e}", row.status))?;

    let processed_at = row
        .processed_at
        .as_deref()
        .map(|s| chrono::DateTime::parse_from_rfc3339(s).map(|d| d.with_timezone(&Utc)))
        .transpose()
        .map_err(|e| anyhow!("invalid processed_at: {e}"))?;

    Ok(Signal {
        id: row.id,
        created_at,
        instrument_symbol: row.instrument_symbol.clone(),
        side,
        signal_type,
        sleeve,
        notional_chf,
        leverage,
        metadata,
        status,
        processed_at,
        rejection_reason: row.rejection_reason.clone(),
    })
}

fn market_data_error_to_json(e: MarketDataError) -> serde_json::Value {
    serde_json::json!({
        "kind": "market_data_unavailable",
        "detail": e.to_string(),
    })
}

/// Placeholder portfolio state until reconciliation is wired up.
///
/// Returns a "fresh" portfolio at NAV 1.00, no open positions, no realised
/// losses, kill-switch inactive. This allows the checklist to run against
/// real market data during the bring-up phase. Do NOT ship a production
/// build with this.
fn stub_portfolio_state() -> PortfolioState {
    PortfolioState {
        snapshot_at: Utc::now(),
        sleeves: vec![
            SleeveState {
                sleeve: Sleeve::CryptoSpot,
                market_value_chf: Decimal::ZERO,
                cash_chf: Decimal::from(1_000),
                realized_pnl_lifetime_chf: Decimal::ZERO,
                unrealized_pnl_chf: Decimal::ZERO,
            },
            SleeveState {
                sleeve: Sleeve::CryptoLeverage,
                market_value_chf: Decimal::ZERO,
                cash_chf: Decimal::ZERO,
                realized_pnl_lifetime_chf: Decimal::ZERO,
                unrealized_pnl_chf: Decimal::ZERO,
            },
        ],
        nav_per_unit: Decimal::from(1),
        nav_hwm_per_unit: Decimal::from(1),
        total_units: Decimal::from(1_000),
        kill_switch_active: false,
    }
}
