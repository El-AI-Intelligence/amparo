# M8 — Sub-agents & scheduling (swarms behind the gate chain)

Status: **landed** (2026-08-30, W1–W6; docs + release in W7). Everything
described here is built and gated. Amparo ships this milestone as
v0.7.0.

---

## 1. Objective

M7 made the single agent legible. M8 makes the *advanced system*
legible: the two features screened in `docs/swarms-advanced.md` —

- **`spawn_agent`** — the agent can decompose a task into sub-agents,
  where a sub-agent is the *same* loop with the *same* gate chain and
  the *same* audit posture as its parent, bounded by a swarm budget and
  reported through a cost line.
- **`schedule`** — the agent can persist a *promise* that a task will
  re-enter the gate chain at a concrete instant, with its original
  requester as the session; the chat driver's ticker fires due promises
  back through the full chain, and a promise that passes its grace
  window is marked missed — fail-closed, never fired late.

Both ride on the carryover screen's two resource-honesty defaults: a
**swarm budget** (maximum sub-agents per task, fail-closed at the
limit) and a **cost line in every report** with its method attached.

**The one rule** (`swarms-advanced.md` §"The one rule"): **no member
exits the gate chain.** A swarm is N agents; every one of them passes
registry lookup → trust ceiling → policy engine → human approval per
tool call. Two corollaries follow, and they decide everything else:

1. **Spawning an agent is a tool call.** A sub-agent begins as
   `spawn_agent` through the registry — policy-checked, approval-gated,
   logged. There is no path from "the parent decided it was useful" to
   "a new agent exists" that does not pass the gate.
2. **Delegation is not exemption.** The child runs the same loop with
   the same registry, the same policy engine, and the parent's trust
   ceiling; its calls are tagged with a chain that names the
   delegation. Nothing a swarm does is *less* auditable than what a
   single agent does — it is *more* auditable, because the delegation
   itself is a logged event.

And one refusal, stated outright: **a model is never the approver.**
Supervisor patterns in which an LLM approves another agent's actions on
the human's behalf move the trust boundary into the least trustworthy
place. In Amparo the operator is the supervisor; automatic "supervisor
agents" are excluded, not as a gap but as a design choice.

---

## 2. First principles

M8 inherits the M6/M7 invariants unchanged. The relevant ones, restated
for this milestone:

- **I1 — nothing auto-tunes policy, gate, or prompts.** The new
  preflight labels (`sub_agent` for a spawn, `system_wide` for a
  schedule promise) are display-only; `spawn_agent` and `schedule` are
  ordinary registry tools at `ExternalEffector`; the budget's
  fail-closed limit is tool behavior, never a gate verdict.
- **I3 — provenance.** The delegation chain is legible in every
  artifact: the child's task id (`{parent}.{n}`), the checkpoint's
  `parent_task_id`, the ledger row's `task_id`/`parent_task_id`, and
  the approval copy's session label. The spawn is itself an event
  (`[spawn] …`).
- **I6 — privacy at capture.** A scheduled promise is PII-stripped at
  write; child ledger rows obey the M7 rule (host at most, counts never
  values, no command text); child checkpoints pass the same M7
  strip-at-write machinery.

---

## 3. The features

### 3.1 Token accounting + the cost line (`amparo-agent`)

The loop streams inference and no provider token count survives; exact
billing belongs to the provider. The cost line is an **estimate** and
says so:

- `estimate_tokens(text) = chars / 4` — the standard approximation,
  uniform across turns and providers, deterministic.
- Counted at the loop's inference call sites: outgoing request text,
  returned content, and tool-call JSON, accumulated per turn into
  `AgentReport.tokens_estimated`; `tool_calls` counted at dispatch.
- `AgentConfig.cost_per_million_tokens: Option<f64>`, default
  `Some(3.0)` — a stated mid-range model assumption; `None` omits the
  cost line (counts still shown).
- Rendered as `~$0.04 in inference (estimate, chars/4, $3/1M tokens)`
  — the voice's evidence rule applied to resources: the method states
  its assumptions. The CLI prints it as the `[report]` line; the chat
  final answer carries it inside the swarm breakdown.

### 3.2 Delegation identity — an explicit parent link, the chain in the id

- **Host-generated ids.** Hosts already build an agent per task; they
  now name the task (`Agent::with_task_id`) and pass the id to both the
  agent and its `SpawnAgentTool`. A child's id is `{parent}.{n}`
  (`sess-123.1`, `sess-123.1.1`) — the chain is legible in every
  artifact.
