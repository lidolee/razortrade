# razortrade — Session Closeout (2026-04-19)

**Status:** Foundation + Kraken Futures client + Daemon-Skeleton mit public/private WS feeds.
Compile-Check auf P620 noch ausstehend. Kein Code Live.

---

## Was steht (v4-Tarball)

### Crates — 6, ~4'900 LOC

| Crate | Status | Was drin |
|---|---|---|
| `rt-core` | ✅ Komplett | Domain-Typen inkl. `PortfolioState` mit NAV-per-unit und adverse-unrealized-P&L-Tracking |
| `rt-risk` | ✅ Komplett | Alle 5 Checks + Kill-Switch + ATR-Sizing, Unit-Tests grün |
| `rt-persistence` | ✅ Komplett | SQLite/WAL, 2 Migrations (initial + nav_tracking) |
| `rt-execution` | ✅ Komplett | `Broker`-Trait (async_trait) |
| `rt-kraken-futures` | ✅ Mostly | REST (HMAC-SHA512, Nonce), WS (Challenge-Flow, Reconnect, Orderbook-State-Machine mit Seq-Validation), Private Feeds (fills, open_orders) |
| `rt-daemon` | 🟡 Partial | Kraken-WS→MarketData→SignalProcessor→Checklist→SQLite verdrahtet. **ABER**: `stub_portfolio_state()` statt echter State-Load, Order-Submission noch nicht verdrahtet, KillSwitchEvaluator noch nicht im Supervisor aktiv |

### Verifizierte Fakten (nicht halluziniert — alle per web_search gegen Kraken-Docs geprüft)

- REST-Signing: SHA-256(postData‖nonce‖endpointPath) → HMAC-SHA-512(base64_decode(secret)) → base64
- WS-URL: `wss://futures.kraken.com/ws/v1` (demo: `demo-futures.kraken.com`)
- WS-Challenge: Client `{event:challenge, api_key}` → Server `{event:challenge, message:<UUID>}` → UUID signieren (kein Concat mit Nonce/Path) → Subscribe mit `api_key + original_challenge + signed_challenge`
- Book-Feeds: `book_snapshot` (full) + `book` (per-level deltas, side/price/qty/seq)
- Private Feeds: `fills_snapshot`/`fills`, `open_orders_snapshot`/`open_orders`

---

## Resume-Point — konkret die nächsten Schritte

### 0. Compile-Check (≤ 10 Min)

```bash
cd ~/receiptflow  # oder wo auch immer
tar xzf razortrade-v4-private-feeds.tar.gz
cd razortrade
cargo check --workspace 2>&1 | tee /tmp/cargo-check.log
```

**Wenn Fehler:** Log mir zeigen, ich fix es.
**Wenn grün:** Weiter zu Schritt 1.

### 1. Portfolio-State real laden (geschätzt 1 Session)

In `crates/rt-daemon/src/signal_processor.rs`, Funktion `stub_portfolio_state()` ersetzen durch:
- Query `equity_snapshots` für aktuellen NAV + drawdown-Metriken
- Query `positions` WHERE `closed_at IS NULL` für aktive Positionen
- Query `capital_flows` für NAV-per-unit-Berechnung
- Berechnung `leverage_sleeve_effective_pnl` aus realisiertem + adverse unrealized

### 2. Order-Submission verdrahten (1 Session)

Im `SignalProcessor`, wenn `ChecklistOutcome::Approved`:
- `match signal.sleeve` → `Broker::KrakenFutures | KrakenSpot | IBKR`
- Instanz aus `BrokerRegistry` holen (braucht neuen Typ)
- `broker.submit_order(...)` aufrufen
- Response in `orders`-Tabelle persistieren

### 3. Fill→Position-Reconciliation (1 Session)

Consumer-Task der `FillsStore` beobachtet:
- Auf `Fill`-Event: `orders.status` updaten, `positions` neu berechnen, `unrealized_pnl` neu rechnen
- `equity_snapshot` nach Fill triggern

### 4. KillSwitch im Supervisor aktivieren (½ Session)

Aktuell loggt Supervisor nur — muss bei Trigger:
- Alle offenen Orders canceln via `broker.cancel_all()`
- Signal-Processing einfrieren (Flag in `SignalProcessor`)
- Event in `kill_switch_events` persistieren

