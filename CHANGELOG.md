# Changelog

All notable changes to Amparo are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
See [VERSIONING.md](VERSIONING.md) for what "stable" means at each stage.

## [Unreleased]

### Added

- **CI on three platforms**: a GitHub Actions matrix (Linux, macOS,
  Windows) runs both gates at the pinned MSRV (1.85) with warnings denied,
  and a tag-triggered release workflow builds all five installer targets
  natively and publishes a GitHub release with SHA256SUMS.

### Changed

- **Platform scratch dirs**: the file/shell sandbox and the preflight
  blast-radius classifier now use platform-aware shared scratch roots —
  `/tmp` and `/dev/shm` on Unix, the system temp directory on Windows
  (which has no `/dev/shm` twin). The default-workspace fallback when
  `HOME` is unset is now the system temp dir instead of `/tmp`.
- **MSRV-safe lockfile**: the first pinned-1.85 CI run caught a lockfile
  that had drifted past the declared MSRV (resolved at rustc 1.98 —
  `idna_adapter` 1.2.2 and `icu` 2.3.x require 1.86/1.88). The lock now
  resolves MSRV-compatible versions via
  `resolver.incompatible-rust-versions = "fallback"` in
  `.cargo/config.toml`, and the CI gates run `--locked` so dependency
  drift fails loudly instead of re-resolving.

### Fixed

- **`run_command` on Windows**: the shell tool hardcoded `bash -c`, which
  on Windows resolves to the WSL shim and fails without a WSL distro —
  commands never ran and the working-directory echo was absent. The tool
  now runs `cmd /C` on Windows (the cwd echo is `cd` with no arguments,
  cmd's equivalent of `pwd`); the Unix `bash` path is unchanged.
- **Chat e2e cwd assertion on Windows**: the per-user-workspace round trip
  asserted the raw workspace path against the recorded LLM request
  bodies, which differ from the needle at two levels: the bodies are
  serialized JSON (every backslash appears doubled), and the needle was
  built by joining the string `users/telegram-111`, whose inner `/` is
  kept verbatim on Windows while the driver derives the tenant workspace
  per-component — all `\` — which is what `cmd /C cd` echoes. The tool
  itself ran fine (right cwd, exit 0); the needle now matches the
  driver's construction and the body's JSON encoding.
- **Integration tests at the pinned MSRV**: cargo 1.85 builds the package
  binaries for `cargo test` but does not set `CARGO_BIN_EXE_<name>`
  (that arrived in a later cargo), so the real-process e2e suites
  (`cli_e2e`, `chat_e2e`, `bin_e2e`) now fall back to locating the
  binaries next to the test executables in the profile directory.
- **Sandbox boundary on macOS and Windows**: both sides of a boundary
  comparison now resolve through the same view — the deepest existing
  ancestor is canonicalized and the tail re-appended — instead of
  canonicalize-or-lexical. On macOS the temp dir and `/tmp` are symlinks
  into `/private/…`, and a not-yet-existing write target can never
  canonicalize, so every fresh workspace write was silently denied. The
  workspace check is also component-wise now: it appended `/` to the
  root string, which can never match a Windows `\`-separated path, so
  every relative file write on Windows was denied at resolution.
- **Windows TUI dead code**: the interactive reader (raw mode, keypress
  approvals, picker) is Unix-only by design — on Windows the surface
  runs piped. The Unix-only machinery is now allow-listed for dead code
  on non-unix instead of faking a Windows reader.
- **`read_file` not-found message is OS-independent**: a missing file now
  reports `File not found: <path>` on every platform instead of the raw
  io error text, which differs per OS ("No such file" vs "The system
  cannot find the file specified") — the agent reasons over this string,
  and it must not shift underneath it. Other io errors keep the OS text.
- **Windows config test fixture**: the absolute-workspace rejection test
  used `/etc/passwd`, which is drive-relative (not absolute) on Windows
  and so sailed through validation. The fixture is now platform-aware —
  a Windows absolute path trips the rule on Windows. The wizard test had
  the mirror-image shape: its `/tmp/…` workspace answer is root-relative
  on Windows and the wizard (correctly) joins the runner's drive prefix
  on, so the assertion now checks the contract — an absolute path
  carrying the workspace name — instead of the Unix literal.
- **CI wait budgets**: the chat test harnesses polled with 5 s ceilings
  and the `chat_e2e` round trips with 30 s; loaded 2-core CI runners
  (especially Windows) blew both on scheduler contention alone, and the
  e2e children were killed mid-round-trip. Budgets are now 30 s and
  120 s — a polling ceiling, not a correctness window: a stalled driver
  still fails, just not prematurely. The ledger tests' fetch targets
  also moved from refused-connection ports to accept-and-drop loopback
  listeners, a deterministic offline failure that closes in milliseconds
  on every OS.
- **Chat approval press race**: the gate registered its approval in the
  router only after the button message was sent; a press delivered in
  that window found no entry and was consumed as already-decided, so the
  gate auto-denied after its 60 s timeout (observed on Windows CI). The
  gate now registers first — a press cannot outrun the buttons it
  presses — and unregisters if the message send fails.
- **Swarm+schedule test's two-second horizon**: the cross-feature test
  committed its promise to `now + 2 s`, but the schedule call runs only
  after a spawn_agent turn AND a child sub-task — on loaded Windows
  runners the turn outran the instant, the schedule tool honestly
  refused the past instant (fail-closed), and the store was empty at the
  assert. The instant is now ten minutes out; the synthetic scan drives
  the fire without waiting.

## [0.11.0] — 2026-09-01

M12, landed: the ecosystem terminal — Amparo writes directly into both
siblings. The TUI stores memories in an Engram vault and manages the
Guardrail Console's org policy rules, with credential setup delegated to
the sibling CLIs.

### Added

- **Org policy rules in the Guardrail Console** (sibling change): a
  deny-only, harden-only rules API under `/api/orgs/{id}/policies`
  (list, create, toggle, remove) plus a key-scoped `GET /api/orgs/current`.
  The console proxy rewrites `/check` verdicts for rules matching the
  check's `tool_name` — the engine's verdict is preserved in
  `engine_verdict`, the deny is attributed (`org_rule_id`, reason
  `"org policy: …"`), and the rewrite applies in audit and enforce modes
  alike.
- **`OrgPolicyClient`** (`amparo-tools/src/org_policy.rs`): a thin
  reqwest client for the console's org-rules API, configured from
  `AMPARO_POLICY_KEY` + `AMPARO_CONSOLE_POLICY_URL` (default
  `https://guardrail.elai-intelligence.com`). Every failure degrades to
  a `[policy] not connected` line with a hint — never a panic, nothing
  written locally.
- **The TUI writes into both siblings** (`amparo tui`): `/memory add`
  stores into the resolved memory backend (Engram when wired — verbatim,
  skips surfaced honestly) and `/memory search` retrieves; `/policy
  list|deny|toggle|enforce|audit` drives the console's org rules and
  surfaces the console's error text verbatim (including the Pro-plan
  tier gate). A new `! <command>` escape runs a shell command from the
  prompt — the delegation path for `! guardrail link`.
