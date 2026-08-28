# Swarms and the advanced-system menu

Status: exploration (2026-08-28). Inputs: the Axiom-OS carryover screen
(`axiom-carryover-screen.md`), the Hermes comparison in the feature-set
page, and the standing UX directives. Proposal, not implementation.

## Objective

What "swarms" should mean for Amparo, and which advanced-system features
earn a place in a policy-first, privacy-first, headless agent — plus
which of the field's favorites are refused and why.

## The one rule: no member exits the gate chain

A swarm is N agents. In Amparo, every one of them is the *same* agent
loop, with the *same* gate chain, and the same audit posture: registry
lookup → trust ceiling → policy engine → human approval, per tool call.
Two corollaries follow from that rule, and they decide everything else:

1. **Spawning an agent is a tool call.** A sub-agent begins as
   `spawn_agent` through the registry — policy-checked, approval-gated,
   logged. There is no path from "the parent decided it was useful" to
   "a new agent exists" that does not pass the gate. (Axiom's
   orchestrator spawns sub-agents as an internal capability; that is the
   gap Amparo closes.)
2. **Delegation is not exemption.** A parent cannot hand a sub-agent
   work it could not do itself: the child's calls pass the same policy
   checks, tagged with a session id that names the chain of delegation
   (`operator → task → sub-agent`). Nothing a swarm does is less
   auditable than what a single agent does; it is *more* auditable,
   because the delegation itself is a logged event.

And one refusal, stated outright: **a model is never the approver.**
Supervisor patterns in which an LLM approves another agent's actions on
the human's behalf move the trust boundary into the least trustworthy
place. In Amparo the operator is the supervisor; automatic
"supervisor agents" are excluded, not as a gap but as a design choice.

## The screened menu

| Feature | Verdict | Notes |
|---|---|---|
| **Sub-agents** (`spawn_agent` tool) | **Core (M8 candidate)** | Child = same loop + gate chain; task-scoped workspace (existing per-tenant machinery); report back through the parent's event stream; delegation recorded in the session id |
| **Scheduling** (`schedule` tool + a daemon-side queue) | **Core (M8 candidate)** | Scheduling is a *promise*, not an execution: a scheduled task re-enters the gate chain when it fires, with its original requester as the session. Firing while nobody is present → the task runs to the approval gate and waits (or auto-denies on the timeout), never silently ahead of it. Amparo's first background loop; the pattern reference is Axiom's epistemic scheduler, the substance is new |
| **Coordination substrate** (event bus / blackboard) | **Defer** | The SQLite-audited pub/sub from `axiom-eventbus` is the right eventual shape for many agents sharing state; `EventSink` suffices until sub-agents exist. Adopt when the second real consumer appears |
| **Supervisor agents** (LLM-as-approver) | **Excluded** | Above |
| **Nexus intention/autonomy spaces, momentum, agenda-argue, curiosity drives** | **Excluded** | Unobservable goals and persona theater: an agent that reports "curiosity" is describing inner states it does not have. §7 of `m6-controlled-growth.md` forbids exactly this register |
| **Imagination/dreaming, proactive reach-outs** | **Excluded** | Background actions with no requester break the attribution chain (who asked for this, and who approves it?) — the companion-loop mistake again |
| **Device mesh / fleet** (multi-host quorum, handoff) | **Later** | Amparo is per-host; devices already reach one agent through the chat surfaces. Revisit only if a real multi-host use case shows up |
| **Voice in/out** | **Later** | Voice *in* is a chat-surface adapter problem (Telegram voice notes exist); voice *out* is TTS. Neither changes the loop; both are surface work |
| **Web UI** | **Excluded** | Headless is a commitment: the faces are the chat surfaces and the MCP server. A UI that talks to the MCP surface is the escape hatch, and it is anyone's to build |
| **Notifications** | **Core-adjacent** | Already directive-level: phone is a chat surface; progress and approvals already travel to it. `send_notification` (Axiom had one, headless-usable) is a small registry addition |
| **Privacy ledger, preflight, session persistence** | **Core (M7)** | From the carryover screen — the instrumentation layer that makes the advanced system *legible* |

## Resource honesty

A swarm multiplies inference, and the voice (evidence over claims) must
extend to cost. Two defaults: a **swarm budget** (maximum sub-agents per
task, configurable, fail-closed at the limit) and a **cost line in every
report** — "this task used 3 sub-agents, 41 tool calls, ~$0.04 in
inference." The budget is the environmental commitment made mechanical;
the cost line is the scientific tone applied to resources. Neither is
optional polish: an "advanced system" that hides what it burns is a
fancier black box, not a better one.

## What the operator sees

One operator, one gate, N agents — and the whole swarm's traffic on the
existing instrument panel: approvals arrive per member with the
delegation chain in the message ("sub-agent of task 3 wants to run
`git push`"), denials name the member and the reason, and the final
report reads like the others do — method, observations, conclusion,
open items — with the swarm breakdown as a section of the method.

That is the advanced-system feel: not a personality, not a hive mind —
an *observatory*. The user runs a small instrumented research operation,
and every instrument reports.

## Landing

Proposed roadmap rows (proposal only; the README stays untouched until
the user decides):

- **M7 — instrumentation & hardening:** privacy ledger, session
  persistence, preflight blast-radius classification (`axiom-carryover-screen.md`).
- **M8 — sub-agents & scheduling:** `spawn_agent` and `schedule` behind
  the gate chain, delegation chain in session ids, swarm budget + cost
  line, the event bus adopted when coordination needs it.
