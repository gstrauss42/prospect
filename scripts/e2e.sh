#!/usr/bin/env bash
# Full-system check with no real spend: release build, mock upstreams, the real server on a temp
# data dir, then the Playwright suite against it. Usage: scripts/e2e.sh [playwright args]
set -euo pipefail
cd "$(dirname "$0")/.."
[ -f "$HOME/.cargo/env" ] && source "$HOME/.cargo/env"
MOCK_PORT=${MOCK_PORT:-9901}
APP_PORT=${APP_PORT:-9902}
DATA=$(mktemp -d)
cargo build --release -q -p server
python3 tests/mock/server.py --port "$MOCK_PORT" >"$DATA/mock.log" 2>&1 &
MOCK_PID=$!
M="http://127.0.0.1:$MOCK_PORT"
env -i PATH="$PATH" HOME="$HOME" \
  ADDR="127.0.0.1:$APP_PORT" PUBLIC_URL="http://127.0.0.1:$APP_PORT" DATA_DIR="$DATA" \
  DEEPSEEK_API_KEY=mock DEEPSEEK_BASE_URL="$M" \
  OPENVERSE_URL="$M/openverse/v1" COMMONS_API_URL="$M/commons/w/api.php" \
  MAILGUN_API_KEY=mock MAILGUN_DOMAIN=mg.example.test MAILGUN_SENDER_EMAIL="Prospect <hello@example.test>" MAILGUN_BASE_URL="$M/mailgun" \
  BEDROCK_RUNTIME_ENDPOINT="$M/bedrock-runtime" BEDROCK_CONTROL_ENDPOINT="$M/bedrock" \
  SLACK_API_BASE="$M/slack" SLACK_AUTH_BASE="$M/slack" MS_LOGIN_BASE="$M/mslogin" MS_GRAPH_BASE="$M/msgraph" \
  ATLASSIAN_AUTH_BASE="$M/atlassian-auth" ATLASSIAN_API_BASE="$M/atlassian-api" CONFLUENCE_SITE_BASES="$M/confluence-site" \
  MS_CLIENT_ID=6731de76-14a6-49ae-97bc-6eba6914391e MS_CLIENT_SECRET='mock-ms-client-secret~not-a-real-one' \
  ATLASSIAN_CLIENT_ID=mock-atlassian-client-id ATLASSIAN_CLIENT_SECRET=mock-atlassian-client-secret \
  ./target/release/prospect >"$DATA/app.log" 2>&1 &
APP_PID=$!
trap 'kill $APP_PID $MOCK_PID 2>/dev/null; echo "logs: $DATA"' EXIT
for _ in $(seq 1 50); do curl -sf "http://127.0.0.1:$APP_PORT/healthz" >/dev/null && break; sleep 0.2; done
curl -sf "http://127.0.0.1:$APP_PORT/healthz" >/dev/null || { echo "server did not start"; tail -20 "$DATA/app.log"; exit 1; }
echo "server up on :$APP_PORT (mock :$MOCK_PORT, data $DATA)"
cd tests/e2e
[ -d node_modules ] || npm ci --silent || npm install --silent
BASE_URL="http://127.0.0.1:$APP_PORT" MOCK_URL="$M" npx playwright test "$@"
echo "violations of read-only: $(curl -s "$M/_violations")"
