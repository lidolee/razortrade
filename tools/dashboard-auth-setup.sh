#!/bin/bash
#
# One-shot installer for public HTTPS access to the razortrade dashboard
# with Google OAuth authentication.
#
# Installs and configures:
#   - Caddy (reverse proxy + automatic Let's Encrypt TLS)
#   - oauth2-proxy (Google OAuth gateway, email allowlist)
#   - Firewall rules (ports 80 + 443 open)
#
# Requires:
#   - DNS record: trader.ghazzo.ch → 178.105.6.124 (already resolved)
#   - Google Cloud Console OAuth client configured with redirect URI
#     https://trader.ghazzo.ch/oauth2/callback
#   - Client ID and Client Secret from the Google OAuth client
#
# Usage:
#   sudo bash dashboard-auth-setup.sh
#   [prompts for Client ID, Client Secret]
#
# Idempotent: safe to re-run. Detects already-installed components and
# only installs what's missing. Always rewrites configs to keep them
# matching what this installer shipped.

set -euo pipefail

# --- Self-locating -------------------------------------------------
# The installer expects Caddyfile, oauth2-proxy.cfg, oauth2-proxy.service
# to be siblings of this script OR in ../packaging/ (repo layout).
SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
TEMPLATE_DIR=""
for CANDIDATE in "$SCRIPT_DIR" "$SCRIPT_DIR/../packaging" "$SCRIPT_DIR/packaging"; do
    if [[ -f "$CANDIDATE/Caddyfile" && -f "$CANDIDATE/oauth2-proxy.cfg" && -f "$CANDIDATE/oauth2-proxy.service" ]]; then
        TEMPLATE_DIR=$(cd "$CANDIDATE" && pwd)
        break
    fi
done
if [[ -z "$TEMPLATE_DIR" ]]; then
    echo "ERROR: couldn't find Caddyfile/oauth2-proxy.cfg/oauth2-proxy.service next to this script" >&2
    echo "       looked in: $SCRIPT_DIR, $SCRIPT_DIR/../packaging, $SCRIPT_DIR/packaging" >&2
    exit 1
fi

# --- Config ----------------------------------------------------------
DOMAIN=trader.ghazzo.ch
ADMIN_EMAIL=walid@ghazzo.ch
OAUTH2_PROXY_VERSION=v7.15.2
OAUTH2_PROXY_SHA256_AMD64=996f8c43c263fa8dc4ceed2ea95a7a9da7cece51c91fa77f0b27a90ac1d3cc6e
# ^ from https://github.com/oauth2-proxy/oauth2-proxy/releases/tag/v7.15.2
#   (2026-04-14 release, stable). Verified at install time; if
#   mismatch, the install aborts.

OAUTH2_PROXY_URL="https://github.com/oauth2-proxy/oauth2-proxy/releases/download/${OAUTH2_PROXY_VERSION}/oauth2-proxy-${OAUTH2_PROXY_VERSION}.linux-amd64.tar.gz"

# --- Preflight -------------------------------------------------------
if [[ $EUID -ne 0 ]]; then
    echo "ERROR: run as root (sudo $0)" >&2
    exit 1
fi

if [[ ! -f /etc/rocky-release ]]; then
    echo "WARN: not running on Rocky Linux; continuing anyway" >&2
fi

echo "=== razortrade dashboard auth setup ==="
echo "Domain:       $DOMAIN"
echo "Admin email:  $ADMIN_EMAIL"
echo

# --- DNS preflight ---------------------------------------------------
echo "[1/8] DNS check"
RESOLVED_IP=$(getent hosts "$DOMAIN" | awk '{print $1}' | head -1 || true)
if [[ -z "$RESOLVED_IP" ]]; then
    echo "  ERROR: $DOMAIN does not resolve. Set an A record to your server's public IP and retry." >&2
    exit 2
