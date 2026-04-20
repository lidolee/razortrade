# Operations Runbook

Praktische Anleitung zum Betrieb von razortrade. Deploy, Rollback, Backup-Recovery, Troubleshooting.

Für Design-Hintergrund siehe `ARCHITECTURE.md`. Für Build siehe `BUILDING.md`. Für Secrets siehe `SECRETS.md`.

---

## Prod-Inventar

| Service | systemd-Unit | User | Binary / Script |
|---|---|---|---|
| Trading-Daemon | `rt-daemon.service` | `razortrade` | `/usr/bin/rt-daemon` |
| Read-Only-Dashboard | `rt-dashboard.service` | `razortrade` (plus `systemd-journal` Supplementary) | `/usr/bin/rt-dashboard` |
| Signal-Generator | `razortrade-signals.service` + `.timer` | `razortrade` | `/usr/libexec/razortrade/donchian_signal.py` |
| Daily Backup | `razortrade-backup.service` + `.timer` | `razortrade` | `/usr/libexec/razortrade/sqlite-backup.sh` |
| TLS-Reverse-Proxy | `caddy.service` | `caddy` | `/usr/bin/caddy` |
| OAuth-Proxy | `oauth2-proxy.service` | `oauth2-proxy` | `/usr/bin/oauth2-proxy` |

**Schnell-Check ob alle laufen:**

```bash
ssh linda@178.105.6.124 'systemctl is-active rt-daemon rt-dashboard razortrade-signals.timer razortrade-backup.timer caddy oauth2-proxy'
```

Erwartet: 6× `active`.

---

## Wichtige Pfade auf Prod

| Pfad | Zweck |
|---|---|
| `/usr/bin/rt-daemon` | Rust-Trading-Daemon, Installed via RPM |
| `/usr/bin/rt-dashboard` | Rust-Dashboard, Installed via RPM |
| `/usr/libexec/razortrade/donchian_signal.py` | Python-Signal-Generator |
| `/usr/libexec/razortrade/indicators.py` | Donchian/ATR-Helper |
| `/usr/libexec/razortrade/sqlite-backup.sh` | Backup-Script (sqlite3 .backup + optional offsite-push) |
| `/usr/libexec/razortrade/offsite-push.sh` | Hetzner-Storage-Box rsync (falls konfiguriert) |
| `/var/lib/razortrade/razortrade.sqlite` | Produktiv-Datenbank, mode 0640, owner razortrade |
| `/var/lib/razortrade/razortrade.sqlite-wal` | WAL-Journal (automatisch, nicht manuell anfassen) |
| `/var/lib/razortrade/razortrade.sqlite-shm` | Shared-Memory für WAL (auto) |
| `/var/lib/razortrade/backups/` | Lokale Tagesbackups, 7 Tage retention |
| `/var/lib/razortrade/.ssh/` | SSH-Key für offsite-push (falls aktiv) |
| `/var/log/razortrade/` | Logs (systemd-managed) |
| `/etc/razortrade/daemon.toml` | Hauptconfig (mode 0640, owner root, group razortrade) |
| `/etc/razortrade/secrets.env` | API-Keys (mode 0600, owner root, group razortrade) |
| `/etc/caddy/Caddyfile` | TLS + Reverse-Proxy |
| `/etc/oauth2-proxy/oauth2-proxy.cfg` | OAuth-Konfiguration |
| `/etc/oauth2-proxy/oauth2-proxy.env` | OAuth-Secrets (mode 0600) |
| `/etc/oauth2-proxy/allowed_emails` | Email-Whitelist |

---

## Lokaler Dev-Loop

```bash
cd ~/Workspace/razortrade
cp deploy/daemon.toml.local.example deploy/daemon.toml.local   # einmalig
mkdir -p ./var                                                  # einmalig
export RT_DAEMON_CONFIG=$(pwd)/deploy/daemon.toml.local
RUST_LOG=info,rt_=debug,sqlx=warn cargo run -p rt-daemon
```

In einem zweiten Terminal:

```bash
python3 tools/inject_test_signal.py
# oder echter Donchian-Signal ohne --commit:
python3 tools/donchian_signal.py
```

Erwartet in Terminal 1 innerhalb ~1s:
```
signal processor … processing pending signals count=1
```
Gefolgt von entweder einem Rejection (fehlende Market-Data in der ersten Sekunde nach Start) oder einem Dry-Run-Order-Intent in `dry_run_orders`.

DB inspizieren:

```bash
sqlite3 ./var/razortrade.sqlite "SELECT id, status, rejection_reason FROM signals;"
sqlite3 ./var/razortrade.sqlite "SELECT * FROM checklist_evaluations;"
sqlite3 ./var/razortrade.sqlite "SELECT * FROM dry_run_orders;"
```

