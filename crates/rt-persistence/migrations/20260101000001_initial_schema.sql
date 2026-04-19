-- razortrade initial schema.
--
-- Design notes:
--  * All timestamps are ISO-8601 strings in UTC. SQLite has no native
--    timestamp type; TEXT ISO-8601 sorts correctly lexicographically.
--  * All monetary amounts are stored as TEXT-encoded decimals. The Rust
--    layer decodes these via rust_decimal::Decimal::from_str to avoid
--    floating-point rounding errors. This is deliberate and non-negotiable.
--  * WAL mode is enabled at connection time (pragma in Rust code), not
--    here, because schema migrations should not assume a journal mode.

-- ============================================================
-- signals: Python writes, Rust polls and consumes.
-- ============================================================
CREATE TABLE IF NOT EXISTS signals (
    id                 INTEGER PRIMARY KEY AUTOINCREMENT,
    created_at         TEXT NOT NULL,
    instrument_symbol  TEXT NOT NULL,
    side               TEXT NOT NULL,                -- 'buy' | 'sell'
    signal_type        TEXT NOT NULL,                -- see SignalType enum
    sleeve             TEXT NOT NULL,                -- 'crypto_spot' | 'etf' | 'crypto_leverage' | 'cash_yield'
    notional_chf       TEXT NOT NULL,                -- rust_decimal
    leverage           TEXT NOT NULL DEFAULT '1',    -- rust_decimal
    metadata_json      TEXT,                         -- free-form strategy metadata
    status             TEXT NOT NULL DEFAULT 'pending',
    processed_at       TEXT,
    rejection_reason   TEXT                          -- JSON-encoded RejectionReason
);

CREATE INDEX IF NOT EXISTS idx_signals_status
    ON signals(status);
CREATE INDEX IF NOT EXISTS idx_signals_created_at
    ON signals(created_at);

-- ============================================================
-- orders: one row per broker-side order.
-- ============================================================
CREATE TABLE IF NOT EXISTS orders (
    id                 INTEGER PRIMARY KEY AUTOINCREMENT,
    signal_id          INTEGER REFERENCES signals(id),
    broker             TEXT NOT NULL,
    broker_order_id    TEXT,                         -- NULL until acknowledged
    instrument_symbol  TEXT NOT NULL,
    side               TEXT NOT NULL,
    order_type         TEXT NOT NULL,
    time_in_force      TEXT NOT NULL DEFAULT 'gtc',
    quantity           TEXT NOT NULL,
    limit_price        TEXT,
    stop_price         TEXT,
    status             TEXT NOT NULL,
    filled_quantity    TEXT NOT NULL DEFAULT '0',
    avg_fill_price     TEXT,
    fees_paid          TEXT NOT NULL DEFAULT '0',
    created_at         TEXT NOT NULL,
    updated_at         TEXT NOT NULL,
    error_message      TEXT
);

CREATE INDEX IF NOT EXISTS idx_orders_signal_id
    ON orders(signal_id);
CREATE INDEX IF NOT EXISTS idx_orders_status
    ON orders(status);
CREATE INDEX IF NOT EXISTS idx_orders_broker_order_id
    ON orders(broker, broker_order_id);

-- ============================================================
-- positions: materialised open positions (cache).
-- Source of truth is orders + fills; positions is rebuilt on
-- reconciliation but read-fast for hot-path risk checks.
-- ============================================================
CREATE TABLE IF NOT EXISTS positions (
    id                   INTEGER PRIMARY KEY AUTOINCREMENT,
    instrument_symbol    TEXT NOT NULL,
    broker               TEXT NOT NULL,
    sleeve               TEXT NOT NULL,
    quantity             TEXT NOT NULL,               -- signed
    avg_entry_price      TEXT NOT NULL,
    leverage             TEXT NOT NULL DEFAULT '1',
    liquidation_price    TEXT,
    opened_at            TEXT NOT NULL,
    updated_at           TEXT NOT NULL,
    closed_at            TEXT,
    realized_pnl_chf     TEXT
);

CREATE INDEX IF NOT EXISTS idx_positions_open
    ON positions(instrument_symbol, broker, closed_at);

-- ============================================================
-- equity_snapshots: periodic portfolio-value samples.
-- Used for drawdown tracking and historical plotting.
-- ============================================================
CREATE TABLE IF NOT EXISTS equity_snapshots (
    id                                INTEGER PRIMARY KEY AUTOINCREMENT,
    timestamp                         TEXT NOT NULL,
    total_equity_chf                  TEXT NOT NULL,
    crypto_spot_value_chf             TEXT NOT NULL,
    etf_value_chf                     TEXT NOT NULL,
    crypto_leverage_value_chf         TEXT NOT NULL,
    cash_chf                          TEXT NOT NULL,
    realized_pnl_leverage_lifetime    TEXT NOT NULL,
    drawdown_fraction                 TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_equity_timestamp
    ON equity_snapshots(timestamp);

-- ============================================================
-- kill_switch_events: audit trail of every kill-switch flip.
-- ============================================================
CREATE TABLE IF NOT EXISTS kill_switch_events (
    id                   INTEGER PRIMARY KEY AUTOINCREMENT,
    triggered_at         TEXT NOT NULL,
    reason_kind          TEXT NOT NULL,
    reason_detail_json   TEXT NOT NULL,
    portfolio_snapshot   TEXT NOT NULL,
    resolved_at          TEXT,
    resolved_by          TEXT,
    resolved_note        TEXT
);

CREATE INDEX IF NOT EXISTS idx_kill_switch_triggered
    ON kill_switch_events(triggered_at);
CREATE INDEX IF NOT EXISTS idx_kill_switch_unresolved
    ON kill_switch_events(resolved_at)
    WHERE resolved_at IS NULL;

-- ============================================================
-- checklist_evaluations: one row per evaluation of the
-- pre-trade checklist, for full audit.
-- ============================================================
CREATE TABLE IF NOT EXISTS checklist_evaluations (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,
    signal_id         INTEGER NOT NULL REFERENCES signals(id),
    evaluated_at      TEXT NOT NULL,
    approved          INTEGER NOT NULL,               -- 0 | 1
    outcomes_json     TEXT NOT NULL                   -- full structured result
);

CREATE INDEX IF NOT EXISTS idx_checklist_signal
    ON checklist_evaluations(signal_id);

-- ============================================================
-- audit_log: free-form structured event log.
-- Catch-all for anything not worth a dedicated table.
-- ============================================================
CREATE TABLE IF NOT EXISTS audit_log (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    timestamp    TEXT NOT NULL,
    level        TEXT NOT NULL,                       -- 'info' | 'warn' | 'error'
    event_type   TEXT NOT NULL,
    entity_type  TEXT,
    entity_id    INTEGER,
    message      TEXT NOT NULL,
    data_json    TEXT
);

CREATE INDEX IF NOT EXISTS idx_audit_timestamp
    ON audit_log(timestamp);
CREATE INDEX IF NOT EXISTS idx_audit_event_type
    ON audit_log(event_type);
