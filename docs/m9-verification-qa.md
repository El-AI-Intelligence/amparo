# M9 — Verification & QA (auditors beside policy, the operator's sweep, the audit notice)

Status: **landed** (2026-08-30, W1–W4; docs + release in W5). Everything
described here is built and gated. Amparo ships this milestone as
v0.8.0.

---

## 1. Objective

M8 made the advanced system legible. M9 makes it **checkable**: the
three features cut from M7 by locked decision
(`docs/m7-instrumentation.md:290` names them as the remainder) —

- **The QC council** — deterministic rule auditors that run *beside*
  policy, after the loop produces a candidate final answer and before
  the self-verification prompt is built. Findings are appended to the
  verification prompt as candidate issues for the model to check;
  verification stays the model's call.
- **`amparo doctor`** — the operator's QA pass: one read-only sweep
  over everything Amparo writes into a workspace plus the configured
  surfaces, exit-coded and cron-able.
- **The audit-mode stderr notice** — the one-time line promised at
  `docs/trial-bundle.md` §"graceful degradation" when a wire engine
  first answers `enforced: false`, with session tagging so engine-side
  audit rows correlate with the caller.

**The one rule, restated for M9** (`docs/swarms-advanced.md` §"The one
rule"): **no member exits the gate chain.** The council is not a
member's gate; it is the audit layer beside the chain. Two corollaries
decide everything else:

1. **The council is advisory, never an approver.** Findings feed the
   verification prompt as issues to check — the verification turn (the
   model) decides, exactly as it did before M9. Nothing the council
   produces auto-tunes policy, gate, or prompts (I1).
2. **A model is never the approver.** axiom-qc's Tier2 LLM panel — a
   model auditing a model's answer as a second judge — is excluded by
   design. Amparo's council is deterministic code over the run's own
   records; the operator is the only human in the loop, and the loop's
   verification is the only model judge.

---

## 2. First principles

M9 inherits the M6–M8 invariants unchanged:

- **I1 — nothing auto-tunes policy, gate, or prompts.** Council
  findings change only the *verification* prompt (never the action
  loop's `SYSTEM_PROMPT`); `QcVerdict` is advisory; the audit notice
  prints to stderr and passes verdicts through untouched; `doctor`
  reports and never repairs.
- **I3 — provenance.** Session ids tag every wire check (`--session-id`,
  defaulting to the task id — the checkpoint's original id on
  `--resume`); the chat driver tags `platform:user_id`; the `[qc]` log
  line carries the audit ordinal and cumulative findings.
- **I6 — privacy at capture.** The PII rule reports category counts —
  the values themselves never enter a finding; the doctor sweep reads
  ledger rows as counts (bytes and rows, never contents).

---

## 3. The features

### 3.1 The QC council (`amparo-agent/src/qc.rs`, W1)

Deterministic rule auditors beside policy. The pipeline shape (one
pass, named rules, a verdict plus per-rule counters in memory) follows
the axiom-qc pattern; the rule set is Amparo's own.

- **Runs after the candidate final answer exists, before the
  verification prompt is built** (`agent.rs`, the M2 self-verification
  stage). Input is `QcInput` — the run's own records: requested and
  executed tool sets, executed calls, tool results still in context,
  the token accounting, the cost rate, the registry vocabulary, and
  whether a sub-agent was spawned. The council adds no new
  observability surface.
- **Four rules**:
  - `unexecuted_tool_claim` — the answer cites a registry tool name
    (word-boundary match) that never executed this run.
  - `evidence` — executed calls exceed tool results still in context;
    a claim citing a result that left the context is ungrounded.
  - `cost_honesty` — a `$` inference figure in the answer diverges
    from the run's `chars/4` accounting estimate. Leaf runs only (a
    parent's context carries child figures legitimately); the rule is
    silent without a configured rate.
  - `pii_shape` — residual PII-shaped values in the answer, reported
    as category counts (`email x1, phone x1`) — I6, values never enter
    the finding.
- **Advisory verdicts**: `Approved` / `WithFindings`. Findings render
  through `qc_prompt_section` into the verification prompt as numbered
  issues with the rule id — "Check each one against the conversation
  and correct your answer where they are right; ignore any that are
  wrong." `VerificationDecision` and `interpret_verification` are
  untouched; the verification turn remains the only judge.
- **Events + stats**: `AgentEvent::QcAudit { verdict, findings }` with
  render and sink arms; a `[qc] audit #N: <verdict>, cumulative
  findings N` tracing line; in-memory `QcStats` counters per process
  (the axiom-qc pattern — logged beside each audit, never persisted).

### 3.2 `amparo doctor` (`amparo-cli/src/doctor.rs`, W2)

The operator's QA pass: one deterministic, read-only sweep.

- **Checks**: workspace existence + writability (one
  `.amparo-doctor-probe` file written and removed), ledger readability
  (row and byte counts; unparseable lines flagged via the notebook's
  `jsonl_stats` — a corrupt line is a problem, a missing file is
  information), session checkpoints (parse; `Running` older than 7
  days is stale), notebook/skills/schedule JSONL parseability (never
  contents), policy-engine reachability (TCP dial with a 5 s timeout;
  `--probe` sends one real `/check` and consumes one engine check),
  and the `--chat-config` TOML.
- **Never writes anything else** — the sweep reports problems; the
  operator acts on them.
- **Exit codes follow the `amparo run` contract**: 0 healthy / 1
  problems / 2 usage. Check lines print to stdout (`[doctor]
  healthy`, one line per check); problem lines print to stderr.
  Cron-able.

### 3.3 Audit-mode notice + session tagging (`amparo-policy`, W3)

- **`AuditNoticeEngine<E>`** — a policy-engine wrapper: on the first
  verdict whose `fired` carries `wire::AUDIT_ONLY_MARKER` (the
  producer's `enforced: false` mapping), it prints exactly once per
  process, to stderr:
  `policy engine is in audit mode; verdicts are advisory` — the exact
  copy promised at `docs/trial-bundle.md`. The verdict passes through
  untouched (I1): the notice is display, never a change.
- **Session tagging**: `WirePolicyEngine::with_session_id` attaches the
  id to every `/check` request (`session_id`, omitted when absent —
  never sent as null). `amparo run --session-id ID` (default: the task
  id; on `--resume`, the checkpoint's original id) and
  `amparo mcp-serve --session-id ID` (optional); the chat driver tags
  `platform:user_id` per task, with one shared notice flag so the
  notice is once per process, not once per task.

**Landed**: W1 QC council core, W2 `amparo doctor`, W3 audit notice +
session tagging, W4 test sweep + edge hardening.

---

## 4. The audit posture, per feature

The acceptance test, made concrete — and how the suite proves it:

- **Council → verification** — *a final answer that cites an
  unexecuted tool raises a finding that reaches the verification
  prompt and the event stream, and verification stays the model's
  call.* Proven by e2e (`agent.rs`
  `qc_finding_flows_into_verification_prompt` — the answer claims a
  tool that never ran; one `QcAudit` event with the rule id lands,
  the recorded verification prompt contains the finding, and the
  task completes on the model's `VERIFIED`).
- **Council rules** — *each rule fires only on its trigger.* Proven by
  units (`cites_requires_word_boundaries`,
  `unexecuted_tool_claims_fire_only_for_known_unexecuted_citations`,
  `evidence_fires_when_results_left_the_context`,
  `cost_honesty_flags_divergent_claims_only` + the rounded-zero case,
  `pii_shapes_report_category_counts_never_values`,
  `audit_returns_approved_and_counts_stats`,
  `prompt_section_names_each_finding_and_stays_advisory`).
- **Doctor** — *a healthy workspace exits 0, a broken one exits 1,
  usage errors exit 2.* Proven process-level (`cli_e2e.rs`
  `doctor_exit_code_matrix` — fresh workspace → 0 with `[doctor]
  healthy` on stdout; file-as-workspace → 1; unparseable ledger → 1;
  `--nonsense` and `--probe` without a URL → 2) plus 15 units over
  every check (unreachable engine, stale/corrupt checkpoints, bad
  JSONL, TOML parse, the answered probe).
- **Audit notice** — *the exact promised line prints exactly once per
  process, however many audit-only verdicts arrive, and never for
  enforced verdicts.* Proven by units (`amparo-policy`: exact copy,
  once, non-audit silent, one shared flag across many engines) and
  process-level e2e (`cli_e2e.rs`
  `audit_mode_notice_prints_once_and_checks_carry_the_session_id` —
  two gated tool calls against a mock audit-mode engine, the notice
  counted exactly once on stderr, the task completes).
- **Session tagging** — *every wire check carries the session id, and
  the default (the task id) is never silently absent.* Proven by wire
  units (`session_id_flows_into_the_check_request`,
  `no_session_id_omits_the_field`) and process-level e2e (explicit
  `--session-id web-1` on every captured request body;
  `run_defaults_the_session_id_to_the_task_id` — no flag, field
  present and non-empty).
- **Audit mode still never blocks** (regression) —
  `audit_only_never_blocks` and `limit_reached_hard_blocks_even_in_
  audit_mode` from the M2b suite, green unmodified.

---

## 5. Storage layout

M9 adds **nothing on disk**. The council's stats are in-memory per
process; `doctor` writes only its self-removing probe file; the notice
is stderr. The only artifacts M9 touches at all are the request bodies
of existing wire checks (the additive `session_id` field). This is the
honest layout: verification is a view over what the loop already
records.

---

## 6. Deliberately excluded

- **LLM-as-approver / a second model judge** — axiom-qc's Tier2 LLM
  panel is excluded: a model is never the approver, and the council
  makes no model calls of its own. Deterministic rules only.
- **Keyword red-line blocking** — axiom-qc's red-lines are excluded:
  per-call blocking from code would be auto-policy (I1). The council
  advises; it never blocks.
- **A new gate** — nothing M9 adds sits in the gate chain. The
  council runs beside it, `doctor` reads around it, the notice
  describes it.
- **Auto-repair** — `doctor` reports problems and never fixes them;
  the operator acts. A self-healing sweep would be an acting agent
  without a gate.
- **Persisted council stats** — the counters are in-memory per
  process (the axiom-qc pattern). Persisting them would make QA
  metrics a stored artifact; nothing consumes them yet, so nothing
  stores them.

---

## 7. I1–I6 audit

| Invariant | How M9 honors it |
|---|---|
| I1 — no auto-tuning | Findings append to the verification prompt only (never the action loop); `QcVerdict` is advisory and `VerificationDecision` is untouched; the notice is display-only with verdicts passed through; `doctor` never repairs; session ids are correlation metadata, not policy inputs. |
| I2 — per-tenant namespacing | Session tagging is per caller (`platform:user_id` in chat, the task id in the CLI) — the M5 namespacing, extended to every check; `doctor` sweeps one workspace at a time, the tenant's own files. |
| I3 — provenance | The `[qc]` line carries the audit ordinal and cumulative counts; `AgentEvent::QcAudit` lands in the event stream with the findings; session ids make engine-side audit rows correlate with the run that caused them. |
| I4 — revocable | Nothing M9 pre-approves or caches a verdict; the notice engine wraps the same engine object the caller built, and unwrapping it is deleting the wrapper. |
| I5 — prompt immutable | The action-loop `SYSTEM_PROMPT` is untouched; the verification prompt gains an appended section only when findings exist — an empty pass leaves it byte-identical to the pre-M9 build. |
| I6 — privacy at capture | The PII rule reports category counts, never values (pinned by test); `doctor` reads ledger rows as counts; findings are rendered into a prompt that passes the same M7 strip-at-use machinery. |

---

## 8. Test inventory

The M9 surface is covered by these suites, all green at the W5 gate
(669 tests across the workspace) alongside the untouched M6–M8
regression suites:

- `amparo-agent/src/qc.rs` — 8 tests: citation word boundaries,
  unexecuted-tool firing only for known unexecuted citations, evidence
  gaps, cost honesty (divergent claims, honest rendered figures,
  non-inference dollar figures, no-rate silence, spawned-parent
  silence, rounded zero), PII category counts never values, verdict +
  stats counters, prompt-section wording.
- `amparo-agent/src/agent.rs` — `qc_finding_flows_into_verification_
  prompt` (finding → verification prompt + event stream e2e;
  verification still the model's call) (+ the M2–M8 suites).
- `amparo-cli/src/doctor.rs` — 15 tests: help anywhere, all flags,
  usage rejections (unknown flag, missing values, probe-without-URL,
  positional), host/port parsing (schemes, ports, IPv6), healthy fresh
  workspace, file-as-workspace, missing workspace, unreadable ledger,
  stale Running checkpoint, corrupt checkpoint, healthy JSONL +
  schedule, unparseable JSONL line, chat-config parse/report,
  unreachable engine, answered probe through a mock engine.
- `amparo-policy/src/lib.rs` — 3 notice-engine tests: exact copy
  once, non-audit silence, one shared flag across many engines.
- `amparo-policy/src/wire.rs` — `session_id_flows_into_the_check_
  request`, `no_session_id_omits_the_field` (+ the M2b suite: audit
  never blocks, limit_reached hard-blocks, fail-safe escalates,
  enforced deny/escalate, engine-direct, 500-with-verdict).
- `amparo-cli/src/run.rs` / `amparo-mcp/src/serve.rs` —
  `--session-id` parse arms, defaults, and missing-value rejections
  (both CLIs).
- `amparo-chat/src/driver.rs` — audit-mode wire engine proceeds and
  serves the task (enforced:false mock; the shared notice flag).
- `amparo-cli/tests/cli_e2e.rs` — `doctor_exit_code_matrix`
  (process-level 0/1/2), `audit_mode_notice_prints_once_and_checks_
  carry_the_session_id` (two checks, one notice, tagged bodies),
  `run_defaults_the_session_id_to_the_task_id` (default reaches the
  wire) — built on the new `MockPolicy` wire responder.

---

## 9. Risks and open questions

- **Council false positives.** The rules are heuristics over the run's
  records (a word-boundary mention, a context-count gap, a dollar
  figure). A false positive costs one verification-turn sentence ("the
  council raised advisory findings… ignore any that are wrong") — the
  prompt says so, and the verdict never overrides the model's call.
- **Verification is still a model judge.** The council improves the
  *evidence* in the verification prompt; it does not replace the
  model's verification with a deterministic check. That is the
  deliberate boundary: the model judges its own answer, the council
  supplies ammunition, the operator reads the audit.
- **`doctor` false alarms.** A missing file is information, not a
  problem (a fresh workspace has no ledger, sessions, or notebook) —
  but an unparseable line in an existing file is flagged by design:
  a corrupt row is a symptom, even when today's reader skips it.
- **Notice semantics on process boundaries.** Once per *process* is
  the contract; a host that rebuilds wire engines per task (the chat
  driver) still prints once because the flag is shared. A host that
  deliberately forks per check would print once per child — that is
  the documented boundary, and the wrapper is cheap to audit.
- **Session-id length.** The id travels with every check; a huge
  caller-supplied `--session-id` enlarges every request. The wire
  spec leaves validation to the engine; Amparo sends what it was
  given.
