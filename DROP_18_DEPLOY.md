# Drop 18 — Audit Remediation Deploy Runbook

## Patches in this drop
- CV-1: `cli_ord_id` on orders + orphan-catcher in fill_reconciler
- CV-3: Python SQLite timeout + service Restart=on-failure
- CV-7: raw-logging on WS deserialization failure
- OF-2: signal expiry (30 min)
- LF-1: hard kill-switch (EffectivePnlBudgetExhausted → panic-close)
- LF-3: equity-writer zero-value filter

## Files changed
```
crates/rt-core/src/order.rs
crates/rt-core/src/signal.rs
crates/rt-daemon/src/equity_writer.rs
crates/rt-daemon/src/fill_reconciler.rs
crates/rt-daemon/src/main.rs
crates/rt-daemon/src/signal_processor.rs
crates/rt-kraken-futures/src/rest.rs
crates/rt-kraken-futures/src/ws.rs
crates/rt-persistence/src/models.rs
crates/rt-persistence/src/repo.rs
crates/rt-risk/src/kill_switch.rs
crates/rt-risk/src/lib.rs
crates/rt-risk/src/checks/funding_rate.rs
crates/rt-risk/src/checks/hard_limit.rs
crates/rt-risk/src/checks/notional_cap.rs
crates/rt-risk/src/checks/spread_liquidity.rs
crates/rt-risk/src/checks/staleness.rs
crates/rt-risk/src/checks/volatility.rs
packaging/razortrade-signals.service
tools/donchian_signal.py
tools/migration-drop18.sql   (NEW)
```

## Deploy steps

### 1. Extract + review
```bash
cd ~/Workspace/razortrade
tar xzf ~/Downloads/razortrade-drop18-audit-remediation.tar.gz
git status --short
git diff --stat
```

### 2. Local build check
```bash
cargo check 2>&1 | tail -30
```
Expected: clean. Fix compile errors before proceeding.

### 3. Local unit tests
```bash
cargo test --lib 2>&1 | tail -30
```
Expected: all pass.

### 4. Build release
```bash
cargo build --release
cargo generate-rpm -p crates/rt-daemon
cargo generate-rpm -p crates/rt-dashboard
ls -la target/generate-rpm/*.rpm
```

### 5. Stop daemon, migrate DB
```bash
ssh linda@178.105.6.124 'sudo systemctl stop rt-daemon.service'
scp tools/migration-drop18.sql linda@178.105.6.124:/tmp/
ssh linda@178.105.6.124 'sudo sqlite3 /var/lib/razortrade/razortrade.sqlite < /tmp/migration-drop18.sql'
```

Verify:
```bash
ssh linda@178.105.6.124 'sudo sqlite3 /var/lib/razortrade/razortrade.sqlite ".schema orders" | grep cli_ord_id && sudo sqlite3 /var/lib/razortrade/razortrade.sqlite ".schema signals" | grep expires_at'
```
Expected: both column names appear.

### 6. Deploy RPMs + python + service unit
```bash
scp target/generate-rpm/rt-daemon-*.rpm linda@178.105.6.124:/tmp/rt-daemon-drop18.rpm
scp target/generate-rpm/rt-dashboard-*.rpm linda@178.105.6.124:/tmp/rt-dashboard-drop18.rpm
scp tools/donchian_signal.py linda@178.105.6.124:/tmp/donchian_signal.py
scp packaging/razortrade-signals.service linda@178.105.6.124:/tmp/razortrade-signals.service

ssh linda@178.105.6.124 '
sudo dnf reinstall -y /tmp/rt-daemon-drop18.rpm
sudo dnf reinstall -y /tmp/rt-dashboard-drop18.rpm
sudo install -m 0755 -o root -g root /tmp/donchian_signal.py /usr/libexec/razortrade/donchian_signal.py
sudo install -m 0644 -o root -g root /tmp/razortrade-signals.service /usr/lib/systemd/system/razortrade-signals.service
sudo systemctl daemon-reload
sudo systemctl start rt-daemon.service
sudo systemctl restart rt-dashboard.service
sleep 5
systemctl is-active rt-daemon rt-dashboard razortrade-signals.timer razortrade-backup.timer caddy oauth2-proxy
'
```
Expected: 6x `active`.

