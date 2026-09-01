# Amparo TUI — design spec

Status: design (2026-08-31). For: `amparo tui` (#166), the first-run wizard
(#167), and the boot banner (#165). Implementable in raw ANSI by one coding
session, no new crates. This spec extends — never replaces — the palette
tokens (`web/public/style.css`), the `[tag]` event vocabulary
(`crates/amparo-agent/src/events.rs`, `format_event`), and the scientific
voice (`docs/m6-controlled-growth.md` §7).

## Identity pillars

1. **The gate chain is the spine.** Every tool call renders its passage
   through `registry → trust ceiling → policy → human` as four symbols that
   light up as each verdict really arrives. This is Amparo's answer to the
   spinner: structure, not shimmer.
2. **The approval moment is the hero.** The human gate is the product's
   centerpiece; the approval card is the most crafted element on screen.
3. **The scientific voice.** `[tag]` status lines, restraint, no
   marketing-speak. Color carries verdict, tags carry category — never a
   rainbow of per-tag colors.
4. **Presence.** Amparo feels inhabited through behavior — voice, attention,
   memory, initiative, manners — never through animation. Motion means
   something changed. (See "Presence behaviors".)

## The chain grammar

### Symbols

One mnemonic symbol per link, plain Unicode (font-safe over ssh, no Nerd
Font dependency):

| Symbol | Link | Mnemonic |
|---|---|---|
| `◆` | registry | the diamond on the roster — the tool must be registered or it does not exist |
| `▲` | trust ceiling | an upper bound — the tool's tier must sit under the ceiling |
| `§` | policy | the section sign, the typographic mark of law — the engine must allow this exact call |
| `◉` | human | the eye — a human approves what the first three cannot settle |

### States

Color carries the verdict; the symbol carries the link. Four states only:

- **green** — passed
- **amber** — escalated (policy hands off) / pending (awaiting the human,
  bright amber; pulses bright↔normal on the countdown tick, one-char redraw)
- **red** — denied. "This link said no." A human denial is red too: a
  decision, not an absence.
- **dim** — never evaluated ("not reached" when an upstream link denied;
  "not configured" when a link has no config). Dim never means "skipped."

### The key

Printed once in the boot banner and re-printable via `/key`:

```
[key]   pass · escalate · denied · not reached ── the symbols light up as each verdict arrives
```

(color each word in its state color.)

### The gutter row (always-on)

Every tool call is one row, chain symbols leading, all-green for the
auto-passed majority. The uniform green baseline is what makes an amber or
red symbol pop. The gutter row replaces separate `[call]`/`[exec]`/`[gate]`
lines for tool calls; `[tag]` lines remain for everything that is not a
tool call (`[task]`, `[turn]`, `[qc]`, `[bus]`, `[complete]`, …).

```
◆▲§◉  read_file src/rollback.rs                        42ms
◆▲§◉  search_code "rollback spec"                      18ms
◆▲§─  edit_file src/hints.rs                  escalated
◆▲§◉  edit_file src/hints.rs                           96ms
◆▲§◉  run_command rm -rf build/    policy: destructive flags
```

(in the real render: row 3 shows amber `§` + bright-amber `◉`; the last row
shows red `§` and dim `◉`, with the deny reason in red.)

Symbols begin dim and light up in real time, driven by actual gate events
(`ToolGate`, `ApprovalRequested`, `ApprovalResolved`) — one symbol colored
per event, no fake animation. Live for every call, not just approvals.

Sub-agent rows indent two spaces per delegation level under the `[spawn]`
line, gutter included — indentation names the chain (`sess-481.1`).

## Boot banner

The banner is the definitive explanation of how Amparo works. The ASCII art
is the mechanism itself — the chain diagram is Amparo's logo, drawn once at
full size and echoed in miniature by every gutter row afterward. Re-printable
on demand via `/chain` (or asking "how do you work?").

```
Greetings! My name is Amparo, built by EL AI Intelligence.
[wake] Amparo is awake.

Every action I take passes one chain. No action exits it.

   ◆ registry  ──▶  ▲ trust ceiling  ──▶  § policy  ──▶  ◉ you

   ◆  the tool must be registered, or it does not exist
   ▲  its tier must sit under your trust ceiling
   §  the policy engine must allow this exact call
   ◉  you approve whatever the first three cannot settle

Deny by default. Fail closed at every link. A model is never the approver.

[key]   pass · escalate · denied · not reached
[chain] ◆ registry: 14 tools  ·  ▲ ceiling: tier-3  ·  § policy: guardrail (enforce)  ·  ◉ human: tier ≥ 2
```

Copy rules: the link descriptions are statements of fact, not features; the
human link's line is the only one in bright fg; the creed line stands alone
as the conclusion. Symbols in the diagram render in accent blue at full
brightness.

The `[chain]` line reports the live configuration in the chain's own
grammar. Companion honesty is a value in a field, never error styling:

- `§ policy: guardrail v0.9.2 (enforce)`
- `§ policy: guardrail v0.9.2 (audit — verdicts advisory)` — "audit" in
  amber; advisory verdicts are a trust-relevant condition
- `§ policy: built-in (deny-all default)` — same register, no red, no
  warning iconography; a missing companion is a configuration, not an error

## The approval card

The hero element. A `warn`-colored left rule (`▐`), not a box — boxes break
on resize in a scrolling stream; a left rule survives everything. The chain
expands horizontally here, the one place it gets space, with dim dashes for
the traveled path and the pending link in bright amber:

```
▐ approval required — edit_file src/hints.rs
▐   ◆──▲──§──◉  policy escalates → human decides
▐   why:      tier-2 write; policy escalate: src/ path
▐   blast:    writes 1 file (src/hints.rs, +14 −3 lines)
▐   rollback: restore hints.rs.amparo-bak
▐
▐   approve? [y/N]  47s ▓▓▓▓▓▓░░░░ ── deny on timeout
```

- The countdown is a **draining meter**, redrawing once per second. The
  copy names the default ("deny on timeout"), not just the time — fail-closed
  is visible at the moment it matters.
- Resolution lines are first-person (see Presence): `[approval] you granted
  this — proceeding` / `[approval] you denied this — I won't touch it`.
- The bell (`BEL`) rings at the approval request — and for nothing else.
  The one audible ping means exactly "a human is needed."
- `Ctrl+C` during a pending approval **equals deny**. Fail-closed holds
  under signal; otherwise Ctrl+C is a policy bypass.
- Single-keypress `y`/`N` (no Enter); default is deny; timeout is deny.

## Input line

The one permanent line; the instrument panel rides here.

- Prompt: `›` in accent blue. No `amparo>` — the banner already said the
  name; repeating it every line is marketing, not voice.
- `Amparo█`: the blinking block cursor (DECSCUSR) lives here at rest. The
  brand moment is the prompt, not a redraw loop.
- **Right-aligned status** (RPROMPT-style, dim), redrawn with the line:
  `◆▲§◉ · enforce · engram · ~$0.04`. All-green when healthy; deviations
  recolor the owning symbol live (`§` amber on audit mode; `engram` →
  `built-in` dim on daemon loss). Running session cost is always visible —
  spend is trust-relevant.
- ↑↓ history, standard line editing, bracketed paste on (a pasted multi-line
  command never auto-executes into N approval cards).
- **Ctrl+C semantics**: idle at prompt → exit; task streaming → first press
  cancels the task, second within ~2s exits; approval pending → deny.
- Width < ~72 cols: drop the right-aligned status (keep the gutter and
  `[chain]` boot line); never wrap the chain.

## Status, tasks, and the stream

- **No persistent status bar, no scroll regions, no alternate screen.**
  Inline rendering only; the stream scrolls naturally. Scroll-region tricks
  glitch over tmux/ssh, and a glitched trust UI reads as an untrustworthy
  product.
- Status prints at boot and **on change** (`[chain]` re-prints mid-stream
  when policy mode, ceiling, or memory backend changes — state changes are
  events in Amparo's vocabulary).
- **Task hairlines** separate tasks, the spec-register aesthetic:

```
──────── sess-481 ──────────────────────────── enforce · engram ──
```

- **Thinking indicator**: while waiting on the model, a dim elapsed counter
  (`12s`) ticks where the `[turn]` line will land. No animated dots, no
  cute verbs. Stillness means model; motion means gates.
- **Cost line** renders in italic dim — the estimate register, matching its
  own copy: `~$0.04 in inference (estimate, chars/4, $3/1M tokens)`.
- No timestamps in the stream; `--verbose` adds them.
- Redraws wrap in synchronous-update mode (DEC 2026) where supported;
  no-op elsewhere.

## Companion surfaces — panes as reports

There are no fixed panes in a streaming REPL; a "pane" is a command that
prints a status block into the scroll. `/policy` and `/memory` produce ruled
blocks that scroll away; OSC 8 hyperlinks at the foot point at the web
consoles (the TUI links, it does not embed; URLs from env config with
public-site defaults).

```
› /policy
── policy ─────────────────────────────────────────────────
  engine:   guardrail v0.9.2 · enforce
  source:   https://guardrail.elai-intelligence.com
  recent:   § allow     read_file                    41s ago
            § escalate  edit_file src/hints.rs       2m ago
            § deny      run_command rm -rf build/    3m ago
───────────────────────────────────────────────────────────
```

Without the companion, same grammar, same register, no error styling:

```
── policy ─────────────────────────────────────────────────
  engine:   built-in · deny-all default
  source:   local policy.toml
  note:     connect a guardrail engine for managed policies
            https://guardrail.elai-intelligence.com
───────────────────────────────────────────────────────────
```

Per-verdict provenance also rides inline at the decision point:
`§ deny (guardrail: no-destructive-exec)` vs `§ deny (built-in)`. Memory
search hits tag their source likewise. Degradation (daemon down) reports as
the built-in equivalent in dim, never as failure.

## Session resume picker

Numbered list, most recent first; ↑↓ + Enter, Esc cancels, digit keys
select directly; capped at 9; no fuzzy search. Selected row gets the accent
`›` and fg color; all else dim. **Cost per session is shown** — resuming is
choosing to spend, and the estimate register belongs at the decision point.

```
[session] resume which session?
  1  sess-481   tighten the rollback hints        2h ago   ~$0.04
  2  sess-477   audit the policy fixtures         1d ago   ~$0.31
› 3  sess-472   migrate vault schema              3d ago   ~$1.12
```

## Presence behaviors

Amparo feels alive through behavior, not animation. Rate-limited, dim,
factual — the lab-assistant register: quiet, attentive, remembers
everything, speaks when it matters, never performs.

1. **Attention — focus reporting.** DEC mode 1004 tells Amparo when its
   window gains/loses focus. On return after a notable absence (>15m), one
   dim line: `[away] you were gone 26m — nothing happened`, or a briefing
   of the `[schedule]`/`[bus]`/`[notification]` lines that arrived while
   unfocused. Perception is bounded to the terminal channel on purpose:
   Amparo knows you left the window, not what you looked at. (Screen
   perception is a confirmed exclusion — `docs/axiom-carryover-screen.md` —
   and stays out.)
2. **Manners — polite interruption.** Mid-keystroke, non-urgent events hold
   until submit or a ~2s typing pause; approval requests interrupt
   immediately. It doesn't talk over you.
3. **Voice — first person at the moments of relationship.** `[tag]` lines
   stay canonical; resolutions and summaries speak as "I"/"you":
   `[approval] you granted this — proceeding`, `[complete] done. 3 files
   read, 1 written`.
4. **Memory — continuity.** Boot with `--resume` or Engram connected adds
   one dim line: `[memory] last we spoke: audit the policy fixtures —
   completed, 2 days ago`.
5. **Pulse — the cursor.** The blinking block is the only idle animation.
   Cursor shape/color reports mode: steady block idle, bar while streaming,
   amber underline while an approval waits (DECSCUSR + OSC 12).
6. **The title follows.** Window title carries mode:
   `amparo · sess-481 · idle` / `running: edit_file` / `awaiting approval`.
   Restored on exit.
7. **Anticipation — memory that speaks first.** At task start, a confident
   Engram match surfaces unprompted, provenance-tagged, one dim line:
   `[memory] sess-472 touched this — the spec markers bit us last time`;
   its failure-mode twin: `[note] this failed the same way in sess-477`.
   Strict noise budget: max one proactive line per task, dim, skippable,
   only on a confident match. This is the deepest companion cue — it
   remembers what happened to us — and it stays inside the scientific voice
   because every line carries evidence.

The test that separates companion-feel from companion-product: a companion
product asks for attention; a companion feel earns it by being useful when
you look. No simulated affect, no engagement bait, no avatar, no typing
theatrics — every line Amparo speaks carries evidence or it isn't spoken.

## ANSI token mapping

Truecolor primary; 256-color nearest (verify with a color-check script at
implementation time); 16-color fallback maps to the basic set. Never paint
the background — fg-only tokens degrade gracefully over ssh/tmux theme
mismatches.

| Token | Truecolor | 256 (nearest) | 16-color fallback |
|---|---|---|---|
| fg | `#e2e8f0` | 254 | white |
| dim | `#94a3b8` | 103 | bright black (SGR 2 default) |
| accent | `#3b82f6` | 69 | blue |
| ok | `#10b981` | 36 | green |
| bad | `#ef4444` | 203 | red |
| warn | `#f59e0b` | 214 | yellow |
| border (mockups) | `#232c3a` | 237 | bright black |

Rules: bold only for emphasis words (approval title, human link in the
banner); SGR 2 is the dim register; SGR 3 italic only for the estimate
register. Honor `NO_COLOR` and `TERM=dumb` (plain lines, no escapes). When
piped, degrade to the same lines with zero escapes — the output must read
clean in a log file. No emoji anywhere.

## Wizard (#167) and boot (#165)

Same language, secondary surfaces: the left-rule card grammar, the chain
symbols, dim/bright register. Four steps — workspace → LLM endpoint →
optional Guardrail policy URL → optional Engram memory URL — each step a
ruled block, optional steps marked `recommended, never required`, summary
echoes the `[chain]` line grammar at the end.

## Web surfaces — the same presence grammar

The operator console (`web/public/index.html` + `app.js`) adopts the same
presence behaviors translated to a medium with visit boundaries instead of
focus events. No second personality: same voice, same symbols, same
evidence discipline.

1. **A greeting line, not a header.** The console opens with Amparo's
   current state in first person: "Amparo is awake. 2 approvals waiting, 1
   schedule due at 14:00. Nothing failed since your last visit." Every
   clause is a link to the thing it asserts.
2. **The `[away]` briefing, ported.** The console knows the last visit; the
   landing view opens with "while you were away" — a digest of events since.
3. **The approval queue is the hero of the page.** Pending approvals sit at
   the top with preflight blast radius and countdown — consistent with the
   approval-card-as-hero pillar. The console exists so the human link can
   do its work; the layout says so.
4. **The chain diagram is the brand header** on console and landing, so the
   `◆▲§◉` grammar is learned once and read everywhere — terminal, console,
   landing hero. One language, three surfaces.

## What NOT to do

- No new dependencies; raw ANSI only. Confirm the input-mode mechanism
  (raw-mode termios for single-keypress y/N, width detection) against the
  existing dependency tree before implementation.
- No scroll regions, no alternate screen, no mouse requirement.
- No changes to policy/gate/approval semantics; no auto-approve defaults.
- No per-tag rainbow: color carries verdict, tags carry category.
- No idle animation beyond the cursor blink; no gradients, no shimmer.
- `amparo run` stdout purity is untouched — the TUI is its own surface.
- Company links to elai-intelligence.com only.

## Acceptance

- Every element above is implementable in raw ANSI in one coding session.
- The identity reads Amparo, not Claude Code: the chain gutter, the
  approval card, `Amparo█`, and the presence behaviors carry it.
- Conventions adopted where standard (streaming, ↑↓ history, Ctrl+C
  semantics, one task at a time); distinct exactly where Amparo is: the
  chain, the gate, the voice, the presence.
