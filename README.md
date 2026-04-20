# razortrade

Systematischer Algo-Trader für Kraken Futures. Rust-Execution-Layer, Python-Signal-Generierung, SQLite als Integrations-Schicht. Built für Rocky Linux 10 auf einem einzelnen Hetzner-VPS.

**Status April 2026:** Paper-Trading gegen Kraken Demo, Live-Gate geplant nach Drop-19-Remediation.

---

## Wo liegt was — Quick Reference

| Was | Wo |
|---|---|
| Source-Repo (lokal) | `~/Workspace/razortrade` |
| Prod-Host | `linda@178.105.6.124` (Hetzner CX32, Rocky Linux 10.1) |
| Domain Dashboard | https://trader.ghazzo.ch (Google OAuth, Whitelist-basiert) |
| SQLite-DB (Prod) | `/var/lib/razortrade/razortrade.sqlite` |
| Daemon-Config (Prod) | `/etc/razortrade/daemon.toml` (mode 0640, group `razortrade`) |
| Secrets (Prod) | `/etc/razortrade/secrets.env` (mode 0600, **nie committen**) |
| Daemon-Binary (Prod) | `/usr/bin/rt-daemon` |
| Dashboard-Binary (Prod) | `/usr/bin/rt-dashboard` |
| Python-Scripts (Prod) | `/usr/libexec/razortrade/` |
| Backups lokal (Prod) | `/var/lib/razortrade/backups/` |
| Systemd-Units | `/usr/lib/systemd/system/{rt-daemon,rt-dashboard,razortrade-signals,razortrade-backup}.{service,timer}` |
| Caddy-Config | `/etc/caddy/Caddyfile` |
| oauth2-proxy | `/etc/oauth2-proxy/oauth2-proxy.cfg` + `.env` + `allowed_emails` |
| Dokumentation | `docs/ARCHITECTURE.md`, `docs/BUILDING.md`, `docs/OPERATIONS.md`, `docs/SECRETS.md` |
| Aktuelle TODO | `DROP_19_TODO.md` |
| Audit-Historie | `AUDIT_2026_04_20.md` + `DROP_18_DEPLOY.md` + `SESSION_CLOSEOUT.md` |

---

## Architektur in einem Absatz

Python rechnet alle 4h Donchian-Breakout-Signale und schreibt sie in die SQLite-Tabelle `signals`. Rust (`rt-daemon`) pollt diese Tabelle im 1-Hz-Takt, validiert jedes Signal durch eine 5-Punkte-Checkliste und submitted bei Approval einen Limit-Order an Kraken Futures. Ein Kill-Switch-Supervisor läuft alle 60s und disabled den Leverage-Sleeve, wenn entweder der Lifetime-Realisierter-Loss-Budget (-1000 CHF, soft) oder der Effective-PnL-Threshold (-1200 CHF, hard → panic-close aller offenen Positionen) oder die Portfolio-Drawdown-Grenze (20 %) breached werden. Fills kommen über Kraken WebSocket, werden per Fill-Reconciler idempotent in `applied_fills` + `orders` + `positions` persistiert. Ein read-only Rust/Axum-Dashboard zeigt Portfolio-State hinter Google OAuth + Caddy-TLS.

Volle Design-Diskussion: `docs/ARCHITECTURE.md`.

---

## Crate-Layout

```
crates/
├── rt-core            Domain-Typen (Order, Signal, Position, MarketSnapshot, Sleeve, Side, …)
├── rt-risk            Pre-Trade-Checklist, Kill-Switch, ATR-Position-Sizing
├── rt-persistence     SQLite + Migrations + typisierter Repository
├── rt-execution       Broker-Trait (async_trait)
├── rt-kraken-futures  Kraken-Futures WS + REST Client (Broker-Impl)
├── rt-daemon          Main-Binary; verdrahtet alles
└── rt-dashboard       Read-only Portfolio-Viewer (Axum, HTML/JS)
```

**Nicht im Repo:** `rt-ibkr` (Interactive-Brokers-ETF-Sleeve) — vertagt.

---

## Build & Deploy — Kurzreferenz

### Lokaler Build

```bash
cd ~/Workspace/razortrade
cargo check --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --release -p rt-daemon
cargo build --release -p rt-dashboard
```

### RPMs bauen

```bash
cargo generate-rpm -p crates/rt-daemon
cargo generate-rpm -p crates/rt-dashboard
ls -la target/generate-rpm/*.rpm
```

Rust-Toolchain gepinnt auf 1.82 (siehe `rust-toolchain.toml`). `cargo-generate-rpm` muss installiert sein (`cargo install cargo-generate-rpm`). Build-Host-Kompatibilität: Rocky 9 oder 10, x86_64. Details in `docs/BUILDING.md`.

### Auf Prod deployen

```bash
scp target/generate-rpm/rt-daemon-*.rpm linda@178.105.6.124:/tmp/
scp target/generate-rpm/rt-dashboard-*.rpm linda@178.105.6.124:/tmp/
ssh linda@178.105.6.124 '
sudo dnf reinstall -y /tmp/rt-daemon-*.rpm
sudo dnf reinstall -y /tmp/rt-dashboard-*.rpm
sudo systemctl daemon-reload
sudo systemctl restart rt-daemon.service rt-dashboard.service
sleep 3
systemctl is-active rt-daemon rt-dashboard razortrade-signals.timer razortrade-backup.timer caddy oauth2-proxy
'
```

Erwartetes Output: 6× `active`.

Siehe `docs/OPERATIONS.md` für Schema-Migrations + Rollback-Prozedur.

