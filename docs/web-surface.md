# Web surface — build contract

Status: **contract v4** (2026-08-31). This document is the binding
specification for the web surface — the operator-facing UI built against
the shipped Amparo binary. It is the "escape hatch" named in
`docs/swarms-advanced.md`: a face for the agent, not a new trust
boundary. The builder is a second engineer working on the production
host; this contract is what they build against, and it wins over any
other instruction they receive.

Amparo is an open agent that acts under policy, bring your own LLM.
The web surface changes none of that.

## 1. The shape

Amparo is headless. The faces it ships are the CLI, the chat surfaces
(Telegram/Discord/Slack), and the MCP server. The web surface adds a
fourth face with exactly the same contract as the others:

- the **gate chain is unchanged** — registry lookup → trust ceiling →
  policy engine → human approval, per tool call;
- the **approver is the human** — the UI renders approval requests, a
  human decides; there is no auto-approve path the operator did not
  explicitly configure (`--auto-approve` is the operator's explicit
  flag, never a UI default);
- the **app holds no secrets** — policy keys, LLM keys, and workspace
  contents live in the spawned Amparo process and the workspace files;
  the app never stores them.

## 2. Process model

The web backend drives tasks by spawning the shipped binary, one
process per task, in the task's workspace:

```
amparo run "<task>" \
  --workspace /srv/amparo/workspaces/<operator>/ \
  [--policy-url <url>] [--trust-ceiling <tier>] [--max-sub-agents N] \
  [--growth] [--resume] \
  --approval-endpoint http://127.0.0.1:<app_port>/approvals
```

Contract with the spawned process:

- **stdout** carries the bare final answer, nothing else.
- **stderr** carries the event stream: one `[tag]` line per event
  (`[task]`, `[gate]`, `[approval]`, `[preflight]`, `[spawn]`,
  `[schedule]`, `[privacy]`, `[qc]`, …). The UI renders these live.
- **exit codes**: `0` ok, `1` task/runtime failure, `2` usage error.
- **`--approval-endpoint`** (shipped in v0.9.0; see §3) replaces the
  interactive gate — mutually exclusive with `--auto-approve` and
  `--auto-deny` (usage error otherwise). The same flag ships on
  `amparo mcp-serve` (mutually exclusive with `--auto-approve` there;
  MCP has no interactive gate).
- The workspace is Amparo's per-operator state root; the UI's operator
  views read it (§5). The backend serializes tasks per operator
  workspace (one `amparo run` at a time per workspace) — Amparo itself
  does not enforce cross-process exclusivity.

For tool-level integrations (not the task runner), the backend may also
spawn `amparo mcp-serve` (stdio JSON-RPC 2.0, newline-delimited,
stdout protocol-only). With `--max-sub-agents N` (v0.9.0, M10 W5) the
server registers `spawn_agent` — the web's task reaches swarms through
the MCP surface under the same shared budget and gate chain as the task
runner. Without the flag `spawn_agent` stays out of the tool list (and
the server needs no inference env). The task runner above is the
primary surface.

`amparo run` always registers the schedule tool (M8 W5): a promise
persists to the workspace queue, and — the CLI is process-scoped, there
is no ticker — it fires at the next run start inside the 60s grace
window, re-entering the same gate chain with a reduced tool set
(no spawn/schedule recursion). A promise past its instant beyond the
grace window is marked missed, never fired late. The operator views
read the same queue (§4).

## 3. The approval contract (`--approval-endpoint`)

Shipped in **v0.9.0** (M10 W4). The JSON below is pinned by Amparo's
tests and will not change without a contract-version bump.

When the gate needs a human, it `POST`s to `<endpoint>/approvals`:

```json
{
  "call_id": "call-…",
  "tool_name": "write_file",
  "arguments": { "path": "note.txt", "content": "…" },
  "reasons": ["policy escalated: needs human review"],
  "blast_radius": "workspace_local",
  "session_label": "sub-agent sess-123.1 of task sess-123",
  "rollback": {
    "undo": "restore the previous contents of note.txt",
    "markers": ["note.txt.amparo-bak"]
  }
}
```

