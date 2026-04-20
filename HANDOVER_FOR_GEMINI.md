# Handover-Dokument für Google Gemini — razortrade Drop 19 Audit

**Ziel:** Gemini soll einen unabhängigen Audit des razortrade-Repos durchführen, nachdem Drop 19 die hier dokumentierten Befunde addressiert hat.

**Diese Datei muss vor dem Audit gelesen werden.** Sie enthält:
1. Kompakter System-Kontext
2. Aktuelle Befundslage (Drop 18 + offener Drop 19)
3. Was genau Gemini auditieren soll
4. Strikte Regeln für den Audit

---

## 1. System in einem Absatz

**razortrade** ist ein algorithmischer Swing-Trader für Kraken Futures (Perpetuals). Python-Signale (4h Donchian-Breakout + ATR) werden in SQLite geschrieben. Ein Rust-Daemon pollt SQLite im 1-Hz-Takt, validiert jedes Signal durch eine 5-Punkte-Checkliste und sendet bei Approval einen Limit-Order an Kraken. Ein Kill-Switch-Supervisor läuft alle 60s und disabled den Leverage-Sleeve bei Budget-/Drawdown-Breaches. Fills kommen via Kraken WebSocket, werden über einen Fill-Reconciler in SQLite persistiert. Ein Read-Only-Dashboard (Rust/Axum) zeigt Portfolio-State hinter Google OAuth + Caddy TLS.

**Hardware/Infra:** Hetzner CX32 VPS, Rocky Linux 10.1, Domain `trader.ghazzo.ch`. Einzel-User-System. Start-Kapital 300 CHF geplant für Live-Gate Apr/Mai 2026. Skalierungsziel 10'000+ CHF.

**Nicht Scope:** HFT (Latenz-Budget 5000ms), ML, Drittkapital.

---

## 2. Was bereits auditiert und gefixt wurde

### Drop 17 → 18 (abgeschlossen, 2026-04-20 vormittags)

Gemini's voriger Audit hatte folgende P0-Findings, alle gefixt:

- **CV-1** `orders.cli_ord_id` Spalte + Orphan-Catcher in `fill_reconciler.rs:226-355` — Fills ohne matching `broker_order_id` werden via `cli_ord_id` wieder an die lokale Order attached
- **CV-2** `fill_id` Audit sauber (Kraken-native UUIDs, `(broker, fill_id)` Composite PK in `applied_fills`)
- **CV-3** SQLite `busy_timeout=30s` auf Python-Seite + `Restart=on-failure` auf systemd-Service für `razortrade-signals`
- **CV-7** Raw-Logging bei WS-Deserialization-Failures (nur Logging, kein Root-Cause-Fix)
- **LF-1** Hard-Kill-Switch: `EffectivePnlBudgetExhausted` → `PanicClose` → `panic_close_leverage_positions` routine
- **LF-3** Equity-Writer filtert Zero-Value-Snapshots
- **OF-2** Signal-Expiry 30 Minuten (Schema + Python + Rust-Read + Processor-Guard)

**Belege:** `DROP_18_DEPLOY.md` dokumentiert Deploy-Steps inkl. Verifikations-Drills die auf Prod grün liefen.

### Drop 19 (geplant, noch nicht implementiert — siehe `DROP_19_TODO.md`)

Der heutige Audit (2026-04-20 nachmittags) hat zwei neue BLOCKER aufgedeckt:

#### CV-A1 — REST-Timeout → Position-Verdopplung bei persistierendem Trend

