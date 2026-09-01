# Kimi prompt — design the Amparo interactive TUI (#166)

Paste-ready for Kimi (K3). The contract in this prompt wins over everything
else in the prompt.

## Mission

Design the visual identity and interaction spec for Amparo's interactive
terminal UI (planned command: `amparo tui`). Deliverable: a design spec that
a coding session implements in **raw ANSI**, plus an HTML preview mockup.
The identity must be distinctively Amparo — built from what makes Amparo
unique — while adopting standard terminal UX conventions rather than
re-inventing them.

## Context — what Amparo is

An open agent that acts under policy. Bring your own LLM. The one rule:
**no member exits the gate chain**:

```
registry → trust ceiling → policy engine → human approval
```

Deny-by-default, fail-closed at every step. A model is never the approver.
Amparo's voice to users is the **scientific voice**: `[tag]` status lines,
restraint, no marketing-speak (`docs/m6-controlled-growth.md` §7). Brand:
**Amparo█** (capitalized, blinking block cursor). Family: the EL AI
Intelligence slate palette.

## What already exists (extend — do not redesign)

- **Palette tokens** (`web/public/style.css` `:root`): bg `#0b0f14`,
  bg-raised `#11161d`, panel `#151b24`, border `#232c3a`, fg `#e2e8f0`,
  dim `#94a3b8`, accent `#3b82f6`, ok `#10b981`, bad `#ef4444`,
  warn `#f59e0b`.
- **The `[tag]` event vocabulary** — canonical list and exact formats in
  `crates/amparo-agent/src/events.rs` (`format_event`): `[gate]`, `[exec]`,
  `[preflight] blast radius: …`, `[rollback] …`, `[bus] …`, `[qc] …`,
  `[spawn] …`, `[schedule] …`, `[memory] …`, `[notification] …`,
  `[growth] …`, the cost line (`~$0.04 in inference (estimate, chars/4,
  $3/1M tokens)`) and the swarm report line.
- **The landing page** (`web/public/landing.html` + `landing.css`) — the
  spec-register aesthetic, the event-stream terminal hero, hairline story
  beats. The TUI should feel like the product the landing describes.
- **The operator web console** (`web/public/index.html` + `app.js`) —
  already on the slate palette.

## What the TUI is (the constraints Kimi designs within)

- Streaming REPL: type a task, watch it run.
- Tool calls visible as they execute, each with its gate-chain decision.
- Inline human approvals: y/N at the input line, 60 s fail-closed,
  preflight blast radius + rollback hint in the copy.
- Session history + `--resume`.
- The same gate chain as every other surface — the TUI is a new surface,
  never a new trust boundary. No auto-policy changes, no auto-approve
  defaults.
- **Raw ANSI only**: no ratatui, no crossterm, no new dependencies
  (hand-rolled escape codes, the project's hand-rolled ethos).
  Terminal-safe: works over ssh, no mouse required, degrades to plain
  lines when piped.
- `amparo run`'s stdout purity is untouched (stdout = final answer only) —
  the TUI is its own command/surface.

## What to design

1. **The Amparo-distinct signature.** Candidate motifs to develop (your
   call, but earn the distinction):
   - **The gate chain as the spine**: every tool call renders its passage
     through the four links (registry → ceiling → policy → human) as a
     visible chain with per-link states (passed / skipped / blocked), so
     the one rule is literally what you see. This is Amparo's answer to
     the spinner — structure, not shimmer.
   - **The approval moment as the hero**: the human gate is the product's
     centerpiece — design the approval card (preflight blast radius,
     rollback hint, y/N, 60 s countdown) as the most crafted element on
     screen.
   - **Amparo█ cursor** in the input line; `[tag]` lines color-coded by
     family tokens.
   - Do not clone Claude Code/Kimi's look. Adopt their conventions
     (streaming, ↑↓ history, Ctrl+C semantics, one-task-at-a-time) and
     re-skin everything else.
2. **ANSI token mapping**: exact 256-color (with truecolor variants)
   values for every token above; decision on dark-only vs
   terminal-default background; 16-color fallback behavior; bold/dim
   usage rules; no emoji unless it survives the scientific voice.
3. **Layout anatomy** (placement, borders, spacing for each element):
   - Boot banner (#165 — exact strings the implementation will print):
     `Greetings! My name is Amparo, built by EL AI Intelligence.` then
     `[wake] Amparo is awake.` then the gate-chain readout lines.
   - Status/header line: model · provider · trust ceiling · policy mode ·
     memory backend.
   - Event stream: tool-call blocks (collapsed summary + expandable
     detail), the chain render per call, `[tag]` lines.
   - Input area; the approval card; the session-resume picker; cost and
     swarm lines.
4. **Native companion surfaces (operator directive — Guardrail & Engram
   are functional pieces, not logos):**
   - **Guardrail (policy)**: a policy status/pane showing which engine is
     live, enforce vs audit mode (the audit badge: "policy engine is in
     audit mode; verdicts are advisory"), per-verdict reasons, recent
     check history; every gate-chain render shows the verdict's
     provenance. Absent Guardrail = the built-in deny-all/local state,
     shown honestly.
   - **Engram (memory)**: a memory status/pane showing the backend
     (engram vault vs built-in store), search-hit provenance in the
     stream (hits tagged with their source), and the degradation state
     when the daemon is down.
   - **Linked to the web surfaces**: affordances that open the companion
     web surfaces (Guardrail console, Engram vault) — design the
     affordance; URLs come from env config with public-site defaults.
     The TUI links, it does not embed.
   - Both companions are **recommended, never required** — design both
     states: with-companions (native, live) and without (built-in
     equivalents, no error styling for absence).
5. **The #167 wizard and the #165 boot share this identity** — extend
   the design to the first-run profile wizard (workspace → LLM endpoint →
   optional Guardrail policy URL → optional Engram memory URL) and the
   boot banner as secondary surfaces, same language.
6. **Mockup deliverable**: an HTML preview page in the landing aesthetic
   showing the TUI end-to-end: boot → a task with tool calls through the
   chain → an approval card → result + cost line; plus ASCII mockups for
   exact spacing of the chain and the approval card.

## What NOT to do

- No new dependencies; design only against raw ANSI.
- No changes to policy/gate/approval semantics; no auto-approve defaults.
- Don't touch the Rust workspace — you design, the coding session
  implements.
- No private-GitHub links in the deliverable; company links to
  elai-intelligence.com only.
- Don't redesign the palette tokens or the `[tag]` vocabulary — extend,
  don't replace.

## Acceptance

- Every designed element is implementable in raw ANSI by one coding
  session, no new crates.
- The identity reads Amparo, not Claude Code — the gate chain, the
  approval card, and Amparo█ carry it.
- Mockup matches the family tokens and the scientific voice.
- Conventions adopted where standard; distinct exactly where Amparo is
  (the chain, the gate, the voice).
