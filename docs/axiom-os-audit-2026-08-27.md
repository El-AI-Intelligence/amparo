# Axiom-OS clean-sheet audit — 2026-08-27

Full-tree read-only audit of `/home/e/projects/axiom-os` (1,117 commits, 1,059 .rs files,
34 crates, 300 daemon routes) ahead of the Amparo extraction. Six parallel passes:
core loop, memory/privacy crates, remaining crates, platforms/apps/enterprise,
docs-vs-code, git history/secrets. All line refs verified by the auditing pass.

## CRITICAL findings

| # | Finding | Evidence |
|---|---|---|
| C1 | Daemon binds `0.0.0.0:47800` with **no auth** on any route; `/agent/tasks/{id}` leaks the pending `call_id` and `/agent/tasks/{id}/approve` needs no identity → remote RCE with self-approval | `crates/axiom-daemon/src/main.rs:1887,1519,1535-1736`; `agent_loop.rs:1181,2019-2103,2687` |
| C2 | `/inference/complete` chat loop exposes `run_command` (`bash -c`) via `ExternalEffector` tier; **Escalate verdicts execute without approval** — only `Deny` blocks | `crates/axiom-daemon/src/routes.rs:2312-2490` |
| C3 | Enterprise API auth middleware **never wired to any route**; default JWT secret fallback `axiom-dev-secret-change-in-production` → any tenant forgeable | `enterprise/axiom-api/src/main.rs:105-141`; `routes.rs:10` claims the opposite |
| C4 | `AXIOM_WALLET_MASTER_KEY` falls back to hardcoded `"dev-wallet-master-key"`; wallet endpoints are a deterministic simulation with no signing | `crates/axiom-wallet/src/lib.rs:173,192` |
| C5 | Red-line QC **fail-open**: `RedLineClient` returns `passed:true` when the `axiom-redline` binary can't spawn — and that binary does not exist in this workspace | `crates/axiom-qc/src/red_line_client.rs:149-152`; `crates/axiom-daemon/src/main.rs:885-887` |

## HIGH findings

| # | Finding | Evidence |
|---|---|---|
| H1 | Sandbox fallback chain degrades to **no isolation**: `unshare --mount --pid --fork` with no remounts is a no-op namespace; bwrap often unavailable (Ubuntu 24.04 userns restriction) | `crates/axiom-daemon/src/process_sandbox.rs:595-622` |
| H2 | Filesystem tools: read path never canonicalizes (symlink escape to `~/.ssh/id_rsa`, `/etc/shadow`); `/tmp`+`/dev/shm` always writable | `tools/filesystem.rs:34,89`; `process_sandbox.rs:130,185` |
| H3 | `fetch_url` SSRF: only scheme check; loopback/link-local/169.254.169.254 reachable; unbounded body buffered before truncation | `tools/web.rs:395,423` |
| H4 | git tools unsandboxed, hooks enabled (`core.hooksPath` not set) → fetched repo executes code on commit; `git_diff` arg without `--` = flag injection | `tools/git.rs:37,141,197` |
| H5 | Relay accepts unbounded `vector_clock` (`u64::MAX` wedges a memory_id permanently for every device) | `crates/engramd-sync/src/routes.rs:96-131` |
| H6 | Sync HMAC over self-declared `vault_id`; vault_id = directory basename → same-passphrase same-named vaults merge cross-machine; malicious relay can migrate blobs | `crates/engramd/src/sync_client.rs:134-145`; `main.rs:314-318` |
| H7 | Relay tombstone purge (30d) < offline window → deleted memories **resurrect** and re-propagate | `crates/engramd-sync/src/main.rs:39-41,153-198` |
| H8 | `push_local_changes` pages only newest 200 → >200 memories in one cycle = permanent silent sync loss | `crates/engramd/src/sync_client.rs:702-747` |
| H9 | Unreal companion actor has duplicate function definitions in one TU = guaranteed compile failure | `unreal-client/Source/AxiomOSClient/Private/Companion/AxiomCompanionActor.cpp` |
| H10 | Pi image build copies x86_64 binaries into ARM rootfs with `|| true` → ships bootable-but-dead image | `build/board/axiom-pi/post-build.sh:10-12` |
| H11 | Homebrew formulas: empty/placeholder sha256, stale PixelPhantomAI release URLs (actual releases are El-AI-Intelligence/engram) | `dist/homebrew/engramd.rb:23-42`; `Formula/engramd.rb:26-38` |
| H12 | `curl|bash` installer resolves `latest` with no shasum verification of the tarball | `install.sh:143-158` |

