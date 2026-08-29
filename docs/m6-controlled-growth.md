# M6 — Controlled growth: memory and skills as falsifiable instruments

Status: **design proposal** (2026-08-28). Nothing described here is built.
Amparo today is v0.3.0: a policy-gated tool-use loop with 298 workspace tests
across 9 crates, all five roadmap milestones shipped.

---

## 1. Objective

Amparo should improve with use — become measurably better at its operator's
tasks, develop durable procedures, remember what worked — without ever
acquiring the capacity to bypass its own policy. Every form of growth that
violates the audit posture (an external reviewer must be able to see, for
every executed action, *who allowed it and under what policy*) is excluded
by design, not by convention.

The posture we adopt for this feature is scientific in the narrow sense:
**every learned artifact is a hypothesis** — proposed, evidenced, tested,
and retired when falsified. Nothing learned is ever taken on faith, and
nothing learned is ever exempt from the gate chain that governs everything
else.

---

## 2. First principles

These are invariants. A design that cannot satisfy all six is not this
design.

- **I1 — Learned material executes as tool calls, never as raw
  instructions.** Anything the agent adopts must, at execution time, run
  through the same gate chain as a tool call: registry lookup → trust
  ceiling → policy engine → human approval. A learned skill that smuggles
  a step past policy is a policy bypass with extra steps, and is refused
  on those grounds.
- **I2 — Learning is namespaced per tenant.** Tenant A's cases, skills and
  metrics never inform tenant B's loop. Cross-tenant promotion is an
  explicit operator action, never automatic.
- **I3 — Every learned artifact carries provenance.** For each skill or
  case: the source runs that produced it, timestamps, and the metrics it
  has accumulated since. The artifact must be reviewable as evidence.
- **I4 — Learning is revocable.** Any artifact can be retired (disabled +
  operator notified) or deleted; retiring or deleting a live skill never
  deletes the audit record of what it did.