- `blast_radius` ∈ `read_only | workspace_local | sub_agent | network |
  system_wide | destructive` (serde snake_case; `null` = unclassified).
- `session_label` names the delegation chain (`null` for top-level
  tasks). It renders in the approval copy as
  `[session] sub-agent sess-123.1 of task sess-123 wants to run:`.
- `blast_radius` renders as the preflight line, e.g.
  `[preflight] blast radius: destructive — rm -rf matches a blocked
  destructive pattern`.
- `arguments` are the real arguments — the operator approves the
  concrete consequence, not an abstraction. The UI shows them verbatim
  and must not persist them beyond the session.
- `rollback` names the tool's declared rollback group (M10 W3): a
  human-readable `undo` description plus the `markers` the tool backs up
  before execution (`.amparo-bak` files). `null` when the tool declares
  no rollback. Like the fields above it is display-only — Amparo never
  executes rollback automatically.

Flow: `POST` returns `200 {"call_id": "…", "status": "pending"}`; the
backend presents the request to the operator. The gate polls
`GET <endpoint>/approvals/<call_id>` → `{"status": "pending"}` until it
returns `{"status": "decided", "decision": true|false}`. **60-second
timeout, fail closed**: no decision by then is a denial. All three
fields (`blast_radius`, `session_label`, `rollback`) are display-only
by invariant I1 — they never feed back into the gate decision.

The `POST` may refuse the registration (v4): `400` for a `call_id`
outside the poll-able charset `[A-Za-z0-9._-]` (max 256), `409` for a
duplicate `call_id` (one registration per id), `429` when the pending
queue is at capacity (64) or the endpoint's POST rate limit is hit.
The gate treats every non-2xx as a denial — refusals fail closed, never
queue.

## 4. Feature parity — what the UI must show

The web surface is the front end chat surfaces cannot be. Minimum
feature set:

1. **Task runner** — start a task, see the `[tag]` stream live, receive
   the final answer. The final report's method section includes the
   **swarm report**: `swarm: N sub-agent(s) (ids), M tool calls, ~$X.XX
   in inference (estimate, chars/4, $R/1M tokens)` — rendered verbatim,
   including the honesty qualifiers.
2. **Approval queue** — pending requests with the full copy above;
   approve/deny per request; expiry at 60s shown as auto-deny; double
   presses are safe (one decision per call_id wins).
3. **Privacy ledger view** — read-only render of
   `<workspace>/.amparo/privacy/ledger.jsonl` (one JSON object per
   line). Counts never values: the file records PII-strip counts and
   sites as scheme+host; never command text. The UI applies the same
   rule and never logs PII client-side.
4. **Sessions + resume** — list `<workspace>/.amparo/sessions/*/*.json`;
   a `Running` checkpoint can be resumed (`--resume`). Show status,
   started_at, task id.
5. **Schedule queue** — the chat host's promise queue
   (`<workspace>/.amparo/schedule/<id>.json`); cancel = status change,
   never deletion.
6. **Notebook views** — under `--growth`: run records, skills, rollup
   state (`<workspace>/.amparo/notebook/`).

## 5. Voice and privacy

- **Scientific tone** (spec `m6-controlled-growth.md` §7): the agent
  reports method, observations, conclusion, open items. The UI's own
  copy follows it — no persona, no inner-state theater, no invented
  metrics. The `[tag]` vocabulary is the shared language; the UI labels
  with it, never against it.
- **PII**: Amparo strips PII before persistence (invariant I6) and the
  ledger records strip counts. The UI must not add a persistence path
  for unstripped text — no localStorage drafts of approval arguments, no
  server logs of task bodies.

## 6. Deployment

Target: `amparo.ellmstack.dev` on the production site box — the same
Hetzner host (204.168.163.161) that serves the other ellmstack.dev
product vhosts (console, guardrail, engram, downloads).

