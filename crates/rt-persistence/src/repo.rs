//! Repository pattern: a single `Database` handle that exposes typed
//! methods for each query path the daemon needs.
//!
//! This is deliberately small. We add methods as the daemon requires them,
//! not speculatively.

use crate::models::{
    CapitalFlowRow, EquitySnapshotRow, KillSwitchEventRow, OrderFillOutcome, PositionFillOutcome,
    PositionRow, PositionTransition, SignalRow,
};
use crate::Result;
use rust_decimal::Decimal;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqliteSynchronous};
use sqlx::SqlitePool;
use std::path::Path;
use std::str::FromStr;
use std::time::Duration;
use tracing::{info, warn};

pub struct Database {
    pool: SqlitePool,
}

impl Database {
    /// Open a database at the given path, creating it if it does not exist,
    /// and run all pending migrations.
    ///
    /// Sets WAL mode and `synchronous = NORMAL` for durability without the
    /// full overhead of `FULL`. For a trading system, losing the last few
    /// ms of writes on an unclean shutdown is acceptable; in exchange we
    /// get an order of magnitude more write throughput.
    pub async fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let options = SqliteConnectOptions::from_str(&format!(
            "sqlite://{}",
            path.as_ref().display()
        ))?
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .busy_timeout(Duration::from_secs(5));

        let pool = SqlitePool::connect_with(options).await?;

        sqlx::migrate!("./migrations").run(&pool).await?;

        info!(path = %path.as_ref().display(), "database opened and migrations applied");

