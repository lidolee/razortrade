# Drop 19 — Part A Deploy Runbook

**Scope Part A:** Audit-Remediation CV-A1 (Doppel-Position) + CV-A2 (FX).
**Nicht in dieser Auslieferung:** Part B (Panic-Close Hardening), Part C (Timestamp-Normalisierung), Part D (Dashboard Turn 5). Diese kommen als separate Drops.

---

## Patches in dieser Drop

### Code-Änderungen (Rust + Python)
- `crates/rt-persistence/migrations/20260420000003_drop19_uncertain_status.sql` — NEU
- `crates/rt-persistence/src/models.rs` — `SignalRow.retry_after_at`
- `crates/rt-persistence/src/repo.rs` — `pending_signals` mit Cooldown-Filter, neue Methoden `mark_order_uncertain`, `set_signal_retry_after`, `clear_signal_retry_after`, `pending_order_for_signal`; `backfill_broker_order_id` transitioned aus `uncertain`
- `crates/rt-execution/src/lib.rs` — `Broker::get_order_by_cli_ord_id`, `OpenOrderSummary.cli_ord_id`
- `crates/rt-kraken-futures/src/messages.rs` — `OpenOrderEntry.cli_ord_id` (serde `cliOrdId`)
- `crates/rt-kraken-futures/src/rest.rs` — `get_order_by_cli_ord_id` Impl, `open_orders` populiert cli_ord_id
- `crates/rt-core/src/portfolio.rs` — `OpenPositionSummary` Typ, `PortfolioState.open_positions` Feld
- `crates/rt-daemon/src/portfolio_loader.rs` — lädt echte open_positions aus DB
- `crates/rt-daemon/src/signal_processor.rs` — Timeout-Path-Split, Uncertain-Resubmit-Pfad
- `crates/rt-risk/src/checklist.rs` — `RejectionReason::DuplicatePositionOpen` + summary-arm, DuplicatePositionCheck in `standard()`
- `crates/rt-risk/src/checks/mod.rs` — registration
- `crates/rt-risk/src/checks/duplicate_position.rs` — NEU (7 Unit-Tests)
- `crates/rt-risk/src/checks/{funding_rate,hard_limit,notional_cap,spread_liquidity,staleness,volatility}.rs` — Test-Konstruktionen um `open_positions` erweitert
- `crates/rt-risk/src/kill_switch.rs` — Test-Konstruktion erweitert
- `tools/donchian_signal.py` — `check_existing_exposure()` Pre-Write-Guard

### Config-Änderungen (Prod, keine Rebuild nötig)
- `/etc/razortrade/daemon.toml` — `usd_per_chf_fallback = "0.78"` (SNB-Referenzkurs 17.04.2026 = 0.7823)
- `/etc/systemd/system/razortrade-signals.service.d/override.conf` — `--fx-quote-per-chf 0.78`

---

## Teil 1 — SOFORT (ohne RPM-Build)

Dieser Teil fixt CV-A2 (FX-Oversizing +41%) mit reiner Config-Änderung. Das ist vor jedem Code-Deploy ausführbar und sollte unmittelbar gemacht werden.

### 1.1 daemon.toml updaten

```bash
ssh linda@178.105.6.124 'sudo cat /etc/razortrade/daemon.toml | grep -A1 "\[risk\]"'
```

Erwartet: `usd_per_chf_fallback = "1.10"` oder die Zeile fehlt (dann ist Default aus `rt-risk/lib.rs:134-137` wirksam).

Ersetzen:

```bash
ssh linda@178.105.6.124 'sudo sed -i "s/usd_per_chf_fallback = \"1.10\"/usd_per_chf_fallback = \"0.78\"/" /etc/razortrade/daemon.toml'
```

Falls die Zeile fehlt: hinzufügen im `[risk]` Abschnitt. Verifizieren:

```bash
ssh linda@178.105.6.124 'sudo grep "usd_per_chf_fallback" /etc/razortrade/daemon.toml'
```

Erwartet: `usd_per_chf_fallback = "0.78"`.

### 1.2 razortrade-signals.service Override

