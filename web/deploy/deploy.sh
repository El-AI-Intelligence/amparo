#!/usr/bin/env bash
# Idempotent on-box install for the Amparo web surface. Run as root on the
# site box (204.168.163.161). Expects:
#   - the `amparo` binary already built and installed at /srv/amparo/bin/amparo
#     (build from the source tree pinned at tag v0.10.0 — see README-deploy.md)
#   - Caddy 2.11 running with /etc/caddy/Caddyfile as the main config
#
# Usage: ./deploy.sh [path-to-web-dir]   (default: the parent of this script)
set -euo pipefail

if [ $# -ge 1 ]; then
  WEB_DIR="$(cd "$1" && pwd)"
else
  WEB_DIR="$(cd "$(dirname "$0")/.." && pwd)"
fi
[ -d "$WEB_DIR/public" ] || { echo "web dir not found (need public/): $WEB_DIR"; exit 2; }

echo "== source: $WEB_DIR"

# ── user + dirs ─────────────────────────────────────────────────────────────
id -u amparo >/dev/null 2>&1 || useradd --system --no-create-home --shell /usr/sbin/nologin amparo
mkdir -p /srv/amparo/app /srv/amparo/bin /srv/amparo/workspaces/operator /etc/amparo-web

# ── app files ───────────────────────────────────────────────────────────────
rsync -a --delete --exclude deploy --exclude test "$WEB_DIR/" /srv/amparo/app/
chown -R amparo:amparo /srv/amparo/app /srv/amparo/workspaces

# ── Update-notification files (changelog page, version.json, RSS) ──────────
# Generated at deploy time from CHANGELOG.md + the [workspace.package]
# version, so the site always advertises exactly what this deploy ships.
# Caddy serves them from /srv/amparo/site (amparo.Caddyfile handles) —
# separate from /srv/amparo/app because the app rsync --delete above
# would otherwise wipe them. The source tree must be alongside the web
# dir (the box layout: /srv/amparo/src/{CHANGELOG.md,Cargo.toml,web/}).
REPO_ROOT="$(cd "$WEB_DIR/.." && pwd)"
if [ ! -f "$REPO_ROOT/CHANGELOG.md" ]; then
  echo "FAIL: CHANGELOG.md not found at $REPO_ROOT — deploy from the repo checkout (/srv/amparo/src)" >&2
  exit 1
fi
if ! command -v python3 >/dev/null 2>&1; then
  echo "ERROR: python3 is required to generate the changelog/version/RSS files." >&2
  exit 1
fi
mkdir -p /srv/amparo/site
python3 "$WEB_DIR/deploy/gen-update-files.py" \
  --changelog "$REPO_ROOT/CHANGELOG.md" \
  --out-dir /srv/amparo/site \
  --site https://amparo.ellmstack.dev \
  --version-from-cargo "$REPO_ROOT/Cargo.toml"
# Generated as root on the box; make sure the caddy user can read them.
chown -R root:root /srv/amparo/site && chmod -R a+rX /srv/amparo/site

# ── env file (created once, never overwritten — holds the operator token) ───
if [ ! -f /etc/amparo-web/amparo-web.env ]; then
  umask 0077
  {
    echo "AMPARO_WEB_TOKEN=$(head -c 24 /dev/urandom | base64 | tr -d '=+/')"
    echo "AMPARO_PORT=47910"
    echo "AMPARO_BIN=/srv/amparo/bin/amparo"
    echo "AMPARO_WORKSPACES=/srv/amparo/workspaces"
    echo "AMPARO_OPERATOR=operator"
    echo "AMPARO_APPROVAL_ENDPOINT=0"
    echo "# Cutover: the operator fills these with real values —"
    echo "# AMPARO_INFERENCE_URL=   AMPARO_INFERENCE_MODEL=   AMPARO_INFERENCE_KEY="
    echo "# AMPARO_POLICY_KEY="
  } > /etc/amparo-web/amparo-web.env
  echo "== created /etc/amparo-web/amparo-web.env (token generated — retrieve with: cat)"
else
  echo "== keeping existing /etc/amparo-web/amparo-web.env"
fi
chmod 0640 /etc/amparo-web/amparo-web.env
chown root:amparo /etc/amparo-web/amparo-web.env

# ── systemd ─────────────────────────────────────────────────────────────────
install -m 0644 "$(dirname "$0")/amparo-web.service" /etc/systemd/system/amparo-web.service
systemctl daemon-reload
systemctl enable --now amparo-web
systemctl restart amparo-web
# No silent failures (audit 2026-08-31 LOW-9): a unit that came back
# down must abort the deploy loudly, not print a green "done".
if ! systemctl is-active --quiet amparo-web; then
  echo "FAIL: amparo-web is not active after restart"
  journalctl -u amparo-web -n 30 --no-pager || true
  exit 1
fi

# ── caddy ───────────────────────────────────────────────────────────────────
install -m 0644 "$(dirname "$0")/README-deploy.md" /srv/amparo/README-deploy.md
install -m 0644 "$(dirname "$0")/amparo.Caddyfile" /etc/caddy/amparo.Caddyfile
if ! grep -q 'import /etc/caddy/amparo.Caddyfile' /etc/caddy/Caddyfile; then
  echo 'import /etc/caddy/amparo.Caddyfile' >> /etc/caddy/Caddyfile
  echo "== added import to /etc/caddy/Caddyfile"
fi
caddy validate --config /etc/caddy/Caddyfile
systemctl reload caddy

echo "== done. Verify: curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1:47910/"
echo "== DNS is the operator's step: grey-cloud A record amparo.ellmstack.dev -> 204.168.163.161"
