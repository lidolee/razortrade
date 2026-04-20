# Architecture

> **See also:**
> - `../README.md` — Projekt-Überblick + Quick-Reference aller Pfade
> - `OPERATIONS.md` — Deploy, Rollback, Backup, Troubleshooting
> - `SECRETS.md` — Secret-Inventar + Rotation-Prozeduren
> - `BUILDING.md` — RPM-Build
> - `../DROP_19_TODO.md` — aktuelle offene Audit-Findings
> - `../AUDIT_2026_04_20.md` — letzter Audit-Report
> - `../HANDOVER_FOR_GEMINI.md` — Auftrag für nächsten externen Audit

This document explains **why** the system is built the way it is. For
"how to run it", see `README.md`.

## Design principles

1. **Boring infrastructure, interesting strategy.** The execution layer is
   deliberately unclever. No LLMs, no generative AI, no adaptive anything.
   The cleverness lives in the Python signal layer where it can be
   iterated on without risking production state.

2. **Brain & Brawn.** Every signal from the Python layer passes through a
   hardcoded pre-trade checklist before any broker API is called. The
   checklist is the single source of truth for "are we allowed to do
   this?" and nothing can bypass it.

3. **Deterministic & auditable.** All risk decisions are pure functions
   of explicit inputs. No wall-clock reads inside checks (`now` is an
   argument). Every decision is recorded in `checklist_evaluations` for
   operator review. We should be able to replay a historical decision and
   get an identical result.

4. **Fail closed.** When the kill-switch trips, leveraged trading is
   disabled and stays disabled until a human manually resets it. There is
   no automatic re-enablement path, ever.

5. **Decimal, not float.** All monetary values are `rust_decimal::Decimal`.
   This is non-negotiable. See `rt-core` module docs.

## The pipeline

```
┌──────────────────────┐     writes      ┌───────────────────┐
│  Python signal gen   │────────────────▶│ SQLite: signals   │
│  (4h cadence)        │                 │                   │
│                      │                 │  status=pending   │
│  * Donchian breakout │                 └─────────┬─────────┘
│  * ATR calc          │                           │
│  * HMM regime (opt)  │                           │
│  * Meta in JSON      │                           │ polls at 1 Hz
│                      │                           ▼
└──────────────────────┘                ┌─────────────────────┐
                                        │ rt-daemon:          │
                                        │  signal_poller      │
                                        │                     │
                                        │  For each pending:  │
                                        │  1. fetch portfolio │
                                        │  2. fetch market    │
                                        │  3. run Checklist   │
                                        │  4. if approved:    │
                                        │       submit order  │
                                        │     else:           │
                                        │       mark rejected │
                                        └──────────┬──────────┘
                                                   │
                              ┌────────────────────┼─────────────────────┐
                              ▼                    ▼                     ▼
                      ┌──────────────┐    ┌──────────────┐     ┌──────────────┐
                      │ Kraken Spot  │    │ Kraken       │     │ IBKR Client  │
                      │ (rt-kraken-  │    │ Futures      │     │ Portal Web   │
                      │  spot, TBD)  │    │ (rt-kraken-  │     │ API (rt-ibkr,│
                      │              │    │  futures,    │     │ TBD)         │
                      │              │    │  TBD)        │     │              │
                      └──────────────┘    └──────────────┘     └──────────────┘

                     Runs independently, every 60 s:
                      ┌────────────────────────────┐
                      │ rt-daemon:                 │
                      │  kill_switch_supervisor    │
                      │                            │
                      │  if portfolio breaches any │
                      │  of the 2 guards →         │
                      │  insert kill_switch_event, │
                      │  which causes all future   │
                      │  leverage signals to be    │
                      │  rejected at the gate.     │
                      └────────────────────────────┘
```

## The 5-point checklist

Each check lives in its own file under `crates/rt-risk/src/checks/` with
its own unit tests. In declaration order:

| # | Check               | Core question                                           | Rejects when                                                 |
|---|---------------------|---------------------------------------------------------|--------------------------------------------------------------|
| 1 | `staleness`         | Is our market data fresh enough?                        | Last order-book update > 500 ms old                          |
| 2 | `spread_liquidity`  | Can we enter without material slippage?                 | Spread > 20 bps OR top-5 depth < 3× our required quantity    |
| 3 | `volatility_regime` | Is the market too turbulent for this kind of trade?     | Leverage sleeve: ATR% > 6. Any sleeve: ATR% > 15             |
| 4 | `hard_limit`        | Would the trade breach any of our hard budgets?         | Leverage cap, lifetime loss budget, drawdown, worst-case sim |
| 5 | `funding_rate`      | Is the cost of carry reasonable?                        | Adverse funding > 2 bps per 8 h (perpetuals only)            |

All five checks are always evaluated, even if an earlier one rejected.
This is deliberate: the audit log captures the full picture, not just
the first failure.

## The kill-switch

Two independent guards, checked every 60 s by a dedicated supervisor task:

1. **Lifetime realised P&L in the `CryptoLeverage` sleeve ≤ -1000 CHF.**
   The hard budget. Once tripped, the leverage sleeve is disabled
   **permanently** until manual reset. Spot sleeves continue. Note: the
   supervisor uses realised-only to avoid triggering on transient
   mark-to-market swings; see `kill_switch.rs` module docs.

2. **NAV-per-unit drawdown > 20 %.** The paper-loss guard, computed on
   NAV per unit (not on absolute CHF total) so deposits and withdrawals
   do not inflate or deflate the metric. See "Drawdown methodology" below.

The pre-trade check `HardLimitCheck` is stricter than the supervisor: it
uses `effective P&L = realised + adverse unrealised`. This deliberate
asymmetry is because admitting new risk while a losing position is open
would let the system pile on trades toward the hard stop.

Manual resets happen via a CLI tool (not yet written) that:
- prompts for an operator name and free-form note,
- records a `resolved_at` / `resolved_by` on the `kill_switch_events` row,
- does NOT reset the underlying metrics. If you reset while still in
  drawdown, the kill-switch will re-trigger on the next supervisor tick.

## Drawdown methodology (NAV-per-unit)

Traditional absolute-CHF drawdown calculations are mathematically wrong for
portfolios with ongoing capital flows. If the portfolio has an ATH of 10'000 CHF,
drops to 5'000 CHF (50% drawdown), and you then deposit 1'500 CHF, a naive
calculation would see a new ATH of 11'500 CHF and subsequent P&L would
be measured against that inflated peak — the 50% crash effectively
"disappears" from the drawdown metric.

We use the same unit-accounting scheme that regulated collective investment
schemes (mutual funds) use:

* The portfolio has `total_units` outstanding (dimensionless).
* On deposit of D CHF at current NAV-per-unit of N, we mint `D/N` new
  units; NAV-per-unit is unchanged by the deposit itself.
* On withdrawal of W CHF, we burn `W/N` units.
* NAV-per-unit moves only with market value and realised P&L.
* Drawdown = 1 − (NAV-per-unit / NAV-per-unit-HWM).

The accounting lives in the `capital_flows` SQLite table and is applied by
the Python deposit handler (not yet written). Rust reads precomputed
`nav_per_unit`, `nav_hwm_per_unit`, and `total_units` from `PortfolioState`.

## FX handling

Crypto instruments on Kraken are quoted in USD. ETFs on IBKR can be in
USD, EUR, or CHF. Our notional amounts are always in CHF. Every
`MarketSnapshot` carries an explicit `fx_quote_per_chf` field; the risk
layer uses it to convert CHF → instrument quote currency before computing
required order quantity. There is no ambient currency assumption anywhere
in the code; an invalid or zero FX rate results in a signal rejection
(`InvalidSignal`).

## Why SQLite instead of Kafka / Redis / gRPC?

At our write rate (~10 signals/hour) and ≤ 1 node of concurrency, SQLite
with WAL mode is faster and vastly simpler. The Python and Rust
processes run on the same host, so network-based IPC buys nothing. A
single SQLite file is also trivially backed up: `sqlite3 .backup` or a
btrfs/zfs snapshot gives you a consistent point-in-time copy while the
daemon is running.

If and when we need multi-node execution (we don't), the SQLite boundary
is a clean interface to replace with Redis Streams or NATS without
touching checklist or strategy code.

## Why not Postgres / TimescaleDB?

Operational complexity. A VPS with PostgreSQL running alongside the
daemon introduces backup, user, role, connection-pool, and version
concerns. SQLite has zero operational surface. At this scale the
performance of a well-indexed SQLite database on NVMe is indistinguishable
from Postgres, and the durability guarantees are identical for our needs.

## Why Python + Rust instead of one or the other?

- Python has the strategy-research ecosystem (pandas, arch, hmmlearn,
  vectorbt, matplotlib). Rewriting this in Rust is a year of work with
  zero strategy improvement.
- Rust's compile-time guarantees, lack of GC pauses, and predictable
  memory behaviour matter a lot for the 24/7 execution path. A
  Python-based execution loop with `asyncio` could easily deadlock or
  silently drop exceptions; Rust's type system makes both harder.

The SQLite boundary is the minimal-friction way to combine them.

## Why not use ML / LLMs for signal generation?

At our capital size (9_000 CHF) and trading frequency (~40 trades/month),
we generate ~240 trades over the 6-month validation window. That is
statistically insufficient to distinguish an ML model's edge from
classical indicators. The ML infrastructure cost (training, validation,
monitoring, drift detection) is pure overhead at this stage.

LLMs (including the one that wrote this file) have no place in the
execution path because they are stochastic by design. A deterministic
`if spread > 20_bps { reject }` is safer and more auditable than any
language-model-based decision.

If the system produces a validated edge after 12+ months of live
operation, adding ML features as *inputs* to the Python signal layer
(never as outputs to the execution layer) is a reasonable Phase 3.