**Reproduzierbares Szenario:**
1. `signal_processor.rs:383` ruft `broker.submit_order(&order).await`
2. Kraken empfängt + platziert Order, HTTP-Response geht verloren (10s Timeout in `rest.rs:38 .timeout(Duration::from_secs(10))`)
3. `map_api_error` in `rest.rs:461` mappt auf `ExecutionError::Timeout`
4. `signal_processor.rs:402-416` — `Err(err)` Branch ist ambiguous: führt zu `mark_order_failed` + `mark_signal_rejected` OHNE Unterscheidung nach Error-Typ
5. Orphan-Catcher in `fill_reconciler.rs:226-280` findet beim WS-Fill die Order via `cli_ord_id` und backfilled den `broker_order_id`
6. ABER: `repo.rs:357-365` transitioned Status nur aus `pending_submission`. Status `failed` bleibt `failed`
7. Position-Row wird via `apply_fill_to_position` (`fill_reconciler.rs:309-330`) korrekt erstellt
8. 4h später: `tools/donchian_signal.py:285-393` — kein SELECT gegen `positions`, neues Signal wird geschrieben
9. `hard_limit.rs` Checkliste hat KEINEN Duplicate-Position-Guard (grep bestätigt: `grep -rn "existing_position\|open_position\|current_position" crates/rt-risk/` → 0 Produktions-Treffer)
10. Signal wird approved, zweiter Submit erfolgreich, **Position doppelt**

#### CV-A2 — FX-Rate 1.10 hartcodiert, real 0.78

**Sofort verifizierbar:**
- `crates/rt-risk/src/lib.rs:134-137`: `default_usd_per_chf_fallback()` = `Decimal::new(110, 2)` = 1.10
- `crates/rt-daemon/src/equity_writer.rs:110`: `let usd_per_chf = risk.usd_per_chf_fallback;` — wird bei **jedem** Snapshot verwendet (nicht nur Fallback)
- `tools/donchian_signal.py:303`: CLI-Default `--fx-quote-per-chf 1.10`
- `/usr/lib/systemd/system/razortrade-signals.service` ExecStart: `--fx-quote-per-chf 1.10`
- Real per SNB/ECB/Bloomberg/Yahoo Finance am 2026-04-20: ~0.78 USD/CHF

**Wirkung:** 300 CHF intendiert × 1.10 = 330 USD Position eröffnet. Echter CHF-Gegenwert bei 0.78 = 423 CHF. **+41% Oversizing** bei jedem Trade. Skaliert linear: bei 15k CHF Notional sind es 6k CHF stille Übergröße.

---

## 3. Was Gemini nach Drop-19-Deploy auditieren soll

### Primäre Verifikationsaufgabe

Prüfen, dass Drop 19 die BLOCKER aus der `DROP_19_TODO.md` korrekt gefixt hat. Die Definition of Done dort ist verbindlich.

Konkret — **Gemini muss gegen den Code folgende Fragen beantworten:**

1. **CV-A1a: Timeout-Path korrigiert?**
   - Datei: `crates/rt-daemon/src/signal_processor.rs`
   - Prüfe: Gibt es einen Branch der `ExecutionError::Timeout` und `::Network` separat behandelt und NICHT zu `mark_signal_rejected` führt?
   - Prüfe: Existiert ein neuer Order-Status `uncertain` in der Persistence-Schicht? Hat er eine Migration?

2. **CV-A1b: Backfill-Status-Transition erweitert?**
   - Datei: `crates/rt-persistence/src/repo.rs` Funktion `backfill_broker_order_id`
   - Prüfe: CASE-Expression transitioned auch aus `uncertain` nach `acknowledged` (oder äquivalenter Logik)?

3. **CV-A1c: Python Positions-Check?**
   - Datei: `tools/donchian_signal.py`
   - Prüfe: Gibt es vor dem `INSERT INTO signals` eine SELECT auf `positions WHERE closed_at IS NULL`? Was passiert bei Treffer?
   - Sekundäre Frage: SELECT auf offene `orders` in nicht-terminalen Status?

4. **CV-A1d: Duplicate-Position Check?**
   - Datei: `crates/rt-risk/src/checks/duplicate_position.rs` (sollte neu sein)
   - Prüfe: Existiert? In `checklist.rs` registriert? Unit-Tests für approve/reject/boundary?

5. **CV-A2: FX gefixt?**
   - Datei: `crates/rt-risk/src/lib.rs`
   - Prüfe: Ist `default_usd_per_chf_fallback()` noch 1.10? (Das allein reicht nicht — die Runtime-Config kann ihn überschreiben.)
   - Prüfe auf Prod: `sudo cat /etc/razortrade/daemon.toml | grep usd_per_chf_fallback` — was steht drin?
   - Prüfe: `systemctl cat razortrade-signals.service` — welcher FX-Wert in ExecStart?
   - Vergleich mit aktuellem Realwert (SNB-Referenzkurs o.ä.) — maximaler Drift 1% akzeptabel, mehr ist Findings.

