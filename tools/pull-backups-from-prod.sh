#!/bin/bash
#
# razortrade SQLite backup — ThinkStation side (pull from server)
#
# Design rationale:
#   - Server writes local backups daily via systemd timer, rotates
#     at 7 days.
#   - ThinkStation is not 24/7 — it's a developer workstation. So
#     we use a PULL model (this script runs on the ThinkStation),
#     not a push from the server.
#   - As long as the ThinkStation is online at least once every 7
#     days, no data is lost (retention window on server).
#
# Permissions:
#   - Backups on server are mode 0640 razortrade:razortrade in dir
#     0750 razortrade:razortrade.
#   - The SSH user (linda) must be in the razortrade group on the
#     server for read access. One-time setup:
#       ssh linda@178.105.6.124 'sudo usermod -aG razortrade linda'
#       # then log out and back in for the group to take effect.
#
# This script is idempotent: rsync only transfers files that are
# new or changed. No --delete — local retention is longer than
# server retention by design.

set -euo pipefail

REMOTE=linda@178.105.6.124
REMOTE_DIR=/var/lib/razortrade/backups
LOCAL_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/backups"

mkdir -p "$LOCAL_DIR"

echo "pulling backups from ${REMOTE}:${REMOTE_DIR}/ -> ${LOCAL_DIR}/"

rsync -az --info=stats2,progress2 \
    -e ssh \
    "${REMOTE}:${REMOTE_DIR}/" "${LOCAL_DIR}/"

echo
echo "local backups:"
ls -lah "$LOCAL_DIR"/*.sqlite 2>/dev/null | tail -10 || echo "  (none yet)"
