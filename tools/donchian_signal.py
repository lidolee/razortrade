#!/usr/bin/env python3
"""
Donchian-breakout signal generator for razortrade.

Pulls recent OHLC candles from the Kraken Futures public REST API,
computes a Donchian channel + ATR, and — if the latest close has broken
out of the prior-period channel — writes a TrendEntry signal to the
local SQLite database. The signal is then consumed by the Rust daemon's
pre-trade checklist exactly like a manually injected one.

Design principles:

  * stdlib only. No requests, no pandas, no numpy. urllib, sqlite3,
    decimal, argparse, json, sys, typing, datetime.

  * Decimal arithmetic throughout. Matches rust_decimal in the daemon.

  * Read-only by default. Pass --commit to actually write the signal.

  * Clear failure modes. Any API error, parse error, or missing-data
    condition exits non-zero with a structured message on stderr. No
    signal is ever written when we are uncertain.

Usage:

    # Dry run: show what would happen, no DB write
    python3 tools/donchian_signal.py

    # Write the signal if a breakout is detected
    python3 tools/donchian_signal.py --commit

    # Different symbol / resolution
    python3 tools/donchian_signal.py \\
        --symbol PI_XBTUSD --resolution 4h --donchian 20 --atr 14

The Rust daemon (dry-run mode) will pick up the signal within the next
poll interval and produce a dry_run_orders entry.

Verified against Kraken API docs at
https://docs.kraken.com/api/docs/futures-api/charts/candles
on 2026-04-19.
"""

from __future__ import annotations

import argparse
import json
import os
import sqlite3
import sys
import urllib.error
import urllib.request
from dataclasses import dataclass
from datetime import datetime, timedelta, timezone
from decimal import Decimal
from typing import List, Sequence

# Import the local indicator module. Requires running from repo root.
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from indicators import average_true_range, donchian_channel  # noqa: E402

KRAKEN_FUTURES_OHLC_URL = (
    "https://futures.kraken.com/api/charts/v1/{tick_type}/{symbol}/{resolution}"
)

# Allowed resolutions per Kraken docs. Anything else is rejected client-side
# so we don't make useless round-trips.
ALLOWED_RESOLUTIONS = {"1m", "5m", "15m", "30m", "1h", "4h", "12h", "1d", "1w"}

# Allowed signal types, must match rt_core::signal::SignalType serde names.
SIGNAL_TYPE_TREND_ENTRY = "trend_entry"

# Must match rt_core::instrument::Sleeve serde names.
SLEEVE_CRYPTO_LEVERAGE = "crypto_leverage"


@dataclass(frozen=True)
class Candle:
    time_ms: int
    open_: Decimal
    high: Decimal
    low: Decimal
    close: Decimal
    volume: Decimal


def fetch_candles(
    symbol: str,
    resolution: str,
    tick_type: str = "trade",
    timeout_s: float = 10.0,
) -> List[Candle]:
    """Fetch OHLC candles from the Kraken Futures public REST API."""
    if resolution not in ALLOWED_RESOLUTIONS:
        raise ValueError(
            f"invalid resolution {resolution!r}; must be one of {sorted(ALLOWED_RESOLUTIONS)}"
        )

    url = KRAKEN_FUTURES_OHLC_URL.format(
        tick_type=tick_type, symbol=symbol, resolution=resolution
    )
    req = urllib.request.Request(
        url, headers={"User-Agent": "razortrade/0.1 donchian-signal"}
    )
    try:
        with urllib.request.urlopen(req, timeout=timeout_s) as resp:
            status = resp.status
            payload = resp.read()
    except urllib.error.HTTPError as e:
        raise RuntimeError(f"Kraken HTTP {e.code}: {e.reason} ({url})") from e
    except urllib.error.URLError as e:
        raise RuntimeError(f"Kraken URL error: {e.reason} ({url})") from e

    if status != 200:
        raise RuntimeError(f"unexpected status {status} from {url}")

    try:
        data = json.loads(payload)
    except json.JSONDecodeError as e:
        raise RuntimeError(f"invalid JSON from Kraken: {e}") from e

    raw_candles = data.get("candles")
    if not isinstance(raw_candles, list):
        raise RuntimeError(f"missing/invalid 'candles' in response: {data!r}")

    candles: List[Candle] = []
    for i, c in enumerate(raw_candles):
        try:
            candles.append(
                Candle(
                    time_ms=int(c["time"]),
                    open_=Decimal(str(c["open"])),
                    high=Decimal(str(c["high"])),
                    low=Decimal(str(c["low"])),
                    close=Decimal(str(c["close"])),
                    volume=Decimal(str(c.get("volume", "0"))),
                )
            )
        except (KeyError, ValueError, TypeError) as e:
            raise RuntimeError(f"candle {i} malformed: {e}; raw: {c!r}") from e

    return candles


