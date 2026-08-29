# Changelog

All notable changes to Amparo are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
See [VERSIONING.md](VERSIONING.md) for what "stable" means at each stage.

## [Unreleased]

### Added

- New `amparo-notebook` crate — the lab notebook (M6a): every completed
  or failed task is recorded as a PII-stripped, tenant-tagged JSON run
  record (task text, tool-sequence hash, per-call gate log, verification,
  truncated answer, duration, token-cost estimate), written through the
  `EventSink` seam into the `Memory` trait. Includes `NotebookSink` (the
  event consumer) and `JsonlStore` (append-only local store).
- `--growth` / `--no-growth` on `amparo run` and `amparo chat` — records
  land in `<workspace>/.amparo/notebook/records.jsonl`, tagged `cli` or
  `platform:user_id`. Off by default: without `--growth`, no record file
  is ever created.
- `FanoutSink` in `amparo-agent` — fans every event out to several sinks
  (plus `truncate`/`TRUNCATE` re-exports).

## [0.3.0] — 2026-08-28

Multi-tenant identity: the chat face moves from a single-operator
allowlist to a TOML tenant directory, with per-user policy, trust
ceilings, workspaces and attributed approvals.

### Added

- **The TOML chat profile** (`--chat-config <path>` /
  `AMPARO_CHAT_CONFIG`, the flag wins): `[users."platform:user_id"]`
  sections are the tenant directory — a user without an entry is refused
  exactly as an unlisted user is today. Per-user `trust_ceiling` (falling
  back to the `--trust-ceiling` flag) and per-user `workspace` (a
  relative subpath under the workspace root, defaulting to
  `users/<platform>-<user_id>/`; absolute and `..` paths are rejected at
  load). The file is read once at startup; an empty directory and the
  legacy `AMPARO_CHAT_ALLOWLIST` interplay are both warned about at
  startup.
- **Per-user policy checks** — with `--policy-url`, every task gets a
  fresh engine session-tagged `platform:user_id`, so engine-side audit
  rows carry the chat user (legacy allowlist mode included).
- **Per-user workspaces** — directory-mode tasks run in their own
  workspace directory, enforced by explicit `PathPolicy` injection
  (`PathPolicy::from_root`, `with_policy` on the 14 workspace-bound
  tools, `default_registry_with_policy`) — never process-global
  environment mutation. The default `users/` path is re-checked per task
  so a hostile user id cannot escape the root.
- **Strict press attribution** — `ApprovalButtonPress` carries the
  pressing user's id; presses from anyone but the requester are rejected
  with a polite toast and never consume the pending approval.
- **Binary-level M5 e2e** — the shipped `amparo chat telegram` process
  against mock Telegram and LLM servers: config flag/env precedence and
  exit codes, the per-user workspace proven with `pwd`, the wrong-user
  toast followed by the requester's own deciding press, refusal of
  unknown users, and a per-user ceiling blocking the tool without ever
  asking.

### Changed

- `ChatDriver::new` takes `Tenants` (directory or legacy allowlist) and
  `PolicySource` (shared engine or per-task wire engine); `allowlisted()`
  becomes `allows()`, and per-task parts are resolved before the busy
  claim so a refused user never holds a chat busy.
- Wrong-user Slack presses now reply ephemerally (the requester's
  buttons stay up — previously the outcome edit replaced them) and
  Discord sends a toast message (previously silent).

## [0.2.0] — 2026-08-28

Chat adapters: the agent now runs from Telegram, Discord, and Slack — the
same loop, gate chain and approval seam behind one transport trait, with
inline-button approval.

### Added

- **The chat layer** (`amparo-chat`): one `ChatTransport` seam (text out,
  approval messages with inline buttons, outcome edits, a receive loop), a
  `ChatDriver` that turns a normalized message into a per-task agent run
  (allowlist, one task per chat, panic-proof task boundary), an
  `ApprovalRouter` for routing button presses back to the waiting gate (an
  atomic take — a second press is already decided), a `ChatApprovalGate`
  with inline Approve/Deny buttons and a 60 s auto-deny timeout, and a
  `ChatEventSink` forwarding the agent's `[tag]` progress lines into the
  chat (the final answer bypasses the sink, so it can never be lost).
