#!/usr/bin/env bash
# Local acceptance smoke for the Amparo web surface (contract web-surface.md,
# prompt acceptance checklist). Runs entirely on loopback against the real
# `amparo` binary and a scripted mock LLM. ~90s (one 60s fail-closed wait).
set -euo pipefail
cd "$(dirname "$0")/.."
WEB_DIR="$(pwd)"
REPO_DIR="$(dirname "$WEB_DIR")"
BIN="$REPO_DIR/target/release/amparo"

PORT=47911
MOCK_PORT=45199
TOKEN='smoke-token'
BASE="http://127.0.0.1:$PORT"
AUTH="Authorization: Bearer $TOKEN"

TMP="$(mktemp -d)"
MOCK_PID=''
SERVER_PID=''
cleanup() {
  [ -n "$MOCK_PID" ] && kill "$MOCK_PID" 2>/dev/null || true
  [ -n "$SERVER_PID" ] && kill "$SERVER_PID" 2>/dev/null || true
  rm -rf "$TMP"
}
trap cleanup EXIT

say() { printf '\n== %s\n' "$1"; }

# ── binary ──────────────────────────────────────────────────────────────────
if [ ! -x "$BIN" ]; then
  say "building amparo (release)"
  cargo build --release --manifest-path "$REPO_DIR/Cargo.toml" >/dev/null
fi

# ── scripted mock LLM: [probe content (killed)], [tool call], [final answer] ─
cat > "$TMP/scripts.json" <<'JSON'
[
  [{"choices":[{"index":0,"delta":{"content":"probe"},"finish_reason":"stop"}]}],
  [{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"run_command","arguments":""}}]},"finish_reason":null}]},
   {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"command\":\"echo mock-tool\"}"}}]},"finish_reason":"tool_calls"}]}],
  [{"choices":[{"index":0,"delta":{"content":"Hello from the mock."},"finish_reason":"stop"}]}]
]
JSON

MOCK_PORT=$MOCK_PORT MOCK_DELAY_MS=8000 MOCK_SCRIPTS="$TMP/scripts.json" \
  node "$WEB_DIR/test/mock-inference.mjs" >"$TMP/mock.log" 2>&1 &
MOCK_PID=$!

AMPARO_WEB_TOKEN=$TOKEN AMPARO_PORT=$PORT AMPARO_BIN="$BIN" \
  AMPARO_WORKSPACES="$TMP/workspaces" AMPARO_OPERATOR=operator \
  AMPARO_INFERENCE_URL="http://127.0.0.1:$MOCK_PORT/v1" \
  AMPARO_INFERENCE_MODEL=mock-model \
  node "$WEB_DIR/server.mjs" >"$TMP/server.log" 2>&1 &
SERVER_PID=$!

for i in $(seq 1 50); do
  # NOTE: do NOT probe the mock here — its MOCK_DELAY_MS applies to the first
  # request, and the checkpoint test below depends on that delay still pending.
  curl -sf -H "$AUTH" "$BASE/api/health" >/dev/null 2>&1 && break
  sleep 0.2
done

say "auth: /api without token is refused"
code=$(curl -s -o /dev/null -w '%{http_code}' "$BASE/api/health")
[ "$code" = 401 ] || { echo "FAIL: want 401, got $code"; exit 1; }

say "auth: /api/health with token"
curl -sf -H "$AUTH" "$BASE/api/health" | grep -q '"ok":true' || { echo FAIL; exit 1; }

say "statics: / serves the shell"
curl -sf "$BASE/" | grep -qi 'amparo' || { echo FAIL; exit 1; }
curl -sf "$BASE/app.js" >/dev/null || { echo "FAIL: app.js"; exit 1; }

say "approvals: gate surface (POST /approvals) needs no token — loopback only"
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/approvals" \
  -H 'Content-Type: application/json' \
  -d '{"call_id":"call-probe","tool_name":"run_command","arguments":{"command":"true"},"reasons":["smoke"],"blast_radius":"read_only","session_label":null}')
[ "$code" = 200 ] || { echo "FAIL: want 200, got $code"; exit 1; }

say "checkpoint: kill a run mid-flight → Running checkpoint, then resume via API"
AMPARO_INFERENCE_URL="http://127.0.0.1:$MOCK_PORT/v1" AMPARO_INFERENCE_MODEL=mock-model \
  "$BIN" run --workspace "$TMP/workspaces/operator/" "checkpoint probe" >/dev/null 2>&1 &
PROBE_PID=$!
sleep 2
kill -9 "$PROBE_PID" 2>/dev/null || true
wait "$PROBE_PID" 2>/dev/null || true
grep -q '"status": *"running"' "$TMP/workspaces/operator/.amparo/sessions/cli/"*.json \
  || { echo "FAIL: no Running checkpoint"; ls -R "$TMP/workspaces"; exit 1; }