- **`Checkpoint.parent_task_id: Option<String>`** (additive, serde
  default) — `sessions/<tenant>/sess-123.1.json` names its parent
  explicitly; M7's resume/continuity machinery is untouched.
- **`LedgerRow.task_id` / `parent_task_id`** (additive — a pre-M8 row
  parses with `None`, pinned by test);
  `LedgerSink::new(store, tenant, task_id, parent_task_id)`. Every
  child's network-call row carries the chain: "nothing a swarm does is
  less auditable" is mechanically true.
- **`ApprovalRequest.session_label: Option<String>`** — `None` for a
  top-level agent, `Some("sub-agent sess-123.1 of task sess-123")` for
  a child. Renders as `[session] sub-agent sess-123.1 of task sess-123
  wants to run:` in CLI approvals and the same label in chat approval
  messages (via `transport::session_line`, shared by all three
  adapters). Display-only (I1): the gate is unchanged.
- Events: `TaskStarted`/`TaskComplete` gain an optional `task_id` —
  a child completes as `[complete sess-123.1] …`.

### 3.3 `spawn_agent` — the tool that builds an agent (`amparo-agent/src/spawn.rs`)

The host constructs the tool per task with what the executor can't
reach (provider, policy engine, approval gate, event sink, path
policy, checkpoint store, tenant, parent id, budget, ceiling, config):

- **Schema**: one param, `task` (string, required — the sub-task
  prompt). Trust tier **`ExternalEffector`** — creating an acting
  entity always asks a human.
