# Amparo

> **Spanish** — protection, shelter, refuge. In Spanish and Latin American law,
> a *recurso de amparo* is the action you file to protect a right.

An open agent that acts under policy. Bring your own LLM.

---

## Status: pre-alpha, M2 landed — the loop and the MCP surface are in

This repository was created on 2026-08-27. **Milestone 1 is in** (the
BYO-LLM provider layer) and **Milestone 2 is in**: the agent loop runs on
native `tool_calls` behind the policy gate, and MCP is first-class — the
server exposes the registry over Model Context Protocol and the client mounts
external MCP tools as ordinary registry tools, both behind the same gate
chain. Next is an install path, a release, and API stability.

If you are reading this expecting to run something, come back after Milestone 3.

### What exists today: the crate set

- **`amparo-inference`** (M1) — one trait (`InferenceProvider`), two providers:
  **`OpenAIProvider`** (any OpenAI-compatible endpoint — Ollama, vLLM,
  OpenRouter, Together, Groq — including an Ollama-native `/api/chat` branch)
  and **`AnthropicProvider`** (the native Anthropic Messages API, translated to
  the same OpenAI-shaped contract, including `tool_use`/`tool_result`
  translation and SSE streaming). Fail-closed by construction: no silent
  localhost default, per-request timeouts plus a stream idle timeout, a
  `max_tokens` clamp, and an optional model allowlist enforced at build time.
- **`amparo-agent`** (M2d) — the loop, ported from Axiom's `run_agent_task`
  and switched from text-parsed ReAct to native `tool_calls`. The gate chain
  Amparo owns is the deny-by-default seam: **trust ceiling → policy gate →
  human approval**, and every tool call — executed or blocked — gets a
  tool-role answer carrying its `tool_call_id`. Loop mechanics preserved from
  Axiom: max steps + conversation trimming, parallel tool batches with retry
  ×2, the one-shot shortcut, the same-tool loop guard, empty-turn recovery,
  and VERIFIED/INCOMPLETE self-verification. Events flow through an
  `EventSink` seam; approval gates default to auto-deny; privacy is enforced
  per turn with Secure Minions PII strip/restore (per-message placeholder
  namespaces), and nudge/verification messages are stripped too.
- **`amparo-mcp`** (M2e) — MCP first-class on both sides. `McpServer`
  speaks JSON-RPC 2.0 over stdio (`initialize`, `tools/list`, `tools/call`,
  `ping`); the policy engine is a required constructor argument and every
  call runs the same gate chain as the loop, with auto-deny approval by
  default. `McpClient` spawns a server process, handshakes, and
  `mount_into`s its tools as registry executors at `ExternalEffector` by
  default, so remote tools cannot skip the approval gate. Ships the
  `amparo-mcp-serve` binary (`--policy-url`, `--allow-all`,
  `--auto-approve`, `--trust-ceiling`).
- **`amparo-policy`** (M2b) — the policy seam (`PolicyEngine`) with a
  deny-all default and `WirePolicyEngine`, a client for the open policy-check
  wire protocol (`POST /check {tool_name, target} → {verdict, reason,
  enforced}`). Guardrail is a commercial implementation of that protocol;
  anyone can write another.
- **`amparo-tools`** (M2c) — the registry and the portable tool set (web,
  filesystem, shell, git, tests, build, memory), each with a trust tier that
  drives the approval gate.
- **`amparo-memory`** (M2a) — the memory interface with a built-in default
  store. Engram is the recommended backend; it is never a dependency.
- **`amparo-privacy`** (M2a) — privacy policy evaluation, blocked/allowed
  domain routing, and the Secure Minions PII strip/restore primitives the
  loop uses.

```sh
cargo test --workspace   # the gate
```

## What Amparo is meant to be

An agent that runs a real tool-use loop — shell, files, git, web, tests — where
**every tool call passes a policy check before it executes**, and where the
model driving the loop is yours to choose.

Three commitments shape the design:

**Bring your own LLM.** Anthropic, OpenAI, or any OpenAI-compatible endpoint
(Ollama, vLLM, OpenRouter, Together, Groq). No bundled model, no required
sidecar, no vendor with a privileged position in the loop.

**Policy is an interface, not a product.** The gate between "the model decided
to do this" and "this ran" is an open interface with an open wire protocol.
Amparo ships a default engine; [Guardrail](https://elai-intelligence.com) is a
commercial implementation of the same interface. Anyone can write another. The
default is **deny**, not allow — an agent whose policy engine waves everything
through is worse than one with no policy engine, because it looks safe.

**Engram is recommended, never required.** Memory is an interface with a
built-in default store; [Engram](https://github.com/El-AI-Intelligence/Engram)
is the recommended memory backend — durable, private, syncable across devices —
but Amparo runs without it. Amparo must never hard-depend on a memory product,
its own or anyone else's.

**Deployable anywhere.** A standalone server, a container, a systemd unit, a
chat bot. Not welded to a desktop session, not dependent on a GUI.

## Roadmap

| # | Milestone | State |
|---|---|---|
| 1 | Provider abstraction — Anthropic + OpenAI-compatible | ✅ done |
| 2 | Native tool calling (replacing text-parsed ReAct) | ✅ landed — loop + MCP client/server, 164 tests green |
| 3 | Headless operation — screen/desktop tools become optional | not started |
| 4 | Chat adapters — Telegram first, then Discord and Slack | not started |
| 5 | Multi-tenant identity and per-user policy | not started |

Milestones 1–3 are the product. 4 is small once 1–3 exist. **5 gates giving
this to anyone but yourself** — a shell-executing agent behind a chat bot is a
security boundary, and until per-user identity and sandboxing land, the only
safe operator is the person who owns the machine.

## Provenance

The agent loop was extracted from
[Axiom-OS](https://github.com/PixelPhantomAI/Axiom-OS) (MIT), which contains a
working ReAct loop with tool retry, self-verification, and conversation
trimming. What did *not* come across: the desktop compositor, screen ingestion,
the companion loop, and the ELLM proxy coupling — see [NOTICE](NOTICE).

Amparo is Apache-2.0 rather than MIT for the explicit patent grant, which
matters more than usual for software that executes arbitrary code.

## License

Apache License 2.0 — see [LICENSE](LICENSE).

Copyright 2026 EL AI Intelligence, LLC.
