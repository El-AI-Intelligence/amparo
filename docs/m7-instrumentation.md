# M7 — Instrumentation & hardening: the privacy ledger, session persistence, and preflight blast radius

Status: **landed** (2026-08-29, W1–W9; docs + release in W10). Everything
described here is built and gated. Amparo is v0.5.0.

---

## 1. Objective

M6 made growth *safe* — every learned artifact is a hypothesis, gated and
falsifiable. M7 makes the agent *legible* while it works: three instruments
that answer the audit-posture acceptance test from the master design —

> An external reviewer must be able to see, for every executed action,
> *who allowed it and under what policy*.

The three instruments split that question three ways:

- **The privacy ledger** records *what left the machine* and *what was
  stripped before it left* — for every network-touching execution attempt,
  a row carrying the human gate's answer, and for every PII strip, a row
  of per-category counts.
- **Session persistence** records *what the task was* — a checkpoint per
  task, PII-stripped and system-prompt-free, so a crashed run resumes with
  the gate chain re-judging every call, and a chat restart hands its next
  task the prior task's tail without leaking it across tenants.
- **Preflight blast radius** tells the human *what they are approving* —
  a severity label rendered in the approval copy, display-only, so the
  approval question names a concrete consequence instead of an
  abstraction.

None of the three changes what the gate allows. The ledger is
observational; the checkpoint is a snapshot; the classification is
display-only. That is deliberate: instrumentation must not become policy.

---

## 2. First principles

M7 inherits the M6 invariants unchanged. The relevant ones, restated for
this milestone:

- **I1 — nothing auto-tunes policy, gate, or prompts.** The preflight
  classification is computed *after* the gate has decided and feeds
  nothing back into it; a wrong label can only misdescribe the approval
  text, never allow or block a call.
- **I5 — the action-loop prompt is immutable.** Checkpoints store no
  system message; a resumed task re-prepends the *current* prompt, so a
  prompt change in a new build reaches a resumed task exactly as it
  reaches a fresh one.
- **I6 — privacy at capture time.** The ledger never stores values (host
  at most, counts only), and checkpoints are PII-stripped at write with
  the placeholder map discarded. The strips themselves are audited: every
  strip is a ledger row.

---

## 3. The three instruments

### 3.1 The privacy ledger

**Location**: `<workspace>/.amparo/privacy/ledger.jsonl` — one JSON line
per row, append-only (open with `append`, flush per row), so a crash
loses at most the in-flight row's final flush. `read_all` skips a
partial final line; the audit trail stays readable.

**Row kinds and fields** (`LedgerKind`, `LedgerRow` in `amparo-privacy`):

| Field | `network_call` | `pii_strip` |
|---|---|---|
| `ts` | RFC 3339 timestamp | same |
| `tenant` | `cli` for runs, `platform:user_id` for chat tasks | same |
| `tool` | the tool name | absent |
| `site` | `fetch_url` only: scheme + host — path and query dropped | absent |
| `outcome` | `ok` / `error` / `denied` | absent |
| `gate` | `human_approved` / `human_denied` when the human gate ran; absent when no human was asked | absent |
| `pii_counts` | absent | per-category counts (`email`, `phone`, `ssn`, …) — counts, never values |

**What is recorded.** Three tools reach beyond the machine and belong in
the ledger: `web_search`, `fetch_url`, `run_command` (`run_command` is
included even for purely local commands — the operator cannot know
without looking, and the row never carries the command text). The
`LedgerSink` in `amparo-agent` (an `EventSink` consumer) tracks each
call from `ToolCallRequested` to its end:

- an execution writes a row at `ToolExecuted` with `outcome`
  `ok`/`error` and — when the human approval gate ran — `gate`
  `human_approved`;
- a human denial writes the row *immediately*, with `outcome: "denied"`
  and `gate: "human_denied"` — the execution never happens, but the
  denial is itself the answer the ledger exists to record;
- a call the policy engine blocked before approval reaches the gate
  writes no row — there was no execution attempt, and the denial lives
  in the notebook's gate log;
- observational and local-mutating tools (`read_file`, `write_file`, …)
  write no rows.

Policy-engine traffic is excluded: it is operator infrastructure, and
the engine keeps its own logs.

**The PII rule.** A row never contains the *value* of anything
sensitive. `site` holds a scheme + host at most (`site_host_only` drops
path, query, fragment); `run_command` rows carry no command text at all
(the row shape has no field for one); `pii_counts` holds counts only.
Unit tests assert the serialized line contains none of the values the
strip replaced and no command payload.