- **The wizard's 10-answer contract** (`amparo wizard`): step 3 gains
  the console URL (`AMPARO_CONSOLE_POLICY_URL`), and steps 3/4 print
  delegation guidance — the sibling CLI found on PATH
  ("`guardrail link` pairs this machine") or its install one-liner.
- **Real-shape verification**: the Engram adapter's mocks are pinned to
  the live engramd v0.1.4 capture/search response shapes, and the full
  cross-repo path was drilled against a fresh console: gk_ key → check
  through the console proxy → deny rule → `verdict: deny` with the
  engine's verdict preserved → enforce flip reflected in
  `/api/orgs/current` → rule removal restores the engine verdict.

## [0.10.0] — 2026-08-31

M11, landed: adoption — Engram and Guardrail go native, and the web
surface goes live (see the README "Native integrations" section and
`docs/trial-bundle.md`).

### Added

- **The Engram memory backend** (`amparo-tools/src/engram_store.rs`):
  `EngramStore` implements the `Memory` trait over engramd's REST
  surface (`POST /memories/search` → `MemoryEntry`, `POST /memories`,
  `GET /health` probe). Env-gated behind `AMPARO_MEMORY_BACKEND=engram`
  with `AMPARO_ENGRAM_URL` (default `http://127.0.0.1:8787`) and an
  optional `AMPARO_ENGRAM_KEY`; wired at all four registry sites (CLI
  run, MCP serve, chat driver + dispatch). Engram stays behind the
  trait — recommended, never a dependency. A daemon that is down at
  startup or mid-run degrades to the built-in store with one
  `[memory]` warn; never fatal.
