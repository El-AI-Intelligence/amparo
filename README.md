# Amparo

> **Spanish** — protection, shelter, refuge. In Spanish and Latin American law,
> a *recurso de amparo* is the action you file to protect a right.

An open agent that acts under policy. Bring your own LLM.

---

## Status: pre-alpha, M6 in progress — M6a (lab notebook) + M6b (case library) + M6c (gated skills) landed

This repository was created on 2026-08-27. **Milestone 1 is in** (the
BYO-LLM provider layer), **Milestone 2 is in** (the agent loop on native
`tool_calls` behind the policy gate, MCP first-class in both directions),
**Milestone 3 is in** (the `amparo` binary installs with
`cargo install --path crates/amparo-cli`, drives the loop end-to-end from
the command line, and the workspace carries a versioned release with a
documented API-stability policy), **Milestone 4 is in**: Telegram,
Discord and Slack chat adapters behind one transport seam, with
inline-button approval and a fail-closed operator allowlist,
**Milestone 5 is in**: a TOML tenant directory with per-user policy
checks, per-user trust ceilings, per-user workspace directories, and
requester-only approval presses, and **Milestone 6a is in**: the lab
notebook — with `--growth`, every completed or failed task is recorded
as a PII-stripped, tenant-tagged run record (off by default).

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
- **`amparo-chat`** (M4) — the chat adapter layer: one `ChatTransport`
  seam, a per-task `ChatDriver` (allowlist, one task per chat, panic-proof
  task boundary), an `ApprovalRouter` for inline-button presses, a
  `ChatApprovalGate` (inline Approve/Deny buttons, 60 s auto-deny), and
  hand-rolled transports for **Telegram** (long polling), **Discord**
  (gateway websocket) and **Slack** (Socket Mode).
- **`amparo-cli`** (M3, M4) — the one binary: `amparo run "task"` drives
  the loop end-to-end (fail-closed BYO-LLM env, interactive terminal
  approval, `--auto-approve`/`--auto-deny` overrides), `amparo mcp-serve`
  reuses the same implementation as the standalone `amparo-mcp-serve`
  binary (same help, errors, exit codes), `amparo chat
  telegram|discord|slack` serves the agent over a messaging platform,
  `amparo version` prints the version.

```sh
cargo test --workspace            # the behavior gate
cargo doc --workspace --no-deps   # the API-stability gate (missing_docs on every crate)
```

Both gates must pass with zero warnings — see [VERSIONING.md](VERSIONING.md).

### Quickstart

```sh
cargo install --path crates/amparo-cli   # or: cargo build --release
```

Environment surface (everything is optional except the two marked
**required**):

| Variable | Meaning |
|---|---|
| `AMPARO_INFERENCE_URL` | **Required.** Provider base URL, e.g. `http://localhost:11434/v1` or `https://api.anthropic.com` |
| `AMPARO_INFERENCE_MODEL` | **Required.** Model ID, e.g. `qwen2.5:14b` |
| `AMPARO_INFERENCE_KEY` | API key (empty for keyless local providers) |
| `AMPARO_INFERENCE_PROVIDER` | `openai` (default) or `anthropic` |
| `AMPARO_INFERENCE_TIMEOUT_SECS` | Request timeout (default 120, clamped 1–3600) |
| `AMPARO_INFERENCE_MAX_TOKENS` | Optional per-request `max_tokens` cap |
| `AMPARO_INFERENCE_MODEL_ALLOWLIST` | Optional comma-separated model allowlist |
| `AMPARO_WORKSPACE` | Directory the tools are confined to |
| `AMPARO_POLICY_KEY` | API key for a remote policy engine (with `--policy-url`) |
| `AMPARO_CHAT_TELEGRAM_TOKEN` | Bot token for `amparo chat telegram` |
| `AMPARO_CHAT_DISCORD_TOKEN` | Bot token for `amparo chat discord` |
| `AMPARO_CHAT_SLACK_APP_TOKEN` | Socket Mode app token for `amparo chat slack` (with `AMPARO_CHAT_SLACK_BOT_TOKEN`) |
| `AMPARO_CHAT_SLACK_BOT_TOKEN` | Bot token for the Slack Web API |
| `AMPARO_CHAT_ALLOWLIST` | Comma-separated user ids who may talk to the chat bot — absent or empty refuses everyone; ignored when a chat config is set |
| `AMPARO_CHAT_CONFIG` | Path to a TOML chat config (the tenant directory); the `--chat-config` flag wins |
| `AMPARO_CHAT_TELEGRAM_BASE` | Telegram Bot API base URL (self-hosted Bot API servers) |

```sh
export AMPARO_INFERENCE_URL=http://localhost:11434/v1
export AMPARO_INFERENCE_MODEL=qwen2.5:14b
amparo run "list the files and tell me what's there" --allow-all
```

Same agent, over Telegram (tokens come from the environment, never argv):

```sh
export AMPARO_CHAT_TELEGRAM_TOKEN=123456:ABC-DEF
export AMPARO_CHAT_ALLOWLIST=111222333      # your Telegram user id
amparo chat telegram --allow-all
```

In chat mode, approvals arrive as inline **Approve/Deny** buttons on the
approval message; unanswered approvals auto-deny after 60 s. Progress
lines mirror the terminal's `[tag]` format. `amparo chat discord` and
`amparo chat slack` work the same way with their `AMPARO_CHAT_*_TOKEN`
variables; without `AMPARO_CHAT_ALLOWLIST` the bot refuses every message.

