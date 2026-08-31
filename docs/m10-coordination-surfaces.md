# M10 — Coordination & surfaces (the blackboard, notifications, rollback hints, the web-approval seam, the CLI scheduler)

Status: **landed** (2026-08-31, W1–W6; docs + release in W7).
Everything described here is built and gated. Amparo ships this
milestone as v0.9.0.

---

## 1. Objective

M9 made the agent checkable. M10 makes it **coordinated and
surface-neutral** — the six items M8 deferred, plus the seam the web
surface builds against:

- **The blackboard** — a workspace-scoped coordination board every
  member of the delegation chain reads and writes.
- **`send_notification`** — an outbound notification tool behind a
  transport seam.
- **Rollback groups** — display-only rollback hints for destructive
  calls, with fail-closed file backups.
- **The web-approval seam** — `WebApprovalGate` +
  `--approval-endpoint`, the contract the web surface
  (`docs/web-surface.md`, v3) is built on.
- **MCP spawn + the CLI scheduler** — `spawn_agent` on the MCP
  surface (opt-in, shared budget), and due promises firing from
  `amparo run` at run start.

**The one rule, restated for M10** (`docs/swarms-advanced.md` §"The one
rule"): **no member exits the gate chain.** Every surface M10 adds is
an adapter *around* the chain, never a bypass. Two corollaries decide
everything:

1. **Surfaces are neutral.** The CLI, the chat hosts, the MCP server,
   and the web approval endpoint all ask the same gate the same way —
   a surface renders the question, the chain answers it. The web never
   holds policy keys; the spawned `mcp-serve` process does.
2. **Display is not policy.** The rollback hint, the blast-radius
   label, the session label, and the `[bus]` row describe what a call
   is about to do; none of them can allow or block it (I1).

---

## 2. First principles

M10 inherits the M6–M9 invariants unchanged:

- **I1 — nothing auto-tunes policy, gate, or prompts.** Rollback
  specs are display-only — computed, rendered, never executed. The
  web approval gate transports the human's answer; the verdict logic
  is the same chain. A fired promise re-enters the chain from zero —
  nothing carries a pre-approved verdict.
- **I3 — provenance.** The `[bus]` row carries the trusted writer —
  the loop's task id — because board rows deliberately carry none
  (tool arguments are model-chosen, untrusted). Fired promises keep
  their requester identity; web approval payloads carry the session
  label.
- **I6 — privacy at capture.** Schedule promises stay PII-stripped
  (M8); the blackboard is a workspace file like any other — values
  persist verbatim, and what re-enters a prompt passes the same
  per-turn strip machinery as every tool result.

---

## 3. The features

### 3.1 The blackboard (`amparo-tools/src/blackboard.rs`, W1)

The event bus in its deferred, hand-rolled shape: two tools —
`blackboard_read` (Observational) and `blackboard_write`
(LocalMutating) — over one append-only JSONL file,
`<workspace>/.amparo/blackboard/board.jsonl`.

- **A write appends one row** (`BlackboardEntry { key, value,
  written_at }`) and flushes; a read folds the whole log with the
  **last write per key winning**. The audit trail keeps every row (I4
  precedent — the skills `adopted.jsonl` pattern). No SQLite, no
  daemon: O(file) reads at board scale.
- **Shared across the delegation chain.** The store is one cheap
  clone around a path; the registry clone handed to sub-agents (M8)
  carries the same store, so every member of the chain reads and
  writes the same board — a child's status lands where the parent
  reads.
- **Rows carry no writer identity** — tool arguments are
  model-chosen and cannot be trusted as provenance. The trusted
  writer (the loop's task id) rides in the agent's `[bus]` event
  instead (`[bus] <key> by <task-id>`), which lands in the event
  stream and the notebook/ledger sinks.
- **Tolerant by design**: a missing file is an empty board,
  unparseable lines are skipped, an empty key is rejected at write
  time.

### 3.2 `send_notification` (`amparo-tools/src/notification.rs`, W2)

An outbound notification tool at `ExternalEffector` — the model can
ask a destination to hear something, and a human approves the send
like any external effect.

- **A transport seam** (`NotificationTransport::deliver`), the
  `PathPolicy` precedent: `StderrTransport` (the default — the
  notification prints as `[notification] to <destination>:
  <message>`) and `WebhookTransport` (POSTs
  `{"destination", "message"}` JSON). `amparo run --webhook-url URL`
  wires the webhook; the chat driver wires its platform transports.
  A transport failure surfaces as a failed tool result, never a task
  crash.
- **The approval copy names the destination** — the human approves a
  concrete recipient, not an abstraction (the preflight precedent).

### 3.3 Rollback groups (`amparo-tools`, W3)

Design-first and display-only: tools declare how the call could be
undone, and the approval copy shows it. Nothing executes the undo —
automatic rollback would be auto-policy (I1).

- **`RollbackSpec { undo, markers }`** via a new
  `ToolExecutor::rollback(&call)` method, defaulting to `None`.
  Destructive-class tools (the file tools' writes and deletes)
  override it. The spec is computed **pre-execution** — the undo
  describes exactly what the call is about to change.
- **Fail-closed backups**: before a destructive write, the file tool
  copies the existing contents to `<path>.amparo-bak`. If the backup
  cannot be made, the write refuses — the rollback hint must never
  promise a backup that does not exist.
- **Rendering**: a `[rollback] restore the previous contents …
  (backup: …)` line in the event stream, and the rollback hint joins
  the preflight line in the approval copy for destructive-class
  calls.

### 3.4 WebApprovalGate + `--approval-endpoint` (`amparo-agent/src/web_approval.rs`, W4)

The approval seam the web surface is built on (`docs/web-surface.md`
contract v3): a surface-neutral adapter that asks a web UI for the
human's decision.

- **The contract**: the gate POSTs the full `ApprovalRequest` JSON
  (`call_id`, `tool_name`, `arguments`, `reasons`, `blast_radius`,
  `session_label` — plus the rollback hint when the tool declares
  one) to the endpoint, then polls `{endpoint}/{call_id}` for the
  decision. The backend renders the request; the decision comes back
  over the poll.
- **Fail-closed everywhere**: 60 s timeout, 1 s poll interval (both
  tunable) — a post failure, a poll failure, a transport error, or a
  timeout all deny. The web UI is an operator convenience, never a
  new trust boundary, and never holds policy keys (the spawned
  `mcp-serve` process does).
- **Flags**: `--approval-endpoint URL` on `amparo run` and
  `amparo mcp-serve` (http(s) validated; mutually exclusive with
  `--auto-approve`). The MCP server's `gate_and_dispatch` gained the
  preflight classification and session label, so web approval
  payloads over MCP carry them too.

### 3.5 MCP spawn + the CLI scheduler (W5)

Two surfaces the web surface and the operator need:

- **`spawn_agent` on MCP** — `amparo mcp-serve --max-sub-agents N`
  registers the spawn tool (opt-in; absent or `0` = no spawn tool —
  the M8 exclusion held until the operator opts in). The budget is
  one shared counter across the spawn chain: children start
  spawn-free and re-register a child-flavored tool under the same
  budget, so the shared cap fails closed exactly as in `amparo run`.
- **The CLI scheduler** — `amparo run` always registers `schedule`
  (top-level tasks only — a fire never inherits it). The CLI is
  process-scoped: no daemon, no ticker, so **due promises fire at
  run start only** — `due_scan` runs the due `cli` promises (I2 —
  chat promises stay untouched) concurrently with the main task,
  sharing the provider, policy, approval gate, and flags. A promise
  past its grace window is marked **missed** — fail-closed, never
  fired late. `--resume` is a run start too, so the scan fires
  there as well. This is documented honestly as best-effort: the
  chat host keeps the 30 s ticker; a CLI promise due at a moment no
  run starts fires at the next one.
- **A fire is a fresh top-level task** with a deliberately reduced
  tool set — no `spawn_agent`, no `schedule`. A denial doesn't
  crash the fire: the task completes with its answer (or the
  no-final-answer fallback) and the promise records `Fired`.

### 3.6 Test sweep + edge hardening (W6)

The W4 append one-off (marker correct, target missing the append) was
hunted across 20 parallel reproduction runs — 24 clean runs total, no
in-code mechanism exists, and it stays classified as an environmental
one-off. The theoretical collision class it exposed is closed anyway:
all four nanos-based test-root helpers
(`filesystem::test_root`, `blackboard::temp_root`,
`agent::w3_root`, `notebook::temp_dir`) now include a per-process
atomic counter, so concurrent tests drawing the same clock reading
can never share a directory. Four new e2e tests close the
surface-gap audit (see §4).

---

## 4. The audit posture, per feature

The acceptance test, made concrete — and how the suite proves it:

- **Blackboard** — *a write emits a `[bus]` row and a read sees the
  value, across the delegation chain.* Proven by units (round-trip,
  last-write-wins with every row surviving, missing-board-empty,
  garbage skipped, empty key rejected, trust tiers, schema
  round-trip), the agent-layer spawn-boundary test
  (`blackboard_is_shared_across_the_spawn_boundary`), and e2e
  (`blackboard_write_emits_a_bus_row_and_the_read_sees_the_value`).
- **Notification** — *a send is approved as an external effect, and
  a transport failure is a failed result, never a crash.* Proven by
  units (stderr default dispatch, webhook POSTs the exact JSON,
  non-success status reported, failure surfaced) and e2e (stderr
  delivery without `--webhook-url`; the configured webhook receives
  the POST).
- **Rollback** — *the hint renders, the backup exists, and nothing
  executes an undo.* Proven by units (write/delete undo copy,
  `.amparo-bak` created and content-verified, no backup on fresh
  writes, no silent overwrite when a backup exists) and e2e
  (`write_over_an_existing_file_emits_the_rollback_row_and_backs_up`,
  `escalated_write_approval_copy_carries_the_rollback_hint`).
- **Web approval** — *approve, deny, and every failure mode fail
  closed.* Proven by units (`post_then_poll_approve`,
  `post_then_poll_deny`, `post_failure_fails_closed`,
  `poll_failure_fails_closed`, `timeout_fails_closed`,
  `transport_error_fails_closed`,
  `post_body_is_the_full_approval_request`) and e2e (approve carries
  the rollback copy and executes; deny blocks the call; a pending
  endpoint is polled until decided).
- **MCP spawn** — *absent by default, budgeted and gated when opted
  in, denied without an approver.* Proven by e2e
  (`mcp_serve_registers_no_spawn_agent_by_default`,
  `mcp_serve_spawns_agents_within_the_shared_budget`,
  `mcp_serve_denies_a_spawn_without_an_approver` — the server
  survives the denial).
- **CLI scheduler** — *a due promise fires through the full gate
  chain, a fire needing approval executes nothing when denied, the
  resume path fires too, and chat promises are never touched.*
  Proven by e2e (`run_schedules_a_stripped_cli_promise`,
  `a_due_cli_promise_fires_at_run_start`,
  `a_due_cli_promise_fires_through_the_gate_chain` — exactly two
  gated and two executed calls, one per task, in any interleaving —
  `a_due_fire_that_needs_approval_is_denied_and_executes_nothing`,
  `a_due_cli_promise_fires_on_resume_too`,
  `an_overdue_cli_promise_is_marked_missed_and_chat_promises_are_
  untouched`).

---

## 5. Storage layout

- **`.amparo/blackboard/board.jsonl`** (W1) — the append-only board.
- **`.amparo-bak` marker files** (W3) — the pre-call copy beside
  every destructive write, named in the rollback spec.
- **`.amparo/schedule/`** (M8, now written by `amparo run` too) —
  `cli`-tenant promises from the CLI scheduler.
- The web approval gate and the webhook transport hold **nothing on
  disk** — their state is in-process for the life of the call.

---

## 6. Deliberately excluded

- **Automatic rollback execution.** The spec is computed and
  rendered; an agent that undoes its own calls without asking would
  be acting outside the gate. The human reads the hint at approval
  time and can say no; the `.amparo-bak` file makes the undo trivial
  for the operator.
- **Fire-path recursion.** A fired promise gets the reduced tool set
  — no `spawn_agent`, no `schedule`. Unattended spawn chains would
  break the attribution chain, and a fire scheduling a fire would
  defeat the queue's purpose. This is a deliberate exclusion, not
  deferred work.
- **Writer identity in board rows.** Arguments are model-chosen;
  provenance lives in the `[bus]` event's task id (I3).
- **A daemon for the CLI scheduler.** The CLI stays process-scoped:
  due promises fire at run start only, honestly documented as
  best-effort. The chat host keeps its 30 s ticker.
- **Voice in/out** — deferred by user decision (the STT dependency
  is too heavy for this milestone).
- **The web surface itself.** Amparo ships the seam; the web app is
  a separate build against `docs/web-surface.md` (v3) by a second
  engineer. The seam's JSON is pinned by test — contract changes
  after W0 bump the doc version and are flagged to the user.

---

## 7. I1–I6 audit

| Invariant | How M10 honors it |
|---|---|
| I1 — no auto-tuning | Rollback specs render and never execute; the web gate transports the human's answer through the unchanged chain (fail-closed on every transport failure); a fire re-enters the chain from zero with no carried verdicts; the blackboard write goes through the gate like every `LocalMutating` call. |
| I2 — per-tenant namespacing | The board is workspace-scoped; the CLI scheduler scans only `cli` promises (chat promises untouched, pinned by test); chat scheduling stays per-tenant (M8). |
| I3 — provenance | The `[bus]` row carries the trusted writer (the loop's task id); fired promises keep their requester identity and re-enter the chain as their requester; web approval payloads carry the session label. |
| I4 — revocable | The board is a workspace file the operator can delete; the webhook transport is a value the host holds (drop the URL, the default stderr transport stands); nothing caches a web decision — every call polls its own. |
| I5 — prompt immutable | The action-loop `SYSTEM_PROMPT` is untouched by all five features; notification copy and rollback hints render in approval text and events, never in the loop's instructions. |
| I6 — privacy at capture | Schedule promises stay PII-stripped (M8); board values persist verbatim like any workspace file, and what re-enters a prompt passes the same per-turn strip machinery as every tool result. |

---

## 8. Test inventory

The M10 surface is covered by these suites, all green at the W7 gate
(739 tests across the workspace) alongside the untouched M6–M9
regression suites:

- `amparo-tools/src/blackboard.rs` — 6 tests: round-trip, last-write
  wins with both rows surviving, missing board empty, garbage
  skipped, empty key rejected, schema/trust tiers round-trip.
- `amparo-tools/src/notification.rs` — 4 tests: stderr default
  dispatch, webhook POSTs the exact JSON, non-success status
  reported, transport failure as a failed result.
- `amparo-tools/src/filesystem.rs` — the W3 section: backup created
  before writes (content-verified), none on fresh writes, delete
  undo copy, no silent overwrite when a backup exists.
- `amparo-tools/src/registry.rs` + `amparo-agent/src/events.rs` —
  `rollback()` default `None`, the `[rollback]` render with and
  without markers, the `[bus]` render with and without the writer.
- `amparo-agent/src/web_approval.rs` — 7 tests: approve, deny,
  post-fail, poll-fail, timeout, transport-error, full request body.
- `amparo-agent/src/agent.rs` +
  `amparo-agent/src/spawn.rs` — the spawn-boundary blackboard test
  (+ the M8 suites).
- `amparo-mcp/src/serve.rs` — `--approval-endpoint` and
  `--max-sub-agents` parse arms, URL validation, `--auto-approve`
  mutual exclusion, help lines.
- `amparo-cli/tests/cli_e2e.rs` — the M10 e2e set: bus-row
  round-trip, both notification deliveries, rollback rows + approval
  copy, the three web-approval end-to-ends, the MCP spawn trio
  (absent by default, shared budget, denied without an approver),
  and the scheduler six (stripped promise, run-start fire,
  gate-chain fire, denied fire executes nothing, resume fire,
  missed-vs-chat-promises).

---

## 9. Risks and open questions

- **The board is only as fresh as its readers.** O(file) folds at
  board scale are deliberate; a board that outgrows the workspace
  pattern is the M6e-style index layer's problem, and nothing
  consumes that yet.
- **The web seam is a poll protocol.** 60 s fail-closed is the
  contract; a slow backend costs the human their decision window.
  The double-press safety follows the `ApprovalRouter` precedent —
  one decision per call id.
- **Best-effort CLI scheduling.** A promise due at a moment no run
  starts waits for the next one — documented, not hidden. The chat
  ticker remains the always-on scheduler.
- **`.amparo-bak` litter.** Every destructive write leaves one
  marker beside the file. It is the price of a rollback hint that
  never lies; cleanup is the operator's, and nothing deletes backups
  automatically.
