# Drop 19 — Audit-Remediation TODO

**Audit-Datum:** 2026-04-20
**Auditor:** Claude (4-Mann Tiger-Team Rollenspiel, strikt Code-basiert)
**Quelle der Findings:** `AUDIT_2026_04_20.md`

Diese Liste enthält **nur** Fakten aus dem Audit mit expliziten Code-Zeilen-Referenzen. Keine Spekulation. Jede TODO-Zeile hat eine eindeutige Herkunft.

---

## 🔴 BLOCKER — müssen VOR Live-Gate gefixt sein

### CV-A1 — REST-Timeout erzeugt doppelte Position

**Herkunft:**
- `crates/rt-daemon/src/signal_processor.rs:402-416` — `Err(err)` (inkl. `Timeout`) führt zu `mark_order_failed` + `mark_signal_rejected`
- `crates/rt-persistence/src/repo.rs:357-365` — `backfill_broker_order_id` transitioned Status NUR aus `pending_submission`. Status `failed` bleibt `failed`.
- `tools/donchian_signal.py:285-393` — kein SELECT gegen `positions` oder `orders` vor INSERT
- `crates/rt-risk/src/checks/hard_limit.rs` (gesamt) — keine Duplicate-Position-Guard. Grep-bestätigt: `grep -rn "existing_position\|open_position\|current_position" crates/rt-risk/` → 0 matches im Produktionscode.

**Wirkung bei persistierendem 4h-Breakout:**
1. T0: Signal → Order → REST-Timeout (10s, `rest.rs:462`) → Kraken hat Order platziert → Daemon markiert `failed/rejected`
2. T0+Sekunden: WS liefert Fills → Orphan-Catcher findet Order via `cli_ord_id` → backfilled broker_order_id → Fill wird angewendet → **Position-Row existiert, Order-Status bleibt `failed`**
3. T0+4h: Python emittiert neues Signal (keine Positions-Abfrage) → Checkliste greift (keine Duplicate-Guard) → zweiter Submit → **Position verdoppelt sich**

**Fix-Anforderungen (alle 4 müssen umgesetzt werden):**

- [ ] **CV-A1a** — `signal_processor.rs:402-416` umbauen: `ExecutionError::Timeout` und `ExecutionError::Network` dürfen NICHT zu `mark_signal_rejected` führen. Neue Pfade:
  - Order: neuer Status `uncertain` (statt `failed`) — Schema-Migration nötig
  - Signal: bleibt `pending` mit einem Cooldown-Mechanismus (z.B. `retry_after_at` Spalte in signals) damit Processor nicht in Sofort-Loop geht
  - `ExecutionError::Rejected` und `ExecutionError::InvalidResponse` bleiben beim bisherigen Verhalten — das sind echte Ablehnungen

- [ ] **CV-A1b** — `repo.rs:backfill_broker_order_id` erweitern: Status-Transition muss auch aus `uncertain` (neu) nach `acknowledged` gehen. Alte Semantik `pending_submission → acknowledged` bleibt bestehen.

- [ ] **CV-A1c** — `tools/donchian_signal.py` vor `write_signal`: SELECT auf `positions WHERE instrument_symbol = ? AND closed_at IS NULL`. Wenn Position existiert mit kompatibler Richtung: kein Signal schreiben, stderr-Log mit Begründung, Exit 0.
  - Bonus: zusätzlich SELECT auf `orders WHERE status IN ('pending_submission', 'uncertain', 'submitted', 'acknowledged', 'partially_filled') AND instrument_symbol = ?` — schützt vor Race wenn Python läuft während Daemon einen Order gerade uncertain hat.

- [ ] **CV-A1d** — neuer Check `checks/duplicate_position.rs` + in `checklist.rs` registrieren: reject, wenn `portfolio` eine offene Position im gleichen `instrument_symbol` mit gleicher `side` aufweist. Unit-Tests analog zu bestehenden Checks (approve / reject / boundary).

**Tests (müssen neu geschrieben werden):**
- Integration-Test: Timeout auf submit → Order-Status `uncertain`, Signal `pending`, kein zweiter Submit bevor WS-Fill kommt oder Cooldown abläuft
- Unit-Test für `duplicate_position` Check: 3 Szenarien (keine Position, gleiche Richtung → reject, Gegenrichtung → approve)
- Python-Test (manuell oder pytest): bei existierender offener Position schreibt donchian_signal.py NICHT