- **`amparo doctor` probes both companions**: the Engram check
  (`--engram-url`, `AMPARO_ENGRAM_URL`, or the default when the engram
  backend is configured) does one real `GET /health`; an unreachable
  daemon is a problem that names the degradation. The Guardrail
  check's `--probe` does one real `/check` and reports audit mode
  ("policy: … in audit mode — verdicts are advisory") when the engine
  answers `enforced: false` — a visible mode, never a finding.
- **The web surface went live** at `amparo.ellmstack.dev` — the
  deployment promised in `docs/web-surface.md`: the thin MCP bridge on
  the site box against the v0.9.0 approval seam
  (`--approval-endpoint`, 60 s fail-closed), sandboxed systemd unit,
  Caddy vhost, grey-cloud DNS, Let's Encrypt. The app holds no policy
  keys — the spawned `amparo mcp-serve` process holds them.
  Deployment-only: no crate surface changed beyond the probes above.

## [0.9.0] — 2026-08-31

M10, landed: coordination & surfaces — the blackboard, notifications,
rollback hints, the web-approval seam, and the CLI scheduler (see
`docs/m10-coordination-surfaces.md`).

### Added

- **The blackboard** (`amparo-tools/src/blackboard.rs`):
  `blackboard_read` (Observational) / `blackboard_write`
  (LocalMutating) over `<workspace>/.amparo/blackboard/board.jsonl` —
  append-only rows, last write per key wins on read, every row kept.
  Shared across the delegation chain via the registry clone; rows
  carry no writer identity (arguments are model-chosen) — the trusted
  writer (the loop's task id) rides in the `[bus]` event.
- **`send_notification`** (`amparo-tools/src/notification.rs`): an
  ExternalEffector tool behind a `NotificationTransport` seam —
  `StderrTransport` (default: `[notification] to <destination>:
  <message>`) and `WebhookTransport` (POSTs `{"destination",
  "message"}` JSON). `amparo run --webhook-url URL` wires the
  webhook; the chat hosts wire platform transports; the approval copy
  names the destination; a transport failure is a failed tool
  result, never a task crash.
- **Rollback groups** (W3): `ToolExecutor::rollback(&call)` returns a
  display-only `RollbackSpec { undo, markers }` — computed
  pre-execution, rendered as a `[rollback]` line and in the
  destructive-call approval copy, never executed. Destructive file
  calls back up the previous contents to `<path>.amparo-bak` before
  writing (fail-closed: no backup, no write).
- **WebApprovalGate + `--approval-endpoint`** (W4,
  `amparo-agent/src/web_approval.rs`): the web-approval seam per
  `docs/web-surface.md` (contract v3) — POST the full
  `ApprovalRequest` (arguments, reasons, blast_radius,
  session_label, rollback hint) and poll `{endpoint}/{call_id}` for
  the decision; 60 s timeout, 1 s poll interval, every failure mode
  fails closed. Flag on `amparo run` and `amparo mcp-serve`
  (http(s) validated; mutually exclusive with `--auto-approve`); the
  MCP server's `gate_and_dispatch` gained preflight classification +
  session label so web payloads over MCP carry them.
- **MCP spawn + CLI scheduler** (W5): `amparo mcp-serve
  --max-sub-agents N` registers `spawn_agent` (opt-in; absent or `0`
  = no spawn tool) under one shared budget across the spawn chain;
  `amparo run` always registers `schedule` (top-level tasks only),
  and `due_scan` fires due `cli` promises at run start — concurrently
  with the main task, through the same gate chain, sharing
  provider/policy/approval/flags; `--resume` is a run start too.
  Missed past the grace window = fail-closed. A fire is a fresh task
  with a reduced tool set (no `spawn_agent`, no `schedule`).
- **M10 test sweep (W6)**: the W4 append one-off closed as an
  environmental one-off (24 clean runs, no in-code mechanism); the
  four nanos-based test-root helpers gained per-process sequence
  counters; four new e2e tests (gate-chain fire, denied fire
  executes nothing, resume fire, MCP spawn denied without an
  approver) — 739 tests across the workspace at the W7 gate.

### Changed

- `spawn_agent` is no longer absent from every non-chat surface: the
  MCP server exposes it as an explicit opt-in (`--max-sub-agents N`,
  shared budget, still gated, still denied without an approver). It
  remains absent from `amparo chat dispatch` (no session, no
  delegation chain, no audit).
- `schedule` is no longer chat-only: `amparo run` registers it with
  the process-scoped scheduler — best-effort by design (fires at run
  start only; the chat host keeps the 30 s ticker).

## [0.8.0] — 2026-08-30

M9, landed: verification & QA — the QC council beside policy, the
operator's sweep, and the audit-mode notice (see
`docs/m9-verification-qa.md`).