`daemon.toml.local` ist gitignored — frei editierbar.

---

## Deploy

### RPMs bauen + auf Prod einspielen

```bash
cd ~/Workspace/razortrade

# 1. Build-Check
cargo check --workspace 2>&1 | tail -5
cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -5
cargo test --workspace 2>&1 | tail -5

# 2. Release-Build + RPM
cargo build --release -p rt-daemon
cargo build --release -p rt-dashboard
cargo generate-rpm -p crates/rt-daemon
cargo generate-rpm -p crates/rt-dashboard
ls -la target/generate-rpm/*.rpm

# 3. Copy + reinstall
scp target/generate-rpm/rt-daemon-*.rpm linda@178.105.6.124:/tmp/rt-daemon-latest.rpm
scp target/generate-rpm/rt-dashboard-*.rpm linda@178.105.6.124:/tmp/rt-dashboard-latest.rpm
ssh linda@178.105.6.124 '
sudo dnf reinstall -y /tmp/rt-daemon-latest.rpm /tmp/rt-dashboard-latest.rpm
sudo systemctl daemon-reload
sudo systemctl restart rt-daemon.service rt-dashboard.service
sleep 5
systemctl is-active rt-daemon rt-dashboard razortrade-signals.timer razortrade-backup.timer caddy oauth2-proxy
'
```

Erwartet: 6× `active`.

### Startup-Log-Check

```bash
ssh linda@178.105.6.124 'sudo journalctl -u rt-daemon --since "60 seconds ago" --no-pager | grep -iE "panic|error|fatal" | head'
```

Erwartet: leer. Warnings wie `no ticker yet` in den ersten 30s sind normal (WS-Handshake).

---

## Schema-Migrationen

Alle Schema-Änderungen liegen als `.sql`-Files in `crates/rt-persistence/migrations/`.

Bei jedem Daemon-Start läuft `sqlx::migrate!` (`repo.rs:44`) und applies pending Migrations in sortierter Reihenfolge. Das ist idempotent — zweimal Starten = gleicher State.

**Bei größeren Migrations mit Datenumzug** (z.B. Drop 18 `cli_ord_id` + `expires_at`): vor Deploy explizit stoppen, migrations manuell laufen lassen, verifizieren, dann Daemon starten. Beispiel siehe `DROP_18_DEPLOY.md` Step 5.

Migration-Verifikation:

```bash
ssh linda@178.105.6.124 'sudo sqlite3 /var/lib/razortrade/razortrade.sqlite ".schema orders" | grep cli_ord_id'
ssh linda@178.105.6.124 'sudo sqlite3 /var/lib/razortrade/razortrade.sqlite ".schema signals" | grep expires_at'
```

---

## Rollback bei Broken Deploy

```bash
ssh linda@178.105.6.124 '
sudo systemctl stop rt-daemon.service rt-dashboard.service
sudo dnf downgrade rt-daemon rt-dashboard
sudo systemctl start rt-daemon.service rt-dashboard.service
'
```

DB-Migrationen müssen nicht zurückgerollt werden — neue Spalten sind `NULL`-fähig und werden vom alten Binary ignoriert.

Falls eine Migration nicht rückwärts-kompatibel ist: DB-Backup aus `/var/lib/razortrade/backups/` wiederherstellen (siehe §Backup-Recovery).

---

## Kill-Switch

### State-Abfrage

```bash
ssh linda@178.105.6.124 '
sudo sqlite3 /var/lib/razortrade/razortrade.sqlite "
  SELECT id, reason_kind, triggered_at, resolved_at
    FROM kill_switch_events
   ORDER BY id DESC LIMIT 5;"
'
```

Aktive (unresolved) Events:

```bash
ssh linda@178.105.6.124 '
sudo sqlite3 /var/lib/razortrade/razortrade.sqlite "
  SELECT id, reason_kind, triggered_at, reason_detail_json
    FROM kill_switch_events
   WHERE resolved_at IS NULL;"
'
```

### Manueller Reset (noch kein CLI-Tool)

**Hinweis:** Das Reset ändert nur das `resolved_at`-Flag. Wenn die zugrundeliegenden Metriken nicht aufgelöst sind (z.B. realized_pnl immer noch ≤ -1000), feuert der Supervisor im nächsten Tick erneut.

```bash
ssh linda@178.105.6.124 "sudo sqlite3 /var/lib/razortrade/razortrade.sqlite \"
UPDATE kill_switch_events
   SET resolved_at = strftime('%Y-%m-%dT%H:%M:%SZ','now'),
       resolved_by = 'walid',
       resolved_note = '<Begründung für Reset>'
 WHERE resolved_at IS NULL;
\""
```

