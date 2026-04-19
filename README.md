# razortrade

Systematic trading daemon for a hybrid crypto + ETF portfolio. Rust execution
layer, Python signal generation, SQLite as the integration seam. Built for
Rocky Linux 9 + Hetzner VPS deployment.

## Architecture in one paragraph

Python calculates strategy signals every 4h and writes them to a SQLite
table. Rust polls that table at 1 Hz, runs every signal through a hardcoded
5-point pre-trade checklist, and either submits the order to Kraken Futures
/ IBKR or records a structured rejection. A separate kill-switch supervisor
runs every 60 s and disables the leverage sleeve if either the lifetime
realized-loss budget (-1000 CHF) or the portfolio-wide drawdown (20%) is
breached. The spot-accumulation sleeves continue to operate independently.

See `docs/ARCHITECTURE.md` for the full design.

## Crate layout

```
crates/
├── rt-core            Domain types (Order, Signal, Position, MarketSnapshot, …)
├── rt-risk            Pre-trade checklist, kill-switch, ATR position sizing
├── rt-persistence     SQLite + migrations + typed repository
├── rt-execution       Broker trait definition
├── rt-kraken-futures  Kraken Futures WS + REST client (Broker impl)
└── rt-daemon          Main binary; wires everything together
```

Planned but not yet in the repo: `rt-ibkr` (Interactive Brokers Client
Portal Web API) for the ETF sleeve.

## Build

Requires Rust 1.82 (pinned in `rust-toolchain.toml`).

```bash
cargo check --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --release -p rt-daemon
```

The test suite covers:
- Every domain-type invariant (`rt-core`).
- Every pre-trade check against at least 3 scenarios (approval, rejection,
  boundary) (`rt-risk`).
- Kill-switch evaluator against all trigger paths.

There are currently **no** integration tests against real broker APIs.
Those require sandbox credentials and live in a separate repository.

## Local development

```bash
mkdir -p ./var
export RT_DAEMON_CONFIG=$(pwd)/deploy/daemon.toml.example
sed -i 's|/var/lib/razortrade/razortrade.sqlite|./var/razortrade.sqlite|' $RT_DAEMON_CONFIG
RUST_LOG=debug cargo run -p rt-daemon
```

## Production deployment (Hetzner CX32, Rocky Linux 9)

See `docs/RUNBOOK.md` (not yet written). Summary:

```bash
# 1. Build release binary on a matching-arch builder (amd64).
cargo build --release -p rt-daemon

# 2. Copy to the VPS.
scp target/release/rt-daemon razortrade@vps:/opt/razortrade/bin/
scp deploy/systemd/razortrade.service root@vps:/etc/systemd/system/
scp deploy/daemon.toml.example root@vps:/etc/razortrade/daemon.toml

# 3. On the VPS:
sudo systemctl daemon-reload
sudo systemctl enable --now razortrade
sudo journalctl -u razortrade -f
```

## Scope & non-goals

**In scope:**
- Crypto spot accumulation (VCA-style) via Kraken Spot.
- Momentum trend-following with ≤ 2x leverage via Kraken Futures.
- ETF accumulation via Interactive Brokers.
- Deterministic pre-trade risk checks with full audit trail.

**Explicitly NOT in scope:**
- High-frequency trading (latency budget is 100 ms, not 100 μs).
- ML-based signal generation. Classical indicators only for the foreseeable
  future.
- Third-party capital / OPM (other people's money).
- Non-CHF base currency.

## Configuration: sensitive material

API keys are **not** stored in `daemon.toml`. They are read from environment
variables (`RT_KRAKEN_API_KEY`, `RT_KRAKEN_API_SECRET`, `RT_IBKR_TOKEN`)
which systemd loads from `/etc/razortrade/secrets.env` with mode 0600, owned
by `razortrade:razortrade`.