### Added

- **The QC council** (`amparo-agent/src/qc.rs`): deterministic rule
  auditors that run beside policy — after the candidate final answer,
  before the verification prompt. Four rules over the run's own
  records: `unexecuted_tool_claim` (the answer cites a registry tool
  that never executed), `evidence` (executed calls exceed tool results
  still in context), `cost_honesty` (a `$` inference figure diverges
  from the `chars/4` accounting estimate; leaf runs only), and
  `pii_shape` (residual PII shapes as category counts — values never
  enter a finding, I6). Verdicts are advisory (`Approved` /
  `WithFindings`); findings append to the verification prompt as
  issues to check ("ignore any that are wrong"), and verification
  stays the model's call. `AgentEvent::QcAudit` (render + sink arms),
  a `[qc] audit #N …` tracing line, and in-memory stats counters
  (logged, never persisted).
- **`amparo doctor`** (`amparo-cli/src/doctor.rs`): one
  deterministic, read-only sweep — workspace existence + writability,
  ledger readability (unparseable lines flagged), session checkpoints
  (`Running` older than 7 days = stale), notebook/skills/schedule
  JSONL parseability, policy-engine reachability (TCP dial;
  `--probe` sends one real `/check` and consumes one engine check),
  and the `--chat-config` TOML. Exit 0 healthy / 1 problems / 2
  usage; check lines on stdout, problems on stderr; cron-able. The
  sweep reports and never repairs.
- **The audit-mode stderr notice** (`amparo-policy`):
  `AuditNoticeEngine<E>` prints the exact promised line — `policy
  engine is in audit mode; verdicts are advisory`
  (`docs/trial-bundle.md` §"graceful degradation") — once per
  process, on the first verdict carrying `wire::AUDIT_ONLY_MARKER`.
  Display-only (I1): verdicts pass through untouched. Wired at all
  three surfaces (run, mcp-serve, chat — the driver shares one flag
  across its per-task engines).
