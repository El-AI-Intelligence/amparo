# Versioning and API stability

Amparo is a workspace of Rust crates that share one version
(`[workspace.package] version = 0.11.0`). This file is the contract for how
that version moves and what "stable" means at each stage.

## Semver

Amparo follows [Semantic Versioning](https://semver.org) with one caveat:
**while the major version is 0, nothing is stable.** Per semver's own rules,
minor bumps in 0.x may break APIs. Patch bumps (0.1.x) should only contain
fixes, but 0.x consumers pin an exact version if they need a guarantee.

- **0.1.0 (2026-08-28)** — first release: the `amparo` CLI, the agent loop,
  the MCP surface, deny-by-default policy gate, interactive approval.
- **0.2.0 (2026-08-28)** — chat adapters: Telegram, Discord and Slack behind
  one transport seam, with inline-button approval and a fail-closed
  operator allowlist.
- **0.3.0 (2026-08-28)** — multi-tenant identity: the TOML chat config
  (`--chat-config` / `AMPARO_CHAT_CONFIG`), per-user policy session tags,
  per-user trust ceilings and workspace directories, and requester-only
  approval presses.
- **0.4.0 (2026-08-29)** — controlled growth: the lab notebook
  (`--growth`), the verification case library, gated skills with metrics
  and retirement, and rollup + archival over the cold archive.
- **0.5.0 (2026-08-29)** — instrumentation & hardening: the always-on
  privacy ledger (`amparo privacy`), session persistence (`amparo run
  --resume`, per-tenant chat continuity), and preflight blast-radius
  classification (display-only labels in the approval copy).
- **0.6.0 (2026-08-29)** — M7's two deliberate exclusions: the WASM
  eval sandbox (the fuel-metered `eval_wasm` tool — untrusted
  computation, approval-gated, honestly labeled `read_only`) and the
  opt-in ledger quota lever (`--ledger-max-bytes`, per-tenant chat
  quotas; rotation marker rows record exactly what was dropped).
- **0.7.0 (2026-08-30)** — sub-agents & scheduling: `spawn_agent` (the
  child is the same loop, gate chain, and ceiling; the delegation chain
  is in the ids, checkpoints, ledger rows, and approval copy; the
  shared budget fails closed), `schedule` (a persisted promise
  re-entering the gate chain as its requester; missed = fail-closed),
  and the swarm report with the cost line.
- **0.8.0 (2026-08-30)** — verification & QA: the QC council
  (deterministic rule auditors beside policy — advisory findings feed
  the verification prompt, verification stays the model's call),
  `amparo doctor` (the operator's read-only workspace sweep, exit
  0/1/2), and the audit-mode stderr notice + session tagging
  (`--session-id`, defaulting to the task id).
- **0.9.0 (2026-08-31)** — coordination & surfaces: the blackboard
  (`blackboard_read`/`blackboard_write` over a workspace-scoped board,
  `[bus]` rows), `send_notification` (the transport seam — stderr or
  webhook, ExternalEffector), rollback groups (display-only undo
  hints, `.amparo-bak` backups, never auto-executed), the
  web-approval seam (`WebApprovalGate` POST/poll, 60 s fail-closed,
  `--approval-endpoint`), and MCP spawn + the CLI scheduler
  (`--max-sub-agents`, due promises firing at run start).
- **0.10.0 (2026-08-31)** — adoption: the Engram memory backend (the
  `Memory` trait over engramd, env-gated, degradation to the built-in
  store), Guardrail-native policy checks with `amparo doctor` probing
  both companions and reporting audit mode, and the web surface live
  at `amparo.ellmstack.dev` (deployment against the v0.9.0 approval
  seam).
- **0.11.0 (2026-09-01)** — the ecosystem terminal: the TUI writes into
  both siblings (`/memory` into the Engram vault, `/policy` into the
  Guardrail Console's org rules, `!` shell escape for delegation), the
  wizard's 10-answer contract captures the console URL and delegates
  credentials to the sibling CLIs, and console-routed checks carry the
  org's deny-only rules.
- When the first stable release happens it will be **1.0.0**, and from then
  on semver applies in full.

## MSRV

- **Declared: Rust 1.85** (`rust-version = "1.85"` in `[workspace.package]`).
  This is a floor for tooling and a promise about what we avoid (no newer
  language features without a deliberate bump).
- **Verified: Rust 1.98.0** — the version the gate actually runs on.
  The verified baseline is the one that matters for CI; the declared floor
  is the one that matters for `cargo install` users. The two-gate command
  below runs in CI and in local development alike.

## The gate

Every change to Amparo must pass both of these, with **zero warnings**:

```sh
cargo test --workspace
cargo doc --workspace --no-deps
```

`cargo test` is the behavior gate. `cargo doc` is the API-stability gate:
every crate carries `#![warn(missing_docs)]`, so a public item without a
doc comment fails the build of the documentation — meaning an undocumented
API cannot ship.

## Protocol constants are NOT Amparo versions

Two constants look like version numbers and are not:

- **MCP `PROTOCOL_VERSION` ("2025-06-18")** in `amparo-mcp` is the
  [Model Context Protocol](https://modelcontextprotocol.io) spec revision
  that Amparo implements. It moves when the MCP spec moves, independently
  of Amparo releases.
- **The policy wire spec** (`POST /check {tool_name, target}`) versions
  itself, inside its own document. See the wire spec published with
  Guardrail's docs; Amparo implements the v1 caller contract (never block
  on `enforced: false`, fail-safe escalate on engine errors, escalate means
  ask a human).

Do not bump the workspace version when either of those changes — bump the
workspace version only for Amparo's own API surface (crates, CLI flags,
tool registry schema).

## Release procedure

1. Update `CHANGELOG.md` (Keep a Changelog) with the new version section.
2. Bump `[workspace.package] version` if and only if the surface changed.
3. Run the two-gate command above — zero warnings, everything green.
4. Tag the release commit: `git tag -a vX.Y.Z -m "vX.Y.Z — <one line>"`.
5. Push `main` and the tag to the private origin.

Crates.io publishing is deliberately **not** part of the procedure while
the repository is private; when that decision changes, it gets its own
entry here.