```bash
ssh linda@178.105.6.124 'sudo mkdir -p /etc/systemd/system/razortrade-signals.service.d'
ssh linda@178.105.6.124 'sudo tee /etc/systemd/system/razortrade-signals.service.d/override.conf' <<'EOF'
# Drop 19 CV-A2: override the --fx-quote-per-chf argument baked into
# the packaged service unit. SNB reference rate 17.04.2026: 0.7823.
# We use 0.78 (conservative round-down by ~0.3%). Update this file
# whenever the rate drifts > 2% from the live rate; see operations
# runbook.
[Service]
ExecStart=
ExecStart=/usr/bin/python3 /usr/libexec/razortrade/donchian_signal.py \
    --symbol PI_XBTUSD \
    --resolution 4h \
    --donchian 20 \
    --atr 14 \
    --notional 300 \
    --leverage 2 \
    --fx-quote-per-chf 0.78 \
    --db /var/lib/razortrade/razortrade.sqlite \
    --commit
EOF
```

### 1.3 Reload + Restart

```bash
ssh linda@178.105.6.124 '
sudo systemctl daemon-reload
sudo systemctl restart rt-daemon.service
sudo systemctl restart razortrade-signals.timer
sleep 5
systemctl is-active rt-daemon rt-dashboard razortrade-signals.timer razortrade-backup.timer caddy oauth2-proxy
systemctl cat razortrade-signals.service | grep -A1 fx-quote
'
```

Erwartet:
- 6x `active`
- ExecStart-Zeile zeigt `--fx-quote-per-chf 0.78`

### 1.4 Verify im nächsten equity_snapshot

Innerhalb von 60s (equity_writer-Tick) sollte der neue Snapshot erscheinen:

```bash
ssh linda@178.105.6.124 'sudo sqlite3 /var/lib/razortrade/razortrade.sqlite "
SELECT id, timestamp, total_equity_chf, crypto_leverage_value_chf
  FROM equity_snapshots
  ORDER BY id DESC LIMIT 3;"
'
```

Der neueste Snapshot sollte für identischen BTC-Bestand etwa **29% höher** in `total_equity_chf` sein als der vorherige (alter Divisor 1.10, neuer 0.78 → Faktor 1.10/0.78 ≈ 1.41).

Wenn kein BTC-Position offen ist, ist der Unterschied 0 (die Divisor-Änderung betrifft nur die Umrechnung des BTC-USD-Werts nach CHF).

---

## Teil 2 — Code-Deploy (Drop 19 Part A)

### 2.1 Tarball extrahieren

```bash
cd ~/Workspace/razortrade
tar xzf ~/Downloads/razortrade-drop19-partA.tar.gz
git status --short
git diff --stat
```

Erwartete geänderte Dateien: ~15 Rust + 1 SQL-Migration + 1 Python.

### 2.2 Build-Check

```bash
cargo check -p rt-daemon 2>&1 | tail -30
```

Muss clean sein. Häufige mögliche Fehler + Fix-Strategie:
- `unused import` → Warning, ok
- `mismatched types` für OpenOrderSummary/OpenPositionSummary → Claude in Session informieren, **nicht raten**
- `E0004 non-exhaustive match` → irgendwo in `RejectionReason` fehlt ein arm → Claude informieren

```bash
cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -20
cargo test --workspace 2>&1 | tail -30
```

Tests müssen alle passen, inklusive der neuen 7 duplicate_position Tests. Wenn einer fehlschlägt: Output zeigen, nicht selber debuggen.

### 2.3 Release-Build + RPM

```bash
cargo build --release -p rt-daemon 2>&1 | tail -5
cargo generate-rpm -p crates/rt-daemon
ls -la target/generate-rpm/*.rpm
```

### 2.4 Migration kopieren + daemon stoppen + Migration anwenden

Die Migration läuft auch automatisch beim Daemon-Start (idempotent via `sqlx::migrate!`). Manueller Pre-Deploy-Step zur Sicherheit, damit der Apply sauber dokumentiert ist:

```bash
scp crates/rt-persistence/migrations/20260420000003_drop19_uncertain_status.sql \
    linda@178.105.6.124:/tmp/drop19-migration.sql

ssh linda@178.105.6.124 '
sudo systemctl stop rt-daemon.service
sudo sqlite3 /var/lib/razortrade/razortrade.sqlite < /tmp/drop19-migration.sql
sudo sqlite3 /var/lib/razortrade/razortrade.sqlite ".schema signals" | grep retry_after_at
sudo sqlite3 /var/lib/razortrade/razortrade.sqlite ".indexes signals" | grep idx_signals_pending_retry
'
```

Erwartet:
- Zeile `retry_after_at TEXT` in der signals-Schema
- Index `idx_signals_pending_retry` gelistet

**Achtung:** Sollte `sqlx::migrate!` beim Daemon-Start die Migration als bereits applied ansehen (ist in unserem Setup nicht zu erwarten, da manual apply den Schema ändert aber nicht die `_sqlx_migrations` Tabelle), müsste nachgeholfen werden. Verifikation:

```bash
ssh linda@178.105.6.124 'sudo sqlite3 /var/lib/razortrade/razortrade.sqlite "SELECT * FROM _sqlx_migrations WHERE version = 20260420000003;"'
```

Falls leer (Row fehlt): das ist ok — sqlx läuft die Migration beim ersten Start erneut, das ist idempotent weil der Apply-Text mit `ADD COLUMN` fehlschlägt wenn die Spalte bereits existiert und `CREATE INDEX IF NOT EXISTS` idempotent ist. ABER: der erste `ADD COLUMN` würde die ganze Migration failen lassen. Deshalb besser so: **NICHT manuell applyen**, sondern sqlx das machen lassen:

Rollback-Variante:

```bash
# Wenn schon manuell applied, Spalte wieder dropen:
ssh linda@178.105.6.124 'sudo sqlite3 /var/lib/razortrade/razortrade.sqlite "
ALTER TABLE signals DROP COLUMN retry_after_at;
DROP INDEX IF EXISTS idx_signals_pending_retry;"
'
```

SQLite unterstützt `ALTER TABLE DROP COLUMN` ab Version 3.35 (2021) — Rocky 10.1 hat 3.40+, also ok.

**Empfohlener Pfad:** Schritt 2.4 SKIP, Migration läuft automatisch beim ersten Start nach dem dnf-reinstall in Schritt 2.5.

### 2.5 RPM deployen

```bash
scp target/generate-rpm/rt-daemon-0.1.0-1.x86_64.rpm \
    linda@178.105.6.124:/tmp/rt-daemon-drop19.rpm

ssh linda@178.105.6.124 '
sudo systemctl stop rt-daemon.service
sudo dnf reinstall -y /tmp/rt-daemon-drop19.rpm
sudo systemctl daemon-reload
sudo systemctl start rt-daemon.service
sleep 5
systemctl is-active rt-daemon rt-dashboard razortrade-signals.timer razortrade-backup.timer caddy oauth2-proxy
sudo journalctl -u rt-daemon --since "30 seconds ago" --no-pager | grep -iE "panic|error|fatal|migrat" | head -20
'
```

Erwartet:
- 6× active
- Im Log eine Zeile à la "migration 20260420000003 applied" (oder ähnlich) — Format je nach sqlx-Tracing-Config
- Keine panic/error/fatal in startup-Log (warnings sind OK)

### 2.6 Schema-Verifikation nach Deploy

```bash
ssh linda@178.105.6.124 'sudo sqlite3 /var/lib/razortrade/razortrade.sqlite "
SELECT sql FROM sqlite_master WHERE name = \"signals\";
SELECT sql FROM sqlite_master WHERE name = \"idx_signals_pending_retry\";
SELECT version, description FROM _sqlx_migrations ORDER BY version DESC LIMIT 3;"
'
```

Erwartet:
- `retry_after_at TEXT` in signals-Definition
- Der Index-CREATE
- `20260420000003` als neueste Migration

---

## Teil 3 — Deploy-Drills (CV-A1 Verification)

### 3.1 Drill Uncertain-Resubmit (Happy-Path ohne Kraken-Interaktion)

Injiziere ein Signal mit bereits einem `uncertain`-Order-Row, simuliert dass Timeout-Path vorher getriggert hat:

```bash
ssh linda@178.105.6.124 "sudo sqlite3 /var/lib/razortrade/razortrade.sqlite \"
BEGIN;
INSERT INTO signals (created_at, instrument_symbol, side, signal_type, sleeve,
                     notional_chf, leverage, metadata_json, status)
VALUES (strftime('%Y-%m-%dT%H:%M:%SZ','now'),
        'PI_XBTUSD', 'buy', 'trend_entry', 'crypto_leverage',
        '300', '2',
        json_object('atr_absolute','500','atr_pct','0.01','fx_quote_per_chf','0.78'),
        'pending');
INSERT INTO orders (signal_id, broker, broker_order_id, instrument_symbol,
                    side, order_type, time_in_force, quantity, limit_price,
                    status, filled_quantity, fees_paid, created_at, updated_at,
                    cli_ord_id, error_message)
VALUES (last_insert_rowid(), 'kraken_futures', NULL, 'PI_XBTUSD',
        'buy', 'limit', 'gtc', '234', NULL,
        'uncertain', '0', '0',
        strftime('%Y-%m-%dT%H:%M:%SZ','now'),
        strftime('%Y-%m-%dT%H:%M:%SZ','now'),
        'rt-s' || last_insert_rowid(),
        'timeout after 10s');
COMMIT;

SELECT id, status, retry_after_at FROM signals ORDER BY id DESC LIMIT 1;
SELECT id, signal_id, status, cli_ord_id FROM orders ORDER BY id DESC LIMIT 1;
\""
```