**Always-on — not growth-gated.** The ledger is an I6 instrument, not a
growth feature: both hosts (`amparo run` and `amparo chat`) attach the
sink on every task, `--growth` or not. Failures never fail the task: an
open failure warns and the task runs without a ledger; a write failure
warns once per task (`[ledger] row failed to persist: …`) and continues.

**Surface**: `amparo privacy [--workspace DIR] [--tenant T] [--last N]`
— the reviewer's front door. Summary first (total network calls, PII
strips, human-approved, human-denied, per-tool and per-category
breakdowns), then the newest rows (`--last`, default 10; `--tenant`,
default `cli`). Exit codes follow the `amparo run` contract: 2 usage,
1 runtime (an unreadable ledger), 0 ok.

**Landed**: W1 core store + sink, W2 CLI + `amparo run` wiring, W3
`amparo chat` per-tenant wiring, W9 denial-row hardening.

### 3.2 Session persistence

**Location**: `<workspace>/.amparo/sessions/<tenant with ':' → '-'>/
<task_id>.json`, one file per task (`task_id` = `sess-<nanos>-<pid>`),
written atomically (tmp file + rename) so a reader never sees a
half-written file; the same task's file is replaced in place. No index
file — "latest" is a directory scan ordered by `started_at`. Corrupt or
unreadable files are skipped with a warn, never fatal.

**Checkpoint shape** (version 1; readers skip unknown versions):