Dann Daemon neu starten, damit der Supervisor-State den Reset sieht:

```bash
ssh linda@178.105.6.124 'sudo systemctl restart rt-daemon.service'
```

### Drill: Panic-Close-Test

Siehe `DROP_18_DEPLOY.md` Step 8. Injiziert einen künstlichen Equity-Snapshot der effective_pnl unter -1200 bringt, wartet auf Supervisor-Tick, verifiziert Event + räumt auf.

---

## Backup & Recovery

### Automatisch

- `razortrade-backup.timer` → täglich 03:00 lokal + 0-15min randomisiert
- Führt `razortrade-backup.service` aus → `/usr/libexec/razortrade/sqlite-backup.sh`
- Lokale Retention: 7 Tage in `/var/lib/razortrade/backups/`
- Offsite-Push: ssh/rsync auf Hetzner Storage-Box (nur aktiv wenn SSH-Key in `/var/lib/razortrade/.ssh/` eingerichtet ist)

### Backup-Status prüfen

```bash
ssh linda@178.105.6.124 '
systemctl status razortrade-backup.service --no-pager | head -20
ls -la /var/lib/razortrade/backups/
'
```

### Manuelles Backup sofort

```bash
ssh linda@178.105.6.124 'sudo systemctl start razortrade-backup.service && sudo journalctl -u razortrade-backup.service --since "2 minutes ago" --no-pager'
```

### Recovery

1. Daemon stoppen: `sudo systemctl stop rt-daemon.service`
2. Backup kopieren: `sudo cp /var/lib/razortrade/backups/razortrade-YYYYMMDD.sqlite /var/lib/razortrade/razortrade.sqlite`
3. Permissions: `sudo chown razortrade:razortrade /var/lib/razortrade/razortrade.sqlite && sudo chmod 0640 /var/lib/razortrade/razortrade.sqlite`
4. Daemon starten: `sudo systemctl start rt-daemon.service`
5. Migrations laufen beim Start automatisch und sind idempotent — kein Handarbeit.

**Achtung:** Die WAL- und SHM-Sidecar-Files (`razortrade.sqlite-wal`, `.sqlite-shm`) NICHT mitkopieren, die werden neu erzeugt.

---

## Dashboard-Zugriff

URL: https://trader.ghazzo.ch

Zugriff nur via Google-OAuth. Whitelist in `/etc/oauth2-proxy/allowed_emails` (eine Email pro Zeile).

Email hinzufügen:

```bash
ssh linda@178.105.6.124 'sudo sh -c "echo neue@email.ch >> /etc/oauth2-proxy/allowed_emails" && sudo systemctl reload oauth2-proxy'
```

---

## Troubleshooting

### "6 services" zeigt weniger als 6 `active`

```bash
ssh linda@178.105.6.124 'systemctl --failed'
ssh linda@178.105.6.124 'journalctl -u <name> --since "10 minutes ago" --no-pager | tail -30'
```

### Daemon crashed mit Panic

```bash
ssh linda@178.105.6.124 'journalctl -u rt-daemon --since "1 hour ago" --no-pager | grep -A 20 -i panic'
```

`rt-daemon.service` hat `Restart=on-failure` + `StartLimitBurst=5`/`IntervalSec=60` — systemd restartet, außer es crasht mehr als 5× pro Minute (dann stoppt systemd und braucht manuelles `systemctl start`).

### Dashboard zeigt "no open positions" aber Kraken hat welche

Orphan-Catcher-Timing, Fill-Reconciliation-Defekt, oder DB/Broker-Drift. Ablauf:

```bash
# Daemon-State
sudo sqlite3 /var/lib/razortrade/razortrade.sqlite "SELECT * FROM positions WHERE closed_at IS NULL;"
sudo sqlite3 /var/lib/razortrade/razortrade.sqlite "SELECT * FROM orders ORDER BY id DESC LIMIT 5;"
sudo sqlite3 /var/lib/razortrade/razortrade.sqlite "SELECT * FROM applied_fills ORDER BY applied_at DESC LIMIT 10;"

# Broker-Ground-Truth (manuell via Kraken Web-UI oder REST)
# Es gibt aktuell KEINEN automatischen Reconcile-Check — geplant für Drop 20 (CV-6)
```

### `signal_expired` häuft sich

Signal-Generator feuert zum 4h-Tick, Daemon verarbeitet ihn nicht binnen 30 Minuten → expired. Mögliche Ursachen:
- WS-Book noch nicht synced → Signal-Rejection `NoBook` → retry bis expiry
- Market-Data-Extract scheitert → Rejection → retry bis expiry

