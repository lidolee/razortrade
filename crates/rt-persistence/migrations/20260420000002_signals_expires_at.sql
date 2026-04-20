-- Drop 18 OF-2: optional hard expiry for signals.
-- The Python writer sets expires_at = now + 30min. signal_processor
-- rejects a signal whose expires_at < now before any checklist work.

ALTER TABLE signals ADD COLUMN expires_at TEXT;