---

### CV-A2 — FX-Rate hartcodiert 1.10, real ~0.78

**Herkunft:**
- `crates/rt-risk/src/lib.rs:134-137` — `default_usd_per_chf_fallback()` = `Decimal::new(110, 2)` = 1.10
- `crates/rt-daemon/src/equity_writer.rs:110` — verwendet `risk.usd_per_chf_fallback` bei jedem Snapshot
- `tools/donchian_signal.py:303` — CLI-Default `--fx-quote-per-chf 1.10`
- `/usr/lib/systemd/system/razortrade-signals.service` ExecStart-Zeile: `--fx-quote-per-chf 1.10`
- Realer USD/CHF am 2026-04-20 per web_search: **0.78** (Yahoo Finance, Investing.com, Federal Reserve H.10 alle konsistent bei ~0.78)

**Wirkung:**
- `signal_processor.rs:603`: `notional_quote = 300 CHF × 1.10 = 330 USD` beabsichtigt
- Bei echtem 0.78: 300 CHF = 234 USD. Wer 330 USD kauft riskiert effektiv 330/0.78 = **423 CHF** statt 300 CHF. **+41% Übergrößenordnung** auf jedem Einstieg.
- `equity_writer`: `total_equity_chf` wird systematisch **um 29% zu niedrig** berichtet (BTC→USD→CHF mit Division durch 1.10 statt 0.78)
- Kill-Switch-Grenzen (-1000/-1200 CHF) in der DB-Buchhaltung feuern erst bei realen CHF-Losses die durch den gleichen FX-Fehler nach unten gezogen werden → **reale Grenzen effektiv bei ~-1410/-1690 CHF**

**Fix-Anforderungen:**

- [ ] **CV-A2a** — Sofort-Fix (kein Code-Change, nur Config): aktuellen Kurs von SNB Referenzkurs oder ECB ziehen, auf 4 Dezimalstellen. In `/etc/razortrade/daemon.toml` `usd_per_chf_fallback` auf diesen Wert setzen. Rt-daemon restart.

- [ ] **CV-A2b** — systemd override anlegen: `/etc/systemd/system/razortrade-signals.service.d/override.conf` mit neuer ExecStart, die `--fx-quote-per-chf <aktueller Wert>` übergibt. Siehe Kommentar in `razortrade-signals.service` Zeile 18-22 — diese Vorgehensweise ist dokumentiert.

- [ ] **CV-A2c** — Drop-19-Code: nightly Cron-Job (neue Timer-Unit) der den SNB-/ECB-Referenzkurs zieht und in die daemon-Config schreibt. Einfachste Variante: bash-Script zieht per `curl` von `https://www.snb.ch/selector/de/mmr/exfeed/rss/daily/en/current` (XML) oder ECB `https://www.ecb.europa.eu/stats/eurofxref/eurofxref-daily.xml` (dann CHF-Umrechnung), schreibt den Wert in eine neue Datei `/etc/razortrade/fx.toml` die daemon.toml per include liest. Failure-Mode: bei Fetch-Fehler **Warnung in Log, alte Datei unverändert**, daemon läuft weiter. Muss in die RPM als zusätzliches systemd .timer + .service aufgenommen werden.

- [ ] **CV-A2d** — langfristig (Drop 20+): FX als Live-Feed in rt-daemon selbst. Nicht Teil von Drop 19.

**Tests:**
- Smoke-Test nach CV-A2a: `sqlite3 razortrade.sqlite "SELECT total_equity_chf FROM equity_snapshots ORDER BY id DESC LIMIT 1;"` Vergleich mit manueller Rechnung `BTC_amount × btc_usd × fx_new` sollte matchen (±1 CHF Rundung).

---

## 🟡 HIGH — vor Live-Gate stark empfohlen, aber nicht technisch blockierend

### LF-A1 — Maintenance Margin nicht modelliert, liquidation_price nicht konsumiert

**Herkunft:**
- `crates/rt-core/src/position.rs:53-64` — `distance_to_liquidation_atrs()` existiert
- `crates/rt-persistence/migrations/20260101000001_initial_schema.sql:79` — `liquidation_price TEXT` Spalte existiert
- `grep -rn "distance_to_liquidation\|liquidation_price" crates/` — alle Treffer nur in Schema/Models/Definition, **kein Consumer**