fi
ACTUAL_IP=$(curl -s https://api.ipify.org || echo "unknown")
if [[ "$RESOLVED_IP" != "$ACTUAL_IP" && "$ACTUAL_IP" != "unknown" ]]; then
    echo "  WARN: DNS resolves to $RESOLVED_IP but this host is $ACTUAL_IP"
    echo "  Let's Encrypt will fail unless DNS points at this host."
    echo -n "  Continue anyway? [y/N] "
    read -r ANSWER
    [[ "$ANSWER" == "y" ]] || exit 3
else
    echo "  OK: $DOMAIN → $RESOLVED_IP (matches this host)"
fi

# --- Secrets -------------------------------------------------------
echo
echo "[2/8] Google OAuth credentials"
if [[ -f /etc/oauth2-proxy/oauth2-proxy.env ]] && grep -q OAUTH2_PROXY_CLIENT_SECRET /etc/oauth2-proxy/oauth2-proxy.env; then
    echo "  existing secrets found at /etc/oauth2-proxy/oauth2-proxy.env — reusing"
    REUSE_SECRETS=1
else
    REUSE_SECRETS=0
    # Read from /dev/tty so the prompts work even if stdin is a pipe
    # (e.g. when the installer is invoked via `ssh -t ... 'bash script'`
    # where bash is the command and stdin is closed).
    echo -n "  Google OAuth Client ID: " > /dev/tty
    IFS= read -r GOOGLE_CLIENT_ID < /dev/tty
    echo -n "  Google OAuth Client Secret: " > /dev/tty
    IFS= read -rs GOOGLE_CLIENT_SECRET < /dev/tty
    echo > /dev/tty

    # Strip accidentally-captured whitespace/newlines.
    GOOGLE_CLIENT_ID="${GOOGLE_CLIENT_ID//[$'\t\r\n ']/}"
    GOOGLE_CLIENT_SECRET="${GOOGLE_CLIENT_SECRET//[$'\t\r\n ']/}"

    if [[ -z "$GOOGLE_CLIENT_ID" || -z "$GOOGLE_CLIENT_SECRET" ]]; then
        echo "  ERROR: both Client ID and Secret are required" >&2
        echo "         (ID length=${#GOOGLE_CLIENT_ID}, secret length=${#GOOGLE_CLIENT_SECRET})" >&2
        exit 4
    fi
    echo "  received Client ID (${#GOOGLE_CLIENT_ID} chars) and Secret (${#GOOGLE_CLIENT_SECRET} chars)"
fi

# --- Install Caddy --------------------------------------------------
echo
echo "[3/8] Caddy"
if command -v caddy &>/dev/null; then
    echo "  already installed: $(caddy version | head -1)"
else
    echo "  enabling @caddy/caddy COPR repository..."
    dnf install -y 'dnf-command(copr)'
    # -y flag auto-confirms the repository-warning prompt in current dnf.
    # We don't pipe `yes` because a SIGPIPE when dnf stops reading kills
    # the installer under `set -euo pipefail`.
    dnf copr enable -y @caddy/caddy || true
    echo "  installing caddy package..."
    dnf install -y --nogpgcheck caddy
    echo "  installed: $(caddy version | head -1)"
fi

# --- Install oauth2-proxy ------------------------------------------
echo
echo "[4/8] oauth2-proxy"
if [[ -x /usr/local/bin/oauth2-proxy ]] && /usr/local/bin/oauth2-proxy --version 2>&1 | grep -q "$OAUTH2_PROXY_VERSION"; then
    echo "  already installed at version $OAUTH2_PROXY_VERSION"
else
    echo "  downloading $OAUTH2_PROXY_VERSION from github..."
    TMPDIR=$(mktemp -d)
    trap "rm -rf $TMPDIR" EXIT
    curl -fL -o "$TMPDIR/oauth2-proxy.tar.gz" "$OAUTH2_PROXY_URL"
    # Verify checksum
    ACTUAL_SHA=$(sha256sum "$TMPDIR/oauth2-proxy.tar.gz" | cut -d' ' -f1)
    if [[ "$ACTUAL_SHA" != "$OAUTH2_PROXY_SHA256_AMD64" ]]; then
        echo "  ERROR: SHA256 mismatch" >&2
        echo "    expected: $OAUTH2_PROXY_SHA256_AMD64" >&2
        echo "    got:      $ACTUAL_SHA" >&2
        exit 5
    fi
    echo "  SHA256 verified"
    tar xzf "$TMPDIR/oauth2-proxy.tar.gz" -C "$TMPDIR"
    install -m 0755 -o root -g root \
        "$TMPDIR/oauth2-proxy-${OAUTH2_PROXY_VERSION}.linux-amd64/oauth2-proxy" \
        /usr/local/bin/oauth2-proxy
    echo "  installed to /usr/local/bin/oauth2-proxy"
fi

# --- System user for oauth2-proxy ----------------------------------
echo
echo "[5/8] system user oauth2-proxy"
if ! getent passwd oauth2-proxy >/dev/null; then
    useradd --system --no-create-home --shell /sbin/nologin --comment "oauth2-proxy" oauth2-proxy
    echo "  created"
else
    echo "  already exists"
fi

# --- Configuration -------------------------------------------------
echo
echo "[6/8] configuration"
install -m 0755 -o root -g root -d /etc/caddy /etc/oauth2-proxy

# Caddy
install -m 0644 -o root -g root \
    "$TEMPLATE_DIR/Caddyfile" /etc/caddy/Caddyfile
echo "  wrote /etc/caddy/Caddyfile"

# oauth2-proxy cfg
install -m 0640 -o root -g oauth2-proxy \
    "$TEMPLATE_DIR/oauth2-proxy.cfg" /etc/oauth2-proxy/oauth2-proxy.cfg
echo "  wrote /etc/oauth2-proxy/oauth2-proxy.cfg"

# oauth2-proxy env file (secrets)
if [[ "$REUSE_SECRETS" == "0" ]]; then
    # Generate a new 32-byte cookie secret (base64-encoded, length 44)
    # oauth2-proxy requires COOKIE_SECRET to be exactly 16, 24, or 32
    # *literal characters* (not bytes). `head -c 32 /dev/urandom | base64`
    # produces 44 literal chars out of 32 raw bytes and fails at startup
    # with "cookie_secret must be 16, 24, or 32 bytes to create an AES
    # cipher". We go via 24 raw → 32 literal: take 24 random bytes
    # (which give us 32 chars of base64), then truncate any padding.
    COOKIE_SECRET=$(head -c 24 /dev/urandom | base64 | head -c 32)
    umask 077
    cat > /etc/oauth2-proxy/oauth2-proxy.env <<EOF
# Secrets for oauth2-proxy. 0600 root:oauth2-proxy. Generated by
# dashboard-auth-setup.sh on $(date -Iseconds). Do not commit.
OAUTH2_PROXY_CLIENT_ID=$GOOGLE_CLIENT_ID
OAUTH2_PROXY_CLIENT_SECRET=$GOOGLE_CLIENT_SECRET
OAUTH2_PROXY_COOKIE_SECRET=$COOKIE_SECRET
EOF
    chown root:oauth2-proxy /etc/oauth2-proxy/oauth2-proxy.env
    chmod 0640 /etc/oauth2-proxy/oauth2-proxy.env
    unset GOOGLE_CLIENT_SECRET COOKIE_SECRET
    echo "  wrote /etc/oauth2-proxy/oauth2-proxy.env (new secrets)"
else
    echo "  preserved /etc/oauth2-proxy/oauth2-proxy.env"
fi

# oauth2-proxy allowed_emails
cat > /etc/oauth2-proxy/allowed_emails <<EOF
# Email addresses permitted to sign in to the razortrade dashboard.
# One email per line. No comments inside the file (oauth2-proxy
# treats every non-empty line as an email) — keep the allowlist terse.
$ADMIN_EMAIL
EOF
chown root:oauth2-proxy /etc/oauth2-proxy/allowed_emails
chmod 0640 /etc/oauth2-proxy/allowed_emails
echo "  wrote /etc/oauth2-proxy/allowed_emails (1 user)"

# oauth2-proxy systemd unit
install -m 0644 -o root -g root \
    "$TEMPLATE_DIR/oauth2-proxy.service" \
    /etc/systemd/system/oauth2-proxy.service
echo "  wrote /etc/systemd/system/oauth2-proxy.service"

systemctl daemon-reload

# --- Firewall -------------------------------------------------------
echo
echo "[7/8] firewall"
if command -v firewall-cmd &>/dev/null; then
    for SVC in http https; do
        if firewall-cmd --permanent --query-service="$SVC" >/dev/null 2>&1; then
            echo "  $SVC already open"
        else
            firewall-cmd --permanent --add-service="$SVC"
            echo "  opened $SVC"
        fi
    done
    firewall-cmd --reload
    echo "  firewalld reloaded"
else
    echo "  WARN: firewall-cmd not found; skipping. Ensure 80 + 443 are open."
fi

# --- Start services ------------------------------------------------
echo
echo "[8/8] enable + start services"
systemctl enable --now oauth2-proxy
systemctl enable --now caddy

sleep 3
echo
echo "=== STATUS ==="
for SVC in caddy oauth2-proxy rt-dashboard; do
    STATE=$(systemctl is-active "$SVC" 2>/dev/null || echo "inactive")
    echo "  $SVC: $STATE"
done

echo
echo "=== DONE ==="
echo
echo "Open https://$DOMAIN in your browser. On first visit:"
echo "  1. Caddy obtains a Let's Encrypt cert (takes 5-20s the first time)"
echo "  2. You are redirected to Google's sign-in page"
echo "  3. Sign in with $ADMIN_EMAIL (Yubikey 2FA prompt from Google)"
echo "  4. You land on the razortrade dashboard"
echo
echo "To add more allowed emails: edit /etc/oauth2-proxy/allowed_emails"
echo "  and run: sudo systemctl restart oauth2-proxy"
echo
echo "To see what's happening:"
echo "  journalctl -u caddy -f"
echo "  journalctl -u oauth2-proxy -f"
