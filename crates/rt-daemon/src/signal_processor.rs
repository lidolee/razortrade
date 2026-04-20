//! Signal processor: the beating heart of the daemon.
//!
//! Polls the `signals` table at 1 Hz, and for each pending signal:
//!
//! 1. Builds a `MarketSnapshot` via [`MarketDataService`].
//! 2. Loads the current `PortfolioState` from SQLite (bootstrap on first run).
//! 3. Evaluates the pre-trade checklist.
//! 4. Records the outcome in `checklist_evaluations` and updates the
//!    `signals` row to `processed` or `rejected`.
//! 5. On approval: routes according to the configured [`ExecutionMode`].
//!    - `DryRun`: records the intended order to `dry_run_orders`, never
//!      touches any broker. Signal is marked `processed`.
//!    - `Paper` / `Live`: currently rejects with an explicit
//!      `execution_mode_unavailable` reason until the broker submission
//!      layer is wired up.
//!
//! Design note: the dry-run path is deliberately complete and
//! well-tested. It is our primary validation vehicle before any real
//! broker submission goes live.

use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use rt_core::execution_mode::ExecutionMode;
use rt_core::instrument::{AssetClass, Broker as BrokerKind, Instrument, Sleeve};
use rt_core::market_data::MarketSnapshot;
use rt_core::order::{Order, OrderStatus, OrderType, Side, TimeInForce};
use rt_core::signal::{Signal, SignalStatus, SignalType};
use rt_execution::{Broker, ExecutionError};
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
    execution_mode: ExecutionMode,
    /// Broker for the CryptoLeverage sleeve (Kraken Futures).
    /// `None` in DryRun mode; required in Paper/Live mode. Startup in
    /// Paper/Live without this set is prevented by main.rs.
    crypto_leverage_broker: Option<Arc<dyn Broker>>,
}

/// Drop 19 — CV-A1a: outcome of reconciling an uncertain order at the
/// start of the submit path. See `SignalProcessor::try_reconcile_uncertain_order`.
#[derive(Debug)]
enum UncertainReconcileOutcome {
    /// No existing order row for this signal — proceed to fresh submit.
    NoExistingOrder,
    /// Existing order is already in a terminal-forward state (orphan-
    /// catcher or Kraken lookup resolved it), or otherwise the caller
    /// must not submit on this tick.
    AlreadyReconciled,
    /// Existing order is `uncertain` and Kraken doesn't have it —
    /// caller should submit reusing this order_id + same cli_ord_id.
    ResubmitWithOrderId(i64),
    /// Broker lookup itself failed. Signal cooldown has been extended;
    /// caller returns.
    LookupFailedRetry,
    /// Existing uncertain order has no cli_ord_id — cannot correlate;
    /// signal has been rejected; caller returns.
    MalformedUnrecoverable,
}