Multiple users, each with their own policy scope — a TOML chat config
(`--chat-config`, or `AMPARO_CHAT_CONFIG`; the flag wins):

```toml
# tenants.toml — the tenant directory: only these users may start tasks
[users."telegram:111222333"]                # one operator, all defaults
[users."telegram:444555666"]
trust_ceiling = "observational"             # per-user ceiling (falls back
workspace = "team-b"                        #   to --trust-ceiling if absent)
```

```sh
amparo chat telegram --chat-config tenants.toml --allow-all
```

Each user's workspace is a directory under the workspace root
(`users/<platform>-<user_id>` by default; a profile `workspace` is a
relative subpath — absolute and `..` paths are rejected). Each task's
policy checks carry `session_id = "platform:user_id"`, so a wire policy
engine sees who asked — the raw platform id goes to the policy server
with every check, so the operator should choose an engine they trust.
The config file is read once at startup. While a config is set,
`AMPARO_CHAT_ALLOWLIST` is ignored. Approval presses are attributed:
only the user who started a task can decide it; anyone else pressing the
buttons gets a polite toast and the approval stays pending.

Deny-by-default: without `--policy-url` or `--allow-all`, every tool call is
refused — `--allow-all` is an explicit opt-in for local experiments. Calls
that need approval ask **y/N at the terminal** (60s timeout; closed or
non-terminal stdin auto-denies), with `--auto-approve`/`--auto-deny`
overrides. stdout carries the final answer only — progress, gate decisions
and the report go to stderr — so `amparo run` scripts cleanly.

## Controlled growth

The lab notebook (M6a) records how the agent actually behaves, so growth
is measurable and inspectable instead of silent. With `--growth` (on
`amparo run` or `amparo chat`; the last `--growth`/`--no-growth` wins),
every completed or failed task is appended as one JSON line to
`<workspace>/.amparo/notebook/records.jsonl`: the PII-stripped task text
(emails become `[EMAIL_1]`, nothing is recoverable — records are
archival), a hash of the tool sequence, the per-call gate log (decision,
reasons, escalation, approval, outcome), the verification outcome, a
truncated final answer, duration and a token-cost estimate. Each record
carries a tenant tag — `cli` for runs, `platform:user_id` for chat tasks
— so one notebook can serve many users. Recording is **off by default**:
without the flag, no record file is ever created. In chat mode the
workspace root is `AMPARO_WORKSPACE`, or the current directory when it
is unset (records land in `./.amparo/notebook/`).

With `--growth` the notebook also becomes the verification case library
(M6b): prior same-tenant records resembling the task are retrieved into
the self-verification prompt as read-only observations ("Prior cases in
this tenant…"), formatted as evidence, never as instructions, and never
shown to the action loop. Growth is write + read — one opt-in, and still
off by default.

`--growth` also enables gated skills (M6c). A skill is a named
procedure — preconditions, an ordered list of tool-call steps, an
expected outcome — managed with `amparo skill add|propose|list|show|
adopt`. Adoption runs the same gate chain as a tool call (a policy check
on `use_skill`) plus human approval showing the full step plan; the
`Proposer` distills recurring VERIFIED tool sequences from the notebook
into inert candidate proposals, but nothing is adopted automatically. At
execution the model may call `use_skill`, and the loop expands it into
its steps — each gated, run and recorded individually, so a skill can
never grant its steps an exemption. Skills live under
`<workspace>/.amparo/skills/`; without `--growth` the `use_skill` tool is
not registered at all. Growth is write + read + act — still one opt-in,
still off by default.

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
| 2 | Native tool calling (replacing text-parsed ReAct) | ✅ landed — loop + MCP client/server, 260 tests green |
| 3 | Install path + release — the `amparo` CLI drives the loop end-to-end (headless: no screen/desktop tools in the registry) | ✅ done |
| 4 | Chat adapters — Telegram first, then Discord and Slack | ✅ done — all three behind one transport seam, inline-button approval |
| 5 | Multi-tenant identity and per-user policy | ✅ done — TOML tenant directory, per-user ceilings/workspaces, attributed approvals |
| 6 | Controlled self-improvement | 🚧 in progress — M6a + M6b + M6c landed: the lab notebook (`--growth`, PII-stripped run records), the verification case library (same-tenant evidence in the verification prompt only), and gated skills (adopted procedures executed step-by-step through the gate chain) |

**Giving this to other people** — a shell-executing agent behind a chat
bot is a security boundary, and the operator owns it: the TOML tenant
directory names exactly who may start tasks, scopes each user's tools to
their own workspace directory, caps each user's trust ceiling, tags each
user's policy checks with their `platform:user_id` session, and makes
approvals requester-only. The flat `AMPARO_CHAT_ALLOWLIST` remains for
single-operator setups, fail-closed: absent or empty means nobody.

## Provenance

The agent loop was extracted from
[Axiom-OS](https://github.com/PixelPhantomAI/Axiom-OS) (MIT), which contains a
working ReAct loop with tool retry, self-verification, and conversation
trimming. What did *not* come across: the desktop compositor, screen ingestion,
the companion loop, and the ELLM proxy coupling — see [NOTICE](NOTICE). What
Amparo adds on top of the extracted mechanics is the runtime gate chain that
sits between "the model asked" and "it ran" — the gap Axiom left open — shipped
as a headless, installable, versioned binary.

Amparo is Apache-2.0 rather than MIT for the explicit patent grant, which
matters more than usual for software that executes arbitrary code.

## License

Apache License 2.0 — see [LICENSE](LICENSE).

Copyright 2026 EL AI Intelligence, LLC.