### Zweite Ebene — neue Angriffsvektoren

Gemini soll NICHT nur die 2 BLOCKER re-verifizieren. Strike neue Vektoren die Claude im heutigen Audit NICHT abgedeckt hat:

#### Vektor G1 — Clock-Skew und Zeitzonen

Claude hat die Timer-Datei gesehen (`razortrade-signals.timer`) mit `OnCalendar=*-*-* 00/4:00:00 UTC` (explizit UTC). Prüfe aber:
- Wie ist `created_at`, `expires_at`, `processed_at` in Python vs. Rust normalisiert?
- Python: `datetime.now(timezone.utc).replace(microsecond=0).isoformat()` → `"2026-04-20T09:15:00+00:00"`
- Rust: `Utc::now().to_rfc3339()` → `"2026-04-20T09:15:00.123456789+00:00"` (mit Nanosekunden)
- `signals.expires_at` Vergleich in Rust: wie genau wird gemacht? String-Compare? Parse zu DateTime?
- Risiko: Wenn Python ohne Mikrosekunden schreibt und Rust mit Mikros parst, gibt es Edge-Cases am Sekunden-Rand.

Suche in: `signal_processor.rs` wo `expires_at` gelesen/verglichen wird.

#### Vektor G2 — Supervisor-Latenz vs. Fill-Geschwindigkeit

Kill-Switch-Supervisor tickt jede 60s (per Audit-Dokumentation und `docs/ARCHITECTURE.md:100`). Fill-Reconciler tickt im Sekunden-Bereich. Adverse Mark-Move von -1200 CHF kann in <60s passieren (Flash-Crash).

- Frage: Wird `EffectivePnlBudgetExhausted` innerhalb des 60s-Supervisor-Ticks erkannt, wenn Fills zwischen Ticks einlaufen?
- Suche: `crates/rt-daemon/src/main.rs` — wie ist der Supervisor instrumentiert? Eine einzige `loop { sleep(60s); ... }`? Oder reagiert er auf Events?
- Prüfe: Gibt es ein Event-Triggered-Path, der bei großen Fills sofort die Kill-Switch-Evaluierung triggert?

#### Vektor G3 — Panic-Close Reduce-Only-Semantik

`panic_close_leverage_positions` ist in Drop 18 als Routine erwähnt. Gemini soll konkret verifizieren:
- Findet eine solche Funktion in `crates/rt-daemon/src/main.rs` oder `signal_processor.rs` oder separatem Modul?
- Verwendet sie `reduce_only=true` auf dem SendOrder-Call? Prüfe Kraken-API-Dokumentation ob diese Flag bei PI_XBTUSD überhaupt akzeptiert wird.
- Was passiert wenn panic-close selbst fehlschlägt (Network-Error Richtung Kraken)? Retry-Strategie?

#### Vektor G4 — Capital-Flows und NAV-per-unit unter Deposit

Python soll Deposits in `capital_flows` schreiben (docs/ARCHITECTURE.md:142). Wird dies aktuell gemacht? Suche:
- `tools/` — gibt es `capital_flows.py` oder ähnlich?
- Wenn nein: Wer updated `nav_per_unit` und `total_units` in `equity_snapshots`?
- `grep -rn "nav_per_unit\|total_units" crates/rt-daemon/src/` — wer schreibt diese Felder?

Risiko falls kein Writer existiert: NAV-Drawdown-Berechnung ist broken, Kill-Switch-Guard 2 feuert entweder falsch oder gar nicht.

#### Vektor G5 — Backup-Recovery getestet?

`razortrade-backup.service` und `.timer` existieren. Aber:
- Wurde jemals eine Recovery getestet? `sqlite3 razortrade.sqlite .restore backup.sqlite` sollte funktionieren, aber auch Schema-Konsistenz (Migrations) muss stimmen.
- Ist die Backup-Datei auf einen zweiten Host repliziert? (Kommentar in `razortrade-backup.service` erwähnt "offsite push" — prüfe ob das implementiert und läuft.)

