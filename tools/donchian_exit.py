#!/usr/bin/env python3
"""
Donchian-Exit signal generator for razortrade leverage positions.

# Was das Script macht

Liest alle offenen Crypto-Leverage-Positionen aus der lokalen SQLite,
holt die aktuelle 4h-Candle-Historie von Kraken Futures public, und
generiert ein `trend_exit` Signal pro Position sobald die Turtle-
Donchian-Exit-Bedingung erfüllt ist:

- Long-Position schliesst wenn die aktuelle Close unter das Tief der
  letzten `N_exit` Bars bricht.
- Short-Position schliesst wenn die aktuelle Close über das Hoch der
  letzten `N_exit` Bars bricht.

Das ist das klassische Turtle-Exit-Verfahren: ein engeres Trailing-
Fenster als der Entry. Donchian-Entry läuft mit 20 Bars
(`donchian_signal.py` default), Exit mit 10 Bars. Dadurch entsteht
ein selbst-trailender Stop-Loss der bei einem trendenden Markt
Gewinne laufen lässt, bei Fehlausbrüchen aber binnen 10 Bars aus
der Position raus ist.

# Duplicate-Protection

Vor dem INSERT prüft das Script ob bereits ein `trend_exit` Signal
auf die gleiche Position im Status `pending` oder `approved` wartet.
Verhindert, dass jeder 5-Minuten-Tick ein neues Exit-Signal
erzeugt.

# Ausführung

Als oneshot von einem systemd-timer alle 5 Minuten. Signal-Processor
(Rust) übernimmt die Exits danach aus der `signals`-Tabelle wie jede
andere Signal-Zeile (build_exit_order path).
"""

import argparse
import json
import sqlite3
import sys
from datetime import datetime, timedelta, timezone
from decimal import Decimal
from typing import Sequence

# Reuse candle fetching + Donchian helper from the entry script.
sys.path.insert(0, "/usr/libexec/razortrade")
from donchian_signal import fetch_candles  # noqa: E402
from indicators import donchian_channel  # noqa: E402


SLEEVE_CRYPTO_LEVERAGE = "crypto_leverage"
SIGNAL_TYPE_TREND_EXIT = "trend_exit"


def _iso_micros_z(dt: datetime) -> str:
    """Canonical ISO-8601 timestamp matching rt_core::time::canonical_iso."""
    return dt.isoformat(timespec="microseconds").replace("+00:00", "Z")


def list_open_leverage_positions(db_path: str) -> list[tuple[int, str, Decimal]]:
    conn = sqlite3.connect(db_path, timeout=30.0)
    try:
        conn.execute("PRAGMA busy_timeout = 30000")
        cur = conn.execute(
            """
            SELECT id, instrument_symbol, quantity
              FROM positions
             WHERE closed_at IS NULL
               AND sleeve = 'crypto_leverage'
               AND CAST(quantity AS REAL) != 0
            """
        )
        out = []
        for row in cur.fetchall():
            pid, sym, qty_str = row
            try:
                out.append((pid, sym, Decimal(qty_str)))
            except Exception:
                print(
                    f"[exit] skipping position {pid} on {sym}: malformed qty {qty_str!r}",
                    file=sys.stderr,
                )
        return out
    finally:
        conn.close()


def has_pending_exit_signal(db_path: str, symbol: str) -> bool:
    conn = sqlite3.connect(db_path, timeout=30.0)
    try:
        conn.execute("PRAGMA busy_timeout = 30000")
        cur = conn.execute(
            """
            SELECT 1
              FROM signals
             WHERE instrument_symbol = ?
               AND signal_type = 'trend_exit'
               AND status IN ('pending', 'approved')
             LIMIT 1
            """,
            (symbol,),
        )
        return cur.fetchone() is not None
    finally:
        conn.close()


def detect_exit(
    candles: Sequence,
    qty: Decimal,
    donchian_exit_period: int,
) -> tuple[bool, Decimal, Decimal, Decimal]:
    """Return (should_exit, close, upper_bound, lower_bound)."""
    if len(candles) < donchian_exit_period + 1:
        raise ValueError(
            f"need at least {donchian_exit_period + 1} candles for exit check; "
            f"got {len(candles)}"
        )
    highs = [c.high for c in candles]
    lows = [c.low for c in candles]
    # Prior-period: exclude last (current) bar.
    upper, lower = donchian_channel(highs[:-1], lows[:-1], donchian_exit_period)
    close = candles[-1].close
    if qty > 0:
        return (close < lower, close, upper, lower)
    else:
        return (close > upper, close, upper, lower)


