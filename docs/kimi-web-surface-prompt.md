# Kimi prompt — build the Amparo web surface

The prompt below is meant to be pasted verbatim into Kimi. Everything
Kimi needs to *read* is in the Amparo repository; everything Kimi needs
to *authenticate* (repo credentials, the production ssh key) is provided
in Kimi's environment — never pasted here.

---

Build the web surface for **Amparo** — an open agent that acts under
policy (bring your own LLM). You are building on the production host and
your output must be deployable and ready.

## Mission

A web UI for Amparo, deployed at `amparo.ellmstack.dev` on the Hetzner
site box that already hosts the other ellmstack.dev products, in the
same Caddy/systemd pattern they use. Amparo stays headless; the web app
is a face for it — not a new trust boundary.

## Read these first — in this order

1. Clone the Amparo repository (private:
   `github.com/El-AI-Intelligence/amparo`, credentials in your
   environment; read-only use — do not push to it) and check out tag
   `v0.7.0`.
2. `docs/web-surface.md` — **the build contract. It wins over everything
   else in this prompt.**
3. `docs/m6-controlled-growth.md` §7 — the scientific voice. The agent
   reports method, observations, conclusion, open items; no persona, no
   inner-state theater. The UI's own copy follows the same register.
4. `docs/m8-swarms.md` — what sub-agents and scheduling are, and what
   the swarm report looks like.
5. `docs/trial-bundle.md` — the graceful-degradation posture (Engram /
   Guardrail may be absent; Amparo degrades, so must the UI).

## What Amparo is (the parts that constrain you)

- The **gate chain** per tool call: registry lookup → trust ceiling →
  policy engine → human approval. The UI must never add a path around
  it. Approvals are always a human's decision, rendered from Amparo's
  approval request; there is no auto-approve the operator did not
  explicitly configure.
- Invariants I1–I6 hold (policy/gate/prompts never auto-tuned;
  per-tenant namespacing; provenance everywhere; revocable; the prompt
  is immutable; PII is stripped before persistence). The UI must not
  weaken any of them — in particular it must not persist unstripped
  task text or approval arguments (no localStorage drafts of
  arguments, no server logs of task bodies).
- The UI **never holds secrets**: no policy keys, no LLM keys. The
  spawned Amparo process holds them.

## Architecture

Backend = a thin bridge over the shipped binary. **One process per
task**:

```
amparo run "<task>" \
  --workspace /srv/amparo/workspaces/<operator>/ \
  [--policy-url <url>] [--trust-ceiling <tier>] [--max-sub-agents N] \
  [--growth] [--resume] \
  --approval-endpoint http://127.0.0.1:<app_port>/approvals
```

- stdout = the bare final answer; stderr = the live `[tag]` event
  stream — render it as it arrives. Exit codes: 0 ok / 1 failure /
  2 usage.
- `--approval-endpoint` ships in Amparo v0.9.0. **Until then, implement
  a mock of the endpoint** in the app per the contract in
  `docs/web-surface.md` §3 (POST the request, GET the decision, 60s
  fail-closed) and build the whole approval UI against the mock. The
  JSON is pinned; the real flag drops in later and nothing else
  changes.
- Serialize tasks per operator workspace (one `amparo run` at a time
  per workspace).
- Tool-level access may also use `amparo mcp-serve` (stdio JSON-RPC) —
  the task runner is the primary surface.

## UI requirements (minimum feature set)

From the contract §4, all of them:

1. **Task runner** — start task, live `[tag]` stream, final answer
   including the swarm report line verbatim
   (`swarm: N sub-agent(s) …, ~$X.XX in inference (estimate, chars/4,
   $R/1M tokens)`).
2. **Approval queue** — render the full copy: tool name, arguments
   verbatim, reasons, the preflight line (`[preflight] blast radius:
   …`), the delegation chain (`[session] sub-agent sess-123.1 of task
   sess-123 wants to run:`). Approve/deny; 60s expiry shown; double
   presses safe.
3. **Privacy ledger view** — read-only render of
   `<workspace>/.amparo/privacy/ledger.jsonl` (one JSON object per
   line). Counts never values.
4. **Sessions + resume** — list `<workspace>/.amparo/sessions/*/*.json`,
   show status, allow `--resume` of a Running checkpoint.
5. **Schedule queue** — `<workspace>/.amparo/schedule/<id>.json`;
   cancel = status change, never deletion.
6. **Notebook views** — under `--growth`: run records, skills, rollup
   state (`<workspace>/.amparo/notebook/`).

Design: the "observatory" register of `docs/swarms-advanced.md` — an
instrument panel, not a chatbot persona. Stack is your choice; it must
run on the box (Node/TS is fine).

## Build environment

The production site box:

- ssh `root@204.168.163.161 -i ~/.ssh/engram_hetzner_ed25519`
  (key in your environment).
- Rust toolchain is already on the box (`/root/engram` and
  `/root/ellm-guardrail` are built in place). Build Amparo from the
  clone pinned at `v0.7.0` (`cargo build --release` in
  `crates/amparo-cli` produces the `amparo` binary).
- Caddy v2.11.4 on the box, config in `/etc/caddy/` with per-product
  import files (see `/etc/caddy/engram.Caddyfile` as the pattern).

## Deployment (deployable and ready)

1. App under `/srv/amparo/`; bind `127.0.0.1` only; non-root user.
2. systemd unit modeled on the sync relay's sandboxed unit pattern
   (NoNewPrivileges, ProtectSystem=strict); enabled and running.
3. `/etc/caddy/amparo.Caddyfile` imported from the main Caddyfile; site
   block copies the console/guardrail header block (X-Frame-Options,
   nosniff, HSTS, per-vhost CSP); `reverse_proxy
   localhost:<app_port>`; `systemctl reload caddy`.
4. DNS: the operator creates the grey-cloud A record for
   `amparo.ellmstack.dev` → 204.168.163.161 (Caddy then issues the
   Let's Encrypt cert automatically on first request). Do not create
   DNS records yourself.
5. Write `README-deploy.md` in the app directory: what runs, where the
   config lives, how to restart, how to roll back.

## Do not

- Do not modify anything under `crates/` in the Amparo repo, or any
  gate/policy semantics. The web app is a consumer of the shipped
  binary.
- Do not push to the Amparo repository.
- Do not add auto-approve defaults or any path that skips the gate
  chain.
- Do not store policy/LLM keys or unstripped task text in the app.
- Do not touch `/srv/engram/`, `/srv/guardrail/`, or the cold archive.

## Acceptance checklist (verify on the box)

1. `https://amparo.ellmstack.dev` serves the app through Caddy.
2. Smoke e2e against a mock LLM: start a scripted task, the `[tag]`
   stream renders live, the final answer lands.
3. Approval round-trip against the mock endpoint: request renders with
   arguments/reasons/blast-radius/session label; approve → the spawned
   process proceeds; deny → it records the denial; no decision within
   60s → auto-deny, shown as such.
4. Ledger viewer renders a real `ledger.jsonl` produced by a smoke run.
5. Sessions and schedule views list real files; resume works once.
6. systemd unit survives a restart of the box service; Caddy config
   validates (`caddy validate`).
7. `README-deploy.md` present and accurate.

Report back with: what you built, the stack, the port, the systemd unit
name, the Caddyfile path, and every item of the checklist with its
result.
