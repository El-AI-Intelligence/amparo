# Amparo — the entire design

Status: master design (2026-08-28). This document is the plan: what
Amparo is, how it is built, what it becomes, in what order, and what it
refuses. The five planning documents it synthesizes remain the detail
references: `m6-controlled-growth.md`, `axiom-carryover-screen.md`,
`m6-local-llms.md`, `trial-bundle.md`, `swarms-advanced.md`.

---

## 1. What Amparo is

**An open agent that acts under policy. Bring your own LLM.**

The name is the brief: *amparo* is the legal action you file to protect
a right. The agent is headless, installable, versioned, and built around
one claim the rest of the field does not make:

> Every tool call passes a policy check before it executes, and the
> model driving the loop is yours to choose.

Three commitments shape everything below:

1. **Bring your own LLM.** Any OpenAI-compatible endpoint (Ollama,
   vLLM, llama.cpp, LM Studio, EXO) or the Anthropic API. No bundled
   model, no required sidecar, no privileged vendor. Local models are
   a first-class, tier-gated, honestly-documented path
   (`m6-local-llms.md`).
2. **Policy is an interface, not a product.** The gate between "the
   model decided to do this" and "this ran" is an open interface with
   an open wire protocol. The default is **deny**, not allow.
3. **Engram and Guardrail are recommended, never required.** Both sit
   behind traits; Amparo runs without either, and their absence is a
   clean state, not an error.

The audit posture, stated once: an external reviewer must be able to
see, for every executed action, *who allowed it and under what policy*.
Every design decision below is tested against that sentence.

## 2. The voice

How Amparo speaks to users — the "deliberate scientific tone" (user
directive 2026-08-28). Full spec: `m6-controlled-growth.md` §7. The
seven principles, compressed:

1. **Claims carry their evidence.** "I ran `cargo test`; 298 passed,
   0 warnings" — not "everything is fine."
2. **Uncertainty is stated, not performed.** "I have not verified
   this" — no hedging theater, no guesses dressed as measurements.
3. **Method before conclusion.** Reports follow lab-note shape:
   attempted, observed, concluded, open.
4. **No persona theater.** First person for actions only; "I think"
   and "I feel" become "the evidence indicates."
5. **Errors are findings, not confessions.** Correct once, plainly.
6. **Precision.** Concrete nouns, units on numbers.
7. **Answer first, method after, open questions last.**

The voice applies to every surface: chat answers, terminal output,
progress `[tag]` lines, approval messages, reports, and product copy.
It ships in the base system prompt (immutable — growth may never edit
it) and graduates to `docs/voice.md`.

## 3. The architecture today (v0.3.0)

Nine crates, one binary, 298 tests, zero warnings, both gates.

```
amparo-inference  BYO-LLM provider layer (OpenAI-compatible + Anthropic-native)
amparo-privacy    PII canary, domain routing, Secure Minions strip/restore
amparo-memory     memory trait + built-in default store   (Engram behind the trait)
amparo-policy     PolicyEngine trait + DenyAll default + wire client (Guardrail behind the trait)
amparo-tools      registry, 17 tools, 4 trust tiers, PathPolicy workspace confinement
amparo-agent      the loop: native tool_calls, gate chain, VERIFIED self-check, EventSink
amparo-mcp        MCP client + server, one gate chain both directions
amparo-chat       ChatTransport seam: Telegram, Discord, Slack; TOML tenant directory
amparo-cli        the binary: run / mcp-serve / chat / version
```

**The gate chain** — the spine, present on every path a tool call can
take (loop, chat, MCP server):

```
registry lookup → trust ceiling → policy engine → human approval
   (known tool?)   (tier allowed?)   (verdict?)     (60 s, auto-deny)
```

- Trust tiers: Observational → LocalMutating → ExternalEffector →
  SystemControl.
- Policy verdicts: allow / deny / escalate (escalate means **ask a
  human**; never silently execute). Wire contract: `POST /check
  {tool_name, target} → {verdict, reason, enforced}`; the caller never
  blocks on `enforced: false`; engine errors escalate (fail-safe).
- **The blocked-answer guarantee:** every call — executed or blocked —
  gets a tool-role answer carrying its `tool_call_id`. The model never
  hallucinates a tool result.
- Defaults: no policy engine and no `--allow-all` → every call refused.
  Approvals auto-deny on timeout; non-interactive stdin auto-denies.

Multi-tenancy (v0.3.0): a TOML tenant directory names exactly who may
start tasks; each user gets their own trust ceiling, their own
workspace directory, policy checks tagged `session_id =
platform:user_id`, and approvals only the requester can decide.

## 4. The principles that govern growth

From `m6-controlled-growth.md` — six invariants any future feature must
satisfy:

- **I1** Learned material executes as tool calls, never raw
  instructions.
