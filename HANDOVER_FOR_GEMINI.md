# Handover-Dokument für Google Gemini — razortrade Drop 19 Audit (Post-Deploy)

**Ziel:** Gemini soll einen unabhängigen Audit des razortrade-Repos durchführen, nachdem Drop 19 vollständig deployed ist.

**Diese Datei muss vor dem Audit gelesen werden.** Sie enthält:
1. Kompakter System-Kontext
2. Was in Drop 18 + Drop 19 gefixt wurde
3. Was genau Gemini auditieren soll (Verifikation + neue Vektoren)
4. Strikte Regeln für den Audit

---

## 1. System in einem Absatz

**razortrade** ist ein algorithmischer Swing-Trader für Kraken Futures (Perpetuals). Python-Signale (4h Donchian-Breakout + ATR) werden in SQLite geschrieben. Ein Rust-Daemon pollt SQLite im 1-Hz-Takt, validiert jedes Signal durch eine 8-Punkte-Checkliste (Drop 19) und sendet bei Approval einen Limit-Order an Kraken. Ein Kill-Switch-Supervisor läuft alle 60s UND wird event-getriggert nach jedem Leverage-Fill (Drop 19 Part B). Fills kommen via Kraken WebSocket, werden über einen Fill-Reconciler in SQLite persistiert. Ein periodischer REST-Reconciler (Drop 19 CV-6) vergleicht lokale vs. Broker-Orders alle 60s. Ein Read-Only-Dashboard (Rust/Axum) zeigt Portfolio-State hinter Google OAuth + Caddy TLS.

**Hardware/Infra:** Hetzner CX32 VPS, Rocky Linux 10.1, Domain `trader.ghazzo.ch`. Einzel-User-System. Start-Kapital 300 CHF geplant für Live-Gate Apr/Mai 2026. Skalierungsziel 10'000+ CHF.

**Nicht Scope:** HFT (Latenz-Budget 5000ms), ML, Drittkapital.

---

## 2. Was in Drop 18 + Drop 19 gefixt wurde

### Drop 17 → 18 (abgeschlossen, 2026-04-20 vormittags)

- **CV-1** `orders.cli_ord_id` Spalte + Orphan-Catcher in `fill_reconciler.rs` — Fills ohne matching `broker_order_id` werden via `cli_ord_id` wieder an die lokale Order attached
- **CV-2** `fill_id` Audit sauber (Kraken-native UUIDs, `(broker, fill_id)` Composite PK in `applied_fills`)
- **CV-3** SQLite `busy_timeout=30s` auf Python-Seite + `Restart=on-failure` auf systemd-Service für `razortrade-signals`
- **CV-7** Raw-Logging bei WS-Deserialization-Failures (nur Logging, Root-Cause in Drop 19 adressiert)
- **LF-1** Hard-Kill-Switch: `EffectivePnlBudgetExhausted` → `PanicClose` → `panic_close_leverage_positions` routine
- **LF-3** Equity-Writer filtert Zero-Value-Snapshots
- **OF-2** Signal-Expiry 30 Minuten (Schema + Python + Rust-Read + Processor-Guard)

### Drop 19 (abgeschlossen, 2026-04-20 nachmittags/abends)

Alle 9 Arbeitsstränge deployed und getestet. Git-History auf `main` ab Commit der Drop-18-Baseline bis HEAD zeigt 9 Commits mit den Tags unten.

#### CV-A1 — REST-Timeout-Doppelposition

**Gefixt in mehreren Ebenen:**

1. **Neuer Order-Status `uncertain`** (Migration `20260420000003_drop19_uncertain_status.sql`):
   - Orders mit `status = 'uncertain'` sind an Kraken submittet aber Response ging verloren
   - `signals.retry_after_at` Cooldown-Feld + partial index für effiziente Abfrage
   - Signal-Processor setzt `uncertain` + 90s Cooldown bei `ExecutionError::Timeout`/`::Network`