- **Execute** (the spawn is itself a gated tool call in the parent's
  loop — the rule's corollary 1): the budget check comes first —
  count == max → `ToolResult` failure `swarm budget exhausted: N
  sub-agents max`, **fail-closed: no agent exists, no budget consumed**.
  Else: child id `{parent_task_id}.{n}` (per-parent ordinal), a fresh
  `Agent` sharing Arc clones of provider/policy/approval/events, the
  same registry clone, the same `PathPolicy`, the same checkpoint
  store and tenant, `with_task_id(child)`, `with_parent_task_id
  (parent)`, the parent's trust ceiling, and the parent's config (the
  rate knob inherits). `child.run(task).await` runs **inside the
  executor** (it is already async — re-entrancy is safe: `run(&self)`
  is `&self`, all fields Arc or Clone). The result is a `ToolResult`
  carrying the child's report, with `display_summary` `sub-agent
  sess-123.1 completed: <final answer head>`; a `SubAgentSummary`
  pushes into the shared reports collector.
- **The budget counter is shared across generations** — the child's own
  spawn tool receives the same `Arc<Mutex<usize>>`, so grandchildren
  count against the one task budget.
- **Events**: `AgentEvent::SubAgentSpawned { task_id, parent_task_id,
  prompt }` renders as `[spawn] sess-123.1 under sess-123: …`; the
  centralized render and the NotebookSink's exhaustive match both have
  their arms (the M7 W1 lesson).
- **Preflight**: `BlastRadius::SubAgent`, severity 2 — the classes
  renumber to 0–5 (`read_only` 0, `workspace_local` 1, `sub_agent` 2,
  `network` 3, `system_wide` 4, `destructive` 5; serde is by name).
  Note: `spawns a sub-agent that acts under the same gate chain` —
  the approval copy says what approving a spawn means. `classify`
  maps `spawn_agent` to it (static override, the M7b `eval_wasm`
  pattern).

### 3.4 Budget + config surfaces

- **CLI**: `amparo run --max-sub-agents N` — default **4**; `0` means
  the tool is not registered at all (swarms off); garbage or negative →
  usage error, exit 2.
- **Chat**: `UserProfile.swarm: Option<SwarmProfile>` —
  `[users."…".swarm]` with `max_sub_agents` (default 4) and `schedule`
  (default false). Per-profile → per-tenant (I2); unknown keys are
  rejected (`deny_unknown_fields` stays strict).
- **MCP and `amparo chat dispatch` do NOT get `spawn_agent`** — those
  paths dispatch tool calls without a session, an event stream, or a
  budget; a spawn there would create agents with no parent, no
  delegation, no audit. Explicit exclusion, documented (§6).

### 3.5 `schedule` — a promise, not an execution (`amparo-chat`)

Scheduling is a persisted promise that a task will *re-enter the gate
chain* when it fires, with its original requester as the session. The
only long-lived host is the chat driver — so the queue lives there; the
CLI gets inspection, not a daemon.

- **Storage**: `<workspace>/.amparo/schedule/<id>.json` —
  `ScheduledTask { id, tenant, requester, task, at: RFC 3339, status:
  pending | fired | missed, result }`, written atomically (tmp +
  rename, the M7 session pattern). **The promise text is PII-stripped
  at write (I6)** — a promise empty after stripping is refused.
  A `ScheduleStore` trait + JSON impl (hand-rolled, no deps).
- **`ScheduleTool`** (registered only in the chat driver, per-profile
  `schedule: true`; tier ExternalEffector; preflight `system_wide` —
  the promise outlives the task): params `at` (RFC 3339 — the model
  must commit to a concrete instant) and `task`. A past instant is a
  failure result. Executing a schedule is never the model's shortcut:
  the firing path is the same gate chain a live task gets.
  Registration ordering matters: the tool is registered *after*
  `with_spawn_agent`, so children never inherit `schedule` — only a
  top-level task may schedule.
- **Ticker**: the driver gains a per-workspace background task —
  Amparo's first background loop (the pattern reference is Axiom's
  epistemic scheduler; the substance is new). `SCHEDULE_TICK` = 30 s
  with `MissedTickBehavior::Delay` (keeps cadence, never bursts); the
  queue is scanned on startup, so a promise due during downtime still
  fires within its grace window.
- **The due-scan is a pure function of `now`** —
  `due_scan(tasks, now, grace)` (clock seam, unit-testable without
  sleeping). `SCHEDULE_GRACE` = 60 s: `at <= now <= at + grace` →
  due; `now > at + grace` → **missed, fail-closed** — late firing
  would be doing work the operator forgot, the proactive-reach-out
  mistake; re-scheduling is the operator's call. A missed promise
  notifies the requester (`[schedule] {id} missed — …`), never fires.
- **The fire path is reduced but honest.** A due promise builds a fresh
  Agent exactly like a chat task — tenant/requester as the session,
  continuity off, ledger sink and checkpoints attached — and runs it
  under `TimeoutApprovalGate` (`SCHEDULE_APPROVAL_TIMEOUT` = 90 s)
  wrapping the chat gate (60 s auto-deny): firing while nobody is
  present runs to the approval gate and **auto-denies on timeout,
  never silently ahead of it**. The fire re-checks the allowlist and
  the task text at fire time (either fails → missed). No
  skills/case-library/`spawn_agent`/`schedule` on the fire path — a
  promise fires one task. The result lands in the promise
  (`fired` + report head) and the requester is notified
  (`[schedule] {id} fired — {answer}`).
- **CLI inspection**: `amparo schedule list | cancel [--workspace
  DIR]` — `list` prints every promise; `cancel` moves a *pending*
  promise to `cancelled` (`cancelled by the operator`) — a status
  change, never a deletion; the record of what was promised survives.
  Exit codes follow the `amparo run` contract (2 usage / 1 runtime /
  0 ok). No CLI scheduler — a `--scheduler` loop for `amparo run` is
  an explicit exclusion; the chat driver is the daemon.

### 3.6 The swarm report

The final report reads like every Amparo report — method,
observations, conclusion, open items — with the swarm breakdown as a
section of the method:

`swarm: 3 sub-agents (sess-123.1, sess-123.2, sess-123.3), 41 tool
calls, ~$0.04 in inference (estimate, chars/4, $3/1M tokens)`

Tool calls and tokens are the parent's *and* every child's. The CLI
prints the line at finish (`[swarm] …`); chat appends it to the final
answer. The cost is a claim with its method attached — the
observatory, not the black box.

**Landed**: W1 token accounting + cost line, W2 delegation identity,
W3 spawn core, W4 wiring + preflight, W5 schedule queue, W6 test sweep
+ edge hardening.

---

## 4. The audit posture, per feature

The acceptance test, made concrete for each feature — and how the
suite proves it:

- **Swarm** — *an operator can ask the agent to decompose a task and
  see, for every executed action — the parent's and every child's —
  who allowed it and under what policy, with the delegation chain in
  the approval copy, the ledger rows, and the checkpoint files.*
  Proven by e2e (`run_spawns_a_child_under_the_shared_gate_and_
  stamps_both_chains` — the child's gated call shows the sub-agent
  label and stamps parent + child chains in the ledger;
  `resume_lands_a_swarm_with_the_chain_stamped` — a resumed parent
  spawns a child whose ledger row, checkpoint and `[swarm]` line all
  chain off the resumed id; telegram `swarm_delegation_names_the_sub_
  agent_chain_in_the_gate_and_answer` — the button names the chain).
- **Budget** — *the swarm stays inside its budget, fail-closed at the
  limit.* Proven by units (`budget_exhaustion_fails_closed_without_
  spawning` — no agent exists past the limit; `grandchildren_share_the_
  one_budget` — grandchildren count against the one task budget;
  `missing_task_refuses_without_touching_the_budget`).
- **Cost honesty** — *the report states what the swarm burned, with
  the method attached.* Proven by units (`estimate_tokens_counts_
  chars_not_bytes`, `cost_line_formats_with_method_stated`,
  `cost_line_honors_none_rate`, `report_accumulates_token_estimate_
  and_tool_calls`).
- **Schedule** — *a promise fires through the gate chain as its
  requester, or is marked missed — never silently executed, never
  fired late.* Proven by units (`store_round_trips_and_leaves_no_tmp`,
  `due_scan_partitions_due_missed_and_waiting`,
  `schedule_writes_a_stripped_pending_promise`) and driver e2e
  (`stale_promise_is_marked_missed_and_never_fired`,
  `scheduled_fire_reruns_the_chain_and_reports`,
  `denied_fire_never_executes_and_tells_the_requester` — a denied
  fire executes nothing, lands a `human_denied` ledger row, and tells
  the requester anyway; `swarm_budget_and_schedule_coexist_in_one_
  task_and_one_ticker`).
- **Failure modes** (W6): a provider failure mid-child fails the child
  loudly with the parent receiving the failure result
  (`a_failed_child_is_not_rerun_by_the_retry_policy`); an approval
  timeout at fire time denies — never ahead of the gate
  (`TimeoutApprovalGate` units). A denied child call lands the denial
  row with the chain (`denied_child_call_lands_a_human_denied_row_
  with_the_chain`).

---

## 5. Storage layout

M8 adds one directory (chat host only); the two existing artifacts
gain columns:

```
<workspace>/.amparo/
  privacy/ledger.jsonl            # M7 + M8: task_id/parent_task_id columns
  privacy/quota                   # M7b: the enforced ledger bound
  sessions/<tenant>/<task>.json   # M7 + M8: parent_task_id field
  schedule/<id>.json              # M8: the promise queue (chat host only)
  notebook/                       # M6: cold archive, hot layer, rollup
  skills/                        # M6: adopted skills, logs, rechecks
```

---

## 6. Deliberately excluded

- **LLM-as-approver / supervisor agents** — a model is never the
  approver. Excluded, not deferred (`swarms-advanced.md` §"The one
  rule").
- **A coordination event bus / blackboard** — deferred: `EventSink`
  suffices until sub-agents exist and a second real consumer appears.
  Swarm coordination is via tool results, not messages.
- **`spawn_agent` on MCP and `amparo chat dispatch`** — those paths
  dispatch calls without a session, an event stream, or a budget; a
  spawn there would create agents with no parent, no delegation, no
  audit. The tool is simply not registered on those surfaces.
- **A CLI scheduler** — no `--scheduler` loop for `amparo run`; the
  chat driver is the daemon, and the CLI only inspects the queue.
- **Firing a schedule on the fire path** — a promise fires *one* task:
  no skills, case library, `spawn_agent`, or `schedule` on the fire
  path, so a promise cannot promise.

---

## 7. I1–I6 audit

| Invariant | How M8 honors it |
|---|---|
| I1 — no auto-tuning | `sub_agent` and `system_wide` preflight labels are display-only; `spawn_agent` and `schedule` are ordinary registry tools at ExternalEffector; the budget limit fails a tool call, it never changes a verdict; the session label feeds approval copy only. |
| I2 — per-tenant namespacing | `SwarmProfile` is per-profile → per-tenant budget and schedule; the schedule queue lives in the tenant's workspace and carries the requester; child checkpoints and ledger rows carry the task chain under the same tenant. |
| I3 — provenance | The chain is in the id (`{parent}.{n}`), the checkpoint (`parent_task_id`), the ledger row (`task_id`/`parent_task_id`), the approval copy (`session_label`), and the event stream (`[spawn] {child} under {parent}`). A denial row carries the chain. |
| I4 — revocable | A child is a task like any other — every call gated, nothing pre-approved; the budget is a bound, and `amparo schedule cancel` moves a pending promise to cancelled (status change, never deletion). |
| I5 — prompt immutable | No M8 change touches prompts: the child runs the same loop with the same `SYSTEM_PROMPT`; a scheduled fire is a fresh task whose prompt is the promise text. |
| I6 — privacy at capture | Schedule promises are PII-stripped at write (a promise empty after stripping is refused); child ledger rows obey the M7 rule (host at most, no command text); child checkpoints pass the M7 strip-at-write machinery. |

---

## 8. Test inventory

The M8 surface is covered by these suites, all green at the W6 gate
(365 tests across the workspace) alongside the untouched M6/M7/M7b
regression suites:

- `amparo-agent/src/tokens.rs` — 4 tests: chars-not-bytes and empty
  estimators, cost-line formatting with the method stated, `None`
  rate honoring.
- `amparo-agent/src/agent.rs` — `report_accumulates_token_estimate_
  and_tool_calls`, `sub_agent_approval_copy_names_the_delegation_
  chain` (+ the M7 suite, green unmodified).
- `amparo-agent/src/spawn.rs` — 8 tests: tool registration on the
  parent, child loop completion + report, budget exhaustion
  fail-closed, grandchildren sharing one budget, ceiling inheritance,
  missing-task refusal, failed-child retry exclusion, denied-child
  ledger row with the chain.
- `amparo-agent/src/preflight.rs` — renumbered severity ranks,
  `sub_agent` display name and note, the `spawn_agent` classify
  override.
- `amparo-privacy/src/ledger.rs` —
  `rows_written_before_task_ids_parse_with_none` (+ the M7/M7b
  suite).
- `amparo-agent/src/session.rs` — checkpoint round-trip with
  `parent_task_id`.
- `amparo-chat/src/schedule.rs` — store round-trip without tmp
  leftovers, corrupt-file skip, due/missed partition (clock seam),
  stripped promise at write, schema, id generation.
- `amparo-chat/src/config.rs` — swarm profile parse with defaults,
  absent-by-default, unknown-key rejection.
- `amparo-chat/src/gate.rs` — `TimeoutApprovalGate` units (answers
  through; auto-denies on timeout).
- `amparo-chat/src/driver.rs` — schedule registration gated on the
  profile, stale promise missed never fired, scheduled fire re-runs
  the chain and reports, budget + schedule coexisting in one task and
  one ticker, denied fire never executes and tells the requester
  (+ the M7/M7b driver suite).
- `amparo-cli/src/run.rs` — `--max-sub-agents` default/zero/parse
  errors.
- `amparo-cli/src/schedule.rs` — `amparo schedule` parse units
  (help, list/cancel with workspace, usage errors).
- `amparo-cli/tests/cli_e2e.rs` — `run_spawns_a_child_under_the_
  shared_gate_and_stamps_both_chains`, `resume_lands_a_swarm_with_
  the_chain_stamped`.
- `amparo-chat/tests/telegram_integration.rs` —
  `swarm_delegation_names_the_sub_agent_chain_in_the_gate_and_answer`.

---

## 9. Risks and open questions

- **Re-entrancy / deadlock.** `run(&self)` is async and shares Arcs, so
  a child running inside a tool executor is safe; the budget counter is
  the only shared mutable state (Mutex, no awaits held). The chat
  driver's per-task `busy` set keeps one task per chat; a child is not
  in the chat map and cannot be re-entered as a chat task.
- **Budget evasions.** Recursion is bounded by the shared counter;
  `max_sub_agents: 0` removes the tool entirely; the MCP/dispatch
  surfaces never carry it.
- **Token estimate drift.** chars/4 under- or over-counts vs provider
  billing; the `~` and the stated method make it a claim, not a
  fiction — and the rate knob is operator-set.
- **Schedule clock skew.** The due-scan uses wall clock; a promise
  scheduled during a host outage beyond the 60 s grace is missed by
  design (fail-closed), never fired late.
- **The fire path is a reduced chain.** A fired promise gets ledger,
  checkpoints, the chat gate wrapped in a timeout — but no skills,
  case library, or swarm tools. That is a deliberate reduction (a
  promise fires one task), stated here and in the tool description,
  not hidden.
- **Preflight misread as gate change.** The `sub_agent` and
  `system_wide` labels are display-only (I1); a test pins that
  `classify` returns `sub_agent` while the tier stays
  ExternalEffector and the approval still asks.
