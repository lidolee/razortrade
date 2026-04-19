#!/usr/bin/env python3
"""
Technical indicators used by the razortrade signal layer.

This module contains pure, deterministic functions only. No I/O.
No network. No external dependencies. stdlib only.

Every function operates on lists of Decimal values so that numerical
behaviour matches the Rust checklist exactly (the Rust risk layer
uses rust_decimal; mixing float and Decimal in the same pipeline is
the classic source of "why does the checklist reject but the signal
generator approved" off-by-one-cent bugs).

Run the self-test:

    python3 -m unittest tools/indicators.py
"""

from __future__ import annotations

from decimal import Decimal
from typing import List, Sequence, Tuple


def donchian_channel(
    highs: Sequence[Decimal],
    lows: Sequence[Decimal],
    period: int,
) -> Tuple[Decimal, Decimal]:
    """
    Return (upper, lower) of a Donchian channel over the last `period`
    highs and lows. The upper is the max over the last `period` highs,
    the lower is the min over the last `period` lows.

    Conventional use: a Donchian breakout long entry fires when the
    most recent close exceeds the upper channel computed over the
    prior period (i.e. excluding the current bar). Callers that want
    this semantic should slice `highs[:-1]` and `lows[:-1]` before
    passing them in, to get the channel of the window that ENDS one
    bar before the bar being evaluated.

    Raises ValueError if `period` is zero or longer than the inputs.
    """
    if period <= 0:
        raise ValueError(f"period must be positive, got {period}")
    if len(highs) != len(lows):
        raise ValueError(
            f"highs and lows must be same length; got {len(highs)} vs {len(lows)}"
        )
    if len(highs) < period:
        raise ValueError(
            f"need at least {period} bars, got {len(highs)}"
        )
    window_highs = highs[-period:]
    window_lows = lows[-period:]
    return max(window_highs), min(window_lows)


def true_range(
    high: Decimal,
    low: Decimal,
    prev_close: Decimal | None,
) -> Decimal:
    """
    True Range for a single bar. If there is no previous close
    (e.g. the very first bar of a series), this degenerates to
    `high - low`.
    """
    hl = high - low
    if prev_close is None:
        return hl
    hc = abs(high - prev_close)
    lc = abs(low - prev_close)
    return max(hl, hc, lc)


def average_true_range(
    highs: Sequence[Decimal],
    lows: Sequence[Decimal],
    closes: Sequence[Decimal],
    period: int,
) -> Decimal:
    """
    Average True Range over `period` bars, using Wilder's smoothing.

    Wilder's method is the original and the one every broker / charting
    platform uses by default. The first ATR value is a simple mean of
    the first `period` TR values; each subsequent ATR is
    (prev_atr * (period-1) + current_tr) / period.

    Returns the ATR at the last bar.

    Raises ValueError if inputs are inconsistent or too short.
    """
    if period <= 0:
        raise ValueError(f"period must be positive, got {period}")
    if not (len(highs) == len(lows) == len(closes)):
        raise ValueError(
            f"highs/lows/closes must be equal length; got "
            f"{len(highs)}/{len(lows)}/{len(closes)}"
        )
    if len(highs) < period + 1:
        raise ValueError(
            f"need at least {period + 1} bars for ATR({period}), got {len(highs)}"
        )

    # Compute true-range series starting from bar 1 (needs prev close).
    tr_series: List[Decimal] = []
    for i in range(1, len(highs)):
        tr = true_range(highs[i], lows[i], closes[i - 1])
        tr_series.append(tr)

    # Seed: simple mean of the first `period` TR values.
    period_dec = Decimal(period)
    atr = sum(tr_series[:period], Decimal(0)) / period_dec

    # Wilder's smoothing for subsequent bars.
    for tr in tr_series[period:]:
        atr = (atr * (period_dec - 1) + tr) / period_dec

    return atr


# ----------------------------------------------------------------------
# Self-test
# ----------------------------------------------------------------------