- **Session tagging** (`WirePolicyEngine::with_session_id`): every
  `/check` request carries the session id (omitted when absent —
  never sent as null). `amparo run --session-id ID` (default: the
  task id; on `--resume`, the checkpoint's original id) and
  `amparo mcp-serve --session-id ID` (optional); the chat driver
  tags `platform:user_id`.
- **M9 test sweep (W4)**: process-level doctor exit-code matrix,
  notice-once + session-tagged checks against a new `MockPolicy`
  wire responder, default-session-id e2e, and wire-level session-id
  serialization tests — 669 tests across the workspace at the W5
  gate.

### Changed

- None — no M6–M8 surface changed. The verification prompt gains an
  appended section only when findings exist; an empty pass leaves it
  byte-identical.

## [0.7.0] — 2026-08-30

M8, landed: sub-agents and scheduling behind the gate chain — no member
exits the gate chain, and a model is never the approver (see
`docs/m8-swarms.md`).

### Added

- **Token accounting + the cost line** (`amparo-agent`):
  `estimate_tokens` (chars/4, the standard approximation — deterministic
  and uniform across providers; an estimate, not provider billing),
  accumulated per turn into `AgentReport.tokens_estimated` and
  `tool_calls` (counted at dispatch). `AgentConfig.
  cost_per_million_tokens: Option<f64>` (default `Some(3.0)`, a stated
  mid-range model assumption; `None` omits the cost line, counts still
  shown). Rendered as `~$0.04 in inference (estimate, chars/4, $3/1M
  tokens)` — the method states its assumptions.
- **Delegation identity** — host-generated task ids
  (`Agent::with_task_id`); child ids chain (`{parent}.{n}`:
  `sess-123.1`, `sess-123.1.1`). `Checkpoint.parent_task_id:
  Option<String>` (additive, serde default); `LedgerRow.task_id` /
  `parent_task_id` (additive — pre-M8 rows parse with `None`);
  `LedgerSink::new` gains `(task_id, parent_task_id)`.
  `ApprovalRequest.session_label: Option<String>` (`None` at
  non-agent sites) renders `[session] sub-agent sess-123.1 of task
  sess-123 wants to run:` in CLI approvals and the same label in chat
  approval messages — display-only (I1), the gate is unchanged.
  `TaskStarted`/`TaskComplete` gain an optional `task_id` in their
  render lines.
- **`spawn_agent`** (`amparo-agent/src/spawn.rs`, registered by the
  hosts): param `task` (required), trust tier `ExternalEffector` —
  creating an acting entity always asks a human. The child is a fresh
  `Agent` sharing the parent's provider, policy engine, approval gate,
  event sink, registry, `PathPolicy`, checkpoint store, tenant, trust
  ceiling, and config (the rate knob inherits); `child.run()` executes
  inside the tool executor and the child's report returns as the tool
  result. **The budget is shared across generations**
  (`Arc<Mutex<usize>>`) and fails closed: past the limit the result is
  `swarm budget exhausted: N sub-agents max` and no agent exists.
  `SubAgentSpawned` event renders `[spawn] {child} under {parent}: …`
  (render + NotebookSink arms).
- **Preflight `sub_agent`**: `BlastRadius::SubAgent` (severity 2; the
  classes renumber 0–5) with the note `spawns a sub-agent that acts
  under the same gate chain`; `classify` maps `spawn_agent` to it
  (static override). Display-only.
- **`--max-sub-agents N` on `amparo run`** — default 4; `0` means the
  tool is not registered (swarms off); garbage or negative → usage
  error, exit 2. Chat: `UserProfile.swarm: Option<SwarmProfile>`
  (`[users."…".swarm]` with `max_sub_agents`, default 4, and
  `schedule`, default false) — per-profile → per-tenant (I2), unknown
  keys rejected. `spawn_agent` is deliberately absent from the MCP and
  `amparo chat dispatch` surfaces.
- **`schedule`** (`amparo-chat`): a persisted promise, not an
  execution. `ScheduledTask { id, tenant, requester, task
  (PII-stripped at write — I6), at: RFC 3339, status: pending | fired
  | missed, result }` at `<workspace>/.amparo/schedule/<id>.json`
  (atomic tmp + rename); `ScheduleTool` params `at` + `task`, tier
  `ExternalEffector`, preflight `system_wide`; registered only in the
  chat driver, per-profile `schedule: true`, after `with_spawn_agent`
  (children never inherit it). The driver's 30 s ticker (Amparo's
  first background loop; `MissedTickBehavior::Delay`, startup scan)
  runs the pure due-scan (`due_scan(tasks, now, grace)`, 60 s grace)
  and fires each due promise back through the full gate chain as its
  original requester — fresh Agent, continuity off, ledger +
  checkpoints attached, wrapped in `TimeoutApprovalGate` (90 s) over
  the chat gate (60 s): firing while nobody is present auto-denies,
  never silently ahead of the gate. Past the grace window a promise is
  marked **missed** — fail-closed, never fired late — and the
  requester is notified; a fire notifies `[schedule] {id} fired —
  {answer}`. The fire path carries no skills/case-library/spawn/
  schedule — a promise fires one task.
- **`amparo schedule list|cancel`** — the operator's window on the
  queue: `list` prints every promise; `cancel` moves a pending promise
  to `cancelled` (`cancelled by the operator`) — a status change,
  never a deletion. Exit codes follow the `amparo run` contract (2
  usage / 1 runtime / 0 ok).