Innerhalb ~1s sollte der processor-tick ausgelöst werden (pending signal ohne retry_after_at). Das signal_processor geht in den `uncertain`-Branch, ruft `broker.get_order_by_cli_ord_id("rt-sN")` — je nach Demo-API-Response:

- **Paper-/Demo-Modus auf Kraken Demo:** gibt `Ok(None)` zurück (Kraken Demo hat keine Order mit dieser cli_ord_id) → signal_processor fällt in Resubmit, versucht `broker.submit_order(...)`. Das setzt den order-row auf `submitted` oder entsprechendes.
- **Wenn das broker-lookup selbst Fehler wirft:** signal setzt retry_after_at = now+90s, tick endet ohne Submit.

Verifikation 30s nach Insert:

```bash
ssh linda@178.105.6.124 "sudo sqlite3 /var/lib/razortrade/razortrade.sqlite \"
SELECT id, status, retry_after_at, processed_at FROM signals ORDER BY id DESC LIMIT 1;
SELECT id, status, broker_order_id, error_message FROM orders ORDER BY id DESC LIMIT 1;
SELECT id, message FROM audit_log ORDER BY id DESC LIMIT 5;
\""
```

Erwartete Ergebnisse (Demo-Modus):
- Signal: `status` bleibt `pending` mit `retry_after_at` gesetzt, ODER `status = processed` nach erfolgreichem Resubmit
- Order: `status = uncertain` mit retry, ODER `status = submitted`/`acknowledged` nach adopted/resubmitted

Im Log sollte einer dieser Einträge auftauchen:
- `"Drop-19 uncertain-resubmit: checking Kraken for order presence"`
- `"uncertain-resubmit: Kraken has no such order; will resubmit with same cli_ord_id"`
- `"uncertain-resubmit: broker.get_order failed; extending cooldown"`

```bash
ssh linda@178.105.6.124 'sudo journalctl -u rt-daemon --since "2 minutes ago" --no-pager | grep -iE "uncertain|resubmit|drop-19" | tail -10'
```

### 3.2 Drill DuplicatePositionCheck

Injiziere ein neues Signal während eine offene Position existiert:

```bash
# Offene Position injizieren (falls nicht bereits vorhanden)
ssh linda@178.105.6.124 "sudo sqlite3 /var/lib/razortrade/razortrade.sqlite \"
INSERT INTO positions (instrument_symbol, broker, sleeve, quantity,
                       avg_entry_price, leverage, opened_at, updated_at)
VALUES ('PI_XBTUSD', 'kraken_futures', 'crypto_leverage', '50',
        '65000', '2',
        strftime('%Y-%m-%dT%H:%M:%SZ','now'),
        strftime('%Y-%m-%dT%H:%M:%SZ','now'));
\""

# Signal in gleicher Richtung
ssh linda@178.105.6.124 "sudo sqlite3 /var/lib/razortrade/razortrade.sqlite \"
INSERT INTO signals (created_at, instrument_symbol, side, signal_type, sleeve,
                     notional_chf, leverage, metadata_json, status)
VALUES (strftime('%Y-%m-%dT%H:%M:%SZ','now'),
        'PI_XBTUSD', 'buy', 'trend_entry', 'crypto_leverage',
        '300', '2',
        json_object('atr_absolute','500','atr_pct','0.01','fx_quote_per_chf','0.78'),
        'pending');
\""

sleep 5

ssh linda@178.105.6.124 "sudo sqlite3 /var/lib/razortrade/razortrade.sqlite \"
SELECT id, status, rejection_reason FROM signals ORDER BY id DESC LIMIT 1;
\""
```

Erwartet: Das Signal ist `rejected`, `rejection_reason` enthält `\"kind\": \"duplicate_position_open\"`.

Aufräumen:

```bash
ssh linda@178.105.6.124 "sudo sqlite3 /var/lib/razortrade/razortrade.sqlite \"
DELETE FROM signals WHERE rejection_reason LIKE '%duplicate_position%';
UPDATE positions SET closed_at = strftime('%Y-%m-%dT%H:%M:%SZ','now'),
       quantity = '0' WHERE avg_entry_price = '65000';
\""
```

### 3.3 Drill Python-Positions-Check

