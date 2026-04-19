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
    pub async fn pending_signals(&self, limit: i64) -> Result<Vec<SignalRow>> {
        let rows = sqlx::query_as::<_, SignalRow>(
            r#"
            SELECT id, created_at, instrument_symbol, side, signal_type, sleeve,
                   notional_chf, leverage, metadata_json, status, processed_at,
                   rejection_reason
            FROM signals
            WHERE status = 'pending'
            ORDER BY created_at ASC
            LIMIT ?
            "#,
        )
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
    ) -> Result<i64> {
        let result = sqlx::query(
            r#"
            INSERT INTO orders
                (signal_id, broker, instrument_symbol, side,
                 order_type, time_in_force, quantity,
                 limit_price, stop_price, status,
                 created_at, updated_at)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 'pending_submission', ?, ?)
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
        .execute(&self.pool)
        .await?;
        Ok(result.last_insert_rowid())
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
