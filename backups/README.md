# SQLite backups from production

Daily backups pulled from the Hetzner production server by
`tools/pull-backups-from-prod.sh` (or the corresponding systemd
user timer installed from `deploy/razortrade-backup-pull.*`).

Actual `.sqlite` files are gitignored — they contain operational
trading data and rotate automatically.

Server-side retention: 7 days.  
Local retention: unbounded (you delete manually when you feel like it).

See `deploy/razortrade-backup-pull.service` for installation
instructions.
