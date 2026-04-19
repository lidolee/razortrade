//! Repository pattern: a single `Database` handle that exposes typed
//! methods for each query path the daemon needs.
//!
//! This is deliberately small. We add methods as the daemon requires them,
//! not speculatively.

use crate::models::{
    CapitalFlowRow, EquitySnapshotRow, KillSwitchEventRow, PositionRow, SignalRow,
};
use crate::Result;
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
