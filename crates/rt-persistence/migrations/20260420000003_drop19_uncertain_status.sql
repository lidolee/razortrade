-- Drop 19 — Audit-Remediation CV-A1
--
-- Neuer Order-Status `uncertain` + Signal-Cooldown `retry_after_at`.
--
-- Rationale: vor Drop 19 führt jeder ExecutionError (inkl. Timeout
-- und Network) in signal_processor.rs:402-416 zu mark_order_failed +
-- mark_signal_rejected. Bei einem REST-Timeout (10s) hat Kraken die
-- Order aber oft schon platziert. Der Orphan-Catcher in
-- fill_reconciler.rs kann den Fill via cli_ord_id an die Order
-- knüpfen, aber der Order-Status bleibt `failed` und das Signal ist
-- `rejected` — der nächste 4h-Donchian-Tick emittiert ein neues
-- Signal (Python prüft keine offenen Positionen), das bei
-- persistierendem Trend einen zweiten Submit und damit eine
-- doppelte Position auslöst.
--
-- Die Lösung:
--   1. Bei Timeout/Network: Order -> 'uncertain' statt 'failed',
--      Signal bleibt 'pending' mit retry_after_at = now + 90s.
--   2. Nach dem Cooldown macht der Processor zunächst einen REST
--      get_order_by_cli_ord_id; nur wenn Kraken die Order nicht
--      kennt, wird resubmitted.
--   3. backfill_broker_order_id (repo.rs) akzeptiert jetzt
--      Status-Transition sowohl aus 'pending_submission' als auch
--      aus 'uncertain' nach 'acknowledged'.

-- Kein Schema-Change für Status-Werte nötig — Spalte ist TEXT.
-- Aber ein neues Feld auf der signals-Tabelle für den Cooldown.

ALTER TABLE signals
    ADD COLUMN retry_after_at TEXT;

-- Index damit pending_signals() die Cooldown-Abfrage effizient
-- filtern kann. Partielle Index nur auf den Zustand den wir
-- tatsächlich abfragen: status='pending' + cooldown gesetzt.
CREATE INDEX IF NOT EXISTS idx_signals_pending_retry
    ON signals(retry_after_at)
    WHERE status = 'pending' AND retry_after_at IS NOT NULL;