def write_exit_signal(
    db_path: str,
    symbol: str,
    side: str,
    close: Decimal,
    upper: Decimal,
    lower: Decimal,
    donchian_exit_period: int,
) -> int:
    now_utc = datetime.now(timezone.utc)
    expires_at = now_utc + timedelta(minutes=30)

    metadata = {
        "strategy": "donchian_exit",
        "donchian_exit_period": donchian_exit_period,
        "close_at_signal": str(close),
        "exit_upper": str(upper),
        "exit_lower": str(lower),
    }

    # Notional + leverage sind für Exits irrelevant — Rust leitet die
    # Exit-Quantität aus der offenen Position ab (build_exit_order).
    row = {
        "created_at": _iso_micros_z(now_utc),
        "instrument_symbol": symbol,
        "side": side,
        "signal_type": SIGNAL_TYPE_TREND_EXIT,
        "sleeve": SLEEVE_CRYPTO_LEVERAGE,
        "notional_chf": "0",
        "leverage": "1",
        "metadata_json": json.dumps(metadata),
        "status": "pending",
        "expires_at": _iso_micros_z(expires_at),
    }

    conn = sqlite3.connect(db_path, timeout=30.0)
    try:
        conn.execute("PRAGMA busy_timeout = 30000")
        cur = conn.execute(
            """
            INSERT INTO signals (
                created_at, instrument_symbol, side, signal_type, sleeve,
                notional_chf, leverage, metadata_json, status, expires_at
            ) VALUES (
                :created_at, :instrument_symbol, :side, :signal_type, :sleeve,
                :notional_chf, :leverage, :metadata_json, :status, :expires_at
            )
            """,
            row,
        )
        conn.commit()
        return cur.lastrowid
    finally:
        conn.close()


def run(db_path: str, resolution: str, donchian_exit_period: int, commit: bool) -> int:
    positions = list_open_leverage_positions(db_path)
    if not positions:
        print("[exit] no open leverage positions; nothing to do")
        return 0

    for pid, symbol, qty in positions:
        if has_pending_exit_signal(db_path, symbol):
            print(f"[exit] {symbol}: exit signal already pending; skip")
            continue

        try:
            candles = fetch_candles(symbol, resolution)
        except Exception as e:
            print(f"[exit] {symbol}: candle fetch failed: {e}", file=sys.stderr)
            continue

        try:
            should_exit, close, upper, lower = detect_exit(
                candles, qty, donchian_exit_period
            )
        except ValueError as e:
            print(f"[exit] {symbol}: {e}", file=sys.stderr)
            continue

        direction = "long" if qty > 0 else "short"
        print(
            f"[exit] {symbol} {direction} qty={qty} close={close} "
            f"upper={upper} lower={lower} -> "
            f"{'EXIT' if should_exit else 'hold'}"
        )

        if not should_exit:
            continue

        side = "sell" if qty > 0 else "buy"
        if commit:
            sig_id = write_exit_signal(
                db_path, symbol, side, close, upper, lower, donchian_exit_period
            )
            print(f"[exit] {symbol}: wrote trend_exit signal id={sig_id} side={side}")
        else:
            print(f"[exit] {symbol}: would write trend_exit side={side} (dry-run)")

    return 0


def main() -> int:
    p = argparse.ArgumentParser(description="Donchian-Exit signal generator")
    p.add_argument("--db", default="/var/lib/razortrade/razortrade.sqlite")
    p.add_argument("--resolution", default="4h",
                   help="Kraken candle resolution, must match entry script")
    p.add_argument("--donchian-exit", type=int, default=10,
                   help="Donchian exit period in bars (Turtle default: 10)")
    p.add_argument("--commit", action="store_true",
                   help="Actually write signals. Omit for dry-run.")
    args = p.parse_args()
    return run(args.db, args.resolution, args.donchian_exit, args.commit)


if __name__ == "__main__":
    raise SystemExit(main())