@dataclass(frozen=True)
class BreakoutSignal:
    side: str  # "buy" or "sell"
    close: Decimal
    upper: Decimal
    lower: Decimal
    atr_absolute: Decimal
    atr_pct: Decimal


def detect_breakout(
    candles: Sequence[Candle],
    donchian_period: int,
    atr_period: int,
) -> BreakoutSignal | None:
    """
    Return a BreakoutSignal if the most recent close has crossed the
    prior-period Donchian channel, otherwise None.

    The 'prior-period' semantic is intentional: we compute the channel
    over the window that ENDS one bar before the current one, then
    check whether the current close has broken out. This avoids the
    circular definition where the current bar's high/low define the
    channel it is being checked against.

    We also require more than `max(donchian, atr) + 1` candles so both
    indicators have a valid look-back.
    """
    required = max(donchian_period, atr_period) + 1
    if len(candles) < required:
        raise ValueError(
            f"need at least {required} candles for donchian={donchian_period}, "
            f"atr={atr_period}; got {len(candles)}"
        )

    highs = [c.high for c in candles]
    lows = [c.low for c in candles]
    closes = [c.close for c in candles]

    # Prior-period channel: exclude the last bar.
    prior_highs = highs[:-1]
    prior_lows = lows[:-1]
    upper, lower = donchian_channel(prior_highs, prior_lows, donchian_period)

    current_close = closes[-1]
    atr_abs = average_true_range(highs, lows, closes, atr_period)
    atr_pct = atr_abs / current_close if current_close > 0 else Decimal(0)

    if current_close > upper:
        return BreakoutSignal(
            side="buy",
            close=current_close,
            upper=upper,
            lower=lower,
            atr_absolute=atr_abs,
            atr_pct=atr_pct,
        )
    if current_close < lower:
        return BreakoutSignal(
            side="sell",
            close=current_close,
            upper=upper,
            lower=lower,
            atr_absolute=atr_abs,
            atr_pct=atr_pct,
        )
    return None


def write_signal(
    db_path: str,
    symbol: str,
    signal: BreakoutSignal,
    notional_chf: Decimal,
    leverage: Decimal,
    fx_quote_per_chf: Decimal,
    donchian_period: int,
    atr_period: int,
    resolution: str,
) -> int:
    """Insert one signal row and return its id."""
    metadata = {
        "atr_absolute": str(signal.atr_absolute),
        "atr_pct": str(signal.atr_pct),
        "fx_quote_per_chf": str(fx_quote_per_chf),
        # Strategy-side context for forensics; not read by the daemon.
        "strategy": "donchian_breakout",
        "donchian_period": donchian_period,
        "atr_period": atr_period,
        "resolution": resolution,
        "breakout_upper": str(signal.upper),
        "breakout_lower": str(signal.lower),
        "close_at_signal": str(signal.close),
    }
    # Drop 19 Part C — G1: einheitliches Timestamp-Format Python↔Rust.
    # Rust schreibt mit `to_rfc3339()` Nanosekunden-Präzision und `+00:00`
    # Suffix. Python's isoformat() ohne microseconds liefert `+00:00`
    # ohne sub-second, was lexikografische Vergleiche am Sekunden-Rand
    # bricht (z.B. `signals.retry_after_at <= ?` oder `expires_at <= now`).
    # Wir normalisieren beide Seiten auf **Mikrosekunden + Z-Suffix**:
    # `"2026-04-20T10:45:00.123456Z"`. Rust setzt dazu `SecondsFormat::Micros`
    # und true Z-Suffix; Python nutzt isoformat() mit microseconds und
    # ersetzt `+00:00` durch `Z`.
    now_utc = datetime.now(timezone.utc)
    # OF-2: signals must expire so a stale breakout is not re-tried
    # forever by the signal_processor retry loop. 30 minutes covers
    # several checklist retry cycles without letting a signal outlive
    # the market context that generated it.
    expires_at_utc = now_utc + timedelta(minutes=30)

    def _iso_micros_z(dt: datetime) -> str:
        # isoformat(timespec="microseconds") garantiert .NNNNNN, danach
        # +00:00 → Z ersetzen damit Format identisch mit Rust's
        # to_rfc3339_opts(SecondsFormat::Micros, true) ist.
        s = dt.isoformat(timespec="microseconds")
        return s.replace("+00:00", "Z")

    row = {
        "created_at": _iso_micros_z(now_utc),
        "instrument_symbol": symbol,
        "side": signal.side,
        "signal_type": SIGNAL_TYPE_TREND_ENTRY,
        "sleeve": SLEEVE_CRYPTO_LEVERAGE,
        "notional_chf": str(notional_chf),
        "leverage": str(leverage),
        "metadata_json": json.dumps(metadata),
        "status": "pending",
        "expires_at": _iso_micros_z(expires_at_utc),
    }
    # CV-3: SQLITE_BUSY on a concurrent writer is otherwise unhandled
    # and kills this script, losing the signal until the next 4h tick.
    # The Rust daemon holds short write locks (< 100ms typical); 30s
    # of patience absorbs any realistic burst including a fill-snapshot
    # replay on reconnect. `busy_timeout` is also set inside the
    # connection for belt-and-braces against driver differences.
    conn = sqlite3.connect(db_path, timeout=30.0)
    try:
        conn.execute("PRAGMA busy_timeout = 30000")
        cur = conn.execute(
            """
            INSERT INTO signals
              (created_at, instrument_symbol, side, signal_type, sleeve,
               notional_chf, leverage, metadata_json, status, expires_at)
            VALUES
              (:created_at, :instrument_symbol, :side, :signal_type, :sleeve,
               :notional_chf, :leverage, :metadata_json, :status, :expires_at)
            """,
            row,
        )
        signal_id = cur.lastrowid
        conn.commit()
    finally:
        conn.close()
    return signal_id