2. **Uncertain-Resubmit** (`signal_processor.rs::try_reconcile_uncertain_order`):
   - Vor Resubmit: `broker.get_order_by_cli_ord_id(cli)` (neuer Broker-Trait-Endpoint)
   - Wenn Kraken die Order kennt → `broker_order_id` backfillen, Status → `acknowledged`, kein Resubmit
   - Wenn Kraken keine Order hat → Resubmit mit gleicher Row + gleichem `cli_ord_id` (Kraken dedupliziert venue-seitig)
   - Outcome-Enum `UncertainReconcileOutcome` mit 5 Varianten (NoExistingOrder, AlreadyReconciled, ResubmitWithOrderId, LookupFailedRetry, MalformedUnrecoverable)
3. **`backfill_broker_order_id` transitioniert jetzt auch aus `uncertain`** (vorher nur aus `pending_submission`)
4. **DuplicatePositionCheck** (`crates/rt-risk/src/checks/duplicate_position.rs`):
   - Neuer Check in `checklist::standard()`
   - Lehnt Signale ab wenn bereits offene Leverage-Position auf gleichem Instrument + gleicher Side
   - `RejectionReason::DuplicatePositionOpen { instrument_symbol, side, existing_quantity }`
   - 7 Unit-Tests
5. **`OpenPositionSummary` + `PortfolioState.open_positions`**:
   - In rt-core: neues Struct mit `instrument_symbol`, `sleeve`, `quantity` (signed), seit LF-A1 auch `avg_entry_price`, `liquidation_price`, `leverage` (alle Option)
   - `portfolio_loader.rs` lädt via neues SQL `list_open_leverage_positions`
6. **Python-seitige Exposure-Guard** (`donchian_signal.py::check_existing_exposure`):
   - SELECT auf `positions WHERE closed_at IS NULL AND instrument_symbol = ?` VOR INSERT
   - Bei Match: kein Signal geschrieben, log `existing_exposure_detected`

**Drill-Ergebnisse (Prod, 2026-04-20 12:38-13:10 UTC):**

- Uncertain-Resubmit: signal=5 processed, order=2 acknowledged, broker_order_id adoptiert
- DuplicatePositionCheck: signal=6 rejected mit kind=duplicate_position_open, qty=50
- Python `check_existing_exposure()`: BUY result="open PI_XBTUSD position in same direction", SELL result=None

#### CV-A2 — FX-Rate

**Gefixt:**
- `default_usd_per_chf_fallback()` in `rt-risk/lib.rs` geändert: alter Wert 1.10 (`Decimal::new(110, 2)`) → neuer Default 0.78 (`Decimal::new(78, 2)`)
- Prod `/etc/razortrade/daemon.toml`: `usd_per_chf_fallback = "0.78"` explizit gesetzt
- systemd-Override `/etc/systemd/system/razortrade-signals.service.d/override.conf`: `--fx-quote-per-chf 0.78`
- SNB-Referenzkurs 2026-04-17: 0.7823 — Drift <0.3%

#### Drop 19 Part B — Panic-Close Hardening

- **`Order.reduce_only: Option<bool>`** Feld in rt-core
- `panic_close_leverage_positions` setzt `reduce_only: Some(true)` (verhindert versehentliches Flip in Gegenrichtung)
- Regular Entries: `reduce_only: None`
- `rest.rs::submit_order` pipet `order.reduce_only` als `reduceOnly` zu Kraken
- **Event-Triggered Kill-Switch**: Nach jedem `apply_fill_to_position` mit `sleeve == "crypto_leverage"` sofortiger `evaluate_kill_switch_once` Aufruf (sowohl happy-path als auch orphan-catcher-Pfad in `fill_reconciler.rs`)
- Schliesst den Flash-Crash-Gap zwischen 60s-Supervisor-Ticks
- `evaluate_kill_switch_once` ist jetzt `pub(crate)` für cross-module Zugriff

#### Drop 19 Part C — Timestamp-Normalisierung (G1)

- Neues Modul `rt-core::time` mit `canonical_iso(dt)` und `now_iso()`
- Format: `SecondsFormat::Micros` + `Z`-Suffix (fix 27 Zeichen, lexikografisch sortierbar)
- Alle `Utc::now().to_rfc3339()` in rt-daemon + rt-persistence durch `rt_core::time::now_iso()` ersetzt (27 Fundstellen via sed)
- Python `donchian_signal.py`: `isoformat(timespec="microseconds").replace("+00:00","Z")`
- 3 Unit-Tests in rt-core