- **I2** Learning is namespaced per tenant.
- **I3** Every learned artifact carries provenance.
- **I4** Learning is revocable; retirement never deletes audit history.
- **I5** The action-loop prompt is immutable.
- **I6** Privacy is enforced at capture time (PII stripped before
  persistence).

And the swarm corollary (`swarms-advanced.md`): **no member exits the
gate chain** — spawning an agent is a tool call, delegation is not
exemption, and a model is never the approver.

## 5. M6 — controlled growth (next)

The agent improves with use without ever acquiring the capacity to
bypass policy. Three artifacts, built in order:

- **M6a — the lab notebook.** Every task becomes a PII-stripped,
  tenant-tagged run record (tool-sequence hash, gate log, verification
  outcome, truncated answer) written through the `EventSink` seam into
  the memory trait. `--growth` / `--no-growth` on the CLI and chat
  driver.
- **M6b — the verification case library.** Retrieval over the notebook
  feeds the *verification prompt only* — read-only evidence formatted
  as observations, never imperatives, never in the action loop.
- **M6c — gated skills.** A skill is a named procedure: preconditions,
  ordered tool-call steps, expected outcome. Candidates (operator
  authored, or distilled from repeated high-VERIFIED sequences) are
  inert until adopted through the same gate chain plus human approval
  showing the full step plan; execution expands into steps that each
  pass the gate individually.
- **M6d — metrics and retirement.** Every skill carries uses,
  VERIFIED rate, mean steps, denials; a periodic policy-drift dry-run
  retires skills the current policy would no longer allow; performance
  thresholds retire the rest. Retirement = disable + notify, never
  silent deletion.
- **M6e — rollup and archival.** Archive everything, index
  selectively: the cold archive holds every record; the hot layer (and
  the sync relay) carries the deduplicated informative subset.
  Defaults: ~5–15% persistence, ~4 KB payload caps, 90-day rollup →
  ~5–7 MB/month/user versus 300–600 MB/month at no levers.

Storage baseline (locked with the user, 200+ tasks/day): no-levers
10–20 MB/day/user, ~4–7 GB/year; team of 10 → ~40–70 GB/year; the
binding constraint is the sync relay, not disk.

**Excluded, permanently:** auto-trusted skills, self-edited system
prompts, RL from production, cross-tenant leakage, imported skills
executing under different rules.

## 6. M7 — instrumentation & hardening

The carryover shortlist from `axiom-carryover-screen.md`, chosen for
headless self-containment and direct legibility:

- **Privacy ledger** (`amparo-privacy`) — a durable outbound-network-
  call log; strengthens I6 and gives the voice something to cite.
- **Session persistence** (`amparo-chat` / `amparo-agent`) — durable
  sessions across restarts; the M6 notebook's substrate.
- **Preflight blast-radius classification** (`amparo-agent`,
  design-first) — risk classification *before* the call; the approval
  message displays the blast radius, so the user approves the concrete
  consequence, not an abstraction.
- **QC council** (later in M7 or M8) — deterministic rule-based
  auditors as a second opinion beside policy; feeds M6b's case
  quality.
- **WASM eval sandbox** (own milestone, M7b candidate) — fuel-metered,
  deterministic execution as a new `eval_wasm` tool at a high trust
  tier. The one new heavyweight dependency (wasmtime); scheduled
  separately for that reason.

## 7. M8 — sub-agents & scheduling

From `swarms-advanced.md`:

- **`spawn_agent`** — a registry tool like any other; the child runs
  the same loop and gate chain in a task-scoped workspace; the
  delegation chain is recorded in the session id
  (`operator → task → sub-agent`).
- **`schedule`** — Amparo's first background loop. Scheduling is a
  promise, not an execution: a scheduled task re-enters the gate when
  it fires, with its original requester; unattended, it waits at the
  approval gate and auto-denies on timeout.
