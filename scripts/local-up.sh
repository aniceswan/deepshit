#!/usr/bin/env bash
# local-up.sh — one command for a local try-out. No sudo, no Docker, no credentials.
# Usage: bash scripts/local-up.sh
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
export PATH="$HOME/.cargo/bin:$PATH"
MEDIADIR="/tmp/opencode/deepshit-media"
DATADIR="/tmp/opencode/deepshit-data"
LISTEN="127.0.0.1:3105"

echo "== [1/3] build =="
cargo build -p ds-gateway 2>&1 | tail -1
BIN="$ROOT/target/debug/deepshit"

echo "== [2/3] file-mode setup =="
"$BIN" --media-dir "$MEDIADIR" --data-dir "$DATADIR" setup --auto

echo "== [3/3] run + smoke test =="
"$BIN" --media-dir "$MEDIADIR" --data-dir "$DATADIR" --listen "$LISTEN" --session-id "" run > /tmp/opencode/deepshit-local.log 2>&1 &
PID=$!
trap "kill $PID 2>/dev/null || true" EXIT
sleep 3
echo "-- health:"; curl -sf "http://$LISTEN/health"; echo
echo "-- models:"; curl -sf "http://$LISTEN/v1/models"; echo
echo "-- login without token (expect 400):"
curl -s -X POST "http://$LISTEN/sessions/login" -H 'content-type: application/json' -d '{"id":"x"}'; echo
echo "-- send without login (expect 404/409):"
curl -s -X POST "http://$LISTEN/sessions/nobody/chats" ; echo
echo "-- OpenAI without session (expect 503):"
curl -s -X POST "http://$LISTEN/v1/chat/completions" -H 'content-type: application/json' -d '{"model":"deepseek-chat","messages":[{"role":"user","content":"hello"}]}'; echo
echo "-- unknown model (expect 404):"
curl -s -X POST "http://$LISTEN/v1/chat/completions" -H 'content-type: application/json' -d '{"model":"gpt-4","messages":[{"role":"user","content":"hello"}]}'; echo

echo "== OK =="
echo "Server still running (PID $PID). Stop it: kill $PID"
echo "File archive: $DATADIR/archive/ | Media: $MEDIADIR | Log: /tmp/opencode/deepshit-local.log"
trap - EXIT