- **The swarm report** — `swarm: N sub-agent(s) (ids), M tool calls,
  ~$X.XX in inference (estimate, chars/4, $R/1M tokens)` (tool calls
  and tokens include the parent's): `[swarm]` in the CLI, appended to
  the final answer in chat.

## [0.6.0] — 2026-08-29

M7's two deliberate exclusions, landed: the WASM eval sandbox and the
ledger quota lever (see `docs/m7b-sandbox-quota.md`).

### Added

- **`amparo-sandbox`** (new crate): `SandboxRuntime` — fuel-metered,
  deterministic WASM execution (10M fuel, 4 MB module, 4 MB memory,
  30 s wall clock, 4 concurrent; all `with_*` builders), with
  `SandboxError` (invalid wasm, base64, execution, fuel, memory,
  module-too-large, compile, timeout, import-rejection) and
  `SandboxResult` (output, elapsed, fuel consumed, memory used).
  **No imports and no WASI**: a module with any import is rejected at
  compile time, so there is no host surface to escape through. The
  ABI: the module exports `memory` and `axiom_eval(i32 input_ptr, i32
  input_len, i32 output_ptr, i32 output_cap) -> i32`; the input JSON
  is written at offset 0 (input region `0 .. OUTPUT_BASE`, 256 KB) and
  the module writes its JSON output at `output_ptr` (output region
  `OUTPUT_BASE .. OUTPUT_BASE + OUTPUT_CAP`, 256 KB — the two regions
  never overlap), returning bytes written (≥ 0) or −1. New deps:
  `wasmtime` 16 (cranelift only, scoped to this crate), `base64`;
  `wat` dev-only.
- **`eval_wasm` — the tool** (`EvalWasmTool`): `wasm_base64` (required;
  modules arrive base64-encoded — LLMs cannot emit raw bytes) and
  `input` (optional, default `{}`). Trust tier `ExternalEffector`, so
  executing untrusted code always asks a human even though the sandbox
  keeps the reach small. Every failure path returns a failed
  `ToolResult` with an explanatory message, never a panic. Hosts
  register the tool themselves (the `use_skill` precedent): `amparo
  run`, the chat driver's per-task registry, `amparo chat dispatch`,
  and MCP serve (auto-deny there until an operator allows). The
  default registry stays wasmtime-free — pinned at its 17 tools by a
  test.
- **Preflight honesty for the sandbox**: a static override in
  `classify` labels `eval_wasm` `read_only` despite its
  ExternalEffector tier — a pure, bounded computation that observes
  and modifies nothing outside its own sandbox. Display-only (I1):
  the tier, the gate and the approval all stay exactly as they were.
- **`eval_wasm` writes no ledger row** — it never leaves the machine;
  the `LedgerSink`'s "what left the machine" contract is pinned by a
  test.
- **The ledger quota lever** in `amparo-privacy`: `LedgerQuota` and
  `LedgerStore::open_with_quota` (the existing `open` = unbounded,
  unchanged). After write + flush, an over-quota file rotates: the
  oldest rows drop until the file is ~`max_bytes / 2` (one rotation
  per burst, not per append), the rewrite is atomic (tmp + rename),
  and a **rotation marker row** — `LedgerKind::Rotated` with
  `dropped_rows` — is appended as the newest row, so it survives the
  rotation it describes: the reviewer sees exactly what was lost and
  why (I3). The newest data row is always kept. `LedgerSummary` gains
  `rotations` and `rows_dropped`.
- **The quota sidecar**: the enforced bound lives in
  `<privacy>/quota` beside the ledger — a bounded open writes it, an
  unbounded open removes it, so the reviewer surface never reports a
  bound that is not currently enforced. New free functions
  `recorded_quota` and `read_ledger`: `amparo privacy` reads through
  them and never opens a store, so reading never creates, rewrites or
  rotates the ledger.
- **`--ledger-max-bytes N` on `amparo run`** — plain bytes or
  `K`/`M`/`G` suffixes (1024-based); zero, garbage or overflow → usage
  error, exit 2. `amparo privacy` reports the bound:
  `ledger: <bytes> bytes (quota <N> | unbounded)` and, when rotations
  exist, `rotations: <n> (rows dropped <m>)`; the tail renders marker
  rows as `rotated  dropped <m> rows`.
- **Per-tenant chat quotas**: `UserProfile.ledger_max_bytes:
  Option<u64>` in the TOML chat config (zero rejected at load — `None`
  is how a profile says "unbounded"). The driver opens each tenant's
  ledger with the quota; tenants rotate independently, and allowlist
  mode stays unbounded.

## [0.5.0] — 2026-08-29

Instrumentation & hardening (M7): the always-on privacy ledger, session
persistence, and preflight blast-radius classification — three
instruments that answer, for every executed action, *who allowed it and
under what policy* (see `docs/m7-instrumentation.md`).

### Added

- The **privacy ledger** in `amparo-privacy`: `LedgerKind`
  (`network_call` / `pii_strip`), `LedgerRow`, the append-only
  `LedgerStore` (one JSON line per row, flushed; `read_all` skips a
  crash's partial final line; `summary()` aggregates), `privacy_dir`
  and `site_host_only` (scheme + host only — path, query and fragment
  dropped). Rows land in `<workspace>/.amparo/privacy/ledger.jsonl`.
  **Always-on in both hosts**, `--growth` or not: the `LedgerSink` in
  `amparo-agent` (an `EventSink` consumer) tracks `web_search`,
  `fetch_url` and `run_command` from request to execution and appends
  one row per execution attempt — tool, host at most (never a path,
  query or command), outcome (`ok` / `error` / `denied`), and the
  human gate's answer (`human_approved` / `human_denied`, absent when
  no human was asked). A human denial writes its row *immediately*
  (the execution never happens; the denial is the audit answer), a
  policy-blocked call writes none, and observational/local tools write
  none. PII strips are rows too — per-category counts, never values —
  via the new `AgentEvent::PrivacyStripped`. Open/write failures warn
  once per task (`[ledger] …`) and never fail the task.
- **`amparo privacy [--workspace DIR] [--tenant T] [--last N]`** — the
  reviewer's front door: summary first (network calls, PII strips,
  human-approved, human-denied, per-tool and per-category breakdowns),
  then the newest rows (default 10, tenant `cli`). Exit codes follow
  the `amparo run` contract: 2 usage, 1 runtime, 0 ok.