impl SignalProcessor {
    pub fn new(
        db: Arc<Database>,
        checklist: Arc<PreTradeChecklist>,
        risk_config: Arc<RiskConfig>,
        market_data: Arc<MarketDataService>,
        execution_mode: ExecutionMode,
        crypto_leverage_broker: Option<Arc<dyn Broker>>,
    ) -> Self {
        Self {
            db,
            checklist,
            risk_config,
            market_data,
            execution_mode,
            crypto_leverage_broker,
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
        let now_iso = Utc::now().to_rfc3339();
        let pending = self.db.pending_signals(64, &now_iso).await?;
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

        // --- OF-2: expiry guard ----------------------------------------
        // If a signal has an expires_at and we've blown past it (e.g.
        // the daemon was down during the window), reject without any
        // further work. This prevents stale breakouts from executing
        // minutes or hours after the market context that generated
        // them has changed.
        if let Some(exp) = signal.expires_at {
            let now = Utc::now();
            if exp <= now {
                warn!(
                    signal_id = signal.id,
                    expires_at = %exp,
                    now = %now,
                    "signal expired before processing; rejecting"
                );
                let reason = serde_json::json!({
                    "kind": "signal_expired",
                    "expires_at": exp.to_rfc3339(),
                    "now": now.to_rfc3339(),
                });
                self.db
                    .mark_signal_rejected(
                        signal.id,
                        &now.to_rfc3339(),
                        &reason.to_string(),
                    )
                    .await?;
                return Ok(());
            }
        }

        // The Drop-19 uncertain-order reconciliation lives in
        // `submit_via_broker::try_reconcile_uncertain_order` — we
        // cannot run it here because we don't yet know if the signal
        // will survive the market-data + checklist gates below. Doing
        // the reconcile here would mean reconciling uncertain orders
        // for signals that get rejected downstream, which is wasteful
        // and could mutate order rows for signals we never submit.

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

        // --- Build PortfolioState from SQLite -------------------------
        // On first run, the loader auto-writes a bootstrap snapshot so
        // the pipeline is never blocked by a cold database. Real capital
        // flows must be recorded via `capital_flows` before hard-limit
        // decisions carry real-money weight.
        let portfolio = match crate::portfolio_loader::load_portfolio_state(
            &self.db,
            Utc::now(),
        )
        .await
        {
            Ok(p) => p,
            Err(e) => {
                warn!(error = %e, "portfolio state load failed; signal rejected");
                let reason = serde_json::json!({
                    "kind": "portfolio_state_unavailable",
                    "detail": e.to_string(),
                });
                self.db
                    .mark_signal_rejected(signal.id, &Utc::now().to_rfc3339(), &reason.to_string())
                    .await?;
                return Ok(());
            }
        };

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
            info!(
                summary = %result.summary(),
                mode = ?self.execution_mode,
                "signal approved by checklist"
            );
            self.handle_approval(&signal, &market).await?;
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

    /// Handles an approved signal by routing according to the configured
    /// execution mode. In `DryRun`, the intended order is logged to
    /// `dry_run_orders` and the signal is marked processed. In `Paper` or
    /// `Live`, the signal is currently rejected with an explicit
    /// `execution_mode_unavailable` reason until the broker submission
    /// layer is wired up in the next drop.
    async fn handle_approval(
        &self,
        signal: &Signal,
        market: &MarketSnapshot,
    ) -> anyhow::Result<()> {
        match self.execution_mode {
            ExecutionMode::DryRun => self.record_dry_run_intent(signal, market).await,
            ExecutionMode::Paper | ExecutionMode::Live => {
                self.submit_via_broker(signal, market).await
            }
        }
    }

    /// Drop 19 — CV-A1a: reconcile an uncertain order if one exists for
    /// this signal. Outcomes steer the caller in `submit_via_broker`:
    ///
    /// | Outcome                     | Caller action                 |
    /// |-----------------------------|-------------------------------|
    /// | `NoExistingOrder`           | fresh submit, insert new row  |
    /// | `AlreadyReconciled`         | return immediately            |
    /// | `ResubmitWithOrderId(id)`   | submit into existing order_id |
    /// | `LookupFailedRetry`         | return; cooldown extended     |
    /// | `MalformedUnrecoverable`    | return; signal already flagged|
    ///
    /// The method keeps its own copy of the broker so the caller does
    /// not need to juggle mutable state. All side effects (signal /
    /// order mutations, cooldown) happen inside.
    async fn try_reconcile_uncertain_order(
        &self,
        signal: &Signal,
        broker: &dyn Broker,
    ) -> anyhow::Result<UncertainReconcileOutcome> {
        let existing = self.db.pending_order_for_signal(signal.id).await?;
        let (existing_order_id, existing_status, existing_cli) = match existing {
            Some(row) => row,
            None => return Ok(UncertainReconcileOutcome::NoExistingOrder),
        };

        match existing_status.as_str() {
            "acknowledged" | "partially_filled" | "filled" | "submitted" => {
                // Orphan-catcher already adopted the broker-side order
                // (via cli_ord_id match on a fill event) and promoted
                // the status. The signal is effectively done; mark it
                // processed so it stops appearing in pending_signals.
                info!(
                    signal_id = signal.id,
                    existing_order_id,
                    existing_status = %existing_status,
                    "signal has an in-flight or filled order already; marking processed"
                );
                let now_iso = Utc::now().to_rfc3339();
                self.db.mark_signal_processed(signal.id, &now_iso).await?;
                self.db.clear_signal_retry_after(signal.id).await?;
                Ok(UncertainReconcileOutcome::AlreadyReconciled)
            }
            "uncertain" => {
                let cli = match existing_cli {
                    Some(c) => c,
                    None => {
                        error!(
                            signal_id = signal.id,
                            existing_order_id,
                            "uncertain order has no cli_ord_id; refusing to resubmit"
                        );
                        let reason = serde_json::json!({
                            "kind": "uncertain_without_cli",
                            "order_id": existing_order_id,
                        });
                        self.db
                            .mark_signal_rejected(
                                signal.id,
                                &Utc::now().to_rfc3339(),
                                &reason.to_string(),
                            )
                            .await?;
                        return Ok(UncertainReconcileOutcome::MalformedUnrecoverable);
                    }
                };

                info!(
                    signal_id = signal.id,
                    existing_order_id,
                    cli_ord_id = %cli,
                    "Drop-19 uncertain-resubmit: checking Kraken for order presence"
                );
                match broker.get_order_by_cli_ord_id(&cli).await {
                    Ok(Some(open)) => {
                        let now_iso = Utc::now().to_rfc3339();
                        self.db
                            .backfill_broker_order_id(
                                existing_order_id,
                                &open.broker_order_id,
                                &now_iso,
                            )
                            .await?;
                        self.db.mark_signal_processed(signal.id, &now_iso).await?;
                        self.db.clear_signal_retry_after(signal.id).await?;
                        info!(
                            signal_id = signal.id,
                            existing_order_id,
                            broker_order_id = %open.broker_order_id,
                            "uncertain-resubmit: Kraken confirmed order; adopted via backfill"
                        );
                        Ok(UncertainReconcileOutcome::AlreadyReconciled)
                    }
                    Ok(None) => {
                        info!(
                            signal_id = signal.id,
                            existing_order_id,
                            cli_ord_id = %cli,
                            "uncertain-resubmit: Kraken has no such order; will resubmit same row + cli_ord_id"
                        );
                        Ok(UncertainReconcileOutcome::ResubmitWithOrderId(
                            existing_order_id,
                        ))
                    }
                    Err(e) => {
                        warn!(
                            signal_id = signal.id,
                            existing_order_id,
                            error = %e,
                            "uncertain-resubmit: broker.get_order_by_cli_ord_id failed; extending cooldown"
                        );
                        let retry_at =
                            (Utc::now() + chrono::Duration::seconds(90)).to_rfc3339();
                        self.db
                            .set_signal_retry_after(signal.id, &retry_at)
                            .await?;
                        Ok(UncertainReconcileOutcome::LookupFailedRetry)
                    }
                }
            }
            "pending_submission" => {
                // Defensive: another process or thread might still be
                // in flight. We don't have a multi-process story but
                // keep the path clean: skip this tick.
                warn!(
                    signal_id = signal.id,
                    existing_order_id,
                    "signal has a pending_submission order already; skipping this tick"
                );
                Ok(UncertainReconcileOutcome::AlreadyReconciled)
            }
            other => {
                warn!(
                    signal_id = signal.id,
                    existing_order_id,
                    status = %other,
                    "pending_order_for_signal returned unexpected status; skipping"
                );
                Ok(UncertainReconcileOutcome::AlreadyReconciled)
            }
        }
    }

    /// Real broker submission path (Paper and Live).
    ///
    /// Currently only the `CryptoLeverage` sleeve is implemented; `CryptoSpot`
    /// and `Etf` reject with a structured reason until their brokers are
    /// wired in a later drop. This is deliberate: silent no-ops or fallbacks
    /// here could lead to the system thinking a trade happened when it didn't.
    #[instrument(skip(self, signal, market), fields(signal_id = signal.id))]
    async fn submit_via_broker(
        &self,
        signal: &Signal,
        market: &MarketSnapshot,
    ) -> anyhow::Result<()> {
        // -- Routing guard: only CryptoLeverage in this drop -------------
        if signal.sleeve != Sleeve::CryptoLeverage {
            warn!(
                sleeve = ?signal.sleeve,
                mode = ?self.execution_mode,
                "sleeve not yet implemented for Paper/Live; rejecting signal"
            );
            let reason = serde_json::json!({
                "kind": "sleeve_not_implemented",
                "sleeve": enum_to_snake_case(&signal.sleeve)?,
                "detail": "only CryptoLeverage (Kraken Futures) is wired in drop 4",
            });
            self.db
                .mark_signal_rejected(
                    signal.id,
                    &Utc::now().to_rfc3339(),
                    &reason.to_string(),
                )
                .await?;
            return Ok(());
        }

        // -- Broker presence ---------------------------------------------
        let broker = match &self.crypto_leverage_broker {
            Some(b) => b.clone(),
            None => {
                // Shouldn't happen: main.rs refuses to start in Paper/Live
                // without a broker. Treat as a programming error, reject
                // loudly, keep running.
                error!(
                    mode = ?self.execution_mode,
                    "no crypto_leverage_broker configured despite non-DryRun mode; \
                     this is a startup-time invariant violation"
                );
                let reason = serde_json::json!({
                    "kind": "broker_not_configured",
                    "mode": enum_to_snake_case(&self.execution_mode)?,
                });
                self.db
                    .mark_signal_rejected(
                        signal.id,
                        &Utc::now().to_rfc3339(),
                        &reason.to_string(),
                    )
                    .await?;
                return Ok(());
            }
        };

        // -- Drop 19 CV-A1a: reconcile any pre-existing uncertain order
        //                   for this signal BEFORE we build a fresh one.
        //
        // If the signal already has an `uncertain` order row (previous
        // submit timed out), we either resolve it via Kraken's REST
        // open-orders lookup or fall through to a resubmit that reuses
        // the same order row + same deterministic cli_ord_id. This
        // prevents the Drop-18 doubled-position-via-new-order-row bug.
        let resubmit_into_order_id: Option<i64> = match self
            .try_reconcile_uncertain_order(signal, broker.as_ref())
            .await?
        {
            UncertainReconcileOutcome::AlreadyReconciled => return Ok(()),
            UncertainReconcileOutcome::LookupFailedRetry => return Ok(()),
            UncertainReconcileOutcome::MalformedUnrecoverable => return Ok(()),
            UncertainReconcileOutcome::NoExistingOrder => None,
            UncertainReconcileOutcome::ResubmitWithOrderId(id) => Some(id),
        };

        // -- Build the order with Kraken-Futures contract semantics ------
        let order = match build_kraken_futures_order(signal, market) {
            Ok(o) => o,
            Err(reason_obj) => {
                warn!(reason = %reason_obj, "failed to build order from signal");
                self.db
                    .mark_signal_rejected(
                        signal.id,
                        &Utc::now().to_rfc3339(),
                        &reason_obj.to_string(),
                    )
                    .await?;
                return Ok(());
            }
        };

        // -- Persist as pending before touching the network --------------
        //
        // If the process crashes between insert_pending_order and the
        // broker response, reconciliation on next startup can list open
        // orders on Kraken and match them by cli_ord_id. For now we rely
        // on the manual operator check; this is acceptable for demo /
        // 300-CHF phase.
        //
        // Drop 19 — CV-A1a: if the uncertain-resubmit path above set
        // resubmit_into_order_id, we reuse the existing order row
        // instead of inserting a new one. The row's status is still
        // `uncertain` — the broker-call result below will drive it
        // forward.
        let now_iso = Utc::now().to_rfc3339();
        let order_type_str = enum_to_snake_case(&order.order_type)?;
        let tif_str = enum_to_snake_case(&order.time_in_force)?;
        let side_str = enum_to_snake_case(&order.side)?;

        let order_id = match resubmit_into_order_id {
            Some(id) => {
                info!(
                    order_id = id,
                    signal_id = signal.id,
                    "resubmitting uncertain order (same cli_ord_id, same row)"
                );
                id
            }
            None => self
                .db
                .insert_pending_order(
                    Some(signal.id),
                    "kraken_futures",
                    &order.instrument.symbol,
                    &side_str,
                    &order_type_str,
                    &tif_str,
                    &order.quantity.to_string(),
                    order.limit_price.map(|p| p.to_string()).as_deref(),
                    None,
                    &now_iso,
                    order.cli_ord_id.as_deref(),
                )
                .await?,
        };

        info!(
            order_id,
            symbol = %order.instrument.symbol,
            side = ?order.side,
            quantity_contracts = %order.quantity,
            limit_price = ?order.limit_price,
            mode = ?self.execution_mode,
            "submitting order to Kraken Futures"
        );

        // -- Hand off to the broker --------------------------------------
        let result = broker.submit_order(&order).await;
        let submit_iso = Utc::now().to_rfc3339();

        match result {
            Ok(sub) => {
                let status_str = order_status_to_string(sub.status);
                info!(
                    order_id,
                    broker_order_id = %sub.broker_order_id,
                    status = %status_str,
                    "order submitted successfully"
                );
                self.db
                    .mark_order_submitted(order_id, &sub.broker_order_id, &status_str, &submit_iso)
                    .await?;
                self.db
                    .mark_signal_processed(signal.id, &submit_iso)
                    .await?;
                self.db.clear_signal_retry_after(signal.id).await?;
            }
            Err(err) => {
                let err_string = err.to_string();
                let err_kind = broker_error_kind(&err);
                // Drop 19 — CV-A1a: distinguish transient uncertainty
                // (Timeout, Network) from genuine rejections. For the
                // uncertain case, Kraken may have placed the order
                // despite our missed response; marking it `failed` and
                // rejecting the signal would invite a doubled position
                // on the next 4h candle via a fresh Python signal. We
                // instead:
                //   1. flag the order `uncertain` with the error string,
                //   2. keep the signal in `pending` status,
                //   3. set `retry_after_at = now + 90s` so the processor
                //      doesn't hammer the same signal in the poll loop.
                // During the cooldown the WS fill feed has a chance to
                // confirm the broker-side outcome. If a fill arrives
                // with our cli_ord_id, the orphan-catcher in
                // fill_reconciler will backfill broker_order_id and
                // promote the status from `uncertain` to `acknowledged`
                // (see repo.rs backfill_broker_order_id Drop-19 change).
                // If no fill arrives, after the cooldown the processor
                // calls broker.get_order(…) via the uncertain-resubmit
                // path at the top of process_signal to decide between
                // reconcile and resubmit.
                let is_uncertain = matches!(
                    err,
                    ExecutionError::Timeout { .. } | ExecutionError::Network(_)
                );
                if is_uncertain {
                    warn!(
                        order_id,
                        signal_id = signal.id,
                        error_kind = err_kind,
                        error = %err_string,
                        "broker submit uncertain (timeout/network); marking order=uncertain and signal cooldown 90s"
                    );
                    self.db
                        .mark_order_uncertain(order_id, &err_string, &submit_iso)
                        .await?;
                    let retry_at = (Utc::now() + chrono::Duration::seconds(90)).to_rfc3339();
                    self.db
                        .set_signal_retry_after(signal.id, &retry_at)
                        .await?;
                } else {
                    error!(
                        order_id,
                        error_kind = err_kind,
                        error = %err_string,
                        "broker rejected order submission; marking order=failed, signal=rejected"
                    );
                    self.db
                        .mark_order_failed(order_id, &err_string, &submit_iso)
                        .await?;
                    let reason = serde_json::json!({
                        "kind": err_kind,
                        "order_id": order_id,
                        "detail": err_string,
                    });
                    self.db
                        .mark_signal_rejected(signal.id, &submit_iso, &reason.to_string())
                        .await?;
                }
            }
        }

        Ok(())
    }

    /// Build an order intent from an approved signal + current market,
    /// persist it to `dry_run_orders`, and mark the signal processed.
    /// No broker calls whatsoever.
    async fn record_dry_run_intent(
        &self,
        signal: &Signal,
        market: &MarketSnapshot,
    ) -> anyhow::Result<()> {
        let intent = build_order_intent(signal, market);
        info!(
            instrument = %signal.instrument_symbol,
            side = ?signal.side,
            sleeve = ?signal.sleeve,
            notional_chf = %signal.notional_chf,
            est_price = ?intent.estimated_price,
            est_quantity = ?intent.estimated_quantity,
            "DRY-RUN: order intent recorded, no broker submission"
        );

        let intent_json = serde_json::to_string(&intent)?;
        let sleeve_str = enum_to_snake_case(&signal.sleeve)?;
        let side_str = enum_to_snake_case(&signal.side)?;
        let est_price = intent.estimated_price.map(|p| p.to_string());
        let est_qty = intent.estimated_quantity.map(|q| q.to_string());

        self.db
            .record_dry_run_order(
                signal.id,
                &Utc::now().to_rfc3339(),
                &signal.instrument_symbol,
                &side_str,
                &sleeve_str,
                &intent.broker_hint,
                &signal.notional_chf.to_string(),
                est_price.as_deref(),
                est_qty.as_deref(),
                &intent_json,
            )
            .await?;

        self.db
            .mark_signal_processed(signal.id, &Utc::now().to_rfc3339())
            .await?;
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

    let expires_at = row
        .expires_at
        .as_deref()
        .map(|s| chrono::DateTime::parse_from_rfc3339(s).map(|d| d.with_timezone(&Utc)))
        .transpose()
        .map_err(|e| anyhow!("invalid expires_at: {e}"))?;

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
        expires_at,
    })
}

fn market_data_error_to_json(e: MarketDataError) -> serde_json::Value {
    serde_json::json!({
        "kind": "market_data_unavailable",
        "detail": e.to_string(),
    })
}

/// A structured description of what order would be submitted if we were
/// in Paper/Live mode. Recorded verbatim as JSON in `dry_run_orders` for
/// offline analysis. Deliberately loose — this is not meant to be a
/// precise Kraken/IBKR request payload, just a reconstructable intent.
#[derive(Debug, Clone, serde::Serialize)]
struct OrderIntent {
    /// Which broker this signal would have been routed to, given its sleeve.
    broker_hint: String,
    /// Best ask (for buy) or best bid (for sell), in the instrument's quote
    /// currency. Used as a conservative slippage reference. `None` if the
    /// book does not contain the relevant side.
    estimated_price: Option<Decimal>,
    /// Best-effort quantity estimate in the quote currency (usually USD).
    /// `None` when the market snapshot lacks the FX rate or price.
    /// This is an *approximation* for logging, not a tradeable size.
    estimated_quantity: Option<Decimal>,
    /// Free-form note for future reconciliation.
    note: &'static str,
}

/// Derive an order intent from an approved signal + current market state.
/// Pure function: does no I/O. Failures produce `None` in the relevant
/// fields rather than panicking, because a dry-run log should never
/// refuse to record.
fn build_order_intent(signal: &Signal, market: &MarketSnapshot) -> OrderIntent {
    let estimated_price = match signal.side {
        Side::Buy => market.order_book.asks.first().map(|l| l.price),
        Side::Sell => market.order_book.bids.first().map(|l| l.price),
    };

    // Quantity estimate: convert CHF notional to the instrument's quote
    // currency using the fx rate already required by the risk checks.
    //   notional_quote = notional_chf * fx_quote_per_chf
    //   quantity_quote_ccy = notional_quote / price
    //
    // For PI_XBTUSD the contract size is 1 USD, so quantity in contracts
    // is roughly equal to notional_usd. For PF_ contracts or spot, this
    // is only approximate. Real conversion happens when we wire the
    // Broker layer.
    let estimated_quantity = estimated_price.and_then(|price| {
        if price.is_zero() {
            return None;
        }
        let notional_quote = signal.notional_chf.checked_mul(market.fx_quote_per_chf)?;
        notional_quote.checked_div(price)
    });

    let broker_hint = broker_hint_for_sleeve(signal.sleeve).to_string();

    OrderIntent {
        broker_hint,
        estimated_price,
        estimated_quantity,
        note: "dry-run intent; not submitted to any broker",
    }
}

/// Which broker would handle this sleeve once the submission layer is
/// wired. Hardcoded here because the routing policy is part of the
/// system design, not runtime configuration.
fn broker_hint_for_sleeve(sleeve: Sleeve) -> &'static str {
    match sleeve {
        Sleeve::CryptoSpot => "kraken_spot",
        Sleeve::CryptoLeverage => "kraken_futures",
        Sleeve::Etf => "ibkr",
        Sleeve::CashYield => "ibkr",
    }
}