        Ok(Self { pool })
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// Fetch all signals awaiting processing, oldest first.
    ///
    /// Drop 19 — CV-A1: Signale mit einem gesetzten `retry_after_at`
    /// werden erst nach Ablauf des Cooldowns zurückgegeben. Das
    /// verhindert, dass ein Signal, dessen Order in `uncertain` steht,
    /// in einer Polling-Schleife re-submittet wird bevor der
    /// Fill-Reconciler Chancen hatte den Status über den Orphan-Catcher
    /// auf `acknowledged` zu heben. `now_iso` ist der aktuelle Zeitpunkt
    /// als RFC3339 — Lexikographischer Vergleich auf UTC-ISO ist safe.
    pub async fn pending_signals(
        &self,
        limit: i64,
        now_iso: &str,
    ) -> Result<Vec<SignalRow>> {
        let rows = sqlx::query_as::<_, SignalRow>(
            r#"
            SELECT id, created_at, instrument_symbol, side, signal_type, sleeve,
                   notional_chf, leverage, metadata_json, status, processed_at,
                   rejection_reason, expires_at, retry_after_at
            FROM signals
            WHERE status = 'pending'
              AND (retry_after_at IS NULL OR retry_after_at <= ?)
            ORDER BY created_at ASC
            LIMIT ?
            "#,
        )
        .bind(now_iso)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// Mark a signal as processed (checklist passed, order submitted).
    pub async fn mark_signal_processed(&self, signal_id: i64, now_iso: &str) -> Result<()> {
        sqlx::query(
            "UPDATE signals SET status = 'processed', processed_at = ? WHERE id = ?",
        )
        .bind(now_iso)
        .bind(signal_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Mark a signal as rejected by the checklist, recording the structured reason.
    pub async fn mark_signal_rejected(
        &self,
        signal_id: i64,
        now_iso: &str,
        reason_json: &str,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE signals SET status = 'rejected', processed_at = ?, rejection_reason = ? WHERE id = ?",
        )
        .bind(now_iso)
        .bind(reason_json)
        .bind(signal_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Record a checklist evaluation for audit.
    pub async fn record_checklist_evaluation(
        &self,
        signal_id: i64,
        evaluated_at_iso: &str,
        approved: bool,
        outcomes_json: &str,
    ) -> Result<()> {
        sqlx::query(
            r#"
            INSERT INTO checklist_evaluations
                (signal_id, evaluated_at, approved, outcomes_json)
            VALUES (?, ?, ?, ?)
            "#,
        )
        .bind(signal_id)
        .bind(evaluated_at_iso)
        .bind(if approved { 1_i64 } else { 0_i64 })
        .bind(outcomes_json)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Record a kill-switch trigger event.
    pub async fn record_kill_switch(
        &self,
        triggered_at_iso: &str,
        reason_kind: &str,
        reason_detail_json: &str,
        portfolio_snapshot_json: &str,
    ) -> Result<i64> {
        let result = sqlx::query(
            r#"
            INSERT INTO kill_switch_events
                (triggered_at, reason_kind, reason_detail_json, portfolio_snapshot)
            VALUES (?, ?, ?, ?)
            "#,
        )
        .bind(triggered_at_iso)
        .bind(reason_kind)
        .bind(reason_detail_json)
        .bind(portfolio_snapshot_json)
        .execute(&self.pool)
        .await?;
        Ok(result.last_insert_rowid())
    }

    /// Is the kill-switch currently active (i.e. any unresolved event)?
    pub async fn kill_switch_active(&self) -> Result<bool> {
        let row: Option<KillSwitchEventRow> = sqlx::query_as(
            r#"
            SELECT id, triggered_at, reason_kind, reason_detail_json, portfolio_snapshot,
                   resolved_at, resolved_by, resolved_note
            FROM kill_switch_events
            WHERE resolved_at IS NULL
            ORDER BY triggered_at DESC
            LIMIT 1
            "#,
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.is_some())
    }

    /// LF-1: has a HARD-TRIGGER (panic-close) kill-switch event already
    /// been recorded and is still unresolved? Used by the supervisor
    /// to avoid re-issuing panic-close on every tick once the first
    /// fires. Reason kinds starting with `hard_` are treated as hard.
    pub async fn has_active_hard_trigger(&self) -> Result<bool> {
        let row: Option<(i64,)> = sqlx::query_as(
            r#"
            SELECT id FROM kill_switch_events
             WHERE resolved_at IS NULL
               AND reason_kind LIKE 'hard_%'
             LIMIT 1
            "#,
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.is_some())
    }

    /// LF-1 panic-close support: list all open leverage positions
    /// (quantity != 0, not yet closed). Used by the hard-trigger
    /// handler to iterate positions that need market reduce-only
    /// closure. Returns (instrument_symbol, broker, sleeve, quantity).
    pub async fn list_open_leverage_positions(
        &self,
    ) -> Result<Vec<(String, String, String, String)>> {
        let rows: Vec<(String, String, String, String)> = sqlx::query_as(
            r#"
            SELECT instrument_symbol, broker, sleeve, quantity
              FROM positions
             WHERE closed_at IS NULL
               AND sleeve = 'crypto_leverage'
               AND CAST(quantity AS REAL) != 0
            "#,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// Free-form audit log entry.
    pub async fn audit(
        &self,
        timestamp_iso: &str,
        level: &str,
        event_type: &str,
        message: &str,
        data_json: Option<&str>,
    ) -> Result<()> {
        sqlx::query(
            r#"
            INSERT INTO audit_log (timestamp, level, event_type, message, data_json)
            VALUES (?, ?, ?, ?, ?)
            "#,
        )
        .bind(timestamp_iso)
        .bind(level)
        .bind(event_type)
        .bind(message)
        .bind(data_json)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Record a dry-run order intent. Called whenever an approved signal
    /// would have resulted in a broker submission but the execution mode
    /// is `DryRun`. Returns the auto-assigned `dry_run_orders.id`.
    #[allow(clippy::too_many_arguments)]
    pub async fn record_dry_run_order(
        &self,
        signal_id: i64,
        recorded_at_iso: &str,
        instrument_symbol: &str,
        side: &str,
        sleeve: &str,
        broker_hint: &str,
        notional_chf: &str,
        estimated_price: Option<&str>,
        estimated_quantity: Option<&str>,
        intent_json: &str,
    ) -> Result<i64> {
        let result = sqlx::query(
            r#"
            INSERT INTO dry_run_orders
                (signal_id, recorded_at, instrument_symbol, side, sleeve,
                 broker_hint, notional_chf, estimated_price,
                 estimated_quantity, intent_json)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(signal_id)
        .bind(recorded_at_iso)
        .bind(instrument_symbol)
        .bind(side)
        .bind(sleeve)
        .bind(broker_hint)
        .bind(notional_chf)
        .bind(estimated_price)
        .bind(estimated_quantity)
        .bind(intent_json)
        .execute(&self.pool)
        .await?;
        Ok(result.last_insert_rowid())
    }

    // ---------------------------------------------------------------
    // Order lifecycle (used by Paper/Live execution path)
    // ---------------------------------------------------------------

    /// Insert a new order in the `pending_submission` state. Returns the
    /// auto-generated local order id. The broker has not been contacted yet.
    #[allow(clippy::too_many_arguments)]
    pub async fn insert_pending_order(
        &self,
        signal_id: Option<i64>,
        broker: &str,
        instrument_symbol: &str,
        side: &str,
        order_type: &str,
        time_in_force: &str,
        quantity: &str,
        limit_price: Option<&str>,
        stop_price: Option<&str>,
        created_at_iso: &str,
        cli_ord_id: Option<&str>,
    ) -> Result<i64> {
        let result = sqlx::query(
            r#"
            INSERT INTO orders
                (signal_id, broker, instrument_symbol, side,
                 order_type, time_in_force, quantity,
                 limit_price, stop_price, status,
                 created_at, updated_at, cli_ord_id)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 'pending_submission', ?, ?, ?)
            "#,
        )
        .bind(signal_id)
        .bind(broker)
        .bind(instrument_symbol)
        .bind(side)
        .bind(order_type)
        .bind(time_in_force)
        .bind(quantity)
        .bind(limit_price)
        .bind(stop_price)
        .bind(created_at_iso)
        .bind(created_at_iso)
        .bind(cli_ord_id)
        .execute(&self.pool)
        .await?;
        Ok(result.last_insert_rowid())
    }

    /// Find an order by its `cli_ord_id`. Used by the orphan-catcher in
    /// fill_reconciler when a fill arrives for a broker_order_id we
    /// don't know but whose cli_ord_id matches a pending local order
    /// (the REST response was lost but the order did reach Kraken).
    pub async fn find_order_id_by_cli_ord_id(
        &self,
        broker: &str,
        cli_ord_id: &str,
    ) -> Result<Option<i64>> {
        let row: Option<(i64,)> = sqlx::query_as(
            r#"
            SELECT id FROM orders
             WHERE broker = ? AND cli_ord_id = ?
             LIMIT 1
            "#,
        )
        .bind(broker)
        .bind(cli_ord_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(id,)| id))
    }

    /// Backfill `broker_order_id` on an order that the daemon already
    /// knows (by cli_ord_id) but never persisted the broker id for —
    /// because the REST submit response was lost to a network error.
    /// The WS fill feed delivers the broker_order_id, closing the gap.
    ///
    /// Drop 19 — CV-A1b: the status transition now also promotes
    /// `uncertain` orders (those that the daemon explicitly flagged
    /// as "timeout, outcome unknown") into `acknowledged`, because the
    /// arrival of a fill for this cli_ord_id proves Kraken accepted
    /// the order. Without this, an `uncertain` order would keep its
    /// label forever even after the fill is correctly applied, and the
    /// dashboard / audit trail would stay inconsistent with the
    /// actual position state.
    pub async fn backfill_broker_order_id(
        &self,
        order_id: i64,
        broker_order_id: &str,
        updated_at_iso: &str,
    ) -> Result<()> {
        sqlx::query(
            r#"
            UPDATE orders
               SET broker_order_id = ?,
                   status = CASE
                       WHEN status IN ('pending_submission', 'uncertain')
                       THEN 'acknowledged' ELSE status END,
                   updated_at = ?
             WHERE id = ?
               AND broker_order_id IS NULL
            "#,
        )
        .bind(broker_order_id)
        .bind(updated_at_iso)
        .bind(order_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Update an order after the broker accepted it. Sets broker_order_id
    /// and the post-submission status returned by the broker.
    pub async fn mark_order_submitted(
        &self,
        order_id: i64,
        broker_order_id: &str,
        status: &str,
        updated_at_iso: &str,
    ) -> Result<()> {
        sqlx::query(
            r#"
            UPDATE orders
               SET broker_order_id = ?,
                   status = ?,
                   updated_at = ?
             WHERE id = ?
            "#,
        )
        .bind(broker_order_id)
        .bind(status)
        .bind(updated_at_iso)
        .bind(order_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Mark an order as failed (broker call errored out before an id was
    /// received). The broker-side state is unknown; reconciliation is
    /// required before any new submission on the same instrument.
    pub async fn mark_order_failed(
        &self,
        order_id: i64,
        error_message: &str,
        updated_at_iso: &str,
    ) -> Result<()> {
        sqlx::query(
            r#"
            UPDATE orders
               SET status = 'failed',
                   error_message = ?,
                   updated_at = ?
             WHERE id = ?
            "#,
        )
        .bind(error_message)
        .bind(updated_at_iso)
        .bind(order_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Drop 19 — CV-A1a: mark an order as `uncertain`.
    ///
    /// Semantically different from `failed`: the REST submit timed out
    /// (or hit a transient network error), so the broker-side state is
    /// genuinely unknown. Kraken may have placed the order or may not
    /// have. Subsequent reconciliation via `backfill_broker_order_id`
    /// (on orphan fill) or an explicit REST `get_order_by_cli_ord_id`
    /// lookup will resolve the state.
    ///
    /// Until then, the processor must not re-submit on this signal —
    /// signal_processor sets `retry_after_at` on the signal to gate the
    /// resubmit path.
    pub async fn mark_order_uncertain(
        &self,
        order_id: i64,
        error_message: &str,
        updated_at_iso: &str,
    ) -> Result<()> {
        sqlx::query(
            r#"
            UPDATE orders
               SET status = 'uncertain',
                   error_message = ?,
                   updated_at = ?
             WHERE id = ?
            "#,
        )
        .bind(error_message)
        .bind(updated_at_iso)
        .bind(order_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Drop 19 — CV-A1a: set a cooldown on a signal.
    ///
    /// While `retry_after_at > now`, `pending_signals` will not return
    /// the signal. After the cooldown expires, the processor may
    /// reconsider it. Signal status stays `pending` throughout so we
    /// do not confuse the cooldown with a terminal outcome.
    pub async fn set_signal_retry_after(
        &self,
        signal_id: i64,
        retry_after_iso: &str,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE signals SET retry_after_at = ? WHERE id = ?",
        )
        .bind(retry_after_iso)
        .bind(signal_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Drop 19 — CV-A1a: clear the cooldown on a signal, typically
    /// after a successful reconciliation or an explicit resubmit.
    pub async fn clear_signal_retry_after(&self, signal_id: i64) -> Result<()> {
        sqlx::query(
            "UPDATE signals SET retry_after_at = NULL WHERE id = ?",
        )
        .bind(signal_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Drop 19 — CV-A1a: fetch the pending order belonging to a signal
    /// (at most one — orders and signals are 1:1 in the current design).
    /// Used by the processor to find an `uncertain` order before
    /// deciding whether to resubmit or reconcile.
    pub async fn pending_order_for_signal(
        &self,
        signal_id: i64,
    ) -> Result<Option<(i64, String, Option<String>)>> {
        let row: Option<(i64, String, Option<String>)> = sqlx::query_as(
            r#"
            SELECT id, status, cli_ord_id
              FROM orders
             WHERE signal_id = ?
               AND status IN ('pending_submission', 'uncertain',
                              'acknowledged', 'submitted',
                              'partially_filled')
             ORDER BY id DESC
             LIMIT 1
            "#,
        )
        .bind(signal_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    /// Drop 19 CV-6: alle Orders die aus unserer Sicht noch live sind.
    /// Der periodic_reconciler vergleicht diese Liste gegen Kraken's
    /// `/openorders` um Drift zu erkennen (z.B. manuell gecancelte
    /// Orders oder verlorene Fills).
    pub async fn list_live_orders(&self) -> Result<Vec<crate::models::OrderRow>> {
        let rows: Vec<crate::models::OrderRow> = sqlx::query_as(
            r#"
            SELECT id, signal_id, broker, broker_order_id, instrument_symbol,
                   side, order_type, time_in_force, quantity,
                   limit_price, stop_price, status, filled_quantity,
                   avg_fill_price, fees_paid, created_at, updated_at,
                   error_message, cli_ord_id
              FROM orders
             WHERE status IN ('submitted', 'acknowledged', 'partially_filled',
                              'uncertain', 'pending_submission')
             ORDER BY id DESC
            "#,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// Apply a single fill from the broker feed to a tracked order.
    ///
    /// Behaviour:
    /// - Returns `Ok(None)` if no local `orders` row matches the given
    ///   `broker_order_id`. This happens when fills arrive for orders
    ///   submitted outside the daemon (e.g. manually via Kraken's web UI
    ///   during paper testing) and is not an error.
    /// - Otherwise computes the new cumulative fill state (quantity,
    ///   quantity-weighted average price, accumulated fees), advances
    ///   the status monotonically (acknowledged -> partially_filled ->
    ///   filled; never regresses), and persists both the order update
    ///   and a conservative `updated_at` timestamp in a single
    ///   transaction.
    ///
    /// The outcome also carries the order's side, broker, instrument
    /// and sleeve attribution (via join on `signals`) so the caller
    /// can key position-table updates without a second query.
    ///
    /// Weighted average price formula:
    ///   new_avg = (old_filled * old_avg + fill_qty * fill_price) / new_filled
    /// with the special case `old_filled == 0 -> new_avg = fill_price`.
    ///
    /// Idempotency is the caller's responsibility: the fill_reconciler
    /// task uses the `FillsRing::highest_seq()` watermark to ensure the
    /// same fill is never submitted twice.
    pub async fn apply_fill_to_order(
        &self,
        broker_order_id: &str,
        fill_qty: Decimal,
        fill_price: Decimal,
        fee: Decimal,
        now_iso: &str,
    ) -> Result<Option<OrderFillOutcome>> {
        let mut tx = self.pool.begin().await?;

        // Left-join on signals so we can pull sleeve in the same query.
        // Sleeve is optional because historical orders (pre-signal-table
        // reform) may not have a signal_id.
        let row: Option<(
            i64,
            String,
            String,
            String,
            String,
            String,
            Option<String>,
            String,
            String,
            Option<String>,
        )> = sqlx::query_as(
            r#"
            SELECT o.id,
                   o.broker,
                   o.instrument_symbol,
                   o.side,
                   o.quantity,
                   o.filled_quantity,
                   o.avg_fill_price,
                   o.fees_paid,
                   o.status,
                   s.sleeve
              FROM orders o
              LEFT JOIN signals s ON s.id = o.signal_id
             WHERE o.broker_order_id = ?
            "#,
        )
        .bind(broker_order_id)
        .fetch_optional(&mut *tx)
        .await?;

        let (
            order_id,
            broker,
            instrument_symbol,
            side_str,
            quantity_str,
            old_filled_str,
            old_avg_opt,
            old_fees_str,
            old_status,
            sleeve,
        ) = match row {
            Some(r) => r,
            None => {
                tx.commit().await?;
                return Ok(None);
            }
        };

        let is_buy = match side_str.as_str() {
            "buy" => true,
            "sell" => false,
            other => {
                return Err(crate::PersistenceError::InvalidEnum {
                    column: "orders.side",
                    value: other.to_string(),
                });
            }
        };

        let quantity = Decimal::from_str(&quantity_str).map_err(|e| {
            crate::PersistenceError::Decimal(format!("orders.quantity: {e}"))
        })?;
        let old_filled = Decimal::from_str(&old_filled_str).map_err(|e| {
            crate::PersistenceError::Decimal(format!("orders.filled_quantity: {e}"))
        })?;
        let old_fees = Decimal::from_str(&old_fees_str)
            .map_err(|e| crate::PersistenceError::Decimal(format!("orders.fees_paid: {e}")))?;
        let old_avg = match old_avg_opt {
            Some(s) => Some(Decimal::from_str(&s).map_err(|e| {
                crate::PersistenceError::Decimal(format!("orders.avg_fill_price: {e}"))
            })?),
            None => None,
        };

        let new_filled = old_filled + fill_qty;
        let new_avg = if old_filled.is_zero() || old_avg.is_none() {
            fill_price
        } else {
            let old_avg = old_avg.unwrap();
            ((old_filled * old_avg) + (fill_qty * fill_price)) / new_filled
        };
        let new_fees = old_fees + fee;

        // Status transition. Monotonic: once 'filled', stays 'filled'.
        let is_fully_filled = new_filled >= quantity;
        let new_status = if old_status == "filled" || is_fully_filled {
            "filled".to_string()
        } else {
            "partially_filled".to_string()
        };

        sqlx::query(
            r#"
            UPDATE orders
               SET filled_quantity = ?,
                   avg_fill_price = ?,
                   fees_paid = ?,
                   status = ?,
                   updated_at = ?
             WHERE id = ?
            "#,
        )
        .bind(new_filled.to_string())
        .bind(new_avg.to_string())
        .bind(new_fees.to_string())
        .bind(&new_status)
        .bind(now_iso)
        .bind(order_id)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;

        Ok(Some(OrderFillOutcome {
            order_id,
            sleeve,
            broker,
            instrument_symbol,
            is_buy,
            new_filled_quantity: new_filled,
            new_avg_fill_price: new_avg,
            new_fees_paid: new_fees,
            new_status,
            is_fully_filled,
        }))
    }

    /// Apply a fill to the positions table for inverse-contract
    /// instruments. Sign convention: `quantity` in `positions` is
    /// signed (positive=long, negative=short, zero=flat).
    ///
    /// State transitions handled:
    /// - **Opened**: no open row for this (instrument, broker) tuple.
    ///   Insert a new row with `quantity = ± fill_qty`, entry = fill_price.
    /// - **Added**: existing same-side position; update quantity and
    ///   quantity-weighted entry.
    /// - **Reduced**: opposite-side fill smaller than current |quantity|.
    ///   Keep entry, reduce quantity, realise PnL for the closed part.
    /// - **Closed**: opposite-side fill exactly equal to |quantity|.
    ///   Stamp `closed_at` and set quantity to zero. Realise full PnL.
    /// - **Flipped**: opposite-side fill larger than |quantity|.
    ///   Close the old row (stamp closed_at, realise PnL on the closed
    ///   qty), then open a *new* row for the residual in the opposite
    ///   direction, entry = fill_price.
    ///
    /// Inverse-contract PnL formula (BTC, for PI_XBTUSD and friends):
    ///     pnl_btc = closed_contracts * (1/entry_price - 1/exit_price)
    /// where `closed_contracts` is signed: positive for a long being
    /// closed, negative for a short being closed. The sign propagates
    /// naturally so a winning long and a winning short both produce
    /// positive pnl_btc.
    pub async fn apply_fill_to_position(
        &self,
        instrument_symbol: &str,
        broker: &str,
        sleeve: &str,
        is_buy: bool,
        fill_qty: Decimal,
        fill_price: Decimal,
        now_iso: &str,
    ) -> Result<PositionFillOutcome> {
        // Signed fill delta: Buy => +qty, Sell => -qty.
        let fill_signed = if is_buy { fill_qty } else { -fill_qty };

        let mut tx = self.pool.begin().await?;

        // Find the currently-open position for this (instrument, broker).
        // Sleeve is not a key — a position belongs to whichever sleeve
        // opened it — but we record sleeve on insert for attribution.
        // We also pull realized_pnl_btc in the same query so we don't
        // need a second round-trip to accumulate it.
        let existing: Option<(i64, String, String, String, Option<String>)> = sqlx::query_as(
            r#"
            SELECT id, quantity, avg_entry_price, sleeve, realized_pnl_btc
              FROM positions
             WHERE instrument_symbol = ?
               AND broker = ?
               AND closed_at IS NULL
             ORDER BY id DESC
             LIMIT 1
            "#,
        )
        .bind(instrument_symbol)
        .bind(broker)
        .fetch_optional(&mut *tx)
        .await?;

        let outcome = match existing {
            None => {
                // Opened
                let id: i64 = sqlx::query_scalar(
                    r#"
                    INSERT INTO positions (
                        instrument_symbol, broker, sleeve,
                        quantity, avg_entry_price, leverage, liquidation_price,
                        opened_at, updated_at, closed_at,
                        realized_pnl_chf, realized_pnl_btc
                    ) VALUES (?, ?, ?, ?, ?, '1', NULL, ?, ?, NULL, NULL, '0')
                    RETURNING id
                    "#,
                )
                .bind(instrument_symbol)
                .bind(broker)
                .bind(sleeve)
                .bind(fill_signed.to_string())
                .bind(fill_price.to_string())
                .bind(now_iso)
                .bind(now_iso)
                .fetch_one(&mut *tx)
                .await?;

                PositionFillOutcome {
                    position_id: id,
                    new_quantity: fill_signed,
                    new_avg_entry_price: Some(fill_price),
                    realized_pnl_btc: Decimal::ZERO,
                    transition: PositionTransition::Opened,
                }
            }
            Some((pos_id, old_qty_str, old_entry_str, _pos_sleeve, old_realized_btc_opt)) => {
                let old_qty = Decimal::from_str(&old_qty_str).map_err(|e| {
                    crate::PersistenceError::Decimal(format!("positions.quantity: {e}"))
                })?;
                let old_entry = Decimal::from_str(&old_entry_str).map_err(|e| {
                    crate::PersistenceError::Decimal(format!(
                        "positions.avg_entry_price: {e}"
                    ))
                })?;
                let old_realized_btc = match old_realized_btc_opt {
                    Some(s) if !s.is_empty() => Decimal::from_str(&s).map_err(|e| {
                        crate::PersistenceError::Decimal(format!(
                            "positions.realized_pnl_btc: {e}"
                        ))
                    })?,
                    _ => Decimal::ZERO,
                };

                let same_side = (old_qty.is_sign_positive() && is_buy)
                    || (old_qty.is_sign_negative() && !is_buy);

                if same_side {
                    // Added: weighted-avg entry update, qty += fill_signed.
                    let abs_old = old_qty.abs();
                    let new_qty = old_qty + fill_signed;
                    let new_abs = new_qty.abs();
                    let new_entry = ((abs_old * old_entry) + (fill_qty * fill_price)) / new_abs;

                    sqlx::query(
                        r#"
                        UPDATE positions
                           SET quantity = ?,
                               avg_entry_price = ?,
                               updated_at = ?
                         WHERE id = ?
                        "#,
                    )
                    .bind(new_qty.to_string())
                    .bind(new_entry.to_string())
                    .bind(now_iso)
                    .bind(pos_id)
                    .execute(&mut *tx)
                    .await?;

                    PositionFillOutcome {
                        position_id: pos_id,
                        new_quantity: new_qty,
                        new_avg_entry_price: Some(new_entry),
                        realized_pnl_btc: Decimal::ZERO,
                        transition: PositionTransition::Added,
                    }
                } else {
                    // Opposite side: reduce, close, or flip.
                    let abs_old = old_qty.abs();
                    let pnl_per_contract = inverse_pnl_per_contract(old_entry, fill_price);

                    if fill_qty < abs_old {
                        // Reduced. Closed portion = fill_qty contracts,
                        // sign matches old position.
                        let closed_signed = if old_qty.is_sign_positive() {
                            fill_qty
                        } else {
                            -fill_qty
                        };
                        let realized = closed_signed * pnl_per_contract;

                        let new_qty = old_qty + fill_signed;
                        

                        let new_realized_btc = old_realized_btc + realized;

                        sqlx::query(
                            r#"
                            UPDATE positions
                               SET quantity = ?,
                                   realized_pnl_btc = ?,
                                   updated_at = ?
                             WHERE id = ?
                            "#,
                        )
                        .bind(new_qty.to_string())
                        .bind(new_realized_btc.to_string())
                        .bind(now_iso)
                        .bind(pos_id)
                        .execute(&mut *tx)
                        .await?;

                        PositionFillOutcome {
                            position_id: pos_id,
                            new_quantity: new_qty,
                            new_avg_entry_price: Some(old_entry),
                            realized_pnl_btc: realized,
                            transition: PositionTransition::Reduced,
                        }
                    } else if fill_qty == abs_old {
                        // Closed exactly.
                        let realized = old_qty * pnl_per_contract;
                        

                        let new_realized_btc = old_realized_btc + realized;

                        sqlx::query(
                            r#"
                            UPDATE positions
                               SET quantity = '0',
                                   closed_at = ?,
                                   updated_at = ?,
                                   realized_pnl_btc = ?
                             WHERE id = ?
                            "#,
                        )
                        .bind(now_iso)
                        .bind(now_iso)
                        .bind(new_realized_btc.to_string())
                        .bind(pos_id)
                        .execute(&mut *tx)
                        .await?;

                        PositionFillOutcome {
                            position_id: pos_id,
                            new_quantity: Decimal::ZERO,
                            new_avg_entry_price: None,
                            realized_pnl_btc: realized,
                            transition: PositionTransition::Closed,
                        }
                    } else {
                        // Flipped: close old, open new for residual.
                        let realized = old_qty * pnl_per_contract;
                        

                        let new_realized_btc = old_realized_btc + realized;

                        sqlx::query(
                            r#"
                            UPDATE positions
                               SET quantity = '0',
                                   closed_at = ?,
                                   updated_at = ?,
                                   realized_pnl_btc = ?
                             WHERE id = ?
                            "#,
                        )
                        .bind(now_iso)
                        .bind(now_iso)
                        .bind(new_realized_btc.to_string())
                        .bind(pos_id)
                        .execute(&mut *tx)
                        .await?;

                        let residual_qty = fill_qty - abs_old;
                        let residual_signed = if is_buy { residual_qty } else { -residual_qty };

                        let new_id: i64 = sqlx::query_scalar(
                            r#"
                            INSERT INTO positions (
                                instrument_symbol, broker, sleeve,
                                quantity, avg_entry_price, leverage, liquidation_price,
                                opened_at, updated_at, closed_at,
                                realized_pnl_chf, realized_pnl_btc
                            ) VALUES (?, ?, ?, ?, ?, '1', NULL, ?, ?, NULL, NULL, '0')
                            RETURNING id
                            "#,
                        )
                        .bind(instrument_symbol)
                        .bind(broker)
                        .bind(sleeve)
                        .bind(residual_signed.to_string())
                        .bind(fill_price.to_string())
                        .bind(now_iso)
                        .bind(now_iso)
                        .fetch_one(&mut *tx)
                        .await?;

                        PositionFillOutcome {
                            position_id: new_id,
                            new_quantity: residual_signed,
                            new_avg_entry_price: Some(fill_price),
                            realized_pnl_btc: realized,
                            transition: PositionTransition::Flipped,
                        }
                    }
                }
            }
        };

        tx.commit().await?;
        Ok(outcome)
    }

    /// Check whether a fill has already been processed by the
    /// reconciler. Keyed by `(broker, fill_id)` which the broker
    /// guarantees globally unique for an account.
    ///
    /// This is the restart-resilient companion to the in-memory
    /// `FillsRing::highest_seq()` watermark: the WS snapshot on
    /// reconnect replays the full visible fill history, and without
    /// this check the reconciler would re-apply every historical
    /// fill to the orders and positions tables on every daemon start.
    pub async fn has_fill_been_applied(
        &self,
        broker: &str,
        fill_id: &str,
    ) -> Result<bool> {
        let row: Option<i64> = sqlx::query_scalar(
            r#"SELECT 1 FROM applied_fills WHERE broker = ? AND fill_id = ?"#,
        )
        .bind(broker)
        .bind(fill_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.is_some())
    }

    /// Record that a fill has been processed. Uses `INSERT OR IGNORE`
    /// so a race between two reconciler ticks (impossible by design
    /// today, but cheap insurance) cannot fail the second writer.
    ///
    /// The fill is recorded even when it matched no local order —
    /// "unknown order" is a stable terminal state, not a transient
    /// failure, so recording it prevents the warning being repeated
    /// at every subsequent restart.
    pub async fn record_fill_applied(
        &self,
        broker: &str,
        fill_id: &str,
        applied_at_iso: &str,
    ) -> Result<()> {
        sqlx::query(
            r#"
            INSERT OR IGNORE INTO applied_fills (broker, fill_id, applied_at)
            VALUES (?, ?, ?)
            "#,
        )
        .bind(broker)
        .bind(fill_id)
        .bind(applied_at_iso)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Aggregate realised PnL (in BTC, the settlement currency for
    /// inverse perpetuals) across every position in a given sleeve —
    /// open or closed. Used by the equity writer (Drop 12) to
    /// populate `equity_snapshots.realized_pnl_leverage_lifetime`
    /// without relying on SQLite's CAST-on-TEXT semantics.
    ///
    /// The column is stored as TEXT (rust_decimal via string), so we
    /// stream the rows and sum in Rust to preserve full precision.
    pub async fn sum_realized_pnl_btc_by_sleeve(&self, sleeve: &str) -> Result<Decimal> {
        let rows: Vec<(String,)> = sqlx::query_as(
            r#"SELECT realized_pnl_btc FROM positions WHERE sleeve = ?"#,
        )
        .bind(sleeve)
        .fetch_all(&self.pool)
        .await?;

        let mut total = Decimal::ZERO;
        for (raw,) in rows {
            let v = Decimal::from_str(&raw).map_err(|e| {
                crate::PersistenceError::Decimal(format!("parsing realized_pnl_btc {:?}: {}", raw, e))
            })?;
            total += v;
        }
        Ok(total)
    }

    /// Fetch the most recent equity snapshot, or `None` if none exist yet.
    pub async fn latest_equity_snapshot(&self) -> Result<Option<EquitySnapshotRow>> {
        let row = sqlx::query_as::<_, EquitySnapshotRow>(
            r#"
            SELECT id, timestamp, total_equity_chf, crypto_spot_value_chf,
                   etf_value_chf, crypto_leverage_value_chf, cash_chf,
                   realized_pnl_leverage_lifetime, drawdown_fraction,
                   unrealized_pnl_leverage_chf, nav_per_unit,
                   nav_hwm_per_unit, total_units
            FROM equity_snapshots
            ORDER BY timestamp DESC, id DESC
            LIMIT 1
            "#,
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    /// All open (not yet closed) positions, ordered by sleeve then symbol.
    pub async fn open_positions(&self) -> Result<Vec<PositionRow>> {
        let rows = sqlx::query_as::<_, PositionRow>(
            r#"
            SELECT id, instrument_symbol, broker, sleeve, quantity,
                   avg_entry_price, leverage, liquidation_price,
                   opened_at, updated_at, closed_at, realized_pnl_chf
            FROM positions
            WHERE closed_at IS NULL
            ORDER BY sleeve, instrument_symbol
            "#,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// Insert a new equity snapshot row. Caller must have already computed
    /// every field including NAV-per-unit; this method does not reconcile.
    #[allow(clippy::too_many_arguments)]
    pub async fn insert_equity_snapshot(
        &self,
        timestamp_iso: &str,
        total_equity_chf: &str,
        crypto_spot_value_chf: &str,
        etf_value_chf: &str,
        crypto_leverage_value_chf: &str,
        cash_chf: &str,
        realized_pnl_leverage_lifetime: &str,
        drawdown_fraction: &str,
        unrealized_pnl_leverage_chf: &str,
        nav_per_unit: &str,
        nav_hwm_per_unit: &str,
        total_units: &str,
    ) -> Result<i64> {
        let result = sqlx::query(
            r#"
            INSERT INTO equity_snapshots
                (timestamp, total_equity_chf, crypto_spot_value_chf,
                 etf_value_chf, crypto_leverage_value_chf, cash_chf,
                 realized_pnl_leverage_lifetime, drawdown_fraction,
                 unrealized_pnl_leverage_chf, nav_per_unit,
                 nav_hwm_per_unit, total_units)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(timestamp_iso)
        .bind(total_equity_chf)
        .bind(crypto_spot_value_chf)
        .bind(etf_value_chf)
        .bind(crypto_leverage_value_chf)
        .bind(cash_chf)
        .bind(realized_pnl_leverage_lifetime)
        .bind(drawdown_fraction)
        .bind(unrealized_pnl_leverage_chf)
        .bind(nav_per_unit)
        .bind(nav_hwm_per_unit)
        .bind(total_units)
        .execute(&self.pool)
        .await?;
        Ok(result.last_insert_rowid())
    }

    /// All capital flow rows, chronologically ordered.
    pub async fn capital_flows(&self) -> Result<Vec<CapitalFlowRow>> {
        let rows = sqlx::query_as::<_, CapitalFlowRow>(
            r#"
            SELECT id, timestamp, amount_chf, flow_type, source,
                   units_minted, nav_at_flow, note
            FROM capital_flows
            ORDER BY timestamp ASC, id ASC
            "#,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// Write a synthetic bootstrap equity snapshot for first-run initialisation.
    /// Emits a warning log so operators notice. The values represent a
    /// zero-capital starting state; real capital flows replace this once
    /// the first deposit is recorded via `capital_flows`.
    pub async fn write_bootstrap_equity_snapshot(&self, timestamp_iso: &str) -> Result<i64> {
        warn!(
            "no equity snapshots in database; writing synthetic bootstrap \
             snapshot (NAV=1, units=1, all sleeves zero). Record a capital \
             flow via the capital_flows table before relying on hard_limit \
             checks for real-money decisions."
        );
        self.insert_equity_snapshot(
            timestamp_iso,
            "0",  // total_equity_chf
            "0",  // crypto_spot_value_chf
            "0",  // etf_value_chf
            "0",  // crypto_leverage_value_chf
            "0",  // cash_chf
            "0",  // realized_pnl_leverage_lifetime
            "0",  // drawdown_fraction
            "0",  // unrealized_pnl_leverage_chf
            "1",  // nav_per_unit
            "1",  // nav_hwm_per_unit
            "1",  // total_units
        )
        .await
    }
}

// ---------------------------------------------------------------------
// Helpers for apply_fill_to_position
// ---------------------------------------------------------------------

/// PnL per contract for an inverse-perpetual close, expressed in the
/// settlement currency (BTC for PI_XBTUSD). The sign is derived from
/// the *signed* closed-contract count at the call site, so this
/// function returns the per-contract scalar only.
///
///   pnl_per_contract = 1/entry - 1/exit
///
/// For a long being closed (signed closed_contracts > 0):
///   - exit > entry  -> pnl_per_contract > 0 scaled by +N -> profit
///   - exit < entry  -> pnl_per_contract < 0 scaled by +N -> loss
///
/// For a short being closed (signed closed_contracts < 0):
///   - exit > entry  -> pnl_per_contract > 0 scaled by -N -> loss
///   - exit < entry  -> pnl_per_contract < 0 scaled by -N -> profit
fn inverse_pnl_per_contract(entry: Decimal, exit: Decimal) -> Decimal {
    let one = Decimal::ONE;
    (one / entry) - (one / exit)
}

// ============================================================
// Tests
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    /// Build an in-memory database with all migrations applied.
    /// Each test gets a fresh database — no shared state between
    /// tests and no ordering dependency.
    async fn test_db() -> Database {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("open in-memory db");
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .expect("migrations");
        Database { pool }
    }

    fn now() -> String {
        chrono::Utc::now().to_rfc3339()
    }

    fn d(n: i64) -> Decimal {
        Decimal::from(n)
    }

    // ---------- inverse_pnl_per_contract -----------------------

    #[test]
    fn pnl_is_zero_when_entry_equals_exit() {
        assert_eq!(inverse_pnl_per_contract(d(50_000), d(50_000)), Decimal::ZERO);
    }

    #[test]
    fn pnl_per_contract_is_positive_when_exit_above_entry() {
        // Per-contract scalar for a closing trade; sign is applied
        // at the call site via the signed contract count.
        let pnl = inverse_pnl_per_contract(d(50_000), d(60_000));
        assert!(pnl > Decimal::ZERO, "got {pnl}");
    }

    #[test]
    fn pnl_per_contract_is_negative_when_exit_below_entry() {
        let pnl = inverse_pnl_per_contract(d(50_000), d(40_000));
        assert!(pnl < Decimal::ZERO, "got {pnl}");
    }

    // ---------- apply_fill_to_position: state machine ----------

    #[tokio::test]
    async fn opens_new_long_from_first_buy_fill() {
        let db = test_db().await;
        let out = db
            .apply_fill_to_position(
                "PI_XBTUSD",
                "kraken_futures",
                "CryptoLeverage",
                true,
                d(100),
                d(50_000),
                &now(),
            )
            .await
            .expect("apply_fill_to_position");

        assert_eq!(out.transition, PositionTransition::Opened);
        assert_eq!(out.new_quantity, d(100));
        assert_eq!(out.new_avg_entry_price, Some(d(50_000)));
        assert_eq!(out.realized_pnl_btc, Decimal::ZERO);
    }

    #[tokio::test]
    async fn opens_new_short_from_first_sell_fill() {
        let db = test_db().await;
        let out = db
            .apply_fill_to_position(
                "PI_XBTUSD",
                "kraken_futures",
                "CryptoLeverage",
                false,
                d(100),
                d(50_000),
                &now(),
            )
            .await
            .expect("apply_fill_to_position");

        assert_eq!(out.transition, PositionTransition::Opened);
        assert_eq!(out.new_quantity, -d(100));
        assert_eq!(out.new_avg_entry_price, Some(d(50_000)));
    }

    #[tokio::test]
    async fn adds_to_long_with_weighted_average_entry() {
        let db = test_db().await;
        // Open long 100 @ 50_000
        db.apply_fill_to_position(
            "PI_XBTUSD",
            "kraken_futures",
            "CryptoLeverage",
            true,
            d(100),
            d(50_000),
            &now(),
        )
        .await
        .expect("opened");

        // Add 100 @ 60_000 → weighted avg = (100*50k + 100*60k)/200 = 55k
        let out = db
            .apply_fill_to_position(
                "PI_XBTUSD",
                "kraken_futures",
                "CryptoLeverage",
                true,
                d(100),
                d(60_000),
                &now(),
            )
            .await
            .expect("added");

        assert_eq!(out.transition, PositionTransition::Added);
        assert_eq!(out.new_quantity, d(200));
        assert_eq!(out.new_avg_entry_price, Some(d(55_000)));
        assert_eq!(out.realized_pnl_btc, Decimal::ZERO);
    }

    #[tokio::test]
    async fn adds_to_short_with_weighted_average_entry() {
        let db = test_db().await;
        // Open short 100 @ 60_000
        db.apply_fill_to_position(
            "PI_XBTUSD",
            "kraken_futures",
            "CryptoLeverage",
            false,
            d(100),
            d(60_000),
            &now(),
        )
        .await
        .expect("opened");

        // Add 100 @ 40_000 → avg entry = 50_000, qty = -200
        let out = db
            .apply_fill_to_position(
                "PI_XBTUSD",
                "kraken_futures",
                "CryptoLeverage",
                false,
                d(100),
                d(40_000),
                &now(),
            )
            .await
            .expect("added");

        assert_eq!(out.transition, PositionTransition::Added);
        assert_eq!(out.new_quantity, -d(200));
        assert_eq!(out.new_avg_entry_price, Some(d(50_000)));
    }

    #[tokio::test]
    async fn reduces_long_and_records_positive_pnl() {
        let db = test_db().await;
        db.apply_fill_to_position(
            "PI_XBTUSD",
            "kraken_futures",
            "CryptoLeverage",
            true,
            d(200),
            d(50_000),
            &now(),
        )
        .await
        .expect("opened");

        // Sell 100 @ 60_000: close half of a long at a profit.
        let out = db
            .apply_fill_to_position(
                "PI_XBTUSD",
                "kraken_futures",
                "CryptoLeverage",
                false,
                d(100),
                d(60_000),
                &now(),
            )
            .await
            .expect("reduced");

        assert_eq!(out.transition, PositionTransition::Reduced);
        assert_eq!(out.new_quantity, d(100));
        // Entry price unchanged for the surviving portion.
        assert_eq!(out.new_avg_entry_price, Some(d(50_000)));
        assert!(out.realized_pnl_btc > Decimal::ZERO, "got {}", out.realized_pnl_btc);
    }

    #[tokio::test]
    async fn reduces_short_and_records_positive_pnl() {
        let db = test_db().await;
        db.apply_fill_to_position(
            "PI_XBTUSD",
            "kraken_futures",
            "CryptoLeverage",
            false,
            d(200),
            d(60_000),
            &now(),
        )
        .await
        .expect("opened");

        // Buy 100 @ 50_000: cover half of a short at a profit.
        let out = db
            .apply_fill_to_position(
                "PI_XBTUSD",
                "kraken_futures",
                "CryptoLeverage",
                true,
                d(100),
                d(50_000),
                &now(),
            )
            .await
            .expect("reduced");

        assert_eq!(out.transition, PositionTransition::Reduced);
        assert_eq!(out.new_quantity, -d(100));
        assert_eq!(out.new_avg_entry_price, Some(d(60_000)));
        assert!(out.realized_pnl_btc > Decimal::ZERO, "got {}", out.realized_pnl_btc);
    }

    #[tokio::test]
    async fn closes_long_exactly_at_profit() {
        let db = test_db().await;
        db.apply_fill_to_position(
            "PI_XBTUSD",
            "kraken_futures",
            "CryptoLeverage",
            true,
            d(100),
            d(50_000),
            &now(),
        )
        .await
        .expect("opened");

        let out = db
            .apply_fill_to_position(
                "PI_XBTUSD",
                "kraken_futures",
                "CryptoLeverage",
                false,
                d(100),
                d(60_000),
                &now(),
            )
            .await
            .expect("closed");

        assert_eq!(out.transition, PositionTransition::Closed);
        assert_eq!(out.new_quantity, Decimal::ZERO);
        assert_eq!(out.new_avg_entry_price, None);
        assert!(out.realized_pnl_btc > Decimal::ZERO);
    }

    #[tokio::test]
    async fn short_closed_at_higher_price_records_loss() {
        let db = test_db().await;
        db.apply_fill_to_position(
            "PI_XBTUSD",
            "kraken_futures",
            "CryptoLeverage",
            false,
            d(100),
            d(50_000),
            &now(),
        )
        .await
        .expect("opened short");

        // Buy to close at a higher price than we shorted — loss.
        let out = db
            .apply_fill_to_position(
                "PI_XBTUSD",
                "kraken_futures",
                "CryptoLeverage",
                true,
                d(100),
                d(60_000),
                &now(),
            )
            .await
            .expect("closed");

        assert_eq!(out.transition, PositionTransition::Closed);
        assert_eq!(out.new_quantity, Decimal::ZERO);
        assert!(
            out.realized_pnl_btc < Decimal::ZERO,
            "got {}",
            out.realized_pnl_btc
        );
    }

    #[tokio::test]
    async fn flips_from_long_to_short_with_residual() {
        let db = test_db().await;
        db.apply_fill_to_position(
            "PI_XBTUSD",
            "kraken_futures",
            "CryptoLeverage",
            true,
            d(100),
            d(50_000),
            &now(),
        )
        .await
        .expect("opened long");

        // Sell 150 @ 60_000: closes the 100 long (+PnL), opens
        // a new short for the residual 50.
        let out = db
            .apply_fill_to_position(
                "PI_XBTUSD",
                "kraken_futures",
                "CryptoLeverage",
                false,
                d(150),
                d(60_000),
                &now(),
            )
            .await
            .expect("flipped");

        assert_eq!(out.transition, PositionTransition::Flipped);
        assert_eq!(out.new_quantity, -d(50));
        // The residual short opens at the flip price.
        assert_eq!(out.new_avg_entry_price, Some(d(60_000)));
        // PnL is recognised on the closed 100 long portion only.
        assert!(out.realized_pnl_btc > Decimal::ZERO);
    }

    #[tokio::test]
    async fn flips_from_short_to_long_with_residual() {
        let db = test_db().await;
        db.apply_fill_to_position(
            "PI_XBTUSD",
            "kraken_futures",
            "CryptoLeverage",
            false,
            d(100),
            d(60_000),
            &now(),
        )
        .await
        .expect("opened short");

        // Buy 150 @ 50_000: covers the 100 short (+PnL since we
        // are buying back cheaper), opens new long for the 50.
        let out = db
            .apply_fill_to_position(
                "PI_XBTUSD",
                "kraken_futures",
                "CryptoLeverage",
                true,
                d(150),
                d(50_000),
                &now(),
            )
            .await
            .expect("flipped");

        assert_eq!(out.transition, PositionTransition::Flipped);
        assert_eq!(out.new_quantity, d(50));
        assert_eq!(out.new_avg_entry_price, Some(d(50_000)));
        assert!(out.realized_pnl_btc > Decimal::ZERO);
    }

    // ---------- applied_fills dedup ----------------------------

    #[tokio::test]
    async fn has_fill_been_applied_is_false_for_unknown_id() {
        let db = test_db().await;
        let seen = db
            .has_fill_been_applied("kraken_futures", "nonexistent-fill")
            .await
            .expect("query");
        assert!(!seen);
    }

    #[tokio::test]
    async fn has_fill_been_applied_is_true_after_record() {
        let db = test_db().await;
        db.record_fill_applied("kraken_futures", "fill-a", &now())
            .await
            .expect("record");
        let seen = db
            .has_fill_been_applied("kraken_futures", "fill-a")
            .await
            .expect("query");
        assert!(seen);
    }

    #[tokio::test]
    async fn record_fill_applied_is_idempotent() {
        let db = test_db().await;
        let t = now();
        db.record_fill_applied("kraken_futures", "fill-b", &t)
            .await
            .expect("first record");
        // Second insert must not fail (INSERT OR IGNORE).
        db.record_fill_applied("kraken_futures", "fill-b", &t)
            .await
            .expect("second record");
    }

    #[tokio::test]
    async fn applied_fills_are_keyed_per_broker() {
        // Same fill_id under different brokers is legal — these
        // are independent identity spaces.
        let db = test_db().await;
        let t = now();
        db.record_fill_applied("kraken_futures", "shared-id", &t)
            .await
            .expect("kraken record");
        db.record_fill_applied("some_other_broker", "shared-id", &t)
            .await
            .expect("other record");

        assert!(db
            .has_fill_been_applied("kraken_futures", "shared-id")
            .await
            .expect("kraken query"));
        assert!(db
            .has_fill_been_applied("some_other_broker", "shared-id")
            .await
            .expect("other query"));
    }

    // ---------- apply_fill_to_order: Drop 10 -------------------

    /// Insert a signal row and return its id. Signals back a paper
    /// order in the tests below and provide the sleeve attribution
    /// that apply_fill_to_order reads via LEFT JOIN.
    async fn insert_test_signal(db: &Database, sleeve: &str) -> i64 {
        let t = now();
        let id: i64 = sqlx::query_scalar(
            r#"
            INSERT INTO signals
                (created_at, instrument_symbol, side, signal_type,
                 sleeve, notional_chf, leverage, status)
            VALUES (?, 'PI_XBTUSD', 'buy', 'breakout',
                    ?, '300', '1', 'pending')
            RETURNING id
            "#,
        )
        .bind(&t)
        .bind(sleeve)
        .fetch_one(&db.pool)
        .await
        .expect("insert signal");
        id
    }

    /// Insert an orders row in the acknowledged state. This is the
    /// state apply_fill_to_order expects — after the broker has
    /// assigned a broker_order_id but before any fill has arrived.
    async fn insert_test_order(
        db: &Database,
        signal_id: Option<i64>,
        side: &str,
        qty: &str,
        broker_order_id: &str,
    ) -> i64 {
        let t = now();
        let order_id = db
            .insert_pending_order(
                signal_id,
                "kraken_futures",
                "PI_XBTUSD",
                side,
                "post_only",
                "gtc",
                qty,
                Some("50000"),
                None,
                &t,
                None,
            )
            .await
            .expect("insert order");
        db.mark_order_submitted(order_id, broker_order_id, "acknowledged", &t)
            .await
            .expect("mark submitted");
        order_id
    }

    #[tokio::test]
    async fn partial_fill_updates_quantity_and_sets_partially_filled_status() {
        let db = test_db().await;
        let sid = insert_test_signal(&db, "CryptoLeverage").await;
        insert_test_order(&db, Some(sid), "buy", "100", "broker-uid-1").await;

        let outcome = db
            .apply_fill_to_order(
                "broker-uid-1",
                d(50),
                d(50_000),
                Decimal::ZERO,
                &now(),
            )
            .await
            .expect("apply")
            .expect("order must be found");

        assert_eq!(outcome.new_filled_quantity, d(50));
        assert_eq!(outcome.new_avg_fill_price, d(50_000));
        assert_eq!(outcome.new_status, "partially_filled");
        assert!(!outcome.is_fully_filled);
        assert_eq!(outcome.is_buy, true);
    }

    #[tokio::test]
    async fn fill_matching_quantity_completes_order() {
        let db = test_db().await;
        let sid = insert_test_signal(&db, "CryptoLeverage").await;
        insert_test_order(&db, Some(sid), "buy", "100", "broker-uid-2").await;

        let outcome = db
            .apply_fill_to_order("broker-uid-2", d(100), d(50_000), Decimal::ZERO, &now())
            .await
            .expect("apply")
            .expect("found");

        assert_eq!(outcome.new_filled_quantity, d(100));
        assert_eq!(outcome.new_status, "filled");
        assert!(outcome.is_fully_filled);
    }

    #[tokio::test]
    async fn two_partial_fills_accumulate_with_weighted_average_price() {
        let db = test_db().await;
        let sid = insert_test_signal(&db, "CryptoLeverage").await;
        insert_test_order(&db, Some(sid), "buy", "100", "broker-uid-3").await;

        // First fill: 50 @ 50_000
        db.apply_fill_to_order("broker-uid-3", d(50), d(50_000), Decimal::ZERO, &now())
            .await
            .expect("apply 1")
            .expect("found");

        // Second fill: 50 @ 60_000
        let outcome = db
            .apply_fill_to_order("broker-uid-3", d(50), d(60_000), Decimal::ZERO, &now())
            .await
            .expect("apply 2")
            .expect("found");

        assert_eq!(outcome.new_filled_quantity, d(100));
        // Weighted average: (50*50k + 50*60k) / 100 = 55_000
        assert_eq!(outcome.new_avg_fill_price, d(55_000));
        assert_eq!(outcome.new_status, "filled");
    }

    #[tokio::test]
    async fn fees_accumulate_across_fills() {
        let db = test_db().await;
        let sid = insert_test_signal(&db, "CryptoLeverage").await;
        insert_test_order(&db, Some(sid), "buy", "100", "broker-uid-4").await;

        // Using small explicit fees to test summation, not realism.
        db.apply_fill_to_order(
            "broker-uid-4",
            d(50),
            d(50_000),
            Decimal::from_str("0.0001").unwrap(),
            &now(),
        )
        .await
        .expect("apply 1")
        .expect("found");

        let outcome = db
            .apply_fill_to_order(
                "broker-uid-4",
                d(50),
                d(50_000),
                Decimal::from_str("0.0002").unwrap(),
                &now(),
            )
            .await
            .expect("apply 2")
            .expect("found");

        assert_eq!(outcome.new_fees_paid, Decimal::from_str("0.0003").unwrap());
    }

    #[tokio::test]
    async fn unknown_broker_order_id_returns_none() {
        let db = test_db().await;
        // No order inserted at all.
        let outcome = db
            .apply_fill_to_order(
                "never-seen-id",
                d(10),
                d(50_000),
                Decimal::ZERO,
                &now(),
            )
            .await
            .expect("apply");
        assert!(outcome.is_none());
    }

    #[tokio::test]
    async fn status_does_not_regress_after_order_is_filled() {
        let db = test_db().await;
        let sid = insert_test_signal(&db, "CryptoLeverage").await;
        insert_test_order(&db, Some(sid), "buy", "100", "broker-uid-5").await;

        // Complete the order.
        let o1 = db
            .apply_fill_to_order("broker-uid-5", d(100), d(50_000), Decimal::ZERO, &now())
            .await
            .expect("apply 1")
            .expect("found");
        assert_eq!(o1.new_status, "filled");

        // A stale late fill arrives — status must stay 'filled'.
        let o2 = db
            .apply_fill_to_order("broker-uid-5", d(5), d(50_000), Decimal::ZERO, &now())
            .await
            .expect("apply 2")
            .expect("found");
        assert_eq!(o2.new_status, "filled");
        // Quantity keeps accumulating; that's a logging fact, not a
        // correctness issue — applied_fills dedup (Drop 6c) prevents
        // the same fill being applied twice in practice.
    }

    #[tokio::test]
    async fn sleeve_propagates_from_signals_via_join() {
        let db = test_db().await;
        let sid = insert_test_signal(&db, "CryptoLeverage").await;
        insert_test_order(&db, Some(sid), "buy", "100", "broker-uid-6").await;

        let outcome = db
            .apply_fill_to_order("broker-uid-6", d(100), d(50_000), Decimal::ZERO, &now())
            .await
            .expect("apply")
            .expect("found");

        assert_eq!(outcome.sleeve.as_deref(), Some("CryptoLeverage"));
        assert_eq!(outcome.broker, "kraken_futures");
        assert_eq!(outcome.instrument_symbol, "PI_XBTUSD");
    }

    #[tokio::test]
    async fn sleeve_is_none_when_order_has_no_signal() {
        let db = test_db().await;
        insert_test_order(&db, None, "buy", "100", "broker-uid-7").await;

        let outcome = db
            .apply_fill_to_order("broker-uid-7", d(100), d(50_000), Decimal::ZERO, &now())
            .await
            .expect("apply")
            .expect("found");

        assert!(outcome.sleeve.is_none());
    }

    #[tokio::test]
    async fn is_buy_flag_reflects_order_side() {
        let db = test_db().await;
        let sid = insert_test_signal(&db, "CryptoLeverage").await;
        insert_test_order(&db, Some(sid), "sell", "100", "broker-uid-8").await;

        let outcome = db
            .apply_fill_to_order("broker-uid-8", d(100), d(50_000), Decimal::ZERO, &now())
            .await
            .expect("apply")
            .expect("found");

        assert_eq!(outcome.is_buy, false);
    }
}
