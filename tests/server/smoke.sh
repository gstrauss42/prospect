#!/usr/bin/env bash
# Smoke test: build the release binary, start the mock upstreams and the server, walk the main
# business flow with curl, then print the binary size and the server's memory use.
#
#   tests/server/smoke.sh            (ports 9930 mock, 9931 server; no real API is called)
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
MOCK_PORT=9930
PORT=9931
BASE="http://127.0.0.1:$PORT"
WORK="$(mktemp -d)"
JAR="$WORK/cookies"
MOCK_PID=""
SERVER_PID=""

cleanup() {
  [ -n "$SERVER_PID" ] && kill "$SERVER_PID" 2>/dev/null || true
  [ -n "$MOCK_PID" ] && kill "$MOCK_PID" 2>/dev/null || true
  wait 2>/dev/null || true
  rm -rf "$WORK"
}
trap cleanup EXIT

step() { printf '\n== %s\n' "$*"; }
json() { curl -sS -b "$JAR" -c "$JAR" -H 'Content-Type: application/json' "$@"; }
field() { python3 -c "import json,sys; d=json.load(sys.stdin); print(eval('d'+sys.argv[1]))" "$1"; }
rss_kb() { awk '/VmRSS/ {print $2}' "/proc/$SERVER_PID/status"; }

wait_idle() {
  for _ in $(seq 1 300); do
    if [ "$(curl -sS -b "$JAR" "$BASE/api/runs/current" | field "['run']")" = "None" ]; then return 0; fi
    sleep 0.2
  done
  echo "a run didn't finish in time" >&2
  exit 1
}

step "build (release)"
[ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"
cargo build --release -p server --manifest-path "$ROOT/Cargo.toml" 2>&1 | tail -1
BIN="$ROOT/target/release/opportunity"

step "start the mock and the server"
python3 "$ROOT/tests/mock/server.py" --port "$MOCK_PORT" >"$WORK/mock.log" 2>&1 &
MOCK_PID=$!
(
  cd "$WORK"
  exec env -i PATH="$PATH" ADDR="127.0.0.1:$PORT" PUBLIC_URL="$BASE" DATA_DIR="$WORK/data" \
    DEEPSEEK_API_KEY=mock DEEPSEEK_BASE_URL="http://127.0.0.1:$MOCK_PORT" \
    OPENVERSE_URL="http://127.0.0.1:$MOCK_PORT/openverse/v1" \
    COMMONS_API_URL="http://127.0.0.1:$MOCK_PORT/commons/w/api.php" \
    "$BIN"
) >"$WORK/server.log" 2>&1 &
SERVER_PID=$!
for _ in $(seq 1 100); do curl -sf "$BASE/healthz" >/dev/null 2>&1 && break; sleep 0.1; done
curl -sS "$BASE/healthz"; echo
sleep 1
IDLE_RSS=$(rss_kb)

step "sign up a business (research starts at once)"
json -d '{"business_name":"Acme Cleaning","website":"acme.example","email":"ops@acme.example","password":"correct horse battery","hints":"Office cleaning in Cape Town"}' \
  "$BASE/api/auth/signup" | field "['user']"
wait_idle
ME=$(curl -sS -b "$JAR" "$BASE/api/me")
echo "onboarding: $(echo "$ME" | field "['user']['onboarding']")   intel confidence: $(echo "$ME" | field "['intel']['confidence']")"

step "confirm the profile (ready + discovery)"
PROFILE=$(echo "$ME" | python3 -c "import json,sys; p=json.load(sys.stdin)['profile']; p.update(headline='Commercial cleaning for offices and clinics', industry='Cleaning services', seeking=['sales','partners'], interests=['green cleaning']); print(json.dumps({'profile': p}))")
json -X PUT -d "$PROFILE" "$BASE/api/profile" | field "['run']['kind']"
wait_idle
curl -sS -b "$JAR" "$BASE/api/runs/current" | field "['last_run']['summary']"

step "opportunities"
LIST=$(curl -sS -b "$JAR" "$BASE/api/opportunities")
echo "$LIST" | field "['counts']"
ID=$(echo "$LIST" | field "['items'][0]['id']")
echo "first: $(echo "$LIST" | field "['items'][0]['title']")"
json -d '{"saved":true}' "$BASE/api/opportunities/$ID/save" | field "['item']['status']"

step "theme, steering, settings, AI model, connectors"
curl -sS -b "$JAR" "$BASE/api/me" | field "['theme']"
json -d '{"text":"More public-sector tenders"}' "$BASE/api/steering" | field "['note']['text']"
json -X PUT -d '{"frequency":"daily"}' "$BASE/api/settings" | field "['settings']"
curl -sS -b "$JAR" "$BASE/api/settings/ai"; echo
curl -sS -b "$JAR" "$BASE/api/connectors" | field "['available'][0]['provider']"

step "a manual refresh while measuring"
json -d '{"kind":"discover"}' "$BASE/api/runs" | field "['run']['status']"
wait_idle
ACTIVE_RSS=$(rss_kb)
PEAK_RSS=$(awk '/VmHWM/ {print $2}' "/proc/$SERVER_PID/status")

step "graceful shutdown"
kill -TERM "$SERVER_PID"
wait "$SERVER_PID" && echo "exited cleanly" || echo "exit status $?"
SERVER_PID=""

step "numbers"
echo "binary:        $(du -h "$BIN" | cut -f1) ($(stat -c %s "$BIN") bytes)"
echo "RSS idle:      $((IDLE_RSS / 1024)) MB ($IDLE_RSS kB)"
echo "RSS after use: $((ACTIVE_RSS / 1024)) MB ($ACTIVE_RSS kB)"
echo "RSS peak:      $((PEAK_RSS / 1024)) MB ($PEAK_RSS kB, includes argon2's 19 MiB per hash)"
echo
echo "smoke test passed"