/// Build a Kraken Futures order from an approved CryptoLeverage signal.
///
/// # Contract semantics — read this before touching
///
/// PI_XBTUSD is a Coin-M inverse perpetual. Kraken's `size` field in the
/// sendorder payload is the **integer number of contracts**, and every
/// PI contract is worth exactly 1 USD of notional. Source:
/// <https://support.kraken.com/articles/360022829971> ("You buy 100,000
/// PI_BTCUSD contracts at $35,000. Each contract is worth $1...").
///
/// Therefore:
///   notional_usd      = notional_chf * fx_usd_per_chf
///   quantity_contracts = floor(notional_usd)   // integer contracts
///
/// This is fundamentally different from the dry-run intent, which reports
/// an approximate BTC-denominated quantity purely for operator reading.
/// Do NOT unify the two paths.
///
/// We set PostOnly + GTC and use the current best bid/ask as the limit
/// price. PostOnly guarantees maker-side execution: Kraken will reject
/// the order rather than cross the spread, which is the safer default
/// for the demo phase.
fn build_kraken_futures_order(
    signal: &Signal,
    market: &MarketSnapshot,
) -> Result<Order, serde_json::Value> {
    use serde_json::json;

    // --- FX rate -----------------------------------------------------
    let fx = market.fx_quote_per_chf;
    if fx <= Decimal::ZERO {
        return Err(json!({
            "kind": "invalid_fx_rate",
            "fx_quote_per_chf": fx.to_string(),
        }));
    }

    // --- Contract quantity (integer!) --------------------------------
    let notional_usd = match signal.notional_chf.checked_mul(fx) {
        Some(v) => v,
        None => {
            return Err(json!({
                "kind": "notional_overflow",
                "notional_chf": signal.notional_chf.to_string(),
                "fx_quote_per_chf": fx.to_string(),
            }))
        }
    };
    let quantity_contracts = notional_usd.floor();
    if quantity_contracts < Decimal::ONE {
        return Err(json!({
            "kind": "below_minimum_contract_size",
            "notional_usd": notional_usd.to_string(),
            "min_contracts": "1",
        }));
    }
    // Drop 5.5a: absolute upper bound as a defensive sanity check.
    // At 1 USD per contract and the 300-500 CHF sleeve sizing of the
    // MVP, a legitimate signal should never ask for more than a few
    // hundred contracts. A bug in the signal generator producing a
    // huge notional is one of the most dangerous failure modes we
    // can imagine (Kraken happily accepts orders up to the available
    // margin), so we hard-cap well above normal but well below
    // damaging and fail loudly. The risk-config `max_notional` check
    // is separate and more nuanced; this is the belt to that
    // suspenders.
    const MAX_CONTRACTS_PER_ORDER: u32 = 10_000;
    if quantity_contracts > Decimal::from(MAX_CONTRACTS_PER_ORDER) {
        return Err(json!({
            "kind": "above_maximum_contract_size",
            "quantity_contracts": quantity_contracts.to_string(),
            "max_contracts": MAX_CONTRACTS_PER_ORDER.to_string(),
        }));
    }

    // --- Limit price -------------------------------------------------
    //
    // For a PostOnly order we MUST stay on our own side of the book,
    // otherwise Kraken rejects with post_order_failed_because_it_would_be_filled.
    // So for a Buy we join (or sit below) the best bid; for a Sell we join
    // (or sit above) the best ask. This is the opposite of what you'd
    // do for an aggressive IOC/market fill.
    let limit_price = match signal.side {
        Side::Buy => market.order_book.bids.first().map(|l| l.price),
        Side::Sell => market.order_book.asks.first().map(|l| l.price),
    };
    let limit_price = match limit_price {
        Some(p) if p > Decimal::ZERO => p,
        _ => {
            return Err(json!({
                "kind": "no_limit_price",
                "reason": "empty or invalid side of the order book",
            }))
        }
    };

    let now = Utc::now();
    Ok(Order {
        id: 0, // local id is assigned by SQLite on insert
        signal_id: Some(signal.id),
        broker: BrokerKind::KrakenFutures,
        broker_order_id: None,
        // CV-1: deterministic client order id. `rt-s{signal_id}` is
        // stable across submit retries (broker dedupes on this key)
        // and lets fill_reconciler attach orphan fills — fills whose
        // broker_order_id was never persisted because the REST
        // response was lost to a network timeout.
        cli_ord_id: Some(format!("rt-s{}", signal.id)),
        instrument: Instrument {
            symbol: signal.instrument_symbol.clone(),
            broker: BrokerKind::KrakenFutures,
            asset_class: AssetClass::CryptoPerp,
            // PI contracts are integer-quantity; tick size is 0.5 USD on
            // PI_XBTUSD as of 2026. These are set here as hints and are
            // not used by the submit path (Kraken enforces them server-side).
            min_order_size: Decimal::ONE,
            tick_size: Decimal::new(5, 1),
        },
        side: signal.side,
        order_type: OrderType::PostOnly,
        time_in_force: TimeInForce::Gtc,
        quantity: quantity_contracts,
        limit_price: Some(limit_price),
        stop_price: None,
        status: OrderStatus::PendingSubmission,
        filled_quantity: Decimal::ZERO,
        avg_fill_price: None,
        fees_paid: Decimal::ZERO,
        created_at: now,
        updated_at: now,
        error_message: None,
    })
}