---

## Lokale Entwicklung

Siehe `docs/OPERATIONS.md` Abschnitt "Lokaler Dev-Loop".

Kurzform mit Dry-Run-Config:

```bash
cp deploy/daemon.toml.local.example deploy/daemon.toml.local
mkdir -p ./var
export RT_DAEMON_CONFIG=$(pwd)/deploy/daemon.toml.local
RUST_LOG=info,rt_=debug,sqlx=warn cargo run -p rt-daemon
```

In einem zweiten Terminal:

```bash
python3 tools/inject_test_signal.py
```

Das Signal landet in der lokalen DB, der Daemon verarbeitet es im Dry-Run-Modus — kein Broker-Call, kein Echtgeld-Risiko.

---

## Tests

```bash
cargo test --workspace                         # Rust-Unit+Integration
python3 tools/indicators.py                    # Donchian-/ATR-Self-Tests
```

Test-Coverage (Stand Drop 18, 43 Tests grün):
- `rt-core` — alle Domain-Typ-Invarianten
- `rt-risk` — jeder der 5 Checks mit mind. 3 Szenarien (approve, reject, boundary)
- `rt-risk::kill_switch` — alle Trigger-Pfade inkl. LF-1 Panic-Close
- `rt-persistence` — Fill-Application, Orphan-Catcher, Idempotenz
- `rt-kraken-futures::orderbook` — Sequence-Gaps, OutOfSync-Flag, Cross-Book-Detection

**Keine Integration-Tests gegen echte Broker-APIs** in diesem Repo. Sandbox-Credentials leben in einem separaten Repo.

---

## Konfiguration — sensitive Daten

API-Keys, OAuth-Secrets und das Cookie-Signing-Secret liegen **nicht** in committeten Dateien. Siehe `docs/SECRETS.md` für die vollständige Liste was wo hinkommt und welche Permissions.

Kurzregel:
- Sensitive Werte in `/etc/razortrade/secrets.env` (mode 0600, owner root, group razortrade)
- OAuth2-proxy-Secrets in `/etc/oauth2-proxy/oauth2-proxy.env` (mode 0600, owner root, group oauth2-proxy)
- Beide Dateien werden von systemd per `EnvironmentFile=` in die Service-Units geladen
- Das Prozess-Environment mit Secrets ist via Hardening-Direktiven (`PrivateTmp`, `NoNewPrivileges`, `ProtectSystem=strict`) abgeschirmt

---

## Scope & Non-Goals

**In Scope:**
- Momentum-Trend-Following mit ≤ 2× Leverage via Kraken Futures (PI_XBTUSD)
- Deterministische Pre-Trade-Risikochecks mit vollem Audit-Trail
- Single-User, Single-Host, Hobby-Kapital-Skala (hunderte bis zehntausende CHF)
- Deutsche Operator-Sprache, Schweizer Zeitzone (Europe/Zurich), CHF als Basis-Währung

**Explizit NICHT in Scope:**
- High-Frequency-Trading (Latenz-Budget hier ist 5 Sekunden, nicht 5 Mikrosekunden)
- ML-basierte Signal-Generierung. Klassische Indikatoren, volle Transparenz.
- Drittkapital / OPM (Other People's Money) → FINMA-Abklärung nötig, nicht unser Scope.
- Multi-Asset-Portfolio (ETF-Sleeve vertagt, Spot-Sleeve vertagt).
- Non-CHF-Base-Currency.

---

## Wichtige Prozeduren

Alle Prozeduren sind in `docs/OPERATIONS.md` dokumentiert. Kurz-Pointer:

- **Deploy:** `docs/OPERATIONS.md` §Deploy
- **DB-Migration:** `docs/OPERATIONS.md` §Migration
- **Rollback bei Broken Deploy:** `docs/OPERATIONS.md` §Rollback
- **Kill-Switch-Reset:** `docs/OPERATIONS.md` §Kill-Switch (es gibt noch keinen CLI-Tool, aktuell manueller SQL-Update)
- **Backup-Recovery:** `docs/OPERATIONS.md` §Backup (Hetzner-Storage-Box-Push via `razortrade-backup.service`)
- **Troubleshoot `signal_expired`-Häufung:** `docs/OPERATIONS.md` §Troubleshooting
- **Audit-Durchführung:** `docs/OPERATIONS.md` §Audit + `HANDOVER_FOR_GEMINI.md`

---

## Audit-Status (Stand 2026-04-20)

**Drop 18** — alle P0-Findings aus dem Gemini-Audit 2026-04-19 gefixt und auf Prod deployed (`DROP_18_DEPLOY.md`).

**Drop 19** (offen) — interner Claude-Audit 2026-04-20 hat zwei neue BLOCKER identifiziert:
- **CV-A1** REST-Timeout → Position-Verdopplung bei persistierendem Trend
- **CV-A2** FX-Rate hartcodiert 1.10 (real ~0.78) → +41 % Position-Oversizing

**Beide blockieren den Live-Gate.** Details + Fix-Plan in `DROP_19_TODO.md`.

Live-Gate-Datum: **frühestens 72h nach Drop-19-Deploy.**

---

## Kontakt & Zuständigkeit

- **Operator:** Walid (Einzel-Betreiber, Zürich)
- **Letsencrypt-Mail (Caddy):** `ops@ghazzo.ch`
- **Dashboard-Whitelist:** `/etc/oauth2-proxy/allowed_emails` (aktuell: `walid@ghazzo.ch`)
- **Repo-Lizenz:** Proprietär, private.