---

## 4. Strikte Regeln für den Audit

Diese Regeln sind **nicht verhandelbar**. Claude hat sich im heutigen Audit an diese Regeln gehalten (modulo einer gefixten Halluzination siehe OF-A2 in DROP_19_TODO.md). Gemini muss denselben Standard halten.

1. **Keine Halluzinationen.** Jedes Finding muss eine Code-Referenz haben: `Datei:Zeile-Zeile`. Wenn Zeilen-Nummer nicht angebbar: "Aus Grep-Output (keine verifizierte Zeile)" — explizit markieren.

2. **Keine Spekulationen.** Wenn ein Detail fehlt um ein Finding zu bewerten, nicht raten — konkret benennen: *"Um dieses Finding zu schließen, brauche ich den Inhalt von X, der nicht im bereitgestellten Material war."*

3. **Keine Best-Practice-Vorträge ohne Finanz- oder Operations-Impact.** Kein "würde ich stylistisch anders machen". Nur Findings die zu Geldverlust, Invalid State, Datenverlust, oder Outage führen.

4. **Prüfe gegen aktuellen Code, nicht gegen alte Audits.** Wenn ein Drop-18-Audit-Finding als "gefixt" markiert ist, prüfe im aktuellen Source, ob es tatsächlich gefixt ist. Nicht vertrauen, sondern nachsehen.

5. **Separiere den Report nach Schweregrad:**
   - CRITICAL VULNERABILITIES (deterministische Geld-Verluste)
   - LOGICAL/MATHEMATICAL FLAWS (Formel-Fehler im Risikomanagement)
   - OPERATIONAL FRAGILITY (führt zu Crashes oder verlorenen Signalen)
   - ARCHITECTURAL CLEARANCE (was passt trotz Stresstest)

6. **Skalierungs-Relevanz:** Jedes Finding markieren ob es bei 300 CHF, 3000 CHF, 30'000 CHF kritisch wird. Einige Flaws sind bei kleinem Kapital egal, werden bei Scale kritisch.

7. **Deutsch oder Englisch?** Der Operator (Walid) arbeitet auf Deutsch. Report darf Englisch sein (für technische Präzision), aber Executive Summary auf Deutsch.

---

## 5. Datei-Inventar das Gemini braucht

Alle in der Audit-Tarball Session von Drop 19 benötigten Dateien. Soll-Pfad-Inventar:

### Rust-Sources (crates/)
- `rt-core/src/*.rs` — Domain-Typen (Signal, Order, Position, PortfolioState, MarketSnapshot, Sleeve, Side, OrderStatus, SignalStatus)
- `rt-risk/src/checklist.rs` + `rt-risk/src/checks/*.rs` — Pre-Trade-Checks
- `rt-risk/src/kill_switch.rs` + `rt-risk/src/lib.rs` — Supervisor + RiskConfig
- `rt-risk/src/position_sizing.rs` — ATR-basierte Quantity-Berechnung
- `rt-persistence/src/repo.rs` + `models.rs` — Database-API
- `rt-persistence/migrations/*.sql` — Schema
- `rt-execution/src/lib.rs` — Broker-Trait
- `rt-kraken-futures/src/*.rs` — Kraken-Client (REST + WS + Orderbook)
- `rt-daemon/src/*.rs` — Main-Binary, Supervisor, Fill-Reconciler, Equity-Writer
- `rt-dashboard/src/*.rs` — Read-Only-Frontend (geringe Audit-Priorität)

### Python (tools/)
- `donchian_signal.py` — Signal-Generator
- `indicators.py` — Donchian, ATR Berechnung
- `inject_test_signal.py` — Test-Helper (nicht Production-Pfad)
- *Falls ein `capital_flows.py` in Drop 19 hinzugefügt wurde, auch das prüfen.*

