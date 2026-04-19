#!/bin/bash
#
# razortrade SQLite backup — server side
#
# Takes a consistent online snapshot of the live SQLite database using
# sqlite3's .backup command (does not interfere with the running
# daemon), writes it to /var/lib/razortrade/backups/YYYY-MM-DD.sqlite,
# and rotates files older than RETENTION_DAYS.
#
# The snapshot is safe because .backup uses SQLite's internal APIs to
# hold a read transaction for the duration of the copy — writes from
# rt-daemon are serialised against it but never blocked for long
# (writes queue behind the backup's page reads, not the whole backup).
#
# Deployed by rt-daemon RPM to /usr/libexec/razortrade/sqlite-backup.sh
# Run by razortrade-backup.service (systemd, daily at 03:00 Zurich).

set -euo pipefail

DB=/var/lib/razortrade/razortrade.sqlite
BACKUP_DIR=/var/lib/razortrade/backups
RETENTION_DAYS=7

# Ensure backup dir exists with correct perms.
# 0750 = owner rwx, group rx, other 0.
# Group read+exec is required so users in the razortrade group
# (e.g. linda) can enter the directory to rsync it off the host.
mkdir -p "$BACKUP_DIR"
chmod 0750 "$BACKUP_DIR"

# UTC date for filenames — avoids DST ambiguity.
DATE=$(date -u +%Y-%m-%d)
TARGET="$BACKUP_DIR/$DATE.sqlite"

# If a backup for today already exists (e.g. manual run then timer
# fires), overwrite it. The daily snapshot is a tip-of-day artefact,
# not a historical record.
sqlite3 "$DB" ".backup '$TARGET'"
chmod 0640 "$TARGET"

# Rotate: delete files older than RETENTION_DAYS days.
# -mtime +N means "modified more than N*24h ago".
find "$BACKUP_DIR" -maxdepth 1 -name '*.sqlite' -type f -mtime "+${RETENTION_DAYS}" -delete

# Report for journal.
SIZE=$(stat -c%s "$TARGET")
COUNT=$(find "$BACKUP_DIR" -maxdepth 1 -name '*.sqlite' -type f | wc -l)
echo "backup ok: $TARGET (${SIZE} bytes); ${COUNT} backup(s) on disk (retention ${RETENTION_DAYS}d)"

# --- Offsite push -----------------------------------------------------
# Push the fresh snapshot to Hetzner Storage Box and rotate remote
# copies. Failure here is logged but does not fail the whole backup:
# the local snapshot is already safe on disk, and the next daily run
# will try the push again.
OFFSITE_SCRIPT=/usr/libexec/razortrade/offsite-push.sh
if [[ -x "$OFFSITE_SCRIPT" ]]; then
    if ! "$OFFSITE_SCRIPT"; then
        echo "WARN: offsite push failed (exit $?) — local backup is intact; will retry next day" >&2
    fi
else
    echo "note: $OFFSITE_SCRIPT not installed; skipping offsite push"
fi
