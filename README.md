# Amparo

> **Spanish** — protection, shelter, refuge. In Spanish and Latin American law,
> a *recurso de amparo* is the action you file to protect a right.

An open agent that acts under policy. Bring your own LLM.

---

## Status: pre-alpha, nothing to run yet

This repository was created on 2026-08-27 and does not yet contain a working
agent. The sections below describe what is being built and where the code is
coming from, not what exists here today. There is no install path, no release,
and no API stability.

If you are reading this expecting to run something, come back after Milestone 2.

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

**Deployable anywhere.** A standalone server, a container, a systemd unit, a
chat bot. Not welded to a desktop session, not dependent on a GUI.

## Roadmap

| # | Milestone | State |
|---|---|---|
| 1 | Provider abstraction — Anthropic + OpenAI-compatible | not started |
| 2 | Native tool calling (replacing text-parsed ReAct) | not started |
| 3 | Headless operation — screen/desktop tools become optional | not started |
| 4 | Chat adapters — Telegram first, then Discord and Slack | not started |
| 5 | Multi-tenant identity and per-user policy | not started |

Milestones 1–3 are the product. 4 is small once 1–3 exist. **5 gates giving
this to anyone but yourself** — a shell-executing agent behind a chat bot is a
security boundary, and until per-user identity and sandboxing land, the only
safe operator is the person who owns the machine.

## Provenance

The agent loop is being extracted from
[Axiom-OS](https://github.com/PixelPhantomAI/Axiom-OS) (MIT), which contains a
working ReAct loop with tool retry, self-verification, and conversation
trimming. What is *not* coming across: the desktop compositor, screen ingestion,
the companion loop, and the ELLM proxy coupling.

Amparo is Apache-2.0 rather than MIT for the explicit patent grant, which
matters more than usual for software that executes arbitrary code.

## License

Apache License 2.0 — see [LICENSE](LICENSE).

Copyright 2026 El AI Intelligence, LLC.
