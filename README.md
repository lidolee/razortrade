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

The repository ships a `deploy/daemon.toml.local.example` tuned for running
out of the repo checkout — SQLite in `./var/`, Kraken demo WS, dry-run
execution mode. To use it:

```bash
cp deploy/daemon.toml.local.example deploy/daemon.toml.local
mkdir -p ./var
export RT_DAEMON_CONFIG=$(pwd)/deploy/daemon.toml.local
RUST_LOG=info,rt_=debug,sqlx=warn cargo run -p rt-daemon
```

`daemon.toml.local` is gitignored, so you can edit it freely.

### Phase 3a: end-to-end smoke test

This is the first real proof that the wiring works. You need two
terminals.

**Terminal 1 — start the daemon:**

```bash
cd ~/Workspace/razortrade
cp deploy/daemon.toml.local.example deploy/daemon.toml.local  # once
mkdir -p ./var                                                 # once
export RT_DAEMON_CONFIG=$(pwd)/deploy/daemon.toml.local
RUST_LOG=info,rt_=debug,sqlx=warn cargo run -p rt-daemon
```

The daemon starts, creates `./var/razortrade.sqlite`, runs all migrations,
tries to connect to `wss://demo-futures.kraken.com/ws/v1`, and begins
polling the `signals` table at 1 Hz. Logs are JSON — pipe through `jq`
in another pane if you want them pretty:
`journalctl -f | jq` style doesn't apply here; for `cargo run` output
use `cargo run -p rt-daemon 2>&1 | tee >(jq -R 'fromjson? // .')`.

**Terminal 2 — inject one signal:**

```bash
cd ~/Workspace/razortrade
python3 tools/inject_test_signal.py
```

This writes exactly one row into `signals` with realistic metadata
(ATR, FX rate, leverage sleeve, BTC perpetual symbol).

**What to expect in Terminal 1 within ~1 second:**

```
signal processor ... "processing pending signals" count=1
market snapshot unavailable; signal rejected ... NoBook { symbol: "PI_XBTUSD" }
```

This rejection is the *expected* outcome: the Kraken demo WS has not
delivered an orderbook yet at the moment of the first poll. The
signal is marked rejected with a structured `market_data_unavailable`
reason. The pipeline ran end-to-end.

**Inspect the database:**

```bash
sqlite3 ./var/razortrade.sqlite "SELECT id, status, rejection_reason FROM signals;"
sqlite3 ./var/razortrade.sqlite "SELECT * FROM checklist_evaluations;"
sqlite3 ./var/razortrade.sqlite "SELECT * FROM dry_run_orders;"
```

- `signals`: one row, `status = 'rejected'`, `rejection_reason` contains
  a JSON object with `"kind": "market_data_unavailable"`.
- `checklist_evaluations`: empty — the checklist never ran because
  market data was missing.
- `dry_run_orders`: empty — no intent recorded.

If you leave the daemon running long enough for Kraken to push a book
snapshot, re-injecting a signal will get further and either land in
`checklist_evaluations` (rejection) or in `dry_run_orders` (approval).
Either way, no broker submission happens because the execution mode is
`dry_run`.

## Production deployment (Hetzner CX32, Rocky Linux 10)

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