### 7. Verify startup log clean
```bash
ssh linda@178.105.6.124 'sudo journalctl -u rt-daemon --since "60 seconds ago" --no-pager | grep -iE "panic|error|fatal" | head'
```
Expected: empty (warnings about "no ticker yet" in first 30s are OK).

### 8. Verify panic-close drill (optional but recommended)
Inject an artificial effective-PnL breach:
```bash
ssh linda@178.105.6.124 "sudo sqlite3 /var/lib/razortrade/razortrade.sqlite" <<'SQL'
INSERT INTO equity_snapshots
  (timestamp, total_equity_chf, crypto_spot_value_chf, etf_value_chf,
   crypto_leverage_value_chf, cash_chf, realized_pnl_leverage_lifetime,
   drawdown_fraction, unrealized_pnl_leverage_chf,
   nav_per_unit, nav_hwm_per_unit, total_units)
VALUES
  (strftime('%Y-%m-%dT%H:%M:%SZ','now'), '20000', '0', '0',
   '5000', '15000', '-800', '0', '-500',
   '1', '1', '1');
SQL

sleep 65

ssh linda@178.105.6.124 "sudo sqlite3 /var/lib/razortrade/razortrade.sqlite \"
SELECT id, reason_kind, triggered_at, resolved_at FROM kill_switch_events ORDER BY id DESC LIMIT 3;
\""
ssh linda@178.105.6.124 "sudo journalctl -u rt-daemon --since '90 seconds ago' --no-pager | grep -iE 'panic.close|kill.switch|hard' | tail -20"
```
Expected: new event with `reason_kind = hard_effective_pnl_budget`, journal shows panic-close invocation (no-op on empty positions but logged).

Cleanup:
```bash
ssh linda@178.105.6.124 "sudo sqlite3 /var/lib/razortrade/razortrade.sqlite \"
UPDATE kill_switch_events
   SET resolved_at = strftime('%Y-%m-%dT%H:%M:%SZ','now'),
       resolved_by = 'drill',
       resolved_note = 'Drop 18 panic-close drill'
 WHERE resolved_at IS NULL;
DELETE FROM equity_snapshots
 WHERE total_equity_chf = '20000'
   AND realized_pnl_leverage_lifetime = '-800';
\""
ssh linda@178.105.6.124 'sudo systemctl restart rt-daemon.service'
```

### 9. Signal expiry smoke test
```bash
ssh linda@178.105.6.124 "sudo sqlite3 /var/lib/razortrade/razortrade.sqlite \"
INSERT INTO signals (created_at, instrument_symbol, side, signal_type, sleeve,
                     notional_chf, leverage, metadata_json, status, expires_at)
VALUES (strftime('%Y-%m-%dT%H:%M:%SZ','now'),
        'PI_XBTUSD', 'buy', 'trend_entry', 'crypto_leverage',
        '300', '2',
        json_object('atr_absolute','500','atr_pct','0.01','fx_quote_per_chf','1.10'),
        'pending',
        strftime('%Y-%m-%dT%H:%M:%SZ','now','-5 minutes'));
\""

sleep 10

ssh linda@178.105.6.124 "sudo sqlite3 /var/lib/razortrade/razortrade.sqlite \"
SELECT id, status, rejection_reason FROM signals ORDER BY id DESC LIMIT 1;
\""
```
Expected: status=`rejected`, rejection_reason contains `signal_expired`.

### 10. Commit + push
```bash
cd ~/Workspace/razortrade
git add -A
git commit -m "Drop 18: audit remediation — cli_ord_id, panic-close, sqlite timeout, signal expiry, raw WS logging, equity zero-filter"
git push
```

## Rollback
If Step 6 leaves a broken daemon:
```bash
ssh linda@178.105.6.124 '
sudo systemctl stop rt-daemon.service
sudo dnf downgrade rt-daemon
sudo systemctl start rt-daemon.service
'
```
DB migration does not need rolling back — new columns are nullable and ignored by the old binary.
