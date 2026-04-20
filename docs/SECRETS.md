# Secrets Management

**Keine Secrets gehen in dieses Repo.** Dieser File dokumentiert nur **wo** welche Secrets **auf Prod** liegen, **wer** sie lesen darf, und **wie** sie ins laufende System kommen. Die Werte selbst werden in einem Password-Manager gehalten.

---

## Secret-Inventar

| Name | Zweck | Datei | Mode | Owner:Group |
|---|---|---|---|---|
| `RT_KRAKEN_API_KEY` | Kraken Futures API Key | `/etc/razortrade/secrets.env` | 0600 | `root:razortrade` |
| `RT_KRAKEN_API_SECRET` | Kraken Futures API Secret (base64) | `/etc/razortrade/secrets.env` | 0600 | `root:razortrade` |
| `OAUTH2_PROXY_CLIENT_ID` | Google OAuth Client ID | `/etc/oauth2-proxy/oauth2-proxy.env` | 0600 | `root:oauth2-proxy` |
| `OAUTH2_PROXY_CLIENT_SECRET` | Google OAuth Client Secret | `/etc/oauth2-proxy/oauth2-proxy.env` | 0600 | `root:oauth2-proxy` |
| `OAUTH2_PROXY_COOKIE_SECRET` | Session-Cookie-Signing-Secret (32 Byte base64) | `/etc/oauth2-proxy/oauth2-proxy.env` | 0600 | `root:oauth2-proxy` |
| SSH-Key offsite-push | Zugriff auf Hetzner Storage-Box für Backup-Push | `/var/lib/razortrade/.ssh/id_ed25519` | 0600 | `razortrade:razortrade` |

---

## Ladeweg

### rt-daemon

systemd-Unit `rt-daemon.service` lädt die Secrets per `EnvironmentFile=/etc/razortrade/secrets.env` (**Achtung: in der aktuellen Service-File ist diese Direktive NICHT gesetzt** — siehe Known Gap unten).

Der Daemon liest dann via `std::env::var("RT_KRAKEN_API_KEY")` etc.

### oauth2-proxy

systemd-Unit `oauth2-proxy.service` lädt `/etc/oauth2-proxy/oauth2-proxy.env`.

### backup offsite-push

Der SSH-Key wird direkt vom `offsite-push.sh` Script als `ssh -i /var/lib/razortrade/.ssh/id_ed25519` benutzt. Kein env-Loading.

---

## Anlage-Prozedur

### Neues Kraken-API-Key-Paar

1. Auf Kraken Futures im Account erstellen, Scope: Trade (nicht Withdraw).
2. Base64-Secret 1:1 kopieren.
3. Auf Prod als root:
   ```bash
   sudo install -m 0600 -o root -g razortrade /dev/null /etc/razortrade/secrets.env
   sudo tee /etc/razortrade/secrets.env <<'EOF'
   RT_KRAKEN_API_KEY=<NEUER_KEY>
   RT_KRAKEN_API_SECRET=<NEUES_SECRET_BASE64>
   EOF
   ```
4. Daemon neu starten: `sudo systemctl restart rt-daemon.service`
5. Verifizieren: `journalctl -u rt-daemon --since "1 minute ago" | grep -iE "auth|unauthorized"`. Leer = gut.

### Neue OAuth-App bei Google

1. Console: neue OAuth2-Client-ID in einem Google-Workspace-Projekt, Redirect-URI exakt `https://trader.ghazzo.ch/oauth2/callback` (Trailing-Slash wichtig).
2. Client-ID und Client-Secret ins password-manager.
3. Cookie-Secret generieren: `openssl rand -base64 32`.
4. Auf Prod als root:
   ```bash
   sudo install -m 0600 -o root -g oauth2-proxy /dev/null /etc/oauth2-proxy/oauth2-proxy.env
   sudo tee /etc/oauth2-proxy/oauth2-proxy.env <<'EOF'
   OAUTH2_PROXY_CLIENT_ID=<CLIENT_ID>
   OAUTH2_PROXY_CLIENT_SECRET=<CLIENT_SECRET>
   OAUTH2_PROXY_COOKIE_SECRET=<RANDOM_BASE64>
   EOF
   ```
