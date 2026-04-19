# razortrade — Hetzner deployment guide

End-to-end recipe for a fresh production host. Expects a Hetzner Cloud
account and an SSH keypair on your dev machine (`~/.ssh/id_ed25519`).

---

## Step 1 — Provision the Hetzner server (2 minutes)

1. Open <https://console.hetzner.cloud/>
2. *New Project* (or use existing) → *Add Server*
3. Settings:
   - **Location**: Falkenstein (fsn1)
   - **Image**: Rocky Linux 10
   - **Type**: CX32 (or CX42 if you want more headroom — both are fine)
   - **Networking**: IPv4 on, IPv6 optional, no extra networks needed
   - **SSH Keys**: *do NOT* add a key here — the cloud-init does it
   - **Firewalls**: none (firewalld handles it inside the VM)
4. Under *User Data*, paste the **contents of** `deploy/hetzner-cloud-init.yml`
   from this repo. **Replace the `<YOUR_SSH_PUBLIC_KEY>` line** with your
   actual public key (output of `cat ~/.ssh/id_ed25519.pub`).
5. *Name*: `razortrade-prod`
6. Click *Create & Buy now*.

Wait ~3-5 minutes for first boot. You can watch progress via:

```bash
ssh linda@<server-ip> sudo tail -f /var/log/cloud-init-output.log
```

When cloud-init finishes you'll see `razortrade host provisioned in N seconds`.

---

## Step 2 — Build the RPMs locally (2 minutes)

On your dev machine:

```bash
cd ~/Workspace/razortrade
cargo build --release -p rt-daemon -p rt-dashboard
cargo generate-rpm -p crates/rt-daemon
cargo generate-rpm -p crates/rt-dashboard
ls -la target/generate-rpm/*.rpm
```

You should see two RPMs:
- `rt-daemon-0.1.0-1.x86_64.rpm`
- `rt-dashboard-0.1.0-1.x86_64.rpm`

---

## Step 3 — Copy RPMs to the server (30 seconds)

```bash
scp target/generate-rpm/rt-daemon-*.rpm \
    target/generate-rpm/rt-dashboard-*.rpm \
    linda@<server-ip>:/tmp/
```

---

## Step 4 — Install on the server (1 minute)

SSH in:

```bash
ssh linda@<server-ip>
```

Install both:

```bash
sudo dnf install -y /tmp/rt-daemon-*.rpm /tmp/rt-dashboard-*.rpm
```

Verify:

```bash
rpm -q rt-daemon rt-dashboard
id razortrade                            # user should exist
ls -la /etc/razortrade/                  # daemon.toml in place
systemctl status rt-daemon rt-dashboard  # both 'inactive (dead)' — not enabled yet
```

---

## Step 5 — Configure the daemon (2 minutes)

Edit `/etc/razortrade/daemon.toml`:

```bash
sudo vim /etc/razortrade/daemon.toml
```

For 72h Paper validation, make sure:

```toml
[daemon]
mode = "paper"    # NOT "live" yet
```

Then drop in Kraken credentials as a systemd override (keeps them out of any
world-readable file):

```bash
sudo mkdir -p /etc/systemd/system/rt-daemon.service.d
sudo tee /etc/systemd/system/rt-daemon.service.d/credentials.conf > /dev/null <<'EOF'
[Service]
Environment="RT_KRAKEN_API_KEY=your-paper-key-here"
Environment="RT_KRAKEN_API_SECRET=your-paper-secret-here"
EOF
sudo chmod 0600 /etc/systemd/system/rt-daemon.service.d/credentials.conf
sudo systemctl daemon-reload
```

---

## Step 6 — Start it (30 seconds)

```bash
sudo systemctl enable --now rt-daemon.service
sudo systemctl enable --now rt-dashboard.service
```

Check both came up cleanly:

```bash
systemctl status rt-daemon rt-dashboard --no-pager
```

If something failed, follow logs:

```bash
sudo journalctl -u rt-daemon -f       # daemon
sudo journalctl -u rt-dashboard -f    # dashboard
```

---

## Step 7 — Access the dashboard (10 seconds)

The dashboard binds to `127.0.0.1:8080` on the server (not exposed to the
internet — firewalld blocks it). Reach it via SSH tunnel from your dev
machine:

```bash
ssh -L 8080:localhost:8080 linda@<server-ip>
```

Leave that SSH session open, then in your browser:

```
http://localhost:8080
```

You should see:
- Summary cards (equity, realized P&L, drawdown, NAV)
- A 24h equity curve chart
- Open positions table
- Recent orders table
- The whole page auto-refreshes every 5 seconds

---

## Step 8 — 72h Paper validation checklist

While paper runs, monitor these things:

1. **Daemon stays up**
   ```bash
   systemctl is-active rt-daemon
   ```

2. **No repeated errors in journal**
   ```bash
   sudo journalctl -u rt-daemon --since "1h ago" | grep -i error
   ```

3. **Orders table grows** — check dashboard or:
   ```bash
   sudo sqlite3 /var/lib/razortrade/razortrade.sqlite "SELECT COUNT(*) FROM orders"
   ```

4. **Kill-switch test** — simulate the leverage-budget breach. Before going
   live, insert a fake loss into `equity_snapshots` and confirm the daemon
   rejects the next leveraged signal with `KillSwitchActive`. (This is a
   separate drill — ask me to write the script when ready.)

5. **Restart survival** — `sudo systemctl restart rt-daemon` and confirm the
   daemon reconnects cleanly, the WS fill-snapshot replay does NOT double-apply
   (Drop 6c), and the dashboard still shows correct state.

Only after all five checks pass three separate days in a row: flip
`mode = "live"` and fund with 300 CHF.
