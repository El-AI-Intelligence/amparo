# Amparo web surface

The operator-facing web UI for Amparo — a thin bridge over the shipped
`amparo` binary. Binding contract: `../docs/web-surface.md` v4. The gate
chain (registry → trust ceiling → policy → human approval) lives in the
spawned `amparo` process; this app holds no secrets on disk, has no
auto-approve path, and never persists task bodies or approval arguments.

## Layout

- `server.mjs` — zero-dependency Node ≥18 backend. Serves the SPA, a JSON
  API, and the SSE task event stream (one-shot `?ticket=` auth, contract
  v4); spawns one `amparo run` per task, serialized per operator
  workspace; serves the contract §3 approval seam (`POST /approvals`,
  `GET /approvals/<call_id>`, 60s fail-closed) that the shipped
  `--approval-endpoint` posts to.
- `public/` — no-build vanilla SPA (hash routing, instrument-panel tone).
- `test/` — `mock-inference.mjs` (scripted mock LLM matching
  `crates/amparo-cli/tests/cli_e2e.rs`), `gate-harness.mjs` (simulates the
  v0.9.0 gate against the mock endpoint), `smoke.sh` (full local
  acceptance, ~90s).
- `deploy/` — systemd unit, Caddyfile, on-box `deploy.sh`, and
  `README-deploy.md` (production operations).

## Local development

```sh
./test/smoke.sh        # builds amparo if needed, runs the whole checklist
```

Manual run:

```sh
AMPARO_WEB_TOKEN=dev-token AMPARO_PORT=47911 \
  AMPARO_BIN=../target/release/amparo \
  AMPARO_WORKSPACES=/tmp/amparo-ws AMPARO_OPERATOR=operator \
  AMPARO_INFERENCE_URL=http://127.0.0.1:45199/v1 \
  AMPARO_INFERENCE_MODEL=mock-model \
  node server.mjs
# in another shell:
MOCK_PORT=45199 node test/mock-inference.mjs
# then open http://127.0.0.1:47911/ and enter dev-token
```

## Env surface

See the header comment of `server.mjs` and `deploy/README-deploy.md`.
