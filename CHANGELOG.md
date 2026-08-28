# Changelog

All notable changes to Amparo are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
See [VERSIONING.md](VERSIONING.md) for what "stable" means at each stage.

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