# ---------------------------------------------------------------------------
# Drop 19 — CV-A1c: pre-write guard against duplicate exposure.
# ---------------------------------------------------------------------------
def check_existing_exposure(
    db_path: str, symbol: str, new_side: str
) -> str | None:
    """Return a human-readable reason string if the daemon already has an
    open position or an in-flight order on `symbol` in the same direction
    as `new_side`. Return None if it is safe to write a new signal.

    `new_side` is expected to be "buy" or "sell" (as produced by
    `detect_breakout`).

    The function uses its own short-lived SQLite connection so it does
    not interfere with `write_signal`'s transaction.
    """
    if new_side not in ("buy", "sell"):
        raise ValueError(f"unexpected side {new_side!r}; want buy or sell")

    conn = sqlite3.connect(db_path, timeout=30.0)
    try:
        conn.execute("PRAGMA busy_timeout = 30000")

        # --- Open positions: sign of quantity is direction --------------
        # positions.quantity is stored as a TEXT Decimal. We compare
        # numerically via CAST. Zero-quantity rows with closed_at=NULL
        # can exist briefly after a full-reduce fill but before the
        # daemon flips closed_at; we treat those as already-closed.
        cur = conn.execute(
            """
            SELECT quantity
              FROM positions
             WHERE instrument_symbol = ?
               AND closed_at IS NULL
               AND sleeve = 'crypto_leverage'
               AND CAST(quantity AS REAL) != 0
            """,
            (symbol,),
        )
        for (qty_str,) in cur.fetchall():
            try:
                qty = Decimal(qty_str)
            except Exception:
                # Malformed row: do not trust either way. Be safe and
                # refuse to write — operator will notice.
                return (
                    f"open position row on {symbol} has malformed quantity "
                    f"{qty_str!r}; cannot safely compare direction"
                )
            is_long = qty > 0
            want_long = new_side == "buy"
            if is_long == want_long:
                return (
                    f"open {symbol} position in same direction "
                    f"(quantity={qty_str}, new_side={new_side})"
                )

        # --- In-flight orders: status not yet terminal ------------------
        cur = conn.execute(
            """
            SELECT id, side, status
              FROM orders
             WHERE instrument_symbol = ?
               AND status IN ('pending_submission', 'uncertain',
                              'acknowledged', 'submitted',
                              'partially_filled')
            """,
            (symbol,),
        )
        for (order_id, order_side, order_status) in cur.fetchall():
            if order_side == new_side:
                return (
                    f"in-flight order id={order_id} on {symbol} "
                    f"side={order_side} status={order_status}"
                )

        return None
    finally:
        conn.close()