```bash
# Position anlegen
ssh linda@178.105.6.124 "sudo sqlite3 /var/lib/razortrade/razortrade.sqlite \"
INSERT INTO positions (instrument_symbol, broker, sleeve, quantity,
                       avg_entry_price, leverage, opened_at, updated_at)
VALUES ('PI_XBTUSD', 'kraken_futures', 'crypto_leverage', '50',
        '65000', '2',
        strftime('%Y-%m-%dT%H:%M:%SZ','now'),
        strftime('%Y-%m-%dT%H:%M:%SZ','now'));
\""

# Donchian manuell ausführen (als razortrade user, weil das der signals.service user ist)
ssh linda@178.105.6.124 'sudo -u razortrade /usr/bin/python3 /usr/libexec/razortrade/donchian_signal.py \
    --symbol PI_XBTUSD --db /var/lib/razortrade/razortrade.sqlite \
    --fx-quote-per-chf 0.78 --commit 2>&1 | tail -5'
```

Erwartet: Das Script erkennt die Position und gibt entweder "no breakout" (wenn kein Breakout aktuell) ODER "REFUSED to write signal: open PI_XBTUSD position in same direction" aus. Exit 0. Kein neues Signal in der DB.

Aufräumen: Position schließen analog 3.2.

---

## Teil 4 — Rollback

Falls Deploy-Drills in Teil 3 fehlschlagen:

```bash
ssh linda@178.105.6.124 '
sudo systemctl stop rt-daemon.service
sudo dnf downgrade rt-daemon
sudo systemctl start rt-daemon.service
'
```

DB-Schema: die `retry_after_at` Spalte kann stehenbleiben (alte Binaries ignorieren sie), aber wenn die Migration explizit zurückgerollt werden soll:

```bash
ssh linda@178.105.6.124 'sudo sqlite3 /var/lib/razortrade/razortrade.sqlite "
ALTER TABLE signals DROP COLUMN retry_after_at;
DROP INDEX IF EXISTS idx_signals_pending_retry;
DELETE FROM _sqlx_migrations WHERE version = 20260420000003;"
'
```

Config-Änderungen zurückrollen:

```bash
ssh linda@178.105.6.124 '
sudo sed -i "s/usd_per_chf_fallback = \"0.78\"/usd_per_chf_fallback = \"1.10\"/" /etc/razortrade/daemon.toml
sudo rm /etc/systemd/system/razortrade-signals.service.d/override.conf
sudo rmdir /etc/systemd/system/razortrade-signals.service.d 2>/dev/null
sudo systemctl daemon-reload
sudo systemctl restart rt-daemon.service
'
```

---

## Teil 5 — Git-Commit

Nach erfolgreichem Deploy + Drills:

```bash
cd ~/Workspace/razortrade
git add -A
git status
git commit -m "Drop 19 Part A: audit remediation CV-A1 + CV-A2

- Migration 20260420000003: signals.retry_after_at + cooldown index
- Order status 'uncertain' for timeout/network errors; signal stays
  pending with 90s cooldown
- Uncertain-resubmit path: REST get_order_by_cli_ord_id before
  resubmit; reuse same order row + cli_ord_id on resubmit
- repo.backfill_broker_order_id promotes uncertain -> acknowledged
- New DuplicatePositionCheck in standard checklist (7 unit tests)
- rt-core OpenPositionSummary + PortfolioState.open_positions
- portfolio_loader loads real open_positions from DB
- Python donchian_signal.py: pre-write check against open positions
  and in-flight orders
- FX fallback config 1.10 -> 0.78 (SNB 17.04.2026: 0.7823)
- Broker trait: get_order_by_cli_ord_id, OpenOrderSummary.cli_ord_id
- Kraken rest.rs: cli_ord_id populated on open_orders responses"
git push
```

---

## Noch offen (nicht in dieser Drop)

- **Part B** Panic-Close Hardening (reduce_only Flag + Event-Trigger aus fill_reconciler) — siehe `DROP_19_TODO.md`
- **Part C** Timestamp-Normalisierung Python↔Rust
- **Part D** Dashboard Turn 5 (16:9, Auto-Cycle, Sparklines)

Timeline: B + C in einem kombinierten Drop (klein), D als eigener Drop (größer).

## Live-Gate-Clock

Drop 19 Part A Deploy resettet den 72h-Live-Gate-Clock. Frühester Live-Start jetzt: **72h nach Abschluss dieses Deploys + extern-Audit (Gemini) bestätigt CV-A1 + CV-A2 als gefixt**.
