# Versioning and API stability

Amparo is a workspace of Rust crates that share one version
(`[workspace.package] version = 0.1.0`). This file is the contract for how
that version moves and what "stable" means at each stage.

## Semver

Amparo follows [Semantic Versioning](https://semver.org) with one caveat:
**while the major version is 0, nothing is stable.** Per semver's own rules,
minor bumps in 0.x may break APIs. Patch bumps (0.1.x) should only contain
fixes, but 0.x consumers pin an exact version if they need a guarantee.

- **0.1.0 (2026-08-28)** — first release: the `amparo` CLI, the agent loop,
  the MCP surface, deny-by-default policy gate, interactive approval.
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