### Systemd + Infra (Prod-State)
- `/usr/lib/systemd/system/rt-daemon.service`
- `/usr/lib/systemd/system/rt-dashboard.service`
- `/usr/lib/systemd/system/razortrade-signals.service`
- `/usr/lib/systemd/system/razortrade-signals.timer`
- `/usr/lib/systemd/system/razortrade-backup.service`
- `/usr/lib/systemd/system/razortrade-backup.timer`
- `/etc/caddy/Caddyfile`
- `/etc/oauth2-proxy/oauth2-proxy.cfg` (ohne .env Secrets)
- `/etc/razortrade/daemon.toml` (ohne secrets.env)

### Cargo
- Workspace `Cargo.toml`
- `Cargo.lock`
- `crates/*/Cargo.toml`

### Dokumentation
- `README.md`
- `docs/ARCHITECTURE.md`
- `docs/BUILDING.md`
- `DROP_18_DEPLOY.md` (historisch)
- `DROP_19_TODO.md` (diese Drop)
- `HANDOVER_FOR_GEMINI.md` (diese Datei)

---

## 6. Was Drop 19 bewusst OFFEN lässt (nicht im Scope)

- **CV-6** Periodische Reconciliation gegen Kraken REST (`/openOrders`, `/openPositions`). Verschoben auf Drop 20 nach Live-Gate. Begründung: neuer Writer-Task würde 72h-Clock resetten.
- **CV-7 Root-Cause** Parser-Bug bleibt. Raw-Logs werden in Drop 19 gesammelt, Fix in Drop 20.
- **LF-A1** Liquidation-Price-Check. Bei 2x Leverage-Cap und 300 CHF nicht kritisch.
- **LF-A2** Fees/Funding-Reserve in worst-case. Bei 300 CHF nicht materiell.
- **FX-Live-Feed.** Drop 19 setzt nur Config. Live-Pull in Drop 20.

Gemini soll diese Punkte im Audit als "Drop-20-Scope" markieren und NICHT als Blocker für Live-Gate, es sei denn Gemini findet einen nicht erkannten Weg wie sie Live-Gate blockieren würden.

---

## 7. Prod-Zugriff für den Audit

Falls Gemini Live-Verifikation braucht (z.B. `grep` gegen aktuellen daemon.toml):

- Host: `linda@178.105.6.124`
- Domain Dashboard: https://trader.ghazzo.ch (Google OAuth, whitelist `walid@ghazzo.ch`)
- SQLite-Path: `/var/lib/razortrade/razortrade.sqlite` (nur `sudo`-Zugriff)
- Config: `/etc/razortrade/daemon.toml` (root-readable, group razortrade)
- Secrets: `/etc/razortrade/secrets.env` (mode 0600, **NIE paste-bar in Audit-Reports**)
- Logs: `journalctl -u rt-daemon -f`

**Achtung:** Gemini darf Secrets weder anfordern noch in Reports zitieren. Der Audit-Report muss auch ohne Secrets-Kenntnis vollständig sein.

---

## 8. Minimale Testsuite die Gemini nach Audit laufen soll

```bash
cd ~/Workspace/razortrade
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
python3 tools/indicators.py  # eingebaute Selbsttests
```

Alle 4 müssen grün sein. Falls nicht: Finding in Report.

---

## 9. Output-Format erwartet von Gemini

Strukturierter Markdown-Report mit:
1. Executive Summary auf Deutsch (max. 10 Zeilen)
2. CRITICAL VULNERABILITIES (mit `Datei:Zeile` Referenzen)
3. LOGICAL & MATHEMATICAL FLAWS
4. OPERATIONAL FRAGILITY
5. ARCHITECTURAL CLEARANCE
6. CV-A1 / CV-A2 Verification Status (explizit)
7. Vektoren G1-G5 Findings (explizit)
8. Empfohlene Timeline für evtl. Drop 20 Fixes
9. Confidence-Level pro Finding: "verified by code" / "partially verified" / "unverified hypothesis"

---

**Erstellt:** 2026-04-20, nach Drop-18-Deploy und internem Drop-19-Audit.
**Autor:** Claude (Opus 4.7) im Auftrag des Operators Walid.
**Nächster Schritt nach diesem Handover:** Walid implementiert Drop 19 per `DROP_19_TODO.md`, deployed, restartet 72h-Clock, übergibt den gesamten Repo-Snapshot an Gemini zum Audit.
