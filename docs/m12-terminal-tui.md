# M12 — interactive terminal interface + awakened boot (design)

Status: **design only — nothing in this document is implemented.** Written
2026-08-31. Implementation starts after the pre-reveal security audit closes
its CRIT/HIGH findings.

## Why

`amparo run` is scriptable and one-shot. The ask (2026-08-31): a Claude Code /
Kimi-style interface for the terminal — a session you stay inside: streamed
replies, tool calls visible as they execute, approvals answered in place,
history and resume.

## The surface is not a trust boundary

The TUI is a new **surface** for the existing loop, exactly like the chat
drivers (telegram/discord/slack). Nothing about the gate chain changes:

- registry &rarr; trust ceiling &rarr; policy engine &rarr; human approval, unchanged;
- approvals remain human-only, fail closed in 60 seconds (the TUI renders the countdown);
- the policy engine's verdicts render as `[policy]` lines, including audit-mode surfacing;
- the ledger still records every consequential act — the TUI shows `[ledger]` lines;
- I1 holds: the TUI never auto-tunes policy, gates, or prompts. No auto-approve
  defaults, no "trust this session" memory.

## Entry points

- `amparo` (no subcommand) &rarr; the TUI. This replaces today's bare-usage
  print. (v0.10.0 prints USAGE; the change is deliberate — the installer's
  next-step line gains `amparo` alongside `amparo --version`.)
- `amparo tui` &rarr; same, explicit.
- `amparo run &hellip;` &rarr; unchanged one-shot; `amparo mcp-serve`, `chat`,
  `skill`, `notebook`, `privacy`, `doctor`, `schedule` unchanged.
- `amparo --resume <session-id>` / `amparo tui --resume` &rarr; re-enter a
  session (history + context read from the session store).

## The awakened boot

On first launch after install — and only once — Amparo powers on before
accepting input:

```
amparo 0.11.0
[spec]   gate chain: registry -> trust ceiling -> policy engine -> human approval
[spec]   approvals: human-only · dangerous acts fail closed in 60 s
[spec]   policy:    Guardrail wire engine (or built-in deny-by-default)
[spec]   memory:    Engram-native, falls back to built-in store
[spec]   ledger:    append-only counts, never values
[ledger] 0 acts recorded so far
Amparo is awake. Every consequential act I take will pass the gate chain
and leave a ledger entry. Bring your own LLM.
```

The sequence is earned, not decorative: each `[spec]` line reports the live
configuration (LLM endpoint configured? policy URL reached? memory backend?),
so the greeting doubles as a config readout. Played once per workspace (marker
in the workspace config), replayable with `amparo --boot`.

If no LLM is configured, the boot ends in the first-run wizard instead of the
prompt (the sign-up wizard workstream — docs to follow).

## Loop UX

- Prompt `amparo ▸` with streamed replies (streamed as model tokens arrive,
  not buffered).
- Tool calls render as rows with live status:
  `[tool] name · queued -> policy verdict -> approved/denied -> running -> result`
  — the same lifecycle the notebook records.
- Approval: inline prompt showing call_id, tool, arguments, reasons, blast
  radius, rollback hint — `approve? [y/n] (auto-deny in 42s)` with a live
  countdown. Escalate renders as `[policy] escalate — human approval required`.
- `[tag]` lines keep the scientific voice (existing event tags + `[tui]` for
  surface events).
- Audit mode: the one-time stderr notice from M9 W3 prints here on first
  `enforced:false` — "policy engine is in audit mode; verdicts are advisory".
- `/help`, `/history`, `/resume`, `/exit`; Ctrl+C during streaming interrupts
  the model call and saves the session (never a bare process kill with lost
  ledger state — the ledger appends per act).

## Rendering

- Raw ANSI escape codes hand-rolled — **no ratatui/crossterm dependency** (no
  new heavyweight deps; terminal behavior is pinned by test against a captured
  ANSI sequence, the same discipline as the installer's ASCII-only rule).
- TERM detection: on a TTY, the full interactive loop; on a pipe/non-TTY,
  `amparo` falls back to printing USAGE (so scripts keep working).
- History: in-memory per session + persisted via the session store; arrow-key
  history via raw mode, best-effort across terminal emulators (documented
  honestly).
- Windows console: `amparo tui`/`amparo code` run full-screen on Windows
  Terminal and the legacy console host — VT processing enabled on stdout,
  quick-edit mode cleared for the session (restored on exit, so a stray
  click can't freeze the surface), keystrokes read as UTF-16 console
  records. Focus and bracketed-paste events don't exist on the console API:
  focus never fires (optional today) and a paste arrives as key bursts —
  both handled as on unix. No `chcp`: the codepage is cosmetic (input is
  UTF-16 records) and is never changed.
- Secrets: key values are never rendered in boot output or `[tui]` lines; tool
  arguments render as the user would see them in their own shell (local
  surface, local disclosure — but the ledger never stores them; unchanged).

## Test plan

- e2e: scripted TTY input (mock LLM) — boot readout once-only, streamed reply,
  tool row lifecycle, approval countdown auto-deny at 60s, `/history`,
  `--resume` re-entry.
- Gate chain unchanged: the TUI path dispatches through the same preflight as
  `run` — regression suites run unmodified.
- Non-TTY fallback prints USAGE, exit 0.
- Both gates (`cargo test --workspace`, `cargo doc --workspace --no-deps`)
  green, zero warnings.

## Risks

- Terminal-emulator variance (ANSI rendering, arrow keys) — mitigation:
  best-effort history, pinned captured-sequence tests.
- Ctrl+C mid-approval must not skip the gate — interrupt cancels the call,
  never converts to allow.
- Scope: no full-screen multi-pane panels in v1; that is a later iteration if
  asked.

## Version

M12 ships as v0.11.0 (implementation weeks TBD after the audit).
README/CHANGELOG/VERSIONING updates land in the release week, per convention.
