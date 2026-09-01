# Amparo

> **Spanish** — protection, shelter, refuge. In Spanish and Latin American law,
> a *recurso de amparo* is the action you file to protect a right.

An open agent that acts under policy. Bring your own LLM.

Amparo runs a real tool-use loop — shell, files, git, web, tests, build,
memory — where **every tool call passes a policy check before it executes**,
and where the model driving the loop is yours to choose. Current version:
**v0.10.0** (all twelve roadmap milestones landed — see
[Roadmap](#roadmap)).

**The one rule: no member exits the gate chain.** The gate chain is

```
registry → trust ceiling → policy engine → human approval
```

and it is deny-by-default and fail-closed at every step: no policy engine
configured means every call is refused until you explicitly opt in
(`--allow-all`); an unreachable engine escalates through the fail-safe path,
never fails open; an unanswered approval auto-denies after 60 s. Sub-agents,
scheduled promises, MCP-mounted tools, chat-approval presses — every path a
tool call can take ends in the same chain. A model is never the approver.

## Install

One line (Linux, macOS, Windows — x86_64 and arm64; installs to
`~/.local/bin`):

```sh
curl -fsSL https://downloads.ellmstack.dev/amparo/install.sh | sh
```

Or from source (Rust 1.85+):

```sh
cargo install --path crates/amparo-cli
```

Then point it at any OpenAI-compatible endpoint (Ollama, vLLM, OpenRouter,
Together, Groq) or the native Anthropic API:

```sh
export AMPARO_INFERENCE_URL=http://localhost:11434/v1   # required
export AMPARO_INFERENCE_MODEL=qwen2.5:14b               # required
amparo run "list the files and tell me what's there" --allow-all
```

Two environment variables are required; everything else is optional —
`AMPARO_INFERENCE_KEY` (empty for keyless local providers),
`AMPARO_INFERENCE_PROVIDER` (`openai` default or `anthropic`),
`AMPARO_WORKSPACE` (the directory tools are confined to),
`AMPARO_POLICY_KEY` (with `--policy-url`), and the `AMPARO_CHAT_*` tokens
for the chat adapters. Without `--policy-url` or `--allow-all`, every tool
call is refused. Approvals ask **y/N at the terminal** with a
`[preflight] blast radius: …` line naming the consequence; stdout carries
the final answer only — progress and gate decisions go to stderr, so
`amparo run` scripts cleanly.

The same agent over Telegram, Discord, or Slack (`amparo chat
telegram|discord|slack`), with inline Approve/Deny buttons, a TOML tenant
directory for per-user policy scopes, and a fail-closed operator allowlist
— see the env table further down for the full surface.

## Three commitments

**Bring your own LLM.** No bundled model, no required sidecar, no vendor
with a privileged position in the loop.

**Policy is an interface, not a product.** The gate between "the model
decided to do this" and "this ran" is an open interface with an open wire
protocol (`POST /check` → `{verdict, reason, enforced}`). Amparo ships a
deny-all default engine; [Guardrail](https://elai-intelligence.com) is a
commercial implementation of the same interface, and anyone can write
another.

**Engram is recommended, never required.** Memory is an interface with a
built-in default store; [Engram](https://elai-intelligence.com) is the
recommended memory backend — durable, private, syncable across devices —
but Amparo runs without it, and hard-depends on no memory product.

**Deployable anywhere.** A standalone binary, a container, a systemd unit,
a chat bot, an MCP server. Not welded to a desktop session, not dependent
on a GUI.

## What's in the box

- **The loop** — native `tool_calls` (no text-parsed ReAct), parallel tool
  batches with retry, conversation trimming, self-verification, and an
  `EventSink` seam for hosts. Events flow as scientific-voice `[tag]`
  progress lines.
- **The gate chain** — trust tiers per tool (`Observational` →
  `ExternalEffector`), the `PolicyEngine` seam (default: deny all; the
  `WirePolicyEngine` client speaks the open wire protocol with a 60 s
  fail-closed timeout), and human approval gates (terminal, chat inline
  buttons, or the web-approval seam) that fail closed on timeout.
- **The privacy ledger** — always-on, append-only: every
  network-touching execution attempt (tool, host at most, outcome, human
  gate answer) and every PII strip as per-category counts, never values
  (`amparo privacy` reads it; `--ledger-max-bytes` bounds it, with
  rotation itself audited). See `docs/m7-instrumentation.md`.
- **Sessions** — every task checkpoints once per loop iteration,
  PII-stripped, written atomically; `amparo run --resume` picks up a
  crashed run and re-judges every call through the gate chain.
- **Sub-agents & scheduling** — `spawn_agent` (a sub-agent is the same
  loop, same gate chain, same ceiling; a shared swarm budget fails closed)
  and `schedule` (a persisted promise re-entering the gate chain as its
  requester; missed = fail-closed, never fired late). See
  `docs/m8-swarms.md`.
- **The QC council & `amparo doctor`** — deterministic rule auditors run
  beside policy before verification (advisory findings, verification stays
  the model's call), and the doctor is a read-only workspace sweep, exit
  0/1/2, cron-able. See `docs/m9-verification-qa.md`.
- **The coordination surfaces** — the blackboard
  (`blackboard_read`/`blackboard_write`), `send_notification` behind a
  transport seam, display-only rollback hints with fail-closed
  `.amparo-bak` backups, the web-approval seam (`--approval-endpoint`,
  60 s fail-closed), MCP spawn, and the CLI scheduler. See
  `docs/m10-coordination-surfaces.md`.
- **The WASM eval sandbox** — a fuel-metered, deterministic
  `SandboxRuntime` for untrusted computation (10M fuel, 4 MB module,
  4 MB memory, 30 s wall clock, no imports, no WASI), approval-gated and
  honestly labeled `read_only`. See `docs/m7b-sandbox-quota.md`.
- **Controlled growth** — opt-in (`--growth`, off by default): the lab
  notebook (PII-stripped run records), the verification case library,
  gated skills adopted through the same gate chain, per-skill metrics
  with policy-drift and performance retirement, and a hot layer over the
  never-modified cold archive. See `docs/m6-controlled-growth.md`.

### The crate set

- **`amparo-inference`** — one provider trait, two implementations:
  OpenAI-compatible (Ollama, vLLM, OpenRouter, Together, Groq) and native
  Anthropic, translated to one contract. Fail-closed by construction: no
  silent localhost default, per-request + stream idle timeouts, a
  `max_tokens` clamp, optional model allowlist.
- **`amparo-agent`** — the loop and the gate chain, extracted from
  Axiom-OS's working ReAct mechanics and rebuilt on native tool calls.
- **`amparo-mcp`** — MCP first-class in both directions: `McpServer`
  (stdio JSON-RPC, every call through the same gate chain) and
  `McpClient` (spawns a server process, mounts its tools as
  `ExternalEffector` — remote tools cannot skip the approval gate).
- **`amparo-policy`** — the policy seam and the wire-protocol client.
- **`amparo-tools`** — the registry and the portable tool set (web,
  filesystem, shell, git, tests, build, memory), each with a trust tier
  that drives the approval gate.
- **`amparo-sandbox`** — the WASM eval sandbox.
- **`amparo-memory`** — the memory interface with a built-in default
  store.
- **`amparo-privacy`** — PII strip/restore primitives and the privacy
  ledger.
- **`amparo-notebook`** — run records, the case library, skills, metrics,
  rollup.
- **`amparo-chat`** — one transport seam; hand-rolled Telegram (long
  polling), Discord (gateway websocket) and Slack (Socket Mode) drivers;
  the schedule ticker.
- **`amparo-cli`** — the one binary: `run`, `resume`, `privacy`,
  `schedule`, `skill`, `notebook`, `doctor`, `chat`, `mcp-serve`, `tui`,
  `wizard`, `version`.

```sh
cargo test --workspace            # the behavior gate
cargo doc --workspace --no-deps   # the API-stability gate (missing_docs on every crate)
```

Both gates must pass with zero warnings — see [VERSIONING.md](VERSIONING.md).

## Native integrations

**Engram memory backend.** With `AMPARO_MEMORY_BACKEND=engram`, the
memory-search tool answers from an Engram vault over engramd's REST
surface (`AMPARO_ENGRAM_URL`, default `http://127.0.0.1:8787`; optional
`AMPARO_ENGRAM_KEY`). The daemon is probed once at startup — down means
one warning and the built-in store; a mid-run outage degrades searches to
empty rather than crashing the loop.

**Guardrail policy engine.** Point `--policy-url` at a Guardrail engine
(or any wire-protocol engine) with `AMPARO_POLICY_KEY`, and every tool
call is checked there before it runs. Audit-mode verdicts
(`enforced:false`) are visible, never silent — one stderr line names the
mode ("policy engine is in audit mode; verdicts are advisory") — and an
unreachable engine escalates through the fail-safe path: it never fails
open. Route the URL through the Guardrail Console
(`https://guardrail.elai-intelligence.com/api/upstream`) and the
operator's org deny rules apply to every check — the wizard steers there
by default.

**Graceful degradation.** Remove both companions and Amparo still runs:
the built-in memory store and the local default engine. `amparo doctor
--engram-url … --policy-url … --probe` sweeps both surfaces for the
operator.

**The trial bundle.** [docs/trial-bundle.md](docs/trial-bundle.md) pairs
the public reveal with one month of Engram's personal tier and Guardrail's
policy enforcement, degrading to the existing free tiers at expiry —
nothing breaks, nothing is silently waived.

## Interactive surface

`amparo tui` is the one-prompt terminal surface: the banner, gutter rows
and approval cards render live, every decision through the same gate
chain. Piped, it degrades to one task per stdin line with zero escapes —
the scripting shape the e2e suite drives. Slash commands at the prompt:

- `/memory add <text>` — store a memory in the resolved backend
  (the Engram vault when wired; skips are surfaced honestly)
- `/memory search <query>` — retrieve up to 5 hits
- `/policy list` — the org's rules plus the org mode (audit/enforce)
- `/policy deny <tool> [reason]` — add a deny-only org rule
  (harden-only: it can never allow what the engine denied)
- `/policy toggle <tool>` — enable/disable an existing rule
- `/policy enforce` / `/policy audit` — flip the org mode (the
  console's error text — including the Pro-plan gate — is shown
  verbatim)
- `! <command>` — run a shell command from the prompt (the delegation
  path: `! guardrail link` pairs this machine with an org key)

`amparo wizard` writes the first-run profile in four steps — workspace,
LLM endpoint, optional Guardrail policy (engine URL, key, console URL),
optional Engram memory URL — saved locally (mode 0600) and read back at
boot to fill environment gaps (env always wins). Steps 3 and 4 print
delegation guidance: the sibling CLI found on PATH
("`guardrail link` pairs this machine") or its install one-liner.

## Environment surface

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
| `AMPARO_CONSOLE_POLICY_URL` | Guardrail Console URL the TUI's `/policy` commands write through (the wizard saves it; default `https://guardrail.elai-intelligence.com`) |
| `AMPARO_MEMORY_BACKEND` | `engram` selects the Engram backend (with `AMPARO_ENGRAM_URL` / `AMPARO_ENGRAM_KEY`) |
| `AMPARO_CHAT_TELEGRAM_TOKEN` | Bot token for `amparo chat telegram` |
| `AMPARO_CHAT_DISCORD_TOKEN` | Bot token for `amparo chat discord` |
| `AMPARO_CHAT_SLACK_APP_TOKEN` | Socket Mode app token for `amparo chat slack` (with `AMPARO_CHAT_SLACK_BOT_TOKEN`) |
| `AMPARO_CHAT_SLACK_BOT_TOKEN` | Bot token for the Slack Web API |
| `AMPARO_CHAT_ALLOWLIST` | Comma-separated user ids who may talk to the chat bot — absent or empty refuses everyone; ignored when a chat config is set |
| `AMPARO_CHAT_CONFIG` | Path to a TOML chat config (the tenant directory); the `--chat-config` flag wins |
| `AMPARO_CHAT_TELEGRAM_BASE` | Telegram Bot API base URL (self-hosted Bot API servers) |

Multiple users, each with their own policy scope — a TOML chat config
(`--chat-config`, or `AMPARO_CHAT_CONFIG`; the flag wins):

```toml
# tenants.toml — the tenant directory: only these users may start tasks
[users."telegram:111222333"]                # one operator, all defaults
[users."telegram:444555666"]
trust_ceiling = "observational"             # per-user ceiling (falls back
workspace = "team-b"                        #   to --trust-ceiling if absent)
```

Each user's workspace is a directory under the workspace root
(`users/<platform>-<user_id>` by default; absolute and `..` paths are
rejected). Policy checks carry `session_id = "platform:user_id"`, so a
wire policy engine sees who asked. Approval presses are attributed: only
the user who started a task can decide it.

## Roadmap

| # | Milestone | State |
|---|---|---|
| 1 | Provider abstraction — Anthropic + OpenAI-compatible | ✅ done |
| 2 | Native tool calling (replacing text-parsed ReAct) | ✅ landed — loop + MCP client/server |
| 3 | Install path + release — the `amparo` CLI drives the loop end-to-end (headless: no screen/desktop tools in the registry) | ✅ done |
| 4 | Chat adapters — Telegram, Discord, Slack | ✅ done — one transport seam, inline-button approval |
| 5 | Multi-tenant identity and per-user policy | ✅ done — TOML tenant directory, per-user ceilings/workspaces, attributed approvals |
| 6 | Controlled self-improvement | ✅ done — the lab notebook (`--growth`, PII-stripped run records), the verification case library, gated skills, metrics + retirement, rollup + archival |
| 7 | Instrumentation & hardening | ✅ landed — the always-on privacy ledger, session persistence (`amparo run --resume`), preflight blast-radius classification |
| 8 | WASM eval sandbox + ledger quota | ✅ landed — the `eval_wasm` tool (fuel-metered, approval-gated) and the opt-in ledger quota lever |
| 9 | Sub-agents & scheduling | ✅ landed — `spawn_agent` (same loop, gate chain, ceiling; shared budget fails closed) and `schedule` (persisted promise re-entering the gate chain; missed = fail-closed) |
| 10 | Verification & QA | ✅ landed — the QC council, `amparo doctor`, the audit-mode stderr notice + session tagging |
| 11 | Coordination & surfaces | ✅ landed — the blackboard, `send_notification`, rollback groups, the web-approval seam, MCP spawn, the CLI scheduler |
| 12 | Engram + Guardrail native, web surface | ✅ landed — the Engram memory backend, Guardrail-native policy, and the web surface live at amparo.ellmstack.dev |
| 13 | Ecosystem terminal | ✅ landed — the TUI writes into both siblings (`/memory`, `/policy`, `!` escape), the wizard's 10-answer contract delegates credentials, org deny rules apply to console-routed checks |

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