- **Preflight blast radius** in `amparo-agent`: `BlastRadius`
  (`read_only` < `workspace_local` < `network` < `system_wide` <
  `destructive`) and `classify` — the tool's trust tier seeds the
  class; argument inspection can only raise it (a `run_command`
  matching `PathPolicy::check_command_blocked` → `destructive`;
  network-transfer utilities `curl`/`wget`/`nc`/`scp`/`rsync`/`ssh` as
  whole shell words → at least `network`; file tools given an absolute
  path outside the workspace, `/tmp` and `/dev/shm` → at least
  `system_wide`; an unknown tool → `system_wide`). The label rides on
  `ApprovalRequest::blast_radius` (`Option` — non-agent sites pass
  `None`) and renders in the approval copy: a `[preflight] blast
  radius: …` line in CLI approvals, the same line in chat approval
  messages. **Display-only by design (I1)**: classification runs after
  the gate has decided and feeds nothing back — a wrong label can only
  misdescribe the approval text, never allow or block anything.
- **Session persistence** in `amparo-agent`: `Checkpoint` (version 1;
  tenant, `task_id` `sess-<nanos>-<pid>`, `started_at`, PII-stripped
  prompt, `status` running/complete/failed, the conversation without
  system-role messages and with `tool_calls` arguments stripped,
  `LoopState` — `last_tool_name`, `same_tool_count`,
  `empty_turn_retried`, `last_good_summary`, `used_tool_names`,
  `steps_used` — and the stripped final answer), the `CheckpointStore`
  trait and `JsonCheckpointStore` at
  `<workspace>/.amparo/sessions/<tenant ':' → '-'>/<task_id>.json` —
  one file per task, atomic tmp + rename, no index ("latest" = scan by
  `started_at`), corrupt files skipped with a warn. A snapshot saves
  once per loop iteration and at every terminal return (a crash loses
  at most one turn); failures warn, never fail the task. `Agent::resume`
  re-prepends the *current* system prompt (I5 — checkpoints store
  none), restores the loop state, emits `TaskResumed` (never
  `TaskStarted`, and a resume opens no new notebook record — the
  checkpoint is the session trail), and the resumed loop re-judges
  every call through the gate chain. Every fresh CLI run checkpoints
  too — a crash mid-task is resumable.
- **`amparo run --resume`** — resumes the newest incomplete checkpoint
  for tenant `cli`; no task argument (with one → usage error, exit 2),
  none → exit 1, a stale `Running` (> 7 days) is skipped with a warn,
  never resumed. Configuration comes from the current flags.
- **Chat continuity** (W8): when a new chat task starts, the driver
  loads the tenant's newest complete checkpoint and hands the agent
  `continuity_context` — the last 6 user/assistant messages (tool
  messages skipped), truncated, plus the task's last good tool
  summary, as one string — injected via `Agent::with_continuity` as
  **one user-role message** between the system prompt and the task
  prompt. Per-tenant only; chat never reads `Running` checkpoints.
  Chat tasks checkpoint through the same store, one format for both
  hosts.
- **The minimal stderr subscriber** in `amparo-cli` (the `tracing`
  0.1 crate, already a workspace dependency): the binary installs a
  ~50-line `Subscriber` that prints agent `warn`/`error` events to
  stderr as-is (each already carries its `[tag]`), so a checkpoint
  save failure or a corrupt session file is visible to the operator;
  `info`/`debug` stay silent.

## [0.4.0] — 2026-08-29

Controlled growth (M6): the lab notebook, the verification case library,
gated skills with metrics and retirement, and rollup + archival over the
cold archive — one `--growth` opt-in, PII-stripped records, the cold
archive never modified.

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
- The verification case library (M6b): `CaseLibrary` / `EvidenceCase` /
  `evidence_section` in `amparo-agent` (the `Agent::with_case_library`
  seam) and `CaseRetriever` in `amparo-notebook` — prior same-tenant
  records are retrieved into the self-verification prompt as read-only
  observations, never instructions, never in the action loop. `--growth`
  now also reads: on both `amparo run` and `amparo chat`, prior `cli` /
  `platform:user_id` records feed the verification prompt (still off by
  default).