- **Three hand-rolled adapters** — no platform SDKs, rustls-only websockets:
  **Telegram** via getUpdates long polling (inline keyboards,
  `answerCallbackQuery` acks, 409/401 → exit), **Discord** via the gateway
  websocket (message intents, heartbeat discipline, resume-first
  reconnects, message components, interaction callbacks), and **Slack**
  via Socket Mode (envelope acks before processing, block-kit buttons,
  bot-echo suppression). Every approval flows through the same inline
  Approve/Deny buttons.
- **`amparo chat telegram|discord|slack`** — the `amparo` CLI's fourth
  subcommand. Tokens come from `AMPARO_CHAT_*` environment variables
  (never argv); `AMPARO_CHAT_ALLOWLIST` is a fail-closed single-operator
  allowlist (empty = every message refused); `AMPARO_CHAT_TELEGRAM_BASE`
  points the Telegram adapter at a self-hosted Bot API server.
- **Canonical event formatting** — `format_event` moved into
  `amparo-agent`, so the CLI and every chat platform render identical
  `[tag]` lines.
- **Full end-to-end coverage** — a binary-level e2e test drives the real
  `amparo chat telegram` process against in-test mock LLM and Telegram
  servers: message → approval keyboard → button press → outcome edit →
  final answer, with the tool executed against a real workspace.

## [0.1.0] — 2026-08-28

First release: the `amparo` CLI, the agent loop, and the MCP surface — an open
agent that acts under policy, bring your own LLM.

### Added

- **Provider layer** (`amparo-inference`): one `InferenceProvider` trait;
  `OpenAIProvider` for any OpenAI-compatible endpoint (Ollama, vLLM,
  OpenRouter, Together, Groq — including an Ollama-native `/api/chat` branch);
  `AnthropicProvider` for the native Anthropic Messages API, translated to the
  same contract (incl. `tool_use`/`tool_result` and SSE streaming).
  Fail-closed configuration, per-request and stream-idle timeouts, a
  `max_tokens` clamp, and an optional model allowlist.
- **The agent loop** (`amparo-agent`): native `tool_calls` (not text-parsed
  ReAct) behind the deny-by-default gate chain — trust ceiling → policy gate →
  human approval — with parallel tool batches, retry ×2, max steps plus
  conversation trimming, and VERIFIED/INCOMPLETE self-verification. Events
  flow through an `EventSink` seam; privacy runs per turn (PII strip/restore).
- **Policy** (`amparo-policy`): the `PolicyEngine` seam with a deny-all
  default, `WirePolicyEngine` for the open policy-check wire protocol
  (`POST /check {tool_name, target} → {verdict, reason, enforced}`), and
  `AllowAllPolicyEngine` as the explicit named opt-in.
- **Tools** (`amparo-tools`): the registry plus the portable tool set (web,
  filesystem, shell, git, tests, build, memory), each with a trust tier that
  drives the approval gate.
- **MCP, both directions** (`amparo-mcp`): `McpServer` exposes an Amparo
  registry to external clients over stdio JSON-RPC 2.0 — every `tools/call`
  runs the same gate chain; `McpClient` spawns an external server and mounts
  its tools at `ExternalEffector`, so remote tools cannot skip approval.
  Ships the `amparo-mcp-serve` binary.
- **Memory and privacy** (`amparo-memory`, `amparo-privacy`): a memory
  interface with a built-in store (Engram is the recommended backend, never a
  dependency); privacy policy evaluation with blocked/allowed domain routing
  and the Secure Minions PII strip/restore primitives.
- **The installable CLI** (`amparo-cli`, the `amparo` binary):
  `amparo run "task"` drives the loop end-to-end with fail-closed BYO-LLM
  environment wiring, interactive terminal approval (y/N, 60 s timeout,
  EOF/non-terminal stdin auto-denies) with `--auto-approve`/`--auto-deny`
  overrides; `amparo mcp-serve` is the same implementation as the standalone
  `amparo-mcp-serve` binary; `amparo version` prints the version. stdout
  carries the final answer only — progress, gate decisions and the report go
  to stderr.
- **API stability**: `#![warn(missing_docs)]` on every crate, so an
  undocumented public item cannot ship; the two-gate policy
  (`cargo test --workspace` and `cargo doc --workspace --no-deps`, both with
  zero warnings) is documented in `VERSIONING.md`; MSRV declared at Rust 1.85.
- **TLS**: rustls everywhere — `openssl-sys` is out of the dependency graph,
  so `cargo install` needs no OpenSSL headers.

### Distribution

The repository is private; there is no crates.io publication yet. Install
from source:

```sh
cargo install --path crates/amparo-cli
```
