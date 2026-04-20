#!/usr/bin/env bash
# razortrade Equity-Snapshot Retention Pruner
#
# Löscht equity_snapshots älter als RETENTION_DAYS (default 90).
# Läuft typischerweise als oneshot systemd-service wöchentlich.
#
# Bei 30s-Tick ergibt das 2'880 Zeilen/Tag → 90 Tage ≈ 260k Rows
# (≈ 40 MB). Das ist die Grenze ab der Dashboard-Queries spürbar
# langsamer werden. Dashboard zeigt eh nur die letzten 24h; alles
# darüber ist historischer Audit-Wert und kann kompakter gelagert
# werden.
#
# Der Prune läuft in einer einzelnen SQL-Transaktion, danach
# VACUUM um Speicher freizugeben.
#
# Usage:
#     sudo /usr/libexec/razortrade/prune-equity-snapshots.sh [RETENTION_DAYS]
#
# Default: 90 Tage. Override per CLI-Arg oder ENV RT_RETENTION_DAYS.

set -euo pipefail

DB="/var/lib/razortrade/razortrade.sqlite"
RETENTION_DAYS="${1:-${RT_RETENTION_DAYS:-90}}"

if [[ $EUID -ne 0 ]]; then
    echo "ERROR: dieses script braucht root (für sqlite3-Zugriff auf $DB)" >&2
    exit 1
fi

if [[ ! -f "$DB" ]]; then
    echo "ERROR: Datenbank nicht gefunden: $DB" >&2
    exit 1
fi

# Cut-off-Timestamp: N Tage vor jetzt, UTC, canonical ISO
CUTOFF=$(date -u -d "$RETENTION_DAYS days ago" +"%Y-%m-%dT%H:%M:%S.%6NZ")

# Pre-count für Log
total_before=$(sqlite3 "$DB" "SELECT COUNT(*) FROM equity_snapshots;")
to_delete=$(sqlite3 "$DB" "SELECT COUNT(*) FROM equity_snapshots WHERE timestamp < '$CUTOFF';")

echo "[$(date -u +%Y-%m-%dT%H:%M:%SZ)] equity_snapshots prune"
echo "  database:       $DB"
echo "  retention_days: $RETENTION_DAYS"
echo "  cutoff:         $CUTOFF"
echo "  rows total:     $total_before"
echo "  rows to delete: $to_delete"

if [[ "$to_delete" -eq 0 ]]; then
    echo "  nothing to prune."
    exit 0
fi

# DELETE + VACUUM separat — VACUUM braucht exklusive Sperre und
# sollte nicht Teil der Haupt-Transaktion sein.
sqlite3 "$DB" <<SQL
BEGIN IMMEDIATE;
DELETE FROM equity_snapshots WHERE timestamp < '$CUTOFF';
COMMIT;
SQL

deleted=$(sqlite3 "$DB" "SELECT changes();")
total_after=$(sqlite3 "$DB" "SELECT COUNT(*) FROM equity_snapshots;")

# VACUUM nur wenn wir signifikant was gelöscht haben (spart I/O
# bei kleinen Prunes).
if [[ "$deleted" -gt 1000 ]]; then
    echo "  running VACUUM (this may take a moment)..."
    sqlite3 "$DB" "VACUUM;"
    echo "  VACUUM done."
fi

echo "  rows after:     $total_after"
echo "  done."