fn order_status_to_string(status: OrderStatus) -> String {
    match status {
        OrderStatus::PendingSubmission => "pending_submission",
        OrderStatus::Submitted => "submitted",
        OrderStatus::Acknowledged => "acknowledged",
        OrderStatus::PartiallyFilled => "partially_filled",
        OrderStatus::Filled => "filled",
        OrderStatus::Cancelled => "cancelled",
        OrderStatus::Rejected => "rejected",
        OrderStatus::Failed => "failed",
    }
    .to_string()
}

/// Classify a broker error into a stable machine-readable kind for the
/// rejection_reason JSON. Keeps the SQL filter surface small and stable.
fn broker_error_kind(err: &ExecutionError) -> &'static str {
    match err {
        ExecutionError::Rejected(_) => "broker_rejected",
        ExecutionError::InvalidResponse(_) => "broker_invalid_response",
        ExecutionError::Network(_) => "broker_network",
        ExecutionError::Timeout { .. } => "broker_timeout",
        ExecutionError::RateLimited { .. } => "broker_rate_limited",
        ExecutionError::Authentication(_) => "broker_authentication",
        ExecutionError::InsufficientMargin(_) => "broker_insufficient_margin",
        ExecutionError::Unsupported(_) => "broker_unsupported",
    }
}