- Gated skills (M6c): `SkillSpec` / `SkillStep` / `UseSkillTool` in
  `amparo-tools`; loop-side expansion in `amparo-agent` (the
  `Agent::with_skills` seam) — `use_skill` expands into its ordered
  steps, each gated, executed and recorded individually, so a skill can
  never grant its steps an exemption. `SkillSet` + `Proposer` in
  `amparo-notebook` (per-tenant adoptions under
  `<workspace>/.amparo/skills/`; recurring VERIFIED sequences distill
  into inert proposals). New `amparo skill add|propose|list|show|adopt`
  CLI: candidates are operator-authored TOML, adoption runs the same
  policy check as a tool call plus human approval rendering the full
  step plan, and nothing is adopted automatically. Skill execution
  requires `--growth` on `amparo run` / `amparo chat` (write + read +
  act — still off by default); without it, `use_skill` is never
  registered.
- Skill metrics and retirement (M6d): per-skill running records derived
  at read time from the run records — uses, VERIFIED rate, mean steps,
  per-step gate denials, last use — with no new write path. A public
  `dry_run_gate` in `amparo-agent` dry-runs a call through the gate
  chain without executing or asking approval (Escalate is not drift);
  the adoption log is now `SkillLogEvent` (`adopt`/`retire` events,
  adopt rows byte-identical to M6c, last event per skill wins the
  fold). Startup drift re-check at every `--growth` task start (`run` +
  `chat`, the task's own policy engine, trust ceiling and registry) —
  a skill whose step plan would now be denied retires before
  registration, with a retire event and a `[growth]` notice (a write
  failure warns, never fails the task). New `amparo skill check|retire`:
  `check` re-runs drift then performance (VERIFIED rate below
  `--min-verified-rate` 0.5 over the last `--window` 20 uses, 3-use
  floor) against every adopted skill, retires by default (`--dry-run`
  reports only), writes `rechecks.jsonl`, and exits 0 even when
  retirements fire (cron-able); `retire <name> [--reason ...]` is the
  operator lever and refuses unknown names. `amparo skill list` shows a
  compact metrics tail; `show` keeps retired skills inspectable with
  status, metrics, the last policy re-check and the full retirement
  history. Retirement is always disable + notify, never deletion.
- Rollup and archival (M6e): a **hot layer** over the cold archive.
  Under `<workspace>/.amparo/notebook/`, beside the untouched
  `records.jsonl`: `hot.jsonl` (same `MemoryEntry` shape, **same id and
  `created_at` as the cold entry**, content = the record capped by the
  ladder — final answer → 300 chars, per-step reasons/summaries → 120,
  first 12 tool calls, task text → 120, then skeleton rungs — so
  `--max-bytes` (default 4096, floor 1024) is a guarantee),
  `hot-hashes.jsonl` (the `(tenant_id, tool_sequence_hash)` dedupe
  index), `rollup.json` (`RollupState` — promotion offset and last-fold
  stamp, atomic saves), `promoted.jsonl` (operator promotions) and
  `rollup.lock` (cross-process; task-start rollups skip silently when
  held, operator commands report busy, stale >10 min reclaimed). Every
  `--growth` task start (`amparo run` and `amparo chat`) promotes the
  cold tail into the hot layer — byte-offset tracked, each record
  scanned once; the predicate: a gate event of interest (approval,
  denial, escalation) or a tool sequence not yet represented — and
  folds daily (24 h since the last fold; rows older than `--days`,
  default 90, fold into the cold archive; operator-promoted rows are
  exempt). `CaseRetriever` now reads the hot layer under `--growth`;
  the cold archive is never modified. New `amparo notebook
  list|promote|rollup` CLI: `list` shows the cold archive newest-first
  with promotion state, `promote <id>` pins one record (idempotent,
  exit 0; unknown ids exit 1), `rollup` forces promote + fold
  (cron-able, exit 0; `--dry-run` reports the same numbers and writes
  nothing). Sync-relay guidance documented in
  `docs/m6-controlled-growth.md` §4.1: the relay moves the hot layer
  and `.amparo/skills/*`; `records.jsonl` stays local and
  authoritative.

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
