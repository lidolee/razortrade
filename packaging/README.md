# razortrade — operator guide

Algorithmic trading daemon, packaged as an RPM for Rocky Linux 9 and
Rocky Linux 10. The daemon runs as an unprivileged `razortrade` user
under a hardened systemd unit.

## Installation

```
sudo dnf install ./rt-daemon-*.rpm
```

This creates the `razortrade` system user and group, installs the
binary at `/usr/bin/rt-daemon`, the systemd unit at
`/usr/lib/systemd/system/rt-daemon.service`, and a default config at
`/etc/razortrade/daemon.toml`.

## Configuration

### The config file

Edit `/etc/razortrade/daemon.toml`. The file is marked
`%config(noreplace)` so it is preserved on package upgrade. A new
default ships as `daemon.toml.rpmnew` if the packaged template
changes.

The most important field is `[execution].mode`:

- **`dry_run`** (default) — approved signals go to the
  `dry_run_orders` table only. No broker contact. Safe for first
  bring-up.
- **`paper`** — orders go to the broker's sandbox. Requires demo
  credentials (see below).
- **`live`** — orders go to the broker's production endpoint. REAL
  MONEY. Requires production credentials.

### Broker credentials

API keys are NOT stored in the config file. They are read from the
systemd environment. The recommended pattern is a drop-in file:

```
sudo install -d -m 0750 -o root -g razortrade /etc/razortrade
sudo install -m 0640 -o root -g razortrade /dev/null \
    /etc/systemd/system/rt-daemon.service.d/credentials.conf
sudo tee /etc/systemd/system/rt-daemon.service.d/credentials.conf >/dev/null <<'EOF'
[Service]
Environment=RT_KRAKEN_API_KEY=...
Environment=RT_KRAKEN_API_SECRET=...
EOF
sudo systemctl daemon-reload
```

The drop-in is readable only by root and the `razortrade` service
user. Do NOT paste keys into the systemd unit itself — it is
world-readable under `/usr/lib/systemd/system`.

## Enabling the service

The RPM deliberately does NOT enable or start the service on
install. This prevents an untested configuration from starting to
trade on reboot. After you have verified the config and provisioned
credentials:

```
sudo systemctl enable --now rt-daemon.service
```

## Operations

### Logs

Structured JSON logs go to the journal:

```
journalctl -u rt-daemon -f
journalctl -u rt-daemon --since "1 hour ago" | jq .
```

### State

The SQLite database lives at `/var/lib/razortrade/razortrade.sqlite`.
The systemd unit creates this directory with `0750` mode and
`razortrade:razortrade` ownership via `StateDirectory`. Back it up
regularly:

```
sudo -u razortrade sqlite3 /var/lib/razortrade/razortrade.sqlite \
    ".backup /var/lib/razortrade/razortrade-$(date +%F).sqlite.bak"
```

### Kill switch

A hard leverage drawdown trip locks leveraged trading until an
operator resolves it in the database:

```
sudo -u razortrade sqlite3 /var/lib/razortrade/razortrade.sqlite \
    "UPDATE kill_switch_events SET resolved_at = datetime('now'), \
     resolved_by = 'operator-name', resolved_note = 'reviewed' \
     WHERE resolved_at IS NULL;"
```

The running daemon picks up the resolution on the next
kill-switch-check interval (default 60s).

### Graceful shutdown

```
sudo systemctl stop rt-daemon
```

The daemon has up to 15s to flush. After that, SIGKILL.

## Uninstall

```
sudo systemctl disable --now rt-daemon.service
sudo dnf remove rt-daemon
```

The `razortrade` user and `/var/lib/razortrade` are deliberately
preserved on removal. Delete them manually only if you are certain
you want to discard the trade history:

```
sudo userdel -r razortrade
sudo rm -rf /var/lib/razortrade /var/log/razortrade
```

## Troubleshooting

- **`Failed to start rt-daemon.service`** — run
  `journalctl -u rt-daemon --no-pager | tail -50`. The most common
  cause is missing credentials in Paper/Live mode.
- **Database locked errors** — a second instance is running, or a
  previous instance did not shut down cleanly. Check
  `systemctl status rt-daemon`; if no process, inspect
  `/var/lib/razortrade/razortrade.sqlite-wal` and
  `.sqlite-shm` — these recover on next open.
- **Configuration errors at startup** — the daemon hard-errors and
  exits on any unknown key, missing required field, or invalid enum
  in `daemon.toml`. This is intentional.

## Source

https://github.com/lidolee/razortrade