#### Drop 19 Part D — Dashboard Turn 5 (16:9 Viewport-Fit)

- Neues CSS-Grid-Layout mit `grid-template-areas`, 12 cols × 6 rows, fixe Höhen
- Gesamthöhe `calc(100vh - 68px)`, Cards mit `overflow: hidden`, Tabellen-Content scrollt intern
- Auto-Cycle Tabs alle 10s, Pause 20s bei User-Interaktion
- Zahlen-Pulse-Animation bei KPI-Änderungen
- 3 Sparklines (Total, Leverage, Realized) aus `/api/equity-history`
- Kill-Switch-Banner-Puls bei soft/hard-Trigger
- Fallback-Layout bei `(max-height: 720px) or (max-width: 1200px)`
- Nur index.html-Änderungen, main.rs unverändert

#### LF-A1 — Liquidation-Distance-Check

- `RiskConfig.min_liq_distance_atrs` Default 2.0 ATR (neuer Config-Parameter, `#[serde(default)]`)
- `RejectionReason::LiquidationTooClose { instrument_symbol, distance_atrs, min_required_atrs }`
- Neuer Check `crates/rt-risk/src/checks/liquidation_distance.rs`
- In `checklist::standard()` nach `DuplicatePositionCheck` eingefügt
- 7 Unit-Tests (NotApplicable-Pfade, Approve, Reject, Spot, Zero-ATR, andere Instrumente)
- **Feeding der Felder aktuell None in Drop 19** — alle `OpenPositionSummary::liquidation_price` kommen als `None` aus dem `portfolio_loader`. Check gibt konsequent `NotApplicable` zurück. Feld-Populierung aus Kraken-Account kommt in Drop 20. Dieser Check steht strukturell bereit.

#### LF-A2 — Fees + Funding Reserve

- `RiskConfig.expected_round_trip_fee_fraction` Default 0.001 (0.10% round-trip Taker bei Kraken Futures)
- `RiskConfig.expected_funding_reserve_fraction` Default 0.002 (0.20% ~3 Funding-Windows bei 24h Halt)
- In `hard_limit.rs::evaluate` Guard 4: `worst_case_loss = adverse_loss + fee_reserve + funding_reserve`
- 1 neuer Unit-Test `fees_plus_funding_reserve_tips_over_budget` (deckt genau den Boundary-Case ab wo Adverse-Move alleine approven würde aber Reserve das Budget reisst)

#### CV-6 — Periodischer REST-Reconciler

- Neues Modul `crates/rt-daemon/src/periodic_reconciler.rs`
- Tick: 60s
- `Broker::open_positions()` Trait-Endpoint hinzugefügt (default-impl: leer) + `OpenPositionEntry` struct in rt-execution
- `Database::list_live_orders()` in rt-persistence
- **Observational only in Drop 19**: 2 Drift-Klassen werden geloggt, keine State-Transitions:
  - Local hat `submitted`/`acknowledged`/`uncertain`/`partially_filled` aber Kraken kennt weder `broker_order_id` noch `cli_ord_id`
  - Kraken hat offene Order deren `broker_order_id` + `cli_ord_id` lokal nicht in live-Orders erscheint
- Auto-Actions (z.B. automatisches Cancel/State-Transition) frühestens Drop 20 nach 1 Woche Drift-Beobachtung

#### CV-7 — Raw-WS Sampled Trace

- `KrakenFuturesWsClient.text_counter: Arc<AtomicU64>` + Konstante `RAW_SAMPLE_PERIOD = 500`
- Jede 500. geparsed WS-Message wird mit Raw-JSON bei INFO geloggt
- Bei Kraken-Demo-Traffic ≈ 10 msg/s → ~1 Sample pro 50s → ~1700 Samples pro 24h
- Ergänzt das existierende Error-Path-Logging (verbatim raw-payload bei Parse-Failures)
- **Next action (Drop 20):** 24-48h laufen lassen, Samples aus Journal extrahieren, Fixture-Library + Parser-Coverage-Tests bauen. Die 2 historischen `ws_deserialisation_failures` im Dashboard wurden aus dem Journal rotiert; für deren Root-Cause wird Drop 20 auf neu auftretende Failures warten.

