#!/usr/bin/env bash
# razortrade Kill-Switch Reset Tool
#
# Wann nutzen: nach einem ausgelösten Soft- oder Hard-Kill-Switch,
# nachdem der Operator die Ursache manuell untersucht hat und das
# System wieder freigeben möchte. Markiert alle aktuell aktiven
# kill_switch_events als resolved mit einem Timestamp + Grund.
#
# Dies erlaubt neue Trades NICHT automatisch — bei Hard-Trigger muss
# zusätzlich sichergestellt werden, dass panic_close_leverage_positions
# erfolgreich alle Positionen geschlossen hat (Check via 'sqlite3 ...
# SELECT * FROM positions WHERE closed_at IS NULL').
#
# Usage:
#     sudo /usr/libexec/razortrade/reset-killswitch.sh
#
# Das Script fragt interaktiv nach Operator-Name und Grund, zeigt
# was resolved wird, und verlangt explizite Bestätigung bevor der
# UPDATE ausgeführt wird.

set -euo pipefail

DB="/var/lib/razortrade/razortrade.sqlite"

if [[ $EUID -ne 0 ]]; then
    echo "ERROR: dieses script braucht root (für sqlite3-Zugriff auf $DB)" >&2
    exit 1
fi

if [[ ! -f "$DB" ]]; then
    echo "ERROR: Datenbank nicht gefunden: $DB" >&2
    exit 1
fi

# --- Aktuelle aktive Events zeigen --------------------------------
echo "=== Aktuelle ungelöste Kill-Switch-Events ==="
active_count=$(sqlite3 "$DB" "SELECT COUNT(*) FROM kill_switch_events WHERE resolved_at IS NULL;")
echo "Anzahl aktive Events: $active_count"
if [[ "$active_count" -eq 0 ]]; then
    echo "Keine aktiven Events — nichts zu tun."
    exit 0
fi

sqlite3 -header -column "$DB" <<SQL
SELECT id, reason_kind, triggered_at, substr(reason_detail_json, 1, 80) AS reason_preview
  FROM kill_switch_events
 WHERE resolved_at IS NULL
 ORDER BY id;
SQL
echo ""

# --- Pre-Flight: offene Positionen bei Hard-Trigger ---------------
hard_count=$(sqlite3 "$DB" "SELECT COUNT(*) FROM kill_switch_events WHERE resolved_at IS NULL AND reason_kind LIKE 'hard_%';")
if [[ "$hard_count" -gt 0 ]]; then
    open_pos=$(sqlite3 "$DB" "SELECT COUNT(*) FROM positions WHERE closed_at IS NULL AND CAST(quantity AS REAL) != 0;")
    if [[ "$open_pos" -gt 0 ]]; then
        echo "WARNUNG: $hard_count Hard-Kill-Switch(es) aktiv UND $open_pos offene Positionen."
        echo "Panic-close hat möglicherweise NICHT alle geschlossen."
        echo "Prüfe: sudo sqlite3 $DB 'SELECT * FROM positions WHERE closed_at IS NULL;'"
        echo ""
    fi
fi

# --- Operator-Input -----------------------------------------------
read -r -p "Operator-Name: " OP_NAME
if [[ -z "$OP_NAME" ]]; then
    echo "ERROR: Operator-Name darf nicht leer sein" >&2
    exit 1
fi

read -r -p "Grund für Reset (eine Zeile): " REASON
if [[ -z "$REASON" ]]; then
    echo "ERROR: Grund darf nicht leer sein" >&2
    exit 1
fi

NOTE=$(printf 'reset by %s: %s' "$OP_NAME" "$REASON" | sed "s/'/''/g")

echo ""
echo "Wird alle $active_count aktiven Events als resolved markieren mit:"
echo "  resolved_at = (jetzt, UTC, mikrosekunden + Z)"
echo "  operator_note = '$NOTE'"
echo ""
read -r -p "Fortfahren? (yes/NO): " CONFIRM
if [[ "$CONFIRM" != "yes" ]]; then
    echo "Abgebrochen."
    exit 1
fi

# --- UPDATE ausführen ---------------------------------------------
# ISO-Format identisch zu rt_core::time::canonical_iso: Mikrosekunden + Z
NOW_ISO=$(date -u +"%Y-%m-%dT%H:%M:%S.%6NZ")

sqlite3 "$DB" <<SQL
UPDATE kill_switch_events
   SET resolved_at = '$NOW_ISO',
       operator_note = '$NOTE'
 WHERE resolved_at IS NULL;
SQL

updated=$(sqlite3 "$DB" "SELECT changes();")
echo ""
echo "=== Fertig ==="
echo "Events markiert als resolved: $updated"
echo "Timestamp: $NOW_ISO"
echo ""
echo "Nächste Schritte:"
echo "  1. Dashboard prüfen (Kill-Switch sollte jetzt 'armed' zeigen)"
echo "  2. rt-daemon-Log beobachten: journalctl -u rt-daemon -f"
echo "  3. Nach einem 4h-Signal Zyklus verifizieren dass Entries wieder durchgehen"