## MED findings (selected)

- M1 `axiom-mesh` WS transport binds `0.0.0.0` with zero auth, merges any `SyncBundle` (also council daemon `0.0.0.0:47900` unauth `/audit`).
- M2 `axiom-engram` `OnnxEmbedder` has a real `UnsafeCell` data race — **the vendored copy in engram already fixed it; never propagated back**.
- M3 Weak at-rest key derivations: machine-id key = SHA-256(machine_id‖APP_SALT) (world-readable input), legacy SipHash zero-key, legacy SHA-256 passphrase — all auto-tried on open.
- M4 `GET /privacy/audit` hardcodes `"sync_enabled": false` — the privacy audit **lies** when sync is on.
- M5 Passphrase via `--passphrase` CLI flag (visible in ps/history); no zeroization anywhere; non-atomic 0644 state writes.
- M6 Embedding model downloaded from HuggingFace unpinned.
- M7 Stub tools **report fabricated success**: `send_email`, `wallet_send`, `research_query`, `imagine_scenario`, blackboard tools all return `success:true` doing nothing (`tools/agent_tools.rs:36-551`) — an agent would tell the user non-existent actions happened.
- M8 Client-controlled `max_steps`/`trust_ceiling` (unauthenticated caller can raise to SystemControl); unclamped `run_build`/`run_tests` timeouts; `computer_use` `wait` action unbounded (2^63 ms); unbounded request bodies on all routes; inference HTTP has no timeouts.
- M9 Conversation history (`sessions.db`) unencrypted while the vault is encrypted.
- M10 Tauri CSP `unsafe-eval` + shell allow-execute/kill; Windows scheduled task kills daemon every 24h; fleet WS clients connect to a server route that doesn't exist.
- M11 `RedLineClient`… see C5. Wellness engine lock dropped immediately in agent loop (`agent_loop.rs:2559`) — dead integration.
- M12 Live basic-auth password file `engram-password.txt` at repo root (gitignored, but rotation advised) + 36 MB live vault `data/engrams.db` in the source tree.
- M13 Fake "Dilithium" post-quantum signature = FNV/LCG hash keyed by u64 seed (`axiom-future`).

## Docs vs code (assessment)

Docs **understate** the runtime core and **overstate** the edges. The daemon genuinely
delivers most of README claims (300 live routes: CDP browser, PTY shell, capability-gated
effects, approval queue, mesh REST, research, screen ingestion). The widest gaps:
VISION.md's "no LLM at runtime / ELLM is sole cognitive authority" (the loop is a
kernel→LLM→keyword ladder; LLM is the default tier), "126,062 rules" (docs disagree by
100×; ~13.5K real), glasses/Meta integration (fabricated stub data), watchOS/wearOS
(unbuildable fragments), daemon-lite/glasses-daemon (worktree-only), fleet WS
(clients with no server), dead route families (`/sim/*`, `/pedagogy/*`,
`/epistemic/stream-search`, `/agenda/goals`, `/companion/world/*`), and
README_PRODUCTION_READY.md referencing deleted files. The engram-product docs are the
only family essentially truthful end-to-end.

## Git history / hygiene

- **No secrets in history** — full-scan over all 1,117 commits: zero private keys,
  tokens, `.env`, `.pem`, `.key`, credential URLs. No rotation needed (the only live
  credential is the gitignored `engram-password.txt`).