- **Swarm budget + cost line** — a configurable maximum of sub-agents
  per task, and a cost line in every report ("3 sub-agents, 41 tool
  calls, ~$0.04"). The environmental commitment made mechanical.
- **Event bus** — adopt the SQLite-audited pub/sub shape from Axiom
  when the second real consumer appears (coordination between
  sub-agents), not before. `EventSink` suffices until then.
- **Excluded:** supervisor agents (a model is never the approver),
  nexus/autonomy/curiosity machinery, proactive reach-outs, web UI,
  device mesh.

## 8. Local models and the bolt-on

Amparo already accepts any OpenAI-compatible endpoint — the bolt-on is
real today; the work is documentation and honesty. From
`m6-local-llms.md`:

- **Recommend Qwen 3.5/3.6 Instruct** (Apache-2.0) as the primary
  local family; GLM-4.7-Flash (MIT) for a 24 GB GPU; Phi-4-Mini /
  Qwen3-4B for low-power.
- **Hardware tiers stated plainly:** 16 GB floor for "capable";
  24 GB GPU / 32 GB unified Mac recommended; below that, the honest
  ceiling is 2–3-step tasks. Q4_K_M is safe for the recommended
  families.
- **Document the serving gotchas** (vLLM tool-call parser flags,
  llama.cpp Jinja templates, Ollama `num_ctx`) — the most common
  "tool calling broken" causes are operator misconfiguration.
- **Candidate feature:** `amparo doctor` — a one-request tool-call
  probe that reports whether structured `tool_calls` come back.
- **Frame honestly:** local is ~1–1.5 generations behind frontier APIs
  on long-horizon autonomy; recommend it for privacy/sovereignty, cost
  caps, and well-scoped 3–10-step tasks.

## 9. The companion story: Engram + Guardrail

Recommended, never required — and one-month trial bundled at download
(`trial-bundle.md`):

- **The offer:** download Amparo → one redemption link, no card → one
  month of Engram personal-tier allowances (5 devices / 10 GiB) and
  Guardrail enforcement. Then each product degrades to its **existing
  free tier**: Engram 1 device / 1 GiB, Guardrail audit-only.
- **Why it is graceful:** the degradation path is the "never
  required" path, already exercised — the wire contract never blocks
  on `enforced: false` (audit mode = advisory verdicts, approval gate
  remains), and the memory trait falls back to the built-in store.
  Nothing breaks; nothing is silently waived; the user is told in
  advance, in the agent's own voice.
- **Cross-product work:** trial entitlements in Engram (quota bump
  with expiry; billing.rs already maps quota columns) and Guardrail
  (no-card 30-day enforcement; `trialing` status already in schema).
- **Self-hosters:** unaffected — Engram is FSL source-available; the
  trial covers the hosted relay and hosted engine only. That is a
  feature to state, not hide.
- **Launch gate:** the bundle goes live at Amparo's public reveal, not
  before.

## 10. Distribution and surfaces

- **The binary** — `cargo install --path crates/amparo-cli`, and
  eventually platform downloads; the MCP server is the integration
  face for other tools (Claude Code, Cursor).
- **The faces** — terminal (`amparo run`), chat surfaces (Telegram /
  Discord / Slack), MCP both directions. No web UI — headless is a
  commitment.
- **First-run UX** — provider wizard (directive), Engram daemon probe
  on `127.0.0.1:8787`, Guardrail key detection, trial pointer printed
  when the conditions hold.
- **The reveal** — public repo + crates.io remain user decisions.

## 11. The complete roadmap

| Milestone | What | Version | Depends on |
|---|---|---|---|
| 1–5 | done — provider, loop, release, chat, multi-tenant | 0.1.0 → 0.3.0 | — |
| **M6** | controlled growth: notebook → case library → gated skills → metrics → rollup | 0.4.0 | v0.3.0; Engram behind the trait as-is |
| **M7** | instrumentation & hardening: privacy ledger, session persistence, preflight, QC council | 0.5.0 | M6a (ledger/persistence are its substrate) |
| **M7b** | WASM eval sandbox | 0.5.1 or 0.6.0 | M7; wasmtime dependency review |
| **M8** | sub-agents & scheduling | 0.6.0 | M7 (persistence + preflight), M6 (notebook provenance) |
| **Trial** | Engram + Guardrail bundle | n/a (cross-product) | entitlement work in both products; launches at reveal |
| **Docs** | local-LLM guide, voice.md, trial page | continuous | — |

Each milestone lands with both gates green (`cargo test --workspace`,
`cargo doc --workspace --no-deps`, zero warnings), `missing_docs` on
every new public item, and no new heavyweight dependencies except the
deliberate wasmtime decision.

## 12. The refusals — kept out of the design

- Desktop compositor, screen ingestion, companion loop, ELLM kernel
  coupling (the original extraction boundaries — screen tools return
  someday as surface-neutral adapters, designed fresh).
- Auto-trusted skills, self-edited prompts, production RL,
  cross-tenant leakage.
- Supervisor agents, autonomy/curiosity/persona machinery, proactive
  reach-outs, web UI, device mesh (for now).
- Any "growth" that moves the trust boundary into the model.

## 13. Open decisions — the user's

1. Milestone ordering: M6 → M7 → M8 as above, or pull anything
   forward (e.g. M7's preflight before M6c's skills)?
2. Trial parameters: 30 days vs 14; no-card vs card; a discounted
   post-trial upgrade path?
3. WASM sandbox: include in the plan at all, and if so M7b or later?
4. `amparo doctor`: build in M6 timeframe or defer?
5. Public reveal timing — everything in §9 and §10 gates on it.
6. M6 defaults: `--growth` on or off by default at first release?
