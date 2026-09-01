# Axiom-OS carryover screen

Status: screening (2026-08-28). Input: a fresh inventory of the Axiom-OS
workspace (33 crates plus `apps/`, `ui/`, `enterprise/`, `platforms/`,
`vision/`, `theory/`) against Amparo v0.3.0 (9 crates). This document is
the judgment pass; the inventory is not reproduced in full.

## Method

Every Axiom feature was classified by coupling — self-contained,
desktop/GUI-bound, ELLM-coupled, hardware-bound, or separate product —
and screened against: headless, self-contained, aligned with the
safety/privacy posture, and no heavyweight new dependencies.

## Confirmed exclusions (restated)

- **Compositor + desktop surface** — screen ingestor, perception stream,
  browser CDP, display/automation/screenshot/framegraph tools, window
  backends. GUI-bound; contradicts headless. (The standing UX directive
  for screen tools is surface-neutral adapters designed fresh, not this
  stack.)
- **Companion loop + face/voice/wellness/prosody** — GUI, audio, and an
  emotional model; the opposite of a policy-gated headless agent.
- **ELLM kernel coupling** — `.ellm`/WASM kernel scripting, polar
  microcode, PCLM missions, trinity/philosophical handlers. Proprietary
  kernel coupling; BYO-LLM forbids it.
- **Governance, integrity monitor, self-improvement, learning foundry** —
  all built on the constitutional-rule kernel and fleet quorum.
  `m6-controlled-growth.md` rebuilds the growth concept from scratch
  under the gate chain instead.
- **Wallet, email/inbox, actuator (lab/medical), device/thermal stack,
  model router, axiom-future theory modules** — stubs, hardware
  couplings, or speculative research.

## Shortlist — carry over

| Candidate | Where (Axiom) | Why | Landing spot |
|---|---|---|---|
| **Privacy ledger** | `axiom-daemon/src/privacy_ledger.rs` | Durable outbound-network-call log; tiny, pure Rust; strengthens the privacy story and the voice's evidence discipline | `amparo-privacy` — high priority |
| **Session persistence** | `crates/axiom-session` | Durable sessions with summary hooks; cross-restart continuity; complements the chat layer and the M6 notebook substrate | `amparo-chat` / `amparo-agent` — high |
| **WASM eval sandbox** | `crates/axiom-sandbox` | Fuel-metered, deterministic execution; a new `eval_wasm` tool at a high trust tier (approval-gated like any ExternalEffector) | new tool in `amparo-tools` — medium (wasmtime dependency) |
| **Preflight/rollback engine** | `axiom-daemon/src/action_engine.rs` | Blast-radius classification + rollback groups; a natural extension of the gate: classify the risk *before* the call, and put the blast radius in the approval message | design-first, `amparo-agent` — medium (surgery) |
| **QC council** | `crates/axiom-qc`, `axiom-agents/src/council.rs` | Deterministic rule-based auditors as a second opinion beside policy; could feed the M6 verification stage and the voice's "claims carry evidence" rule | `amparo-agent` verification stage — medium |

## Deferred (revisit conditions)

- **Screen/desktop perception — build note (2026-09-01).** The exclusion of
  Axiom's GUI-bound stack (compositor, window backends, CDP) stands, but the
  *capability* stays on the table as a fresh, surface-neutral adapter, per
  the standing directive. Build shape when revisited: local capture,
  PII-stripped before inference (`amparo-privacy`), every capture session
  through the same gate chain (preflight blast radius + human approval,
  never an always-on ingestor), delivered as gated registry tools. Revisit
  when computer-use becomes a user requirement — Amparo's trust topology is
  the differentiator that could make screen perception credible.
- **Code intelligence** (`crates/axiom-code`, tree-sitter + LSP) — defer
  until Amparo positions as a coding agent; heavy dependency surface.
- **Event bus + blackboard** (`crates/axiom-eventbus`,
  `axiom-blackboard`) — defer until sub-agents exist; `EventSink`
  suffices today. The SQLite-audited pub/sub design is the reference for
  the swarm substrate (task #65).
- **Epistemic ingest + scheduler** — niche research crawler; the
  takeaway is its *background-loop pattern* (Amparo currently has none)
  as an input to the scheduling design in task #65.
- **Topic clustering** — folds into M6e rollup (memory), not a
  standalone feature.
- **QEM L1 cache** — Engram product boundary; skip.

## Effects on roadmap

Proposal only — the README roadmap is untouched until the user decides:

- **M7 candidate: "instrumentation & hardening"** — privacy ledger +
  session persistence + preflight blast-radius classification. All
  headless, all self-contained, all directly legible in the approval UX.
- **Later candidate:** WASM sandbox as its own milestone (new
  dependency, new tool surface).
- Any new registry tool is a surface change → the workspace version
  moves to 0.4.0 (per VERSIONING.md).

## Feeds

- **M6** (`m6-controlled-growth.md`): session persistence backs the
  notebook substrate; topic clustering backs the M6e rollup.
- **Task #65 (swarms / advanced system):** the deferred multi-agent
  machinery — orchestrator, nexus intention/autonomy spaces, agenda
  (goals/curiosity), council, fleet mesh, blackboard/event bus — is the
  substrate inventory to screen *behind the same gate chain*.