5. Reload: `sudo systemctl restart oauth2-proxy.service`
6. Testen: `https://trader.ghazzo.ch/` sollte nach Google-Login verlangen und die whitelisted Email akzeptieren.

### SSH-Key für Backup-Offsite-Push

1. Key erzeugen auf Prod als razortrade:
   ```bash
   sudo -u razortrade ssh-keygen -t ed25519 -N '' -f /var/lib/razortrade/.ssh/id_ed25519 -C 'razortrade-backup@prod'
   ```
2. Public-Key bei Hetzner Storage-Box autorisieren (Hetzner Console → Storage Box → Zugangsdaten).
3. Known-Hosts vorab populieren:
   ```bash
   sudo -u razortrade ssh-keyscan -H <storage-box-hostname> >> /var/lib/razortrade/.ssh/known_hosts
   ```
4. Test:
   ```bash
   sudo -u razortrade /usr/libexec/razortrade/offsite-push.sh
   ```

---

## Rotation-Prozedur

### Kraken-Keys rotieren

1. Neue Keys im Kraken-Account erstellen, alte noch aktiv lassen.
2. Auf Prod `/etc/razortrade/secrets.env` überschreiben mit neuen Werten.
3. Daemon neu starten.
4. Erfolgreichen API-Call im Log verifizieren (ein equity_snapshot-Tick oder `signal_processor` Aufruf).
5. Alte Keys in Kraken deaktivieren.

### OAuth-Rotation

Analog. Client-Secret in Google Console rotieren, `oauth2-proxy.env` updaten, `systemctl restart oauth2-proxy.service`. Bereits aktive Sessions bleiben gültig bis Cookie-Expiry (7 Tage) oder Sign-Out.

### Cookie-Secret

Rotation invalidiert alle bestehenden Sessions. Operator muss sich neu einloggen. Kein Risiko, nur Inconvenience.

---

## Git-Hygiene

`.gitignore` muss folgende Pfade enthalten (prüfen, nachziehen wenn nicht vorhanden):

```
deploy/daemon.toml.local
*.env
secrets.env
*.rpm
target/
./var/
```

**Pre-Commit-Check**, bei jedem Commit:

```bash
git diff --cached | grep -iE "api[_-]?(key|secret)|token|password|cookie_secret|bearer" && echo "STOP — potentielle Secret-Leak" || echo "ok"
```

Falls ein Secret versehentlich committed wurde: **sofort** bei Kraken/Google rotieren, dann Git-History mit `git filter-repo` säubern. Ein einzelnes Auschecken aus dem History reicht NICHT.

---

## Known Gaps (Stand 2026-04-20)

- `rt-daemon.service` hat aktuell kein `EnvironmentFile=` directive für `/etc/razortrade/secrets.env`. Wenn der Daemon API-Keys aus der Umgebung liest, müssen sie aktuell anders bereitgestellt werden (z.B. via `daemon.toml` was ein schlechterer Place ist weil mode 0640 statt 0600). **Action:** in Drop 19 oder 20 die Service-Unit um `EnvironmentFile=-/etc/razortrade/secrets.env` ergänzen. Das `-` davor macht es optional (Unit startet auch ohne das File, was für Dry-Run/Demo-Mode nützlich ist).
- `rt-daemon` im Source (`main.rs`) müsste dann konsistent via `std::env::var()` lesen. Prüfen ob das aktuell so ist.
- Für Kraken Demo-Mode werden gar keine Secrets gebraucht — Demo ist unauthenticated für Public-Feeds und nur WS-Challenge-Flow für Private-Feeds mit Demo-Credentials. Das ist der aktuelle Paper-Trading-Zustand.

---

## Was NIEMALS in Secrets-Dateien kommt

- Klartext-Passwörter von Drittdiensten (nur Tokens/Keys, die man revoken kann)
- Private Keys ohne Passphrase (für SSH und Kraken sollen sie in Hardware-YubiKey oder Password-Manager liegen, Prod-Datei ist nur die Operational-Copy)
- Backup-Recovery-Keys (gehören in einen separaten Secret-Store, nicht auf Prod)