---

## 3. Was Gemini auditieren soll

### Primäre Verifikationsaufgabe

Prüfen dass Drop 19 die BLOCKER aus dem alten Handover (CV-A1, CV-A2) und die LF/CV-Punkte korrekt gefixt hat. Vertrauen auf Drill-Ausgaben nicht ausreichend — Gemini muss Code lesen.

Konkrete Fragen:

1. **CV-A1a: Uncertain-Path semantisch korrekt?**
   - Datei: `crates/rt-daemon/src/signal_processor.rs`
   - `Err(ExecutionError::Timeout | Network)` → `mark_order_uncertain` + `set_signal_retry_after`?
   - Werden Timeout + Network von anderen Error-Typen (`InvalidOrder`, `InsufficientFunds`) getrennt behandelt?
   - Ist die Unterscheidung vollständig oder gibt es Error-Typen die weder zu `uncertain` noch zu `mark_signal_rejected` routen?

2. **CV-A1b: Uncertain-Resubmit-Flow**
   - `try_reconcile_uncertain_order` + `UncertainReconcileOutcome` — sind alle 5 Varianten-Pfade korrekt verdrahtet?
   - `get_order_by_cli_ord_id` wird vor Resubmit aufgerufen?
   - Bei `AlreadyReconciled` kein Double-Submit?
   - `resubmit_into_order_id` nutzt dieselbe Row + dasselbe `cli_ord_id`?

3. **CV-A1c: Duplicate-Guard auf beiden Ebenen**
   - Python: `check_existing_exposure` wird wirklich **vor** INSERT aufgerufen?
   - Rust: `DuplicatePositionCheck` in `checklist.rs::standard()` registriert?
   - Unit-Tests decken: same-side-reject, opposite-side-allow, other-instrument-allow, closed-position-allow?
   - Race-Condition zwischen Python-Check und Rust-Check: kann in dem Spalt was passieren?

4. **CV-A2: FX überall konsistent**
   - Alle Pfade die `fx_quote_per_chf` nutzen (equity_writer, donchian_signal, signal metadata) — nutzen sie den aktuellen Wert?
   - Gibt es Caching-Layer der alte Werte behält?

5. **Part B: Panic-Close Hardening**
   - `Order.reduce_only` wird in `rest.rs` zu Kraken's `reduceOnly`?
   - Kraken-API-Doku verifizieren (nicht raten) dass PI_XBTUSD `reduceOnly` akzeptiert
   - Event-Triggered-Kill-Switch: feuert auch im orphan-catcher-Pfad?
   - Flash-Crash-Szenario: 2 Fills in 500ms — doppelter panic-close-fire? Es gibt `has_active_hard_trigger`-Guard, reicht der?

6. **Part C: Timestamp-Konsistenz**
   - Python + Rust produzieren beide `"2026-04-20T10:45:00.123456Z"` — Runtime verifizieren
   - Vergleich `signal.expires_at <= now` in Rust: String-Compare oder Parse? Bei String-Compare: sortiert lexikografisch = chronologisch?
   - Sub-Sekunden-Edge-Case: Wenn Python mit µs schreibt und Rust mit `DateTime::parse_from_rfc3339` liest, Round-Trip stabil?

7. **LF-A1: Liquidation-Distance**
   - Check ist struktureller Platzhalter (immer NotApplicable in Drop 19). Ist die `NotApplicable`-Logik robust?
   - Wenn 1 von 3 Positionen `liquidation_price = Some(x)` hat und 2 `None`: wird nur die eine geprüft?
   - Ist das semantisch richtig oder ein Fallstrick für Drop 20?

8. **LF-A2: Fee/Funding-Reserve**
   - Gemini: sind 0.10% round-trip + 0.20% funding **realistisch** für Kraken Futures bei 24h-Halt?
   - Unit-Test deckt Boundary-Case — deckt er auch den Fall wo Reserve allein (ohne Adverse-Move) das Budget reisst?