if __name__ == "__main__":
    import unittest

    D = Decimal

    class TestDonchian(unittest.TestCase):
        def test_simple_window(self):
            highs = [D(10), D(12), D(15), D(13), D(14)]
            lows = [D(8), D(9), D(11), D(10), D(12)]
            upper, lower = donchian_channel(highs, lows, period=5)
            self.assertEqual(upper, D(15))
            self.assertEqual(lower, D(8))

        def test_last_window_only(self):
            # Tail window matters, head is ignored.
            highs = [D(99), D(99), D(10), D(12), D(14)]
            lows = [D(0), D(0), D(8), D(9), D(11)]
            upper, lower = donchian_channel(highs, lows, period=3)
            self.assertEqual(upper, D(14))
            self.assertEqual(lower, D(8))

        def test_breakout_semantic(self):
            # Typical breakout check: exclude the current bar from the
            # channel, then compare the current close against the channel.
            highs = [D(10), D(12), D(15), D(13), D(14), D(20)]
            lows = [D(8), D(9), D(11), D(10), D(12), D(13)]
            upper, _ = donchian_channel(highs[:-1], lows[:-1], period=5)
            self.assertEqual(upper, D(15))
            current_close = D(20)
            self.assertGreater(current_close, upper)  # breakout

        def test_period_zero_raises(self):
            with self.assertRaises(ValueError):
                donchian_channel([D(1)], [D(1)], 0)

        def test_too_short_raises(self):
            with self.assertRaises(ValueError):
                donchian_channel([D(1), D(2)], [D(1), D(2)], 5)

    class TestTrueRange(unittest.TestCase):
        def test_no_prev_close(self):
            self.assertEqual(true_range(D(12), D(10), None), D(2))

        def test_gap_up(self):
            # High 12, low 10, prev close 7 -> gap up, HC dominates
            self.assertEqual(true_range(D(12), D(10), D(7)), D(5))

        def test_gap_down(self):
            # High 10, low 7, prev close 15 -> gap down, LC dominates
            self.assertEqual(true_range(D(10), D(7), D(15)), D(8))

        def test_inside_bar(self):
            # HL = 3, HC = 1, LC = 2 -> HL wins
            self.assertEqual(true_range(D(12), D(9), D(11)), D(3))

    class TestAverageTrueRange(unittest.TestCase):
        def test_flat_bars(self):
            # All bars identical HL=2, no gaps.
            # Each TR = 2. Mean over any period = 2.
            highs = [D(10)] * 10
            lows = [D(8)] * 10
            closes = [D(9)] * 10
            atr = average_true_range(highs, lows, closes, period=5)
            self.assertEqual(atr, D(2))

        def test_wilder_seeding(self):
            # With period=3 and exactly 4 bars, Wilder's smoothing kicks
            # in once (for bar index 3). TR series from index 1..3.
            highs = [D(10), D(11), D(13), D(12)]
            lows = [D(8), D(9), D(10), D(10)]
            closes = [D(9), D(10), D(12), D(11)]
            # TR[1] = max(11-9, |11-9|, |9-9|) = 2
            # TR[2] = max(13-10, |13-10|, |10-10|) = 3
            # TR[3] = max(12-10, |12-12|, |10-12|) = 2
            # Seed ATR = mean(2, 3) = 2.5  (first `period-1` = 2 TRs? NO:
            #                              the seed is mean of first `period` TRs)
            # Wait — period is 3 but we only have 3 TR values total.
            # So the seed uses all 3: mean(2, 3, 2) = 7/3.
            # No smoothing step is applied (no extra TRs remain).
            atr = average_true_range(highs, lows, closes, period=3)
            self.assertEqual(atr, Decimal(7) / Decimal(3))

        def test_too_short_raises(self):
            with self.assertRaises(ValueError):
                # period 5 needs at least 6 bars (one for prev close)
                average_true_range(
                    [D(1)] * 4, [D(1)] * 4, [D(1)] * 4, period=5
                )

    unittest.main()