- **I5 — The action-loop prompt is immutable.** Growth may add *evidence*
  to read-only stages (verification) but may never rewrite the system
  prompt or the action-selection context. (This is the "no self-edited
  system prompt" exclusion made structural.)
- **I6 — Privacy is enforced at capture time.** Records are PII-stripped
  *before* persistence, using the existing Secure Minions primitives in
  `amparo-privacy`. The placeholder-restore mapping stays in process
  memory and is never written to storage. What is stored is a laboratory
  log, not a mirror of the user's data.

---

## 3. The three artifacts

### 3.1 The lab notebook — run records

Every completed task becomes a protocol entry:

| Field | Content |
|---|---|
| task text | PII-stripped |
| tool-sequence hash | canonical order+args digest, for dedupe |
| gate log | per-call verdicts: allowed / denied / escalated / approved-by-human |
| verification | VERIFIED / INCOMPLETE outcome, and the verification round |
| outcome | final answer (truncated), duration, token cost estimate |
| tenant tag | the namespace the record belongs to |

This is the substrate everything else reads from. It is written through
the existing `EventSink` seam in `amparo-agent`, so a deployment that
wants no growth writes nothing (see §6.1). Storage lives behind the
`Memory` trait in `amparo-tools` (the `amparo-memory` crate is an unused
context-assembly crate, not the storage seam): the built-in default
store today, Engram as the recommended backend — never a dependency.
**M6a is landed**: the `amparo-notebook` crate records every completed
or failed task as a PII-stripped, tenant-tagged JSON line in
`<workspace>/.amparo/notebook/records.jsonl` behind `--growth` /
`--no-growth` on `amparo run` and `amparo chat` (off by default).

The notebook's scientific function: it makes the agent's history
*inspectable*. The agent can be asked evidence questions — "what did I
attempt last week, how often did the gate stop me, which of my procedures
is degrading" — and answer from the record rather than from a
reconstructed memory of itself.

### 3.2 The verification case library — evidence, not instruction

The loop already self-verifies (VERIFIED/INCOMPLETE, ported from Axiom
and preserved). Today that verification runs as a standalone completion
with the candidate answer. The case library adds a retrieval stage to
*that one read-only step*: the verification prompt gains a section of the
form —

> Prior cases in this tenant resembling the current task:
> - Case 412 (2026-08-14, VERIFIED): to achieve X the agent ran
>   `git status`, then `git diff`; the gate allowed both; the result was
>   confirmed by the operator.
> - Case 388 (2026-08-09, INCOMPLETE): the agent attempted Y with a single
>   step and abandoned after one empty turn.

Two structural rules keep this safe:

1. **Retrieval feeds verification only.** Cases never enter the action
   loop's tool-selection context. The verification stage is
   read-only with respect to action: it judges the candidate answer, it
   does not choose tools. Evidence cannot execute anything.
2. **Cases are formatted as observations, not imperatives.** A retrieved
   case describes what happened; it contains no instruction the model is
   told to follow. This keeps the library's influence
   *evidential* — the verification prompt remains a prompt asking for a
   judgment, with the library as exhibits.

The same-tenant filter is enforced at retrieval time (I2), and any case
an operator promotes for long-term retention goes through their review.

Why this is the growth mechanism rather than "the agent reads its own
diary in the action loop": the diary in the action loop is prompt
injection with extra steps. The diary in the verification step is a
controlled experiment — one variable (evidence) added to one measurement
(the judgment of the candidate answer), with the rest of the loop
unchanged.

### 3.3 Gated skills — procedures as hypotheses

A **skill** is a named procedure: preconditions, an ordered list of steps
(each a tool call with arguments), and an expected outcome. Skills
originate two ways:

- **Operator-authored** — written by the person accountable for the
  deployment, exactly as one writes a script today.
- **Distilled candidates** — when the notebook shows a tool sequence
  recurring with a high VERIFIED rate, the system may *propose* it as a
  candidate skill. Proposals are inert. They are flagged to the operator
  and never become executable on their own.

Adoption of any candidate goes through **the same gate chain as a tool
call**, plus human approval. Concretely: adoption is a policy event on
the skill's namespace (a synthetic check the policy engine must answer),
followed by the ordinary approval UI — and the approval message shows the
full step plan, exactly as a command approval shows the command. Nothing
is auto-trusted; there is no path from "observed to work" to "allowed to
run" that does not pass through a human decision recorded in the audit
log.

Execution: invoking a skill is itself a tool call (`use_skill`), and the
executor expands it into its constituent steps — **each of which passes
the gate chain individually**. A skill can never grant its steps an
exemption: the gate checks per step, not per skill. This is the
structural difference from self-improvement schemes where a distilled
skill becomes raw instructions the model reads and follows. Here the
distilled artifact is, at execution time, indistinguishable from any
other sequence of tool calls — because it *is* a sequence of tool calls.

### 3.4 Metrics — falsifiability made operational

Every adopted skill carries a running record:

- **uses** — invocations since adoption;
- **VERIFIED rate** — fraction of uses whose outcome the loop's own
  verification confirmed;
- **mean steps** — procedure length in practice vs. the plan;
- **denials** — gate outcomes per step (allowed / escalated / denied);
- **last policy re-check** — see below.

Two automatic events follow from the metrics:

1. **Policy-drift re-check.** On a schedule (and on demand), the skill's
   step plan is dry-run through the policy gate as a synthetic check — no
   execution. If the gate *would now deny* a step it previously allowed
   (the operator tightened the engine, the tenant's ceiling moved), the
   skill is **retired**: disabled, flagged to the operator with the
   offending step and the engine's current verdict. A skill is a
   hypothesis of the form "this procedure is safe and effective for this
   tenant, under the policy in force at adoption." When the policy
   changes, the hypothesis is re-tested. If it fails, it is falsified —
   and it is retired, not patched in secret.
2. **Performance retirement.** A skill whose VERIFIED rate falls below a
   configured threshold over a configured window (e.g. < 50% over its
   last 20 uses) retires the same way: disabled, operator notified, audit
   record intact.

Retirement is always *disabling with notification*, never silent
deletion. The audit log does not forget; the skill merely stops being
available.

### 3.5 Namespaces

Skills and cases are keyed by tenant scope, and the scope is enforced at
three points: capture (the record is tagged with the tenant that produced
it), retrieval (the same-tenant filter), and adoption (a skill adopted
in tenant A's namespace is invisible to tenant B's loop). This is the
same machinery that already separates per-user workspaces and
session-tagged policy checks in v0.3.0 — M6 extends the existing
multi-tenant boundary rather than inventing a new one.

---

## 4. Storage plan

The sizing baseline was established with the user (2026-08-28) under a
**"no levers" assumption**: a heavy user runs 200+ tasks per day, and
nothing is trimmed, capped or rolled up.

| Measure | Per task | Per day (200 tasks) | Per month | Per year |
|---|---|---|---|---|
| median record | 15–30 KB | 3–6 MB | 90–180 MB | ~1.1–2.2 GB |
| mean record (tail-heavy) | 50–100 KB | 10–20 MB | **300–600 MB** | **~4–7 GB** |
| team of 10, no levers | — | 100–200 MB | **3–6 GB** | **~40–70 GB** |
| with defaults (§ below) | — | ~0.2 MB | **~5–7 MB** | ~60–85 MB |

Reconciliation — **archive everything, index selectively**:

- **Cold archive** holds the full record for every task. Disk is cheap
  and the audit posture wants completeness; the cost is storage, not
  attention.
- **The hot layer** (what lives in the Engram vault and, for teams, what
  the sync relay moves) carries only the *informative subset*: records
  that survive dedupe (by tool-sequence hash), gate events of interest
  (approvals, denials, escalations), and the artifacts of §3 — adopted
  skills, their metrics, and operator-promoted cases.

Defaults that achieve the ~5–7 MB/month/user row: selective persistence
(~5–15% of records promoted to the hot layer), payload caps (~4 KB per
record body), and a 90-day rollup that folds older hot records into the
cold archive. Each is a documented lever; none changes the audit posture,
because the cold archive is not a cache — it is the record, and it is
retained.

The binding constraint is not disk: it is the **sync relay** — the
channel that moves the hot layer between devices for multi-device users
and teams. Everything in this design is shaped so the relay carries
deduplicated evidence, not raw noise; a user with defaults burns roughly
two orders of magnitude less relay traffic than the no-levers baseline,
at no cost to completeness.

---

## 5. Deliberately excluded

These are the forms of growth the field routinely offers, and the reason
each is refused here:

- **Auto-trusted skills.** Nothing distilled from observation ever
  becomes executable without a human decision in the audit log. The
  convenience of "the agent noticed it works, so it now does it" is
  precisely the capability that converts a prompt-injection or a policy
  change into an autonomous action.
- **Self-edited system prompts.** The action-loop prompt is immutable
  (I5). Growth may add evidence to read-only stages only.
- **RL from production** (the Atropos pattern). Metrics may inform human
  decisions; they never auto-tune the policy engine, the gate, or the
  prompts. Production data is an input to operator judgment, not a
  training signal the system applies to itself.
- **Cross-tenant leakage.** By I2, at capture, retrieval and adoption.
  This is also a privacy commitment: one tenant's procedures are not
  another tenant's training set.
- **Third-party skills executing under any different rules than local
  ones.** An imported skill is a candidate like any other; adoption is
  the same gate, and its steps run the same per-step checks.

---

## 6. Environment, and what "advanced" should feel like

**Environment.** The honest framing: the dominant environmental cost of
an agent is inference, and a memory feature does not change that order of
magnitude. What this design contributes is the absence of waste —
dedupe and rollup keep the hot layer and the sync relay small, which
means fewer bytes stored, fewer moved, less energy for both, while the
cold archive remains complete. Growth is also *optional* (§6.1): a
deployment that wants a stateless agent runs it stateless, and nothing is
spent on memory it does not use. And because local models are a
first-class inference path (BYO-LLM, see `docs/m6-local-llms.md`), an entirely on-device Amparo —
inference, memory and all — is a supported configuration, which is where
the environmental story actually lives.

**Advanced feel.** The notebook, case library and metrics surface as an
*instrument panel* rather than a diary. The agent can answer evidence
questions about its own conduct; its procedures carry visible,
falsifiable performance records; and when the operator changes policy,
the system shows its work — "this skill passed under the old policy and
fails under the new one; it is retired." That is the scientific tone made
into product behavior — and §7 specifies the same tone as the agent's
voice: claims about the agent's competence are always backed by the
record, and the record is always checkable.

---

## 7. The agent's voice — the scientific tone, made audible

User directive (2026-08-28): the "deliberate scientific tone" is **how
Amparo speaks to users** — in chat answers, terminal output, approval
prompts and reports. It is a product-voice specification, distinct from
(and consistent with) the growth design in this document.

Seven principles:

1. **Claims carry their evidence.** "I ran `cargo test`; 298 passed,
   0 warnings" — not "everything is fine." Where the evidence is thin,
   the agent says so: "I have not verified this."
2. **Uncertainty is stated, not performed.** Real qualifications with
   real specificity ("the diff suggests X; I have not reproduced it") —
   no hedging theater, no false precision, no guesses dressed as
   measurements.
3. **Method before conclusion.** Reports follow lab-note shape: what was
   attempted, what the gate allowed and denied, what was observed, what
   is concluded, what remains open.
4. **No persona theater.** The agent never claims feelings, opinions or
   consciousness. First person is reserved for its own actions ("I ran",
   "I found"), never its inner states; "I think" and "I feel" become
   "the evidence indicates".
5. **Errors are findings, not confessions.** A mistake is reported as a
   correction with the corrected result — stated once, without apology
   spirals.
6. **Precision in language.** Concrete nouns, active voice, short
   sentences; numbers carry units; "approximately" only when the
   quantity is approximate.
7. **Respect for the reader.** The answer first, the method after, the
   open questions last.

Mechanics: the principles ship compressed into the base system prompt
(consistent with I5 — the prompt is fixed at release, not
self-editable), with `docs/voice.md` as the full spec when it graduates.
The progress `[tag]` lines already read like instrument output; the
voice extends that register to prose. Approval messages are part of the
voice: they display the exact call under decision and its outcome — the
user is never asked to approve an abstraction.

---

## 8. Implementation phases (proposal)

Each phase lands with both gates green (`cargo test --workspace`,
`cargo doc --workspace --no-deps`, zero warnings), `#![warn(missing_docs)]`
on every new public item, and no new heavyweight dependencies. Retrieval
defaults to dependency-free keyword/tool-sequence-hash matching; the
embedding-quality path exists only when the memory backend provides it
(Engram does natively — the recommended-backend value proposition —
while the built-in default store stays cheap).

- **M6a — notebook substrate.** ✅ landed — `EventSink` hook → record
  builder (PII strip before persistence, tenant tag, dedupe hash); write
  path through the `Memory` trait in `amparo-tools`; `--growth` /
  `--no-growth` switch on the CLI and chat driver; audit-log-style
  append only.
- **M6b — case library.** Retrieval over the store with the same-tenant
  filter; evidence section appended to the verification prompt only;
  observation-format serialization; operator promotion of cases.
- **M6c — gated skills.** `SkillSpec` schema (preconditions, ordered
  steps, expected outcome); candidate proposal from repeated high-VERIFIED
  sequences (inert, flagged); adoption = policy event + full-plan human
  approval; `use_skill` tool whose executor runs each step through the
  gate chain individually.
- **M6d — metrics and retirement.** Per-skill counters; policy-drift
  dry-run re-check; performance-retirement thresholds; retire = disable +
  notify, audit record intact.
- **M6e — rollup and archival.** Defaults from §4 (selective persistence,
  4 KB caps, 90-day rollup to cold archive); sync-relay guidance for
  teams.

---

## 9. Risks and open questions

- **Poisoned evidence.** A case in the library could bias verification.
  Mitigations: tenant-scoped retrieval, observation formatting,
  operator review for promoted cases, and the fact that evidence can
  influence a *judgment* but never an *action*. Residual risk accepted:
  the alternative (no retrieval at all) forfeits the growth the feature
  exists to provide.
- **Skills are scripts.** An adopted skill is code-adjacent; approval
  must display the full step plan, and the per-step gate remains the
  backstop. The trust ceiling of `use_skill` itself is a config decision
  (its expansion runs at the ceiling of its steps).
- **Storage drift.** Defaults keep the hot layer at ~5–7 MB/month/user;
  operators can raise levers and the relay will tell them the cost.
  Documented, not hidden.
- **Verification-prompt identity.** The verification step must remain a
  standalone completion (as today); the evidence section is injected
  text, not a change to how the round runs.
- **Open:** is candidate proposal on by default or opt-in per tenant?
  What is the default VERIFIED-rate retirement threshold? Should the
  case library also inform the *nudge* path (same-tool repeats), or stay
  strictly in verification?