- **ssh**: `root@204.168.163.161 -i ~/.ssh/engram_hetzner_ed25519`.
- **App**: under `/srv/amparo/`, binds `127.0.0.1:<port>` only. A
  sandboxed non-root systemd unit modeled on the sync relay's
  `engramd-sync.service` (NoNewPrivileges, ProtectSystem=strict).
- **Caddy 2.11**: new `/etc/caddy/amparo.Caddyfile` imported from the
  main Caddyfile (the engram.Caddyfile import pattern); site block
  copies the console/guardrail header block (X-Frame-Options, nosniff,
  HSTS, per-vhost CSP); `reverse_proxy localhost:<port>`. Reload with
  `systemctl reload caddy`.
- **DNS**: `amparo.ellmstack.dev` A record → 204.168.163.161,
  **DNS-only (grey cloud)** like every sibling product vhost — Caddy
  auto-HTTPS (Let's Encrypt) terminates TLS. The record is created by
  the operator (Cloudflare).
- **Binary**: build the Amparo repo from source on the box (Rust
  toolchain already present), pinned at tag **`v0.15.0`** — the release
  the approval seam ships in.

## 7. Pinning and versioning

The builder pins the Amparo binary by tag and this contract by version.
A change to the approval JSON, the flag surface, or the workspace file
layouts is a contract-version bump and is communicated to the builder
before it lands. The seam's JSON is pinned by Amparo's tests.

**v1 → v2**: the approval JSON gained `rollback` (M10 W3, display-only)
and the example now shows a `write_file` that declares one;
`--approval-endpoint` also ships on `amparo mcp-serve`. No
wire-semantics changes — the POST/poll flow and the 60s fail-closed
deadline are unchanged.

**v2 → v3**: M10 W5. `amparo mcp-serve` gains `--max-sub-agents N` — the
opt-in `spawn_agent` tool, same budget machinery as `amparo run` (the
budget is shared across the server; exhaustion fails closed). The CLI
scheduler lands in `amparo run`: the always-on schedule tool plus a
run-start scan of the workspace queue — due promises fire through the
same gate chain with a reduced tool set (spawn/schedule recursion
excluded), overdue promises are marked missed, and the scan touches
only promises the CLI itself wrote (chat promises belong to the chat
host's ticker). No approval-JSON or flag-removal changes.

**v3 → v4**: pre-reveal security audit (2026-08-31). The approval JSON
and the gate's POST/poll flow are unchanged; the backend's surface
hardens and the binary pin moves to v0.10.0: the SSE stream switches
from `?token=` to a **one-shot `?ticket=`** issued over the authed API
(`POST /api/tickets`) — the operator token never rides a URL; the
`POST /approvals` registration rejects invalid (`400`), duplicate
(`409`) and over-capacity (`429`) `call_id`s, with the pending queue
capped at 64; POSTs are rate-limited per source address (`429`).

**v4 → v5**: update notifications. The backend gains a public
`GET /version` (no token) — the shipped binary's version, probed once
at boot; Caddy serves the site's generated `/version.json`, `/changelog`
and `/feed.xml` from `/srv/amparo/site` before the reverse_proxy (see
`web/deploy/`). The operator UI shows a dismissible banner when the
site's version is newer than the binary's; the CLI gets
`amparo update check` and the TUI boot banner an `[update]` line. No
approval-JSON, flag-removal, or workspace-layout changes.

## 8. Deliberately excluded

- No policy keys, LLM keys, or secrets in the app (the spawned Amparo
  process holds them).
- No auto-approve defaults; no path that skips the gate chain.
- No writes to the cold archive (`records.jsonl` is never modified by
  anyone or anything).
- No modification of the Rust workspace or gate semantics — the web
  surface is a consumer of the shipped binary.
- Not a replacement for the chat surfaces; it adds the operator views
  they cannot show.