| Field | Content |
|---|---|
| `tenant` | `cli`, or `platform:user_id` |
| `task_id` | `sess-<nanos>-<pid>`, stable across saves |
| `started_at` | Unix seconds — the newest-Running scan orders by this |
| `prompt` | the task prompt, PII-stripped |
| `status` | `running` / `complete` / `failed` |
| `conversation` | the task's messages — **no system-role messages** (I5), PII-stripped including `tool_calls` arguments (I6) |
| `loop_state` | the loop locals: `last_tool_name` + `same_tool_count` (the same-tool guard keeps counting across the restart), `empty_turn_retried`, `last_good_summary`, `used_tool_names` (the case library's retrieval signal), `steps_used` |
| `final_answer` | present once the task reached one, stripped |

**Cadence and role.** A snapshot is saved once per loop iteration and
at every terminal return, through the `CheckpointStore` trait (a seam —
hosts may substitute; every failure warns and continues). A crash loses
at most one turn. The checkpoint is a *resume UX*: the event stream and
the notebook remain the audit trail.

**CLI resume**: `amparo run --resume` — no task argument (with one,
usage error, exit 2; the prompt comes from the checkpoint). It resumes
the newest `Running` checkpoint for tenant `cli`; none → exit 1; a
stale `Running` (> 7 days) is skipped with a warn line, never resumed —
its gate decisions are too old to re-litigate. Resume is
resume-in-progress only: a completed task is re-run fresh, because that
is a new policy decision.

On resume: the current `SYSTEM_PROMPT` is re-prepended (I5 — the
checkpoint stores none), `loop_state` is restored, the conversation is
trimmed to the loop's window, and `TaskResumed` is emitted — **never
`TaskStarted`**, and a resume opens no new notebook record: the
checkpoint is the session trail. Configuration comes from the current
flags, and the per-call gate chain is stateless, so resumed calls
re-judge normally — nothing carries a pre-approved verdict across the
restart.

**Chat continuity**: one task per chat, so continuity is not a
mid-loop resume. When a new task starts, the driver loads the tenant's
`latest_complete` checkpoint and builds `continuity_context`: the last
6 user/assistant messages (tool messages skipped — their call ids mean
nothing to a new task), truncated, plus the task's `last_good_summary`,
all as one string. `Agent::with_continuity` injects it as **one
user-role message** between the system prompt and the task prompt,
prefixed with an explicit instruction that it is context only, not
something to act on. Chat **never** reads `Running` files — a killed
task's in-memory gate/claim cannot be continued, so a crash in chat is
a lost task, not a resumed one. Everything read comes from a
PII-stripped checkpoint, so the context string is safe to send on.

**Landed**: W6 core (store, snapshot cadence, `Agent::resume`), W7 CLI
resume + the fresh-run checkpoint fix (every run — fresh or resumed —
snapshots its loop), W8 chat continuity + I6 gap closures
(`tool_calls` arguments, `last_good_summary`, `final_answer` all
stripped), W9 cross-feature sweeps.

### 3.3 Preflight blast radius

`classify(registry, policy, call) -> BlastRadius` labels a call by what
executing it could touch, worst case:

| Class | Severity | Meaning (rendered note) |
|---|---|---|
| `read_only` | 0 | observes only; nothing is modified |
| `workspace_local` | 1 | changes stay inside the workspace |
| `network` | 2 | reaches the network |
| `system_wide` | 3 | touches files outside the workspace |
| `destructive` | 4 | matches a blocked destructive pattern |

The rules: the tool's registered trust tier seeds the class
(Observational → `read_only`, LocalMutating → `workspace_local`,
ExternalEffector → `network`, SystemControl → `system_wide`; an unknown
tool — which the gate blocks first — is labeled `system_wide`, the
honest worst-case claim). Argument inspection can only *raise* the
seed: a `run_command` whose command matches
`PathPolicy::check_command_blocked` → `destructive` (first raise wins);
a `run_command` naming a network-transfer utility (`curl`, `wget`,
`nc`, `scp`, `rsync`, `ssh`, matched as whole shell words — `sync` does
not trip `nc`) → at least `network`; a file tool (`read_file`,
`write_file`, `edit_file`, `patch_file`, `list_dir`) given an absolute
path outside the workspace and the shared scratch dirs (`/tmp`,
`/dev/shm`) → at least `system_wide`.

**Display-only by design (I1).** Classification runs *after* the gate
chain has decided the call may execute, inside `gate_call`; nothing in
the module feeds back into the gate, the policy engine, or the prompts.
A misclassification can only misdescribe the approval text — never
allow or block anything. The label rides on
`ApprovalRequest::blast_radius` and renders in the approval copy:

- CLI: `[preflight] blast radius: destructive — rm -rf matches a
  blocked destructive pattern` above the `because:` lines;
- Chat: the same line in the approval message text.

The point is the voice spec's own requirement: the user approves the
concrete consequence, not an abstraction. Note the `destructive` class
is not a second gate — the gate blocks those patterns independently;
the label says how bad the ask was.

**Landed**: W4 classification core + `ApprovalRequest` ripple (all
construction sites fixed in one commit; non-agent sites pass `None`),
W5 CLI + chat approval copy and e2e.

---

## 4. The audit posture, per feature

The acceptance test, made concrete for each instrument — and how the
suite proves it:

- **Ledger** — *for every executed network-touching action, the row
  answers who allowed it and under what policy; for every denial, the
  denial is itself a row; for every strip, the strip is audited as
  counts.* Proven by: unit tests (execution rows, human-approved rows,
  denial-only rows, no rows for policy-blocked or local tools, counts
  never values, host-only sites, no command payload) and e2e
  (`resume_lands_ledger_growth_and_checkpoint_together`,
  `destructive_call_denied_shows_radius_and_lands_a_human_denied_row`,
  the `amparo privacy` subcommand over a real ledger, the
  unwritable-ledger warn path).
- **Sessions** — *a killed mid-loop run resumes to completion with the
  gate chain re-judging every call; a chat restart hands its next task
  prior context with no PII and no cross-tenant bleed.* Proven by: unit
  tests (round-trip, no system message stored, atomic tmp, newest-
  Running preference, corrupt skip, in-place replace) and e2e (CLI
  resume from a fixture checkpoint to completion, resume-without-
  session exit 1, chat continuity with per-tenant session directories).
- **Preflight** — *the human approves a concrete consequence.* Proven
  by: unit tests over the classification table (every seed, every
  raise, the word-boundary and workspace-boundary cases) and e2e (the
  CLI approval prompt shows the radius line; the chat approval message
  carries it; the denied destructive call shows both the label and the
  denial's ledger row).

---

## 5. Storage layout

Everything M7 adds lives under the workspace root's `.amparo/`,
alongside the M6 files:

```
<workspace>/.amparo/
  privacy/ledger.jsonl           # M7: the ledger — append-only, always-on
  sessions/<tenant>/<task>.json  # M7: checkpoints — one per task, atomic
  notebook/                      # M6: cold archive, hot layer, rollup
  skills/                        # M6: adopted skills, logs, rechecks
```

The two new trees follow the M6 conventions: JSONL audit-log style for
the ledger (like `records.jsonl`), one-file-per-task JSON with atomic
rename for sessions. Neither is behind the `Memory` trait — the ledger
is a keyword-free evidence log, not a content store.

---

## 6. Deliberately excluded

- **QC council** and **`amparo doctor`** — proposed as M7 items, cut by
  locked decision (2026-08-29); the three landed instruments are the
  core.
- **An audit-mode stderr notice** — cut with the above.
- **WASM sandboxing** — out of this milestone entirely; its own future
  M7b.
- **Ledger quota/rotation** — the ledger grows without bound by
  design (an audit log; disk is cheap, and the reviewer wants
  completeness). A `--max-bytes`/retention lever is noted as M7b
  material.
- **Auto-tightening the gate from classification.** The `destructive`
  label never blocks anything itself. Tightening the gate from code
  would be automatic policy tuning (I1) — refused on those grounds.
- **Cross-task resume in chat.** Chat continuity carries the *tail* of
  a completed task into a fresh one; it never resumes a `Running`
  checkpoint, and chat never reads `Running` files.
- **Policy-engine calls in the ledger.** Operator-infrastructure
  traffic stays out; the engine logs itself.

---

## 7. I1–I6 audit

| Invariant | How M7 honors it |
|---|---|
| I1 — no auto-tuning | Preflight is computed after the gate decides and feeds nothing back; the ledger and checkpoints are write-only instruments. Nothing in M7 changes a verdict. |
| I2 — per-tenant namespacing | Ledger rows carry `tenant`; checkpoint directories are per-tenant (`:` → `-`); continuity loads only the requesting tenant's `latest_complete`. |
| I3 — provenance | A ledger row is the provenance record itself: what ran, when, and who allowed it. Checkpoints carry `started_at` and `task_id`; the event stream and notebook remain the full trail. |
| I4 — revocable | Neither instrument gates anything, so nothing is revoked by them; they do not outlive the operator's deletion and do not resist it. |
| I5 — prompt immutable | Checkpoints store no system message; resume re-prepends the current one. The continuity context arrives as one user-role message, exactly where M6b evidence arrives. |
| I6 — privacy at capture | Strip-at-write for `prompt`, `conversation` (incl. `tool_calls` arguments), `last_good_summary`, `final_answer`; the placeholder map is discarded at the strip site. The ledger stores host at most, counts never values, and each strip is itself a `pii_strip` row — the strip is auditable, and resumed text's placeholders (`[EMAIL_1]`) are the proof the strip happened. |

---

## 8. Test inventory

The M7 surface is covered by these suites, all green at the W9 gate
alongside the untouched M6 regression suites:

- `amparo-privacy/src/ledger.rs` — 7 tests: round-trip, parent-dir
  creation, partial-line skip, summary aggregation, no-PII-shapes,
  `site_host_only`, no-command-payload.
- `amparo-agent/src/ledger_sink.rs` — 7 tests: execution rows,
  host-only sites, human-approved/human-denied rows, no rows for local
  tools or policy-blocked calls, counts-only strips, error outcomes.
- `amparo-agent/src/preflight.rs` — 11 tests: severity ranks, display
  names, notes, tier seeds, unknown tools, destructive raise, network
  raise, destructive-first, word boundaries, workspace boundaries,
  non-file tools.
- `amparo-agent/src/session.rs` — 11 tests: round-trip, newest-Running
  preference, status filters, corrupt skip, tenant path, missing dirs,
  in-place replace, continuity tail/summary/truncation.
- `amparo-agent/src/agent.rs` — resume + continuity behavior on the
  loop itself (resume completes, guards restored, I5 re-prepend,
  store-failure never fatal, continuity as a user message, stripped).
- `amparo-cli/src/privacy.rs` — 4 parse tests (usage errors, `--last`
  validation).
- `amparo-cli/src/approve.rs` — radius rendering in the interactive
  gate.
- `amparo-chat/src/gate.rs` — radius rendering in chat approval.
- `amparo-chat/src/driver.rs` — 22 tests including the W8 continuity
  e2e (context present for the same tenant, absent for another) and the
  W9 two-tenant isolation test.
- `amparo-cli/tests/cli_e2e.rs` — 10 binary-level tests including the
  W9 cross-feature runs: resume + ledger + growth together; destructive
  call → radius + denial → `human_denied` row; unwritable-dir warn
  paths.

---

## 9. Risks and open questions

- **Misclassification is cosmetic by construction.** The one residual
  risk of the display-only rule: a wrong label misdescribes the
  approval text. Accepted — the alternative (classification feeds the
  gate) is an I1 violation.
- **Ledger growth.** Unbounded by design; the M7b lever is a retention
  window, not a cap on completeness.
- **Resumed text carries placeholders.** A resumed task sees
  `[EMAIL_1]` where a value was. Privacy-correct and audited (the
  strip row proves it); if a future milestone wants value-level
  continuity, that is an in-memory restore-map design, not a storage
  change.
- **Stale `Running` checkpoints** accumulate when crashes never resume
  (the 7-day skip applies at resume time). A sweep command is M7b
  material; the files are one per task and cheap.
- **The W9 findings themselves** — denials now write rows, fresh runs
  checkpoint, and agent warnings are visible in the CLI (the minimal
  stderr subscriber) — were caught by the sweep the week exists for;
  the sweep tests pin all three so they cannot regress silently.