```bash
sudo sqlite3 /var/lib/razortrade/razortrade.sqlite "
  SELECT id, created_at, status, rejection_reason
    FROM signals
   WHERE status IN ('rejected','expired')
   ORDER BY id DESC LIMIT 10;"
```

Wenn `rejection_reason` `market_data_unavailable` enthält: Kraken-WS prüfen (`journalctl -u rt-daemon -f`). Wenn gap-reconnect-Loops: CV-6-Symptom (siehe `DROP_19_TODO.md`).

### "database is locked" in Logs

Sollte NICHT passieren bei Standard-Load. Python hat 30s busy_timeout, Rust 5s. Wenn es doch kommt:
- Langer Fill-Snapshot-Replay auf Reconnect? Log nach "fills snapshot applied count=N" checken, N sollte < 200 sein.
- Parallel laufender `sqlite3`-Process vom Operator? Nicht gleichzeitig mit Daemon DB-Writes machen.

---

## Systemd-Direktiven die einmal kaputtging und jetzt festgezurrt sind

1. `razortrade-signals.timer` `OnCalendar=*-*-* 00/4:00:00 UTC` — das `UTC`-Suffix ist zwingend. Ohne UTC läuft der Timer auf CEST und verschiebt sich 2× pro Jahr bei DST. Symptom: Signals feuern 1-2h nach 4h-Candle-Close. Details im Kommentar der `.timer`-Datei.

2. `rt-daemon.service` `StartLimitBurst` + `StartLimitIntervalSec` gehören ins `[Unit]`-Segment, nicht ins `[Service]`. systemd loggt bei Falschplatzierung eine Warnung (`Unknown key name`) und ignoriert sie. Dann wird unbegrenzt restartet. Details im Kommentar der `.service`-Datei.

3. `rt-dashboard.service` braucht `SupplementaryGroups=systemd-journal` damit das Dashboard `journalctl` für die Live-Log-Streaming-Endpunkt ausführen kann. Ohne diese Group: Dashboard zeigt leere Logs.

4. `rt-dashboard.service` hat `MemoryDenyWriteExecute` entfernt (in Drop 16). Grund: Go-Runtime-Paths innerhalb `journalctl` triggerten SIGSYS unter strikt MDWX. Details im Kommentar der `.service`-Datei.

---

## Audit-Prozedur

Wenn ein externer Auditor (Gemini etc.) oder ein Nachfolger den Code durchleuchten soll:

1. `HANDOVER_FOR_GEMINI.md` lesen
2. Code-Tarball ziehen:
   ```bash
   cd ~/Workspace/razortrade
   tar czf /tmp/razortrade-src.tar.gz crates/ tools/ docs/ *.md Cargo.toml Cargo.lock
   ```
3. Prod-Meta-Tarball ziehen:
   ```bash
   ssh linda@178.105.6.124 'sudo tar czf /root/razortrade-prod-meta.tar.gz \
     /usr/lib/systemd/system/rt-daemon.service \
     /usr/lib/systemd/system/rt-dashboard.service \
     /usr/lib/systemd/system/razortrade-signals.service \
     /usr/lib/systemd/system/razortrade-signals.timer \
     /usr/lib/systemd/system/razortrade-backup.service \
     /usr/lib/systemd/system/razortrade-backup.timer \
     /etc/caddy/Caddyfile \
     /etc/oauth2-proxy/oauth2-proxy.cfg \
     /etc/oauth2-proxy/allowed_emails \
     /etc/razortrade/daemon.toml && \
     sudo cp /root/razortrade-prod-meta.tar.gz /tmp/ && \
     sudo chown linda:linda /tmp/razortrade-prod-meta.tar.gz'
   scp linda@178.105.6.124:/tmp/razortrade-prod-meta.tar.gz ~/Downloads/
   ```

   **`secrets.env` NICHT mitzippen.** Secrets gehen nie in Audit-Tarballs.

4. Beide Tarballs + `HANDOVER_FOR_GEMINI.md` + `DROP_19_TODO.md` dem Auditor geben.

---

## Verweise

- `ARCHITECTURE.md` — Design-Entscheidungen, Pipeline-Diagramm, Warum-SQLite
- `BUILDING.md` — RPM-Build-Details
- `SECRETS.md` — Welche Keys wo, welche Permissions
- `DROP_18_DEPLOY.md` — Historischer Deploy-Runbook für Drop 18 (Template für zukünftige Drops)
- `DROP_19_TODO.md` — Aktuelle offene Audit-Findings die Live-Gate blockieren
- `HANDOVER_FOR_GEMINI.md` — Handover an den nächsten Auditor
