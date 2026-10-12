#!/usr/bin/env bash
# install.sh — install deepshit on a server, native, no Docker, idempotent.
# If sudo is needed, your password is asked on screen (interactive).
# Usage: sudo bash deploy/install.sh [path-to-binary]
set -euo pipefail
BIN_SRC="${1:-./target/release/deepshit}"
BIN_DST="/usr/local/bin/deepshit"
DATA_DIR="/var/lib/deepshit/ds-data"
MEDIA_DIR="/var/lib/deepshit/ds-media"
API_TOKEN="${API_TOKEN:-}"

need_sudo() {
  if [ "$(id -u)" -ne 0 ]; then
    echo ">> Root access needed — type your sudo password below:"
    sudo -v
  fi
}
SUDO=""; [ "$(id -u)" -ne 0 ] && SUDO="sudo"
need_sudo

echo "[1/3] binary -> $BIN_DST"
$SUDO install -m 0755 "$BIN_SRC" "$BIN_DST"

echo "[2/3] user ds + data folders"
id ds >/dev/null 2>&1 || $SUDO useradd -r -m -d /var/lib/deepshit ds
$SUDO mkdir -p "$DATA_DIR" "$MEDIA_DIR"; $SUDO chown -R ds:ds /var/lib/deepshit

if [ -n "$API_TOKEN" ]; then
  echo ">> API_TOKEN is set: endpoints need Bearer (except /health)."
else
  echo ">> API_TOKEN is empty: API is open. Set API_TOKEN=... for production."
fi
echo "[3/3] service"
$SUDO install -m 0644 deploy/deepshit.service /etc/systemd/system/deepshit.service
$SUDO systemctl daemon-reload
$SUDO systemctl enable --now deepshit
$SUDO systemctl --no-pager status deepshit | head -15
echo "OK: curl http://127.0.0.1:3000/health"
echo "Login first: curl -X POST localhost:3000/sessions/login -H 'content-type: application/json' -d '{\"id\":\"utama\",\"token\":\"...\"}'"
