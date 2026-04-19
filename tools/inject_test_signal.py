#!/usr/bin/env python3
"""
Inject a single pending signal into the local razortrade SQLite DB.

This is a smoke-test tool for Phase 3a. It writes ONE row to the
`signals` table with valid structure so the daemon has something to
poll, pick up, and process.

Expected daemon behaviour when you run this while the daemon is active:

  1. Daemon polls `signals` at 1 Hz and notices the new row.
  2. Daemon tries to build a `MarketSnapshot`.
  3. If no Kraken WS book has arrived yet for PI_XBTUSD, the snapshot
     build fails with `NoBook`, and the signal is recorded as rejected
     with `kind: market_data_unavailable` in `rejection_reason`.
  4. If a book HAS arrived (demo WS is up and streaming), the pre-trade
     checklist runs. Depending on the current book shape the signal
     will either be approved (→ written to `dry_run_orders`) or rejected
     (→ written to `checklist_evaluations` with structured outcomes).

Either outcome is a passing smoke test: it proves the signal pipeline
is wired end-to-end.

Usage (from repo root):

    python3 tools/inject_test_signal.py

The script writes to ./var/razortrade.sqlite by default. Override with
the RT_DB_PATH environment variable if needed.
"""

import json
import os
import sqlite3
import sys
from datetime import datetime, timezone

DB_PATH = os.environ.get("RT_DB_PATH", "./var/razortrade.sqlite")


def utc_now_iso() -> str:
    """ISO-8601 UTC timestamp with seconds precision, same format as Rust."""
    return datetime.now(timezone.utc).replace(microsecond=0).isoformat()


def main() -> int:
    if not os.path.exists(DB_PATH):
        print(
            f"ERROR: database not found at {DB_PATH}.\n"
            "Start the daemon at least once so it creates and migrates "
            "the schema, then rerun this script.",
            file=sys.stderr,
        )
        return 1

    # Metadata matches what the daemon expects in
    # `rt-daemon/src/market_data.rs::extract_atr` and `extract_fx`.
    # Values are illustrative: BTC ATR around $500 absolute, 2% of price,
    # USD/CHF at parity-ish (1 CHF = 1.10 USD).
    metadata = {
        "atr_absolute": "500",
        "atr_pct": "0.02",
        "fx_quote_per_chf": "1.10",
        "_note": "injected by tools/inject_test_signal.py for smoke test",
    }

    row = {
        "created_at": utc_now_iso(),
        "instrument_symbol": "PI_XBTUSD",
        "side": "buy",
        # Must match rt_core::signal::SignalType variants (snake_case serde).
        "signal_type": "trend_entry",
        # Must match rt_core::instrument::Sleeve variants (snake_case serde).
        "sleeve": "crypto_leverage",
        # Decimals are stored as TEXT in the schema.
        "notional_chf": "300",
        "leverage": "2",
        "metadata_json": json.dumps(metadata),
        "status": "pending",
    }

    conn = sqlite3.connect(DB_PATH)
    try:
        cursor = conn.execute(
            """
            INSERT INTO signals
              (created_at, instrument_symbol, side, signal_type, sleeve,
               notional_chf, leverage, metadata_json, status)
            VALUES
              (:created_at, :instrument_symbol, :side, :signal_type, :sleeve,
               :notional_chf, :leverage, :metadata_json, :status)
            """,
            row,
        )
        signal_id = cursor.lastrowid
        conn.commit()
    finally:
        conn.close()

    print(f"Inserted signal id={signal_id} into {DB_PATH}")
    print("Expected behaviour in the running daemon:")
    print("  - within ~1s the daemon polls and processes this signal")
    print("  - without a Kraken WS book, it will be rejected with")
    print("    rejection_reason.kind = 'market_data_unavailable'")
    print("")
    print("Inspect results:")
    print(f"  sqlite3 {DB_PATH} 'SELECT id, status, rejection_reason FROM signals;'")
    print(f"  sqlite3 {DB_PATH} 'SELECT * FROM checklist_evaluations;'")
    print(f"  sqlite3 {DB_PATH} 'SELECT * FROM dry_run_orders;'")
    return 0


if __name__ == "__main__":
    sys.exit(main())
