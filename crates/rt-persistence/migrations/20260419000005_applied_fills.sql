-- Drop 6c: per-broker fill deduplication.
--
-- Before this migration the fill_reconciler's dedup watermark
-- (last_applied_seq) was kept in memory only. On daemon restart the
-- broker WS snapshot replays the full visible fill history, which
-- then re-entered the reconciler loop. Fills against local orders
-- would double-apply to the orders and positions tables (bad);
-- fills against orders not in the local DB would log a warning per
-- fill at every restart (noisy).
--
-- This table records every (broker, fill_id) pair the reconciler
-- has ever seen, whether or not it matched a local order. The
-- reconciler checks this table before applying a fill and records
-- it after. `applied_at` is for audit / cleanup; no UNIQUE index
-- beyond the primary key is needed since (broker, fill_id) is
-- globally unique by construction on the broker side.
CREATE TABLE IF NOT EXISTS applied_fills (
    broker     TEXT NOT NULL,
    fill_id    TEXT NOT NULL,
    applied_at TEXT NOT NULL,
    PRIMARY KEY (broker, fill_id)
);