resume_id=$(curl -sf -X POST -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"resume":true}' "$BASE/api/tasks" | node -e 'let s="";process.stdin.on("data",c=>s+=c).on("end",()=>console.log(JSON.parse(s).id))')
for i in $(seq 1 50); do
  st=$(curl -sf -H "$AUTH" "$BASE/api/tasks/$resume_id" | node -e 'let s="";process.stdin.on("data",c=>s+=c).on("end",()=>console.log(JSON.parse(s).status))')
  [ "$st" = done ] || [ "$st" = failed ] && break
  sleep 0.3
done
[ "$st" = done ] || { echo "FAIL: resume status=$st"; cat "$TMP/server.log"; exit 1; }
echo "resume completed (task $resume_id)"

say "task e2e: growth run against mock LLM, answer on stdout"
task_id=$(curl -sf -X POST -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"task":"say hello","growth":true}' "$BASE/api/tasks" | node -e 'let s="";process.stdin.on("data",c=>s+=c).on("end",()=>console.log(JSON.parse(s).id))')
for i in $(seq 1 50); do
  rec=$(curl -sf -H "$AUTH" "$BASE/api/tasks/$task_id")
  st=$(echo "$rec" | node -e 'let s="";process.stdin.on("data",c=>s+=c).on("end",()=>console.log(JSON.parse(s).status))')
  [ "$st" = done ] || [ "$st" = failed ] && break
  sleep 0.3
done
[ "$st" = done ] || { echo "FAIL: task status=$st"; cat "$TMP/server.log"; exit 1; }
echo "$rec" | grep -q 'Hello from the mock.' || { echo "FAIL: answer missing: $rec"; exit 1; }

say "event stream: SSE replay carries [tag] lines and a terminal done event"
sse=$(curl -sf -N -H "$AUTH" "$BASE/api/tasks/$task_id/events")
echo "$sse" | grep -q 'event: line' || { echo "FAIL: no line events"; echo "$sse"; exit 1; }
echo "$sse" | grep -q '\[task' || { echo "FAIL: no [task] line"; echo "$sse"; exit 1; }
echo "$sse" | grep -q 'event: done' || { echo "FAIL: no done event"; exit 1; }

say "workspace views"

# A policy-denied call never reaches execution, so the ledger only records
# executed network-tool calls and human denials (ledger_sink.rs). Produce a
# real row with a direct CLI run (--allow-all lives outside the app — the
# web surface deliberately never exposes it).
MOCK_PORT=45200 MOCK_MODE=tool node "$WEB_DIR/test/mock-inference.mjs" >"$TMP/mock2.log" 2>&1 &
MOCK2_PID=$!
sleep 1
AMPARO_INFERENCE_URL="http://127.0.0.1:45200/v1" AMPARO_INFERENCE_MODEL=mock-model \
  "$BIN" run --allow-all --workspace "$TMP/workspaces/operator/" "ledger probe" \
  </dev/null >"$TMP/ledger-probe.log" 2>&1 || true
kill "$MOCK2_PID" 2>/dev/null || true

curl -sf -H "$AUTH" "$BASE/api/privacy-ledger" > "$TMP/ledger.json"
node -e 'const r=require(process.argv[1]).rows; if(!r.length) throw new Error("ledger empty"); console.log("ledger rows:", r.length)' "$TMP/ledger.json"
curl -sf -H "$AUTH" "$BASE/api/sessions" | grep -q '"status":"complete"' || { echo "FAIL: sessions"; exit 1; }
curl -sf -H "$AUTH" "$BASE/api/schedule" | grep -q '"schedule"' || { echo "FAIL: schedule"; exit 1; }
curl -sf -H "$AUTH" "$BASE/api/notebook/records" | grep -q 'Hello from the mock.' \
  || { echo "FAIL: notebook records"; exit 1; }
curl -sf -H "$AUTH" "$BASE/api/notebook/skills" >/dev/null
curl -sf -H "$AUTH" "$BASE/api/notebook/rollup" >/dev/null

say "approval round-trip (gate harness against the mock endpoint)"
export AMPARO_WEB_TOKEN=$TOKEN
node "$WEB_DIR/test/gate-harness.mjs" "$BASE" approve
node "$WEB_DIR/test/gate-harness.mjs" "$BASE" deny
node "$WEB_DIR/test/gate-harness.mjs" "$BASE" double
node "$WEB_DIR/test/gate-harness.mjs" "$BASE" expire   # 60s fail-closed wait

say "queue: two submissions run serialized"
curl -sf -o /dev/null -X POST -H "$AUTH" -H 'Content-Type: application/json' -d '{"task":"one"}' "$BASE/api/tasks"
curl -sf -o /dev/null -X POST -H "$AUTH" -H 'Content-Type: application/json' -d '{"task":"two"}' "$BASE/api/tasks"
sleep 3
curl -sf -H "$AUTH" "$BASE/api/tasks" | grep -q '"status":"done"' || { echo "FAIL: queue"; exit 1; }

say "SMOKE OK"