9. **CV-6: Periodic Reconciler Observability**
   - Loggt er jedes Drift, oder nur Stichproben?
   - Wenn Kraken länger down ist (open_orders-Call timeoutet): Task-Panik oder silent retry?
   - Welche Dashboard-Metrik zeigt CV-6-Aktivität? Oder nur Journal-Grep?

10. **CV-7: Sampled Raw-Log**
    - Probabilities-OK: bei `RAW_SAMPLE_PERIOD = 500` werden seltene Message-Typen (open_orders-delta, fills, challenge) mit hoher Wahrscheinlichkeit verpasst
    - Argument für Gemini: reicht stochastisches Sampling, oder sollte Drop 20 pro Message-Typ mindestens 1 Sample sichern?

### Zweite Ebene — neue Angriffsvektoren

Gemini soll NICHT nur re-verifizieren. Strike neue Vektoren:

#### Vektor G1 — SQLite-Race zwischen Python + Rust

Drop 19 fügt `check_existing_exposure` in Python hinzu. Python prüft + schreibt in 2 separaten Statements. Zwischen SELECT und INSERT kann (theoretisch) ein anderer Prozess eine Position schliessen oder öffnen. Transaktions-Isolation?

- `donchian_signal.py` verwendet default isolation mode. BEGIN/COMMIT explizit?
- Was passiert bei konkurrenter Fill-Reconciler-Transaktion?

#### Vektor G2 — Event-Trigger-Reentrancy

Event-Triggered-Kill-Switch im fill_reconciler ruft `evaluate_kill_switch_once` auf. Diese Funktion macht DB-Writes. Der Call passiert während die Fill-Reconciler-Transaktion möglicherweise noch hält (siehe `apply_fill_to_position` Rückgabe-Punkt):

- Ist `evaluate_kill_switch_once` reentrant-safe?
- Wenn panic_close_leverage_positions submit_order aufruft, dessen Response ein weiteres Fill triggert, das wiederum `evaluate_kill_switch_once` anruft: Endlosschleife?
- Der `hard_already`-Guard am Anfang sollte es stoppen — aber nur wenn `record_kill_switch` schon durch ist. Race im 10ms-Fenster?

#### Vektor G3 — CV-6 Reconciler + Fill-Reconciler Inkonsistenz

Beide Tasks laufen parallel. `fill_reconciler` transitioniert Orders `acknowledged → filled`. `periodic_reconciler` macht `list_live_orders()` mit eigener Connection. Mögliche Lag-Inkonsistenz:

- `fill_reconciler` markiert Order `filled`, aber Kraken's `open_orders` zeigt sie noch weil das REST-Cache langsam ist
- `periodic_reconciler` loggt "Kraken has order we don't know" obwohl kein Fehler vorliegt
- Schweregrad niedrig (nur Log-Noise), aber prüfen ob das Rate-Alarme triggert

#### Vektor G4 — Migration-Backward-Compatibility

Migration `20260420000003_drop19_uncertain_status.sql` fügt `signals.retry_after_at` + enum-Wert `uncertain` hinzu. Bei Rollback (alter Binary auf neuem Schema) → was passiert?

- Rollback-Tauglichkeit von Drop 19 überhaupt gegeben?
- Wenn nicht: ist das als "einweg" dokumentiert?

#### Vektor G5 — 72h Paper-Clock Definition of Done

Operator plant 72h Paper-Lauf vor Live-Gate. Prüfe:

- Welche Kennzahlen müssen grün sein? Zero-Warns in Journal? Zero-Rejections auf Duplicate-Position? Zero-Uncertains?
- Ist das dokumentiert? Wenn nein: Finding als Operational Fragility (Live-Gate ohne klare Definition of Done ist Risiko)

---

## 4. Strikte Regeln für den Audit

Unverändert zu vorherigem Handover:

1. **Keine Halluzinationen.** Jedes Finding muss `Datei:Zeile-Zeile`. Wenn nicht angebbar: "aus Grep-Output" explizit markieren.
2. **Keine Spekulationen.** Fehlende Details namentlich fordern statt raten.
3. **Kein Stylistik-Feedback.** Nur Findings die zu Geldverlust/Invalid State/Datenverlust/Outage führen.
4. **Prüfe aktuellen Code, nicht alte Audits.** Als "gefixt" markiert heisst nichts — im Source nachsehen.
5. **Report nach Schweregrad:**
   - CRITICAL VULNERABILITIES (deterministische Geld-Verluste)
   - LOGICAL/MATHEMATICAL FLAWS (Formel-Fehler im Risikomanagement)
   - OPERATIONAL FRAGILITY (führt zu Crashes oder verlorenen Signalen)
   - ARCHITECTURAL CLEARANCE (was passt trotz Stresstest)
6. **Skalierungs-Relevanz pro Finding:** 300 CHF / 3000 CHF / 30'000 CHF.
7. **Executive Summary auf Deutsch.** Rest darf Englisch.

---

## 5. Was Drop 20 offen lässt

- **Liquidation-Price-Feeding** in `portfolio_loader` aus Kraken-Account. Check ist strukturell da, Feld bleibt `None` bis Drop 20
- **FX-Live-Feed**: Drop 19 nutzt Config-Wert, Drop 20 pullt von SNB/ECB
- **CV-6 Auto-Actions**: aktuell nur Logging, Drop 20 aktiviert auto-cancel
- **CV-7 Fixture-Library**: 24-48h Samples aus Drop 19 ernten, Fixture-Coverage-Tests
- **CV-7 Root-Cause** der 2 historischen Parser-Failures: Journal-Rotation hat sie entfernt; Drop 20 reagiert auf neu-auftretende Failures

Gemini soll diese Punkte als Drop-20-Scope markieren und nicht als Live-Gate-Blocker, es sei denn ein konkreter Geldverlust-Pfad wird erkannt.

---

## 6. Prod-Zugriff

- Host: `linda@178.105.6.124`
- Domain: https://trader.ghazzo.ch (Google OAuth, whitelist `walid@ghazzo.ch`)
- SQLite: `/var/lib/razortrade/razortrade.sqlite` (sudo)
- Config: `/etc/razortrade/daemon.toml`
- Secrets: `/etc/razortrade/secrets.env` (mode 0600, **NIE zitieren**)
- systemd-Override Signals: `/etc/systemd/system/razortrade-signals.service.d/override.conf`
- Logs: `journalctl -u rt-daemon -f`

---

## 7. Testsuite die Gemini laufen soll

```bash
cd ~/Workspace/razortrade
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
python3 tools/indicators.py
```

Erwartung: Alle 4 grün. `cargo test -p rt-risk` sollte >= 51 Tests zeigen (stand vor Audit-Deploy).

**Hinweis**: Auf der Prod-VM ist clippy möglicherweise nicht installiert (`error: no such command: clippy`). In diesem Fall Finding als "cargo clippy nicht installiert auf Prod/Stage, Test übersprungen".

---

## 8. Output-Format

Strukturierter Markdown-Report:
1. Executive Summary auf Deutsch (max. 10 Zeilen)
2. CRITICAL VULNERABILITIES (mit Datei:Zeile-Referenzen)
3. LOGICAL & MATHEMATICAL FLAWS
4. OPERATIONAL FRAGILITY
5. ARCHITECTURAL CLEARANCE
6. CV-A1 / CV-A2 / Part-B / Part-C / LF-A1 / LF-A2 / CV-6 / CV-7 Verification Status (explizit pro Punkt)
7. Vektoren G1-G5 Findings (explizit)
8. Timeline-Empfehlung für Drop 20
9. Confidence-Level pro Finding: "verified by code" / "partially verified" / "unverified hypothesis"

---

**Erstellt:** 2026-04-20, nach Drop-19-Deploy (alle 9 Parts grün auf Prod).
**Autor:** Claude (Opus 4.7) im Auftrag des Operators Walid.
**Nächster Schritt nach Handover:** 72h Paper-Clock, dann Live-Gate 300 CHF vorausgesetzt Gemini-Audit + Paper-Clock beide grün.