- `.git` = 840 MB: history polluted by committed `target/` artifacts (top blob 196 MB
  macos debug binary). Bloat only — nothing sensitive.
- MIT clean, GPL clean (false-positive "gPl" in BillingPlan). No per-file headers —
  provenance for mined files must be re-attached manually.
- **Amparo mine exclusions**: `.superpowers/` (61 tracked internal files),
  `.planning/` (191 tracked), `.claude/`, `.worktrees/`, `data/`, `engram-data/`,
  `engram-password.txt`, `unreal-client/`, `enterprise/`, `test_capture`,
  `.github/workflows/LICENSE` (untracked stray copy).
- Remote: `https://github.com/PixelPhantomAI/Axiom-OS.git` — no embedded creds.

## Verdicts (34 crates)

REAL: axiom-blackboard (orphaned), axiom-cli, axiom-compositor (20k-line Smithay),
axiom-device, axiom-eventbus, axiom-face, axiom-imagination, axiom-model-router,
axiom-qc, axiom-research, axiom-vision (name "CodeSandbox" = unisolated /tmp runner),
axiom-world, axiom-engram, axiom-memory, axiom-privacy, engramd, engramd-sync
(upstream skeleton only).
PARTIAL: axiom-actuator (simulated adapters), axiom-email (no transport), axiom-future
(orphaned), axiom-mesh (no auth/discovery), axiom-sim (orphaned), axiom-trainer
(bash LoRA), axiom-voice (mock default), axiom-wallet (simulation), axiom-wellness,
engramd-mcp (no auth support), axiom-session (plaintext DB), axiom-inference
(no response-side tool_calls).
DEAD-relative-to-daemon: axiom-trust (unwired), axiom-blackboard, axiom-future,
axiom-sim. Orphaned route files: `sim_routes.rs`, `pedagogy_routes.rs` (never mounted;
one wouldn't compile).

## Divergence: axiom-os engram crates vs the shipping engram repo

The two copies are a **fork with one-way flow** (axiom-os → engram, never merged back).
Vendored side is far ahead: store v3 vs **v6** (+quarantine, noise filter, link
inference, sync cursors), revoked-devices in sync, embed data-race fixed, teams/billing/
handoff/link_crypto routes that axiom-os predates. axiom-inference differs only
cosmetically. Anyone editing axiom-os is editing an ancestor of the shipping daemon.

## Consequences for the Amparo extraction

1. **Must fix before any release** (not optional): C1/C2 (loopback bind + bearer auth;
   approval on all Execute tiers incl. Escalate), H1-H4 (fail-closed sandbox,
   canonicalize paths, SSRF filter, `--` + hooks off), M7 (no fabricated-success tools),
   M8 (server-side caps, timeouts, body limits), H5-H8 if the Engram sync code is ever
   reused (it isn't — Engram is a separate product; Amparo only talks to the daemon).
2. **Do not carry**: enterprise/, unreal-client/, platforms/glasses|watchos|wearos,
   `.superpowers/`, `.planning/`, `.claude/`, `.worktrees/`, `data/`, live-vault files,
   wallet (or carry behind an explicit "simulation" flag), red_line fail-open client,
   dead route files, README_PRODUCTION_READY.md-style docs.
3. **Carry with fixes**: agent_loop (after native-tool-calls switch — the chat loop
   already parses `tool_calls`, proving the path), tool registry minus stubs, axiom-engram/
   memory/privacy/qc cores, session (add encryption), model-router, CDP browser tool.
4. **Policy gate (the Amparo spine)**: the audit confirms the guardrail check is the
   *only* gate on the chat loop's shell — and it's a stale ELLM fork. The
   PolicyEngine seam (#29) plus the wire spec (#30) are exactly the right surgery.
5. **Attribution**: axiom-os is MIT © Pixel Phantom AI; Amparo is Apache-2.0. Files
   carry no headers — the extraction must attach provenance (NOTICE + per-file headers)
   since history is deliberately not copied.