# ---------------------------------------------------------------------------


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Donchian-breakout signal generator for razortrade",
    )
    parser.add_argument("--symbol", default="PI_XBTUSD")
    parser.add_argument(
        "--resolution",
        default="4h",
        choices=sorted(ALLOWED_RESOLUTIONS),
    )
    parser.add_argument("--donchian", type=int, default=20,
                        help="Donchian channel look-back in bars")
    parser.add_argument("--atr", type=int, default=14,
                        help="ATR look-back in bars")
    parser.add_argument("--notional", type=str, default="300",
                        help="Trade notional in CHF")
    parser.add_argument("--leverage", type=str, default="2",
                        help="Leverage multiplier (must be <= max_leverage)")
    parser.add_argument("--fx-quote-per-chf", type=str, default="1.10",
                        help="USD (or quote ccy) per 1 CHF; daemon uses "
                             "this for cross-currency risk checks")
    parser.add_argument("--db", default="./var/razortrade.sqlite",
                        help="Path to the razortrade SQLite database")
    parser.add_argument("--commit", action="store_true",
                        help="Actually write the signal to the DB. "
                             "Without this flag, the script only reports "
                             "what it would have done.")
    args = parser.parse_args(argv)

    try:
        notional_chf = Decimal(args.notional)
        leverage = Decimal(args.leverage)
        fx = Decimal(args.fx_quote_per_chf)
    except Exception as e:
        print(f"ERROR: parsing decimal argument: {e}", file=sys.stderr)
        return 2

    print(
        f"[donchian] fetching {args.resolution} candles for {args.symbol} "
        f"from Kraken Futures public API...",
        file=sys.stderr,
    )
    try:
        candles = fetch_candles(args.symbol, args.resolution)
    except Exception as e:
        print(f"ERROR: {e}", file=sys.stderr)
        return 3

    if not candles:
        print("ERROR: zero candles returned", file=sys.stderr)
        return 3

    print(
        f"[donchian] got {len(candles)} candles; last close = "
        f"{candles[-1].close} at {candles[-1].time_ms}",
        file=sys.stderr,
    )

    try:
        breakout = detect_breakout(candles, args.donchian, args.atr)
    except ValueError as e:
        print(f"ERROR: {e}", file=sys.stderr)
        return 3

    if breakout is None:
        print("[donchian] no breakout; no signal written.", file=sys.stderr)
        return 0

    print(
        f"[donchian] BREAKOUT detected: side={breakout.side} "
        f"close={breakout.close} upper={breakout.upper} "
        f"lower={breakout.lower} atr_abs={breakout.atr_absolute} "
        f"atr_pct={breakout.atr_pct}",
        file=sys.stderr,
    )

    if not args.commit:
        print(
            "[donchian] dry-run (no --commit flag); signal NOT written.",
            file=sys.stderr,
        )
        return 0

    if not os.path.exists(args.db):
        print(
            f"ERROR: database not found at {args.db}. Start the daemon "
            "at least once so it creates and migrates the schema.",
            file=sys.stderr,
        )
        return 4

    # Drop 19 — CV-A1c: pre-write guard against doubled exposure.
    #
    # The Rust side also refuses a duplicate via the DuplicatePositionCheck
    # pre-trade check, but refusing *here* keeps the signals table clean
    # (no `rejected: duplicate_position` rows cluttering the audit log
    # every 4h during a persistent-trend regime) and makes reasoning
    # about state easier: if the signal exists in the DB at all, it was
    # at least admissible at write time.
    #
    # We bail out on a match on either:
    #   * an open position (`positions.closed_at IS NULL`) on the same
    #     symbol with the same direction, OR
    #   * an in-flight order (`orders.status IN
    #       ('pending_submission','uncertain','acknowledged',
    #        'submitted','partially_filled')`)
    #     on the same symbol with the same direction.
    #
    # Direction on a position is the sign of `quantity`; on an order
    # it's the textual `side` column. Both are checked.
    try:
        dup_reason = check_existing_exposure(args.db, args.symbol, breakout.side)
    except sqlite3.Error as e:
        print(
            f"ERROR: pre-write duplicate check failed: {e}",
            file=sys.stderr,
        )
        return 5
    if dup_reason is not None:
        print(
            f"[donchian] REFUSED to write signal: {dup_reason}. "
            f"Waiting for existing exposure to resolve before next tick.",
            file=sys.stderr,
        )
        return 0

    try:
        signal_id = write_signal(
            args.db,
            args.symbol,
            breakout,
            notional_chf=notional_chf,
            leverage=leverage,
            fx_quote_per_chf=fx,
            donchian_period=args.donchian,
            atr_period=args.atr,
            resolution=args.resolution,
        )
    except sqlite3.Error as e:
        print(f"ERROR: SQLite insert failed: {e}", file=sys.stderr)
        return 5

    print(f"[donchian] wrote signal id={signal_id} to {args.db}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
