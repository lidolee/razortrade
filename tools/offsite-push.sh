#!/bin/bash
#
# Push local daily backups to Hetzner Storage Box and rotate remote
# copies at REMOTE_RETENTION_DAYS. Invoked by sqlite-backup.sh after
# the local snapshot is written.
#
# Design:
#   - rsync over SSH (port 23 — Hetzner's SSH/SCP port for Storage
#     Boxes) with key auth. No password lives on disk anywhere.
#   - --ignore-existing: never overwrite remote files. Each day's
#     backup is a single atomic upload; no partial updates.
#   - Remote rotation via a single SSH command that lists and deletes
#     YYYY-MM-DD.sqlite files older than the cutoff. Single round-trip.
#   - Non-fatal on push failure. Local backup is already on disk, so
#     a failed offsite push still leaves yesterday's on-disk copy.
#     systemd will retry next day.

set -euo pipefail

LOCAL_DIR=/var/lib/razortrade/backups
REMOTE_USER=u377890
REMOTE_HOST=u377890.your-storagebox.de
REMOTE_PORT=23
REMOTE_DIR=backups/razortrade
REMOTE_RETENTION_DAYS=90
SSH_KEY=/var/lib/razortrade/.ssh/storagebox_ed25519
KNOWN_HOSTS=/var/lib/razortrade/.ssh/storagebox_known_hosts

# Bundled SSH options used by both rsync and the rotation command.
# Fixed options: strict hostkey checking (known_hosts was pinned at
# setup time), batch mode (no password fallback ever), explicit key.
SSH_BASE="ssh -p ${REMOTE_PORT} -i ${SSH_KEY} \
  -o UserKnownHostsFile=${KNOWN_HOSTS} \
  -o StrictHostKeyChecking=yes \
  -o PasswordAuthentication=no \
  -o BatchMode=yes"

# Preflight: ensure auth works. Cheap, fast-fails on key issues so the
# systemd unit marks us failed cleanly rather than hanging on rsync.
# Hetzner Storage Box has a restricted command set — `true` / arbitrary
# shell is rejected with "Command not found". Use `pwd` which is in the
# allow-list (see Hetzner docs "Verfügbare Befehle").
if ! $SSH_BASE "${REMOTE_USER}@${REMOTE_HOST}" "pwd" >/dev/null 2>&1; then
    echo "ERROR: SSH to Storage Box failed — key auth broken?" >&2
    exit 10
fi

# Push new files. --ignore-existing prevents clobbering a complete
# backup with a partial (e.g. if the local script runs twice in one day).
echo "pushing local backups to ${REMOTE_USER}@${REMOTE_HOST}:${REMOTE_DIR}/"
rsync -av --ignore-existing \
    -e "${SSH_BASE}" \
    "${LOCAL_DIR}/" \
    "${REMOTE_USER}@${REMOTE_HOST}:${REMOTE_DIR}/"

# Remote rotation. The Storage Box's restricted shell supports ls/rm
# but not find(1), so we do the date comparison client-side: list
# files, parse YYYY-MM-DD from the filename, build a deletion list,
# issue one rm command.
CUTOFF=$(date -u -d "${REMOTE_RETENTION_DAYS} days ago" +%Y-%m-%d)
echo "rotating remote: deleting backups older than ${CUTOFF}"

REMOTE_FILES=$($SSH_BASE "${REMOTE_USER}@${REMOTE_HOST}" "ls ${REMOTE_DIR}/ 2>/dev/null" \
    | grep -E '^[0-9]{4}-[0-9]{2}-[0-9]{2}\.sqlite$' || true)

TO_DELETE=()
while read -r fname; do
    [[ -z "$fname" ]] && continue
    file_date="${fname%.sqlite}"
    if [[ "$file_date" < "$CUTOFF" ]]; then
        TO_DELETE+=("${REMOTE_DIR}/${fname}")
    fi
done <<< "$REMOTE_FILES"

if [[ ${#TO_DELETE[@]} -gt 0 ]]; then
    echo "  deleting ${#TO_DELETE[@]} old file(s)"
    # Hetzner Storage Box's restricted shell balks at multi-arg rm
    # with globs. Issue one rm per file — slower but reliable.
    for path in "${TO_DELETE[@]}"; do
        $SSH_BASE "${REMOTE_USER}@${REMOTE_HOST}" "rm ${path}" || \
            echo "  WARN: failed to delete ${path}" >&2
    done
else
    echo "  nothing to rotate"
fi

# Report final remote state. The Storage Box shell doesn't allow pipes,
# so we fetch the listing and count it locally.
REMOTE_COUNT=$($SSH_BASE "${REMOTE_USER}@${REMOTE_HOST}" "ls ${REMOTE_DIR}/" 2>/dev/null | wc -l)
echo "offsite push complete: ${REMOTE_COUNT} backup(s) remote (retention ${REMOTE_RETENTION_DAYS}d)"