### 5. rt-ibkr Crate (2-3 Sessions)

Client Portal Web API ist aufwändiger als Kraken:
- Gateway muss lokal laufen (oder im Container auf Hetzner)
- Session-Keepalive alle 60s
- OAuth-Flow für initiale Auth

### 6. Python Signal-Generator (1-2 Sessions)

- `signals/donchian.py` — 4h-Cadence, OHLCV von Kraken public REST, Donchian + ATR
- `signals/capital_flows.py` — monatliche Einzahlungen in `capital_flows` schreiben
- Keine ML nötig für MVP, HMM später

### 7. Infrastruktur Hetzner (1 Session)

- **Server:** CX32, **Falkenstein** (FSN1) — nicht NBG1, Latenz-Argument von Gemini ist für 4h-Strategie irrelevant
- Rocky Linux 9 Base-Image
- `firewalld`: nur Port 22 von deiner CH-IP
- User `razortrade` anlegen, systemd-Unit deploy'en (`deploy/systemd/razortrade.service` ist fertig)
- Secrets nach `/etc/razortrade/secrets.env` (0600, owner razortrade)
- **Zusatz Claude:** `/var/backups/razortrade/` einrichten, hourly SQLite `.backup` via systemd timer, wöchentlicher rsync zurück auf P620

### 8. Paper-Trading-Validierung (72h gegen Demo-API)

- Kraken Demo-Credentials holen (demo-futures.kraken.com)
- `config.kraken.environment = "demo"` setzen
- 72h Dauerlauf, beobachten: WS-Stabilität, keine Memory-Leaks, keine Deadlocks
- Kill-Switch manuell triggern (Fake-Drawdown in SQLite injizieren) → muss Orders canceln

---

## Harte Gates bis Live-Gang

1. ☐ `cargo check --workspace` grün
2. ☐ `cargo test --workspace` grün (alle Risk-Tests weiterhin passieren)
3. ☐ `cargo clippy --workspace -- -D warnings` grün
4. ☐ Paper-Trading 72h ohne Intervention
5. ☐ Manueller Kill-Switch-Test funktioniert
6. ☐ SQLite-Backup-Strategie läuft automatisch
7. ☐ Monat-1-Kill-Switch (-300 CHF realized) getestet

**Dann** — und nur dann — mit 300-500 CHF Echtgeld starten. Nicht früher.

---

## Bewusste offene Themen (nicht vergessen)

- **Steuerliche Einordnung:** Du hast entschieden, das zunächst zu ignorieren. Klare Warnung weiterhin: ESTV Kreisschreiben Nr. 36, algo trading erfüllt mindestens 3 der 5 Kriterien für gewerbsmässigen Wertschriftenhandel → ~35-45% effektive Steuer auf Gewinne statt Kapitalgewinnsteuerfreiheit. Beim ersten signifikanten Gewinn Steuerberater einbeziehen.
- **OPM / Drittkapital:** Explizit aus der Roadmap raus. Nicht anfassen ohne FINMA-Abklärung.
- **IBKR ETF-Sleeve kann verschoben werden:** Falls rt-ibkr zu aufwändig wird, MVP erstmal nur mit Kraken-Spot + Kraken-Futures. Welt-ETF-Sleeve kann initial manuell über IBKR-Web-UI gekauft werden, Daemon sieht es als statische Position.

---

## Gemini-Cross-Validation-Notizen

Die letzte Infrastruktur-Empfehlung enthält zwei überzeichnete Punkte:

1. **NBG1 vs FSN1 Latenz:** Claim von 10-15 ms Unterschied ist physikalisch nicht plausibel (realistisch 1-2 ms via DE-CIX). Für 4h-Strategie sowieso irrelevant. Empfehlung: **FSN1** wegen Verfügbarkeit/Preis.
2. **"5-Sekunden-Disconnect = blind beim Flash-Crash":** Dramatisierung. Stops liegen beim Broker, nicht client-side. Risiko ist real (Desync beim Reconnect), aber nicht apokalyptisch.

Der Kernpunkt (Datacenter statt lokal) bleibt korrekt — nur die Begründung war HFT-Denken.

---

**Geschrieben: 2026-04-19, Session-Ende.**
**Resume-Command:** `cargo check --workspace` auf P620, dann Fehler-Log zeigen.
