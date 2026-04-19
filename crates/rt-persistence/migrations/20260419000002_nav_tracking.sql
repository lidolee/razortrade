-- Migration 2: NAV-per-unit drawdown methodology + unrealised P&L tracking.
--
-- Motivation: see crates/rt-core/src/portfolio.rs module docs.
-- The previous drawdown calculation using absolute CHF was mathematically
-- invalid in the presence of capital flows. This migration introduces the
-- storage required to compute drawdown on a NAV-per-unit basis.

-- ============================================================
-- capital_flows: every deposit / withdrawal, persisted for
-- unit-accounting. Positive amount = deposit, negative = withdrawal.
-- ============================================================
CREATE TABLE IF NOT EXISTS capital_flows (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    timestamp    TEXT NOT NULL,
    amount_chf   TEXT NOT NULL,       -- signed; rust_decimal
    flow_type    TEXT NOT NULL,       -- 'deposit' | 'withdrawal'
    source       TEXT,                -- e.g. 'sepa_kraken', 'ibkr_wire'
    units_minted TEXT,                -- units minted (deposits) or burned (withdrawals)
    nav_at_flow  TEXT,                -- nav_per_unit at time of flow
    note         TEXT
);

CREATE INDEX IF NOT EXISTS idx_capital_flows_timestamp
    ON capital_flows(timestamp);

-- ============================================================
-- equity_snapshots: add unrealised-P&L, NAV, and units columns.
-- Older snapshots will have these as empty / default values.
-- ============================================================
ALTER TABLE equity_snapshots
    ADD COLUMN unrealized_pnl_leverage_chf TEXT NOT NULL DEFAULT '0';
ALTER TABLE equity_snapshots
    ADD COLUMN nav_per_unit TEXT NOT NULL DEFAULT '1';
ALTER TABLE equity_snapshots
    ADD COLUMN nav_hwm_per_unit TEXT NOT NULL DEFAULT '1';
ALTER TABLE equity_snapshots
    ADD COLUMN total_units TEXT NOT NULL DEFAULT '1';