**Wirkung:** Bei Kapital ≤ 500 CHF und Leverage-Cap 2x ist das unkritisch (Liquidationspunkt sehr weit weg bei 2x). Mit wachsendem Kapital wird eine ehrliche Liq-Distance-Berechnung relevant.

**Fix-Anforderung (kann nach Live-Gate in Drop 20):**
- [ ] `equity_writer` oder neuer `position_risk_writer` Task: bei jedem Tick `/accounts` Kraken REST ziehen, `liq_price` pro offene Position in `positions.liquidation_price` schreiben.
- [ ] Neuer Check `checks/liq_distance.rs`: reject wenn erwartete Position einen Liq-Price näher als N ATRs (Config-Feld) am aktuellen Mark bringt.

### LF-A2 — Fees und Funding nicht im worst_case reserviert

**Herkunft:**
- `crates/rt-risk/src/checks/hard_limit.rs:85-86` — `worst_case_loss = notional × 0.10`. Keine Addition von Entry/Exit-Fee oder accrued Funding.

**Wirkung:** ~1% des Notional pro Woche Holding bei aktueller Config. Bei 300 CHF Kapital vernachlässigbar, skaliert linear.

**Fix-Anforderung (kann nach Live-Gate):**
- [ ] `hard_limit.rs`: `worst_case_loss += notional × (fee_rate_round_trip + expected_funding_per_held_duration)`. Werte in `RiskConfig`.

---

## 🟠 MEDIUM — Architektur-Schulden aus Drop 18 explizit zurückgestellt

### CV-6 — Kein Reconcile gegen Kraken-Ground-Truth

**Herkunft:** `fill_reconciler.rs` + `equity_writer.rs` sind die einzigen Konsumenten von Kraken-Endpoints. `grep "open_orders\|open_positions" crates/rt-daemon/src/` → 0 Treffer außer `equity_writer` (ruft nur `get_accounts`).

**Wirkung:** Wenn der Daemon crasht während ein Fill noch in Flight ist, oder wenn ein WS Blind-Window eine Fill-Snapshot-Lücke schafft, fehlt die Möglichkeit, die lokale DB mit Kraken's Sicht zu vergleichen.

**Fix-Anforderung (Drop 20 nach Live-Gate):**
- [ ] Beim Daemon-Start: `GET /openOrders` und `GET /openPositions` → Abgleich mit `orders`/`positions` Tabelle → Diskrepanzen loggen (nicht automatisch korrigieren — nur Alarm).
- [ ] Periodisch alle 15min gleicher Check.
- [ ] Dashboard-Widget "Kraken vs Daemon" — war ursprünglich für Turn 4 geplant, wurde verschoben.

### CV-7 Root Cause — WS-Parser-Bug bleibt offen

**Herkunft:** Drop 18 hat nur Raw-Logging hinzugefügt, nicht den Parser repariert. Conversation-History: Position `id=1` wurde manuell geschlossen, weil Daemon Fills wegen Deserialization-Failure nicht angewendet hatte.

**Fix-Anforderung (Drop 20 mit echten Raw-Payloads):**
- [ ] Raw-Logs auswerten die seit Drop 18 gesammelt wurden. Fehlende Variante im Parser identifizieren. `crates/rt-kraken-futures/src/messages.rs` um fehlende Variante ergänzen oder bestehende `#[serde(other)]` Catch-All einbauen.

### OF-A2 — signals.service silent loss bei SQLITE_BUSY im worst-case

**Herkunft:** `/usr/lib/systemd/system/razortrade-signals.service` — `Type=oneshot`, `Restart=on-failure`, `RestartSec=60s`, `StartLimitBurst=3`, `StartLimitIntervalSec=600`. **3 Retries à 60s innerhalb 10min** — das IST besser als ich im Audit ursprünglich gesagt habe. Korrektur: dieser Punkt ist faktisch bereits mitigiert, ich hatte im initialen Audit die Service-Unit nicht im Blick.

**Status: KEIN Fix nötig.** Nur Doku-Korrektur: ursprünglicher Audit-Claim war falsch.

---

## 🟢 VERIFIZIERT OK — kein Fix nötig

Diese Punkte wurden explizit geprüft und sind in Ordnung:

- **AC-1** L2-Orderbook-Concurrency: `tokio::sync::RwLock` um `HashMap<String, LocalOrderBook>`. Sequence-Gap und State-Flip nach `OutOfSync` passieren atomar unter dem Write-Lock (`orderbook.rs:170-177`). Reader bekommen entweder frische Daten oder `BookError::OutOfSync` (`orderbook.rs:242-246`).
- **AC-2** Kill-Switch LF-1: Hard trigger auf `effective_pnl ≤ -1200` feuert unconditional (`kill_switch.rs:117-126`), auch bei bereits soft-disabled Sleeve (Test `kill_switch.rs:247-254` grün).
- **AC-3** CV-1 Orphan-Catcher Fill-Application: `fill_reconciler.rs:226-355` recovered Fills auch wenn REST-Ack verloren ging. (Defekt bleibt nur am Order-Status-Label → CV-A1.)
- **AC-4** SQLite WAL asymmetric timeouts: Rust 5s (`repo.rs:40`), Python 30s (`donchian_signal.py:264-266`). Pythons 30s absorbiert alle realistischen Rust-Write-Bursts.
- **AC-5** Per-Fill Transaction Scope: `fill_reconciler.rs` ruft `apply_fill_to_order` + `apply_fill_to_position` + `record_fill_applied` als getrennte kurze Transactions. Kein 500ms-Lock-Window.
- **AC-6** applied_fills Idempotency: `has_fill_been_applied` (`fill_reconciler.rs:117`) schützt vor Doppel-Processing über Restart-Snapshot-Replays.

---

## Timeline

| Phase | Dauer | Inhalt |
|---|---|---|
| **Sofort (Tag 0)** | 5 Min | CV-A2a + CV-A2b: FX-Config auf realen Kurs setzen, daemon restart. Keine Code-Change. |
| **Drop 19 Code (Tag 0-1)** | ~3h Arbeit | CV-A1a-d: Order-Status `uncertain` + Signal-Cooldown + Python-Positions-Check + DuplicatePositionCheck |
| **Drop 19 Tests (Tag 1)** | ~1h | Unit- und Integration-Tests für neue Logik, cargo test grün |
| **Drop 19 Deploy (Tag 1)** | 15 Min | RPM build + scp + reinstall + Drills (analog Drop 18 Runbook) |
| **72h Paper-Clock (Tag 1-4)** | 72h | Beobachtung: keine `signal_expired` Häufung, keine `uncertain` Orders, WS/Fill-Reconciliation weiterhin sauber |
| **Live-Gate-Entscheidung** | Tag 4 | Wenn 72h sauber → Go. Sonst: extend bis sauber. |
| **Live-Start (Tag 4+)** | — | 300 CHF, `mode=live` |

**Früheste realistische Live-Gate:** 3 volle Tage nach Drop-19-Deploy.

---

## Definition of Done

Drop 19 gilt als done, wenn:

1. ☐ CV-A1a: `ExecutionError::Timeout` und `::Network` führen zu `status='uncertain'`/`signal='pending'` (nicht `failed`/`rejected`)
2. ☐ CV-A1b: `backfill_broker_order_id` transitioned aus `pending_submission` UND `uncertain` nach `acknowledged`
3. ☐ CV-A1c: `tools/donchian_signal.py` macht Positions-Check vor INSERT
4. ☐ CV-A1d: `checks/duplicate_position.rs` existiert, registriert, getestet, grün
5. ☐ CV-A2a: `usd_per_chf_fallback` in daemon.toml auf aktuellen SNB-Referenzkurs
6. ☐ CV-A2b: `razortrade-signals.service` ExecStart enthält aktuellen FX-Kurs (via override)
7. ☐ CV-A2c: FX-Updater Cron läuft nightly (kann optional auf Drop 20 verschoben werden wenn CV-A2a manuell dokumentierbar)
8. ☐ `cargo test --workspace` grün
9. ☐ `cargo clippy --workspace --all-targets -- -D warnings` grün
10. ☐ Deploy-Drill auf Prod: Drop-19-RPMs installiert, 6x services active
11. ☐ 72h Paper-Clock ab Drop-19-Deploy ohne unerwartete Events
12. ☐ Gemini oder anderer externer Auditor bestätigt CV-A1 + CV-A2 als gefixt