/// Serialize an enum via serde and strip the surrounding quotes to get the
/// snake_case form used in SQLite columns. Preserves exact parity with
/// how the same enums round-trip through the JSON column elsewhere.
fn enum_to_snake_case<T: serde::Serialize>(value: &T) -> anyhow::Result<String> {
    let v = serde_json::to_string(value)?;
    Ok(v.trim_matches('"').to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rt_core::market_data::{OrderBookLevel, OrderBookSnapshot};
    use rt_core::signal::SignalType;

    fn sample_signal(side: Side, sleeve: Sleeve, notional_chf: Decimal) -> Signal {
        Signal {
            id: 42,
            created_at: Utc::now(),
            instrument_symbol: "PI_XBTUSD".to_string(),
            side,
            signal_type: SignalType::TrendEntry,
            sleeve,
            notional_chf,
            leverage: Decimal::from(2),
            metadata: None,
            status: SignalStatus::Pending,
            processed_at: None,
            rejection_reason: None,
            expires_at: None,
        }
    }

    fn sample_market(
        best_bid: Decimal,
        best_ask: Decimal,
        fx_quote_per_chf: Decimal,
    ) -> MarketSnapshot {
        MarketSnapshot {
            instrument_symbol: "PI_XBTUSD".to_string(),
            order_book: OrderBookSnapshot {
                bids: vec![OrderBookLevel {
                    price: best_bid,
                    quantity: Decimal::from(10),
                }],
                asks: vec![OrderBookLevel {
                    price: best_ask,
                    quantity: Decimal::from(10),
                }],
                timestamp: Utc::now(),
            },
            last_price: best_ask,
            last_trade_at: Utc::now(),
            atr_absolute: Decimal::from(500),
            atr_pct: Decimal::new(2, 2), // 0.02 = 2%
            funding_rate_per_8h: Some(Decimal::new(1, 4)),
            fx_quote_per_chf,
        }
    }

    #[test]
    fn buy_intent_uses_best_ask() {
        let signal = sample_signal(Side::Buy, Sleeve::CryptoLeverage, Decimal::from(300));
        let market = sample_market(
            Decimal::from(50_000),      // bid
            Decimal::from(50_010),      // ask
            Decimal::new(110, 2),       // 1 CHF = 1.10 USD
        );
        let intent = build_order_intent(&signal, &market);
        assert_eq!(intent.estimated_price, Some(Decimal::from(50_010)));
        assert_eq!(intent.broker_hint, "kraken_futures");
    }

    #[test]
    fn sell_intent_uses_best_bid() {
        let signal = sample_signal(Side::Sell, Sleeve::CryptoLeverage, Decimal::from(300));
        let market = sample_market(
            Decimal::from(50_000),
            Decimal::from(50_010),
            Decimal::new(110, 2),
        );
        let intent = build_order_intent(&signal, &market);
        assert_eq!(intent.estimated_price, Some(Decimal::from(50_000)));
    }

    #[test]
    fn quantity_approximates_notional_usd_over_price() {
        // 300 CHF * 1.10 USD/CHF = 330 USD notional.
        // 330 USD / 50_010 USD per BTC ≈ 0.006598 BTC.
        let signal = sample_signal(Side::Buy, Sleeve::CryptoLeverage, Decimal::from(300));
        let market = sample_market(
            Decimal::from(50_000),
            Decimal::from(50_010),
            Decimal::new(110, 2),
        );
        let intent = build_order_intent(&signal, &market);
        let q = intent.estimated_quantity.expect("quantity should be set");
        // Allow small rounding; assert the first 5 decimal places.
        let expected = Decimal::new(6598, 6); // 0.006598
        let diff = (q - expected).abs();
        assert!(diff < Decimal::new(1, 4), "got {q}, expected ~{expected}");
    }

    #[test]
    fn empty_book_yields_none_price() {
        let signal = sample_signal(Side::Buy, Sleeve::CryptoSpot, Decimal::from(300));
        let mut market = sample_market(
            Decimal::from(50_000),
            Decimal::from(50_010),
            Decimal::from(1),
        );
        market.order_book.asks.clear();
        let intent = build_order_intent(&signal, &market);
        assert_eq!(intent.estimated_price, None);
        assert_eq!(intent.estimated_quantity, None);
        assert_eq!(intent.broker_hint, "kraken_spot");
    }

    #[test]
    fn sleeve_to_broker_hint_is_exhaustive() {
        assert_eq!(broker_hint_for_sleeve(Sleeve::CryptoSpot), "kraken_spot");
        assert_eq!(
            broker_hint_for_sleeve(Sleeve::CryptoLeverage),
            "kraken_futures"
        );
        assert_eq!(broker_hint_for_sleeve(Sleeve::Etf), "ibkr");
        assert_eq!(broker_hint_for_sleeve(Sleeve::CashYield), "ibkr");
    }

    #[test]
    fn enum_to_snake_case_strips_quotes() {
        assert_eq!(enum_to_snake_case(&Side::Buy).unwrap(), "buy");
        assert_eq!(
            enum_to_snake_case(&Sleeve::CryptoLeverage).unwrap(),
            "crypto_leverage"
        );
    }
}
