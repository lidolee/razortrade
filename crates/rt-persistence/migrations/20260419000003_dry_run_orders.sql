-- dry_run_orders: audit trail of every order intent in DryRun mode.
--
-- Design: this table is write-only from the daemon side. Every time an
-- approved signal would result in a broker submission, but the execution
-- mode is DryRun, we record the full intent here instead. This gives us
-- a complete log of "what the daemon would have done" during the
-- validation phase, without any broker risk.
--
-- This table is intentionally separate from `orders`. The `orders` table
-- represents real broker-side state; this one represents counterfactuals.

CREATE TABLE IF NOT EXISTS dry_run_orders (
    id                   INTEGER PRIMARY KEY AUTOINCREMENT,
    signal_id            INTEGER NOT NULL REFERENCES signals(id),
    recorded_at          TEXT NOT NULL,
    instrument_symbol    TEXT NOT NULL,
    side                 TEXT NOT NULL,                -- 'buy' | 'sell'
    sleeve               TEXT NOT NULL,                -- snake_case Sleeve
    broker_hint          TEXT NOT NULL,                -- which broker would have been used
    notional_chf         TEXT NOT NULL,                -- from signal
    estimated_price      TEXT,                         -- best ask (buy) / best bid (sell)
    estimated_quantity   TEXT,                         -- derived, may be NULL
    intent_json          TEXT NOT NULL                 -- full structured intent for debugging
);

CREATE INDEX IF NOT EXISTS idx_dry_run_orders_signal
    ON dry_run_orders(signal_id);
CREATE INDEX IF NOT EXISTS idx_dry_run_orders_recorded
    ON dry_run_orders(recorded_at);
