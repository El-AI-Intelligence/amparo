#!/usr/bin/env bash
# On-box acceptance for the Amparo web surface (prompt checklist). Run as
# root on the site box after deploy.sh. Uses the mock LLM on loopback; the
# service's env file already points AMPARO_INFERENCE_* at it.
# ~90s (one 60s fail-closed approval wait).
set -euo pipefail

PORT=47910
MOCK_PORT=45199
BASE="http://127.0.0.1:$PORT"
TOKEN=$(grep '^AMPARO_WEB_TOKEN=' /etc/amparo-web/amparo-web.env | cut -d= -f2)
AUTH="Authorization: Bearer $TOKEN"
WS=/srv/amparo/workspaces/operator
MOCK=/srv/amparo/src/web/test/mock-inference.mjs
HARNESS=/srv/amparo/src/web/test/gate-harness.mjs

say() { printf '\n== %s\n' "$1"; }

# Fresh mock scripts: [probe (killed mid-delay)], [tool call], [final answer]
SCRIPTS=$(mktemp)
cat > "$SCRIPTS" <<'JSON'
[
  [{"choices":[{"index":0,"delta":{"content":"probe"},"finish_reason":"stop"}]}],
  [{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"run_command","arguments":""}}]},"finish_reason":null}]},
   {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"command\":\"echo mock-tool\"}"}}]},"finish_reason":"tool_calls"}]}],
  [{"choices":[{"index":0,"delta":{"content":"Hello from the mock."},"finish_reason":"stop"}]}]
]
JSON
MOCK_PORT=$MOCK_PORT MOCK_DELAY_MS=8000 MOCK_SCRIPTS="$SCRIPTS" \
  nohup node "$MOCK" >/var/log/amparo-mock-inference.log 2>&1 &
MOCK_PID=$!
cleanup() { kill "$MOCK_PID" 2>/dev/null || true; rm -f "$SCRIPTS"; }
trap cleanup EXIT
sleep 1

say "1. service + statics + auth"
systemctl is-active --quiet amparo-web && echo "unit active"
[ "$(curl -s -o /dev/null -w '%{http_code}' "$BASE/")" = 200 ] && echo "statics 200"
[ "$(curl -s -o /dev/null -w '%{http_code}' "$BASE/api/health")" = 401 ] && echo "unauth → 401"
curl -sf -H "$AUTH" "$BASE/api/health" | grep -q '"ok":true' && echo "health ok"

say "2. e2e task against mock LLM (killed run → Running checkpoint → resume, then fresh run)"
# Killed run as the amparo user (keeps workspace ownership) leaves a
# Running checkpoint; the first mock request is delayed 8s.
runuser -u amparo -- env \
  AMPARO_INFERENCE_URL="http://127.0.0.1:$MOCK_PORT/v1" AMPARO_INFERENCE_MODEL=mock-model \
  /srv/amparo/bin/amparo run --workspace "$WS/" "checkpoint probe" >/dev/null 2>&1 &
PROBE=$!
sleep 2
kill -9 "$PROBE" 2>/dev/null || true
wait "$PROBE" 2>/dev/null || true
grep -q '"status": *"running"' "$WS/.amparo/sessions/cli/"*.json && echo "Running checkpoint present"

wait_done() { # $1 = task id
  for i in $(seq 1 60); do
    st=$(curl -sf -H "$AUTH" "$BASE/api/tasks/$1" | node -e 'let s="";process.stdin.on("data",c=>s+=c).on("end",()=>console.log(JSON.parse(s).status))')
    case "$st" in done|failed|usage_error) echo "$st"; return;; esac
    sleep 0.5
  done
  echo "timeout"
}

rid=$(curl -sf -X POST -H "$AUTH" -H 'Content-Type: application/json' -d '{"resume":true}' "$BASE/api/tasks" \
  | node -e 'let s="";process.stdin.on("data",c=>s+=c).on("end",()=>console.log(JSON.parse(s).id))')
[ "$(wait_done "$rid")" = done ] && echo "resume run: done" || { echo "FAIL: resume"; exit 1; }

tid=$(curl -sf -X POST -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"task":"say hello","growth":true}' "$BASE/api/tasks" \
  | node -e 'let s="";process.stdin.on("data",c=>s+=c).on("end",()=>console.log(JSON.parse(s).id))')
[ "$(wait_done "$tid")" = done ] || { echo "FAIL: task"; exit 1; }
curl -sf -H "$AUTH" "$BASE/api/tasks/$tid" | grep -q 'Hello from the mock.' && echo "final answer landed"

say "3. live [tag] stream (SSE replay after completion)"
sse=$(curl -sf -N -H "$AUTH" "$BASE/api/tasks/$tid/events")
echo "$sse" | grep -q '\[task' && echo "$sse" | grep -q 'event: done' && echo "SSE stream ok"

say "4. approval round-trip via gate harness (mock of --approval-endpoint)"
export AMPARO_WEB_TOKEN=$TOKEN
node "$HARNESS" "$BASE" approve
node "$HARNESS" "$BASE" deny
node "$HARNESS" "$BASE" double
node "$HARNESS" "$BASE" expire   # 60s fail-closed wait

say "5. ledger / sessions / schedule / notebook views"
# A policy-denied call never reaches execution, so the ledger only records
# executed network-tool calls and human denials (ledger_sink.rs). Produce a
# real row with a direct CLI run as the amparo user (--allow-all lives
# outside the app — the web surface deliberately never exposes it).
MOCK_PORT=45200 MOCK_MODE=tool nohup node "$MOCK" >/var/log/amparo-mock2.log 2>&1 &
MOCK2=$!
sleep 1
runuser -u amparo -- env \
  AMPARO_INFERENCE_URL="http://127.0.0.1:45200/v1" AMPARO_INFERENCE_MODEL=mock-model \
  /srv/amparo/bin/amparo run --allow-all --workspace "$WS/" "ledger probe" \
  </dev/null >/dev/null 2>&1 || true
kill "$MOCK2" 2>/dev/null || true
curl -sf -H "$AUTH" "$BASE/api/privacy-ledger" > /tmp/ledger-view.json
node -e 'const r=require("/tmp/ledger-view.json").rows; if(!r.length) process.exit(1); console.log(`ledger rows: ${r.length}`)' \
  || { echo "FAIL: ledger empty"; exit 1; }
curl -sf -H "$AUTH" "$BASE/api/sessions" | grep -q '"status":"complete"' && echo "sessions list ok"
curl -sf -H "$AUTH" "$BASE/api/schedule" | grep -q '"schedule"' && echo "schedule view ok"
curl -sf -H "$AUTH" "$BASE/api/notebook/records" | grep -q 'Hello from the mock.' && echo "notebook records ok"
curl -sf -H "$AUTH" "$BASE/api/notebook/skills" >/dev/null && echo "skills view ok"
curl -sf -H "$AUTH" "$BASE/api/notebook/rollup" >/dev/null && echo "rollup view ok"

say "6. restart survival + caddy"
systemctl daemon-reload
systemctl restart amparo-web
sleep 1
systemctl is-active --quiet amparo-web && echo "restart: active"
curl -sf -H "$AUTH" "$BASE/api/health" >/dev/null && echo "healthy after restart"
caddy validate --config /etc/caddy/Caddyfile 2>&1 | grep -q 'Valid configuration' && echo "caddy valid"

say "7. README-deploy.md"
[ -s /srv/amparo/README-deploy.md ] && echo "present"

say "BOX ACCEPTANCE OK"
