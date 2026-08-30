# M7b — WASM eval sandbox + the ledger quota lever

Status: **landed** (2026-08-29, W1–W5; docs + release in W6). Everything
described here is built and gated. Amparo is v0.6.0.

---

## 1. Objective

M7's two deliberate exclusions — named in `docs/m7-instrumentation.md`
§6 — are this milestone: **WASM sandboxing** ("its own future M7b") and
the **ledger quota/rotation lever** ("M7b material"). Two headless,
self-contained features:

- **The sandbox** lets an operator ask the agent to run *untrusted
  computation* — a WASM module the model produced — under hard bounds
  (instruction fuel, module size, memory size, wall clock), with an
  approval prompt that names the consequence honestly. The carryover
  screen (`docs/axiom-carryover-screen.md`) pre-screened it: fuel-metered
  execution as a new `eval_wasm` tool at a high trust tier,
  approval-gated like any ExternalEffector.
- **The quota lever** lets an operator bound the always-on privacy
  ledger's file size. It is opt-in, records its own loss (rotation
  marker rows carrying exact dropped counts), and changes nothing for
  anyone who didn't opt in — the M7 audit-completeness guarantee stands
  at the default.

Neither touches the gate chain's verdicts. The sandbox is
observational-plus (it *executes*, but only inside a sealed runtime, and
only after approval); the quota is a storage lever on an instrument.

---

## 2. First principles

M7b inherits the M6/M7 invariants unchanged. The relevant ones,
restated for this milestone:

- **I1 — nothing auto-tunes policy, gate, or prompts.** The preflight
  label for `eval_wasm` is a static, display-only override; the gate
  still asks, the tier stays ExternalEffector, and the override feeds
  nothing back.
- **I2 — per-tenant namespacing.** The chat quota is a per-profile
  (`UserProfile.ledger_max_bytes`) → per-tenant ledger bound; two
  tenants' ledgers rotate independently.
- **I3 — provenance.** Rotation is the one place the ledger loses
  completeness, and it records its own loss: a marker row, newest in
  the file, stating exactly how many rows the rotation dropped. The
  loss of audit completeness is itself an audited event — and it only
  happens because the operator set the quota.
- **I6 — privacy at capture.** `eval_wasm` never leaves the machine, so
  it writes no ledger row — the "what left the machine" contract is
  pinned by a test, not relaxed.

---

## 3. The two features

### 3.1 The WASM eval sandbox (`amparo-sandbox`)

**Precedent**: Axiom-OS's `crates/axiom-sandbox` — `SandboxRuntime`,
wasmtime 16, fuel-metered execution. Amparo ports the *runtime* and
drops the stubs: Axiom's Python/JavaScript/Rust source compilers are
placeholders that fabricate success (flagged in the Axiom-OS audit), and
the HTTP route belongs to a daemon (Amparo ships a library). Neither
came across.

**The bounds** (`SandboxRuntime`, all `with_*` builders):

| Bound | Default | Failure when exceeded |
|---|---|---|
| instruction fuel | 10M | `FuelExhausted` — an infinite loop dies by fuel |
| module size | 4 MB | `ModuleTooLarge` |
| linear memory | 4 MB | `MemoryExceeded` |
| wall clock | 30 s | `ExecutionTimeout` (belt-and-braces: fuel catches infinite loops; the timeout catches pathological wall time) |
| concurrency | 4 parallel evals | permit exhaustion fails the eval, never blocks a caller |

**Hardening beyond the precedent.** A module with *any* import is
rejected at compile time (`ImportRejected`) — there is no host surface
to escape through, so an unsatisfiable import fails loudly instead of
cryptically. No WASI. The engine is per-eval (no cross-eval module
cache, so no shared-state surface). With no imports, no WASI, and a
single-threaded store, the same module and input always produce the
same output (up to NaN payloads) — determinism is stated in the tool
description, not just the doc.

**The ABI** — the model-facing contract, fixed in the `eval_wasm` tool
description:

- the module exports `memory` (min 1 page) and
  `axiom_eval(i32 input_ptr, i32 input_len, i32 output_ptr, i32
  output_cap) -> i32`;
- the runtime writes the input JSON at **offset 0**; the module writes
  its JSON output at `output_ptr` and returns the number of bytes
  written (≥ 0), or −1 for an error;
- the input region is `0 .. OUTPUT_BASE` and the output region is
  `OUTPUT_BASE .. OUTPUT_BASE + OUTPUT_CAP` — **two regions, 256 KB
  each, never overlapping** (input must fit inside the module's memory
  — `MemoryExceeded` — *and* the input region — `InputTooLarge`;
  output beyond the cap is refused).

This deliberately replaces the Axiom ABI, whose return value doubled as
exit code and whose output lived at `memory[0]` *overlapping* the
input. Note the deviation from this plan's own draft, which placed the
input at `INPUT_BASE` and the output at 0: the implemented contract
writes the input at offset 0 and the output at `OUTPUT_BASE`
(256 KB) — same two-region shape, opposite ends, so a module that
reads offset 0 gets the input, exactly as the schema description
states.

**`eval_wasm` — the tool.** `EvalWasmTool` (in `amparo-sandbox`)
implements `ToolExecutor`: params `wasm_base64` (string, required —
LLMs cannot emit raw bytes) and `input` (string, optional, default
`{}`), **trust tier `ExternalEffector`** (deny-by-default posture:
executing untrusted code always asks a human, even though the sandbox
keeps the reach small). Execution wraps the synchronous eval in
`spawn_blocking` with the wall-clock timeout; every failure path —
missing parameter, bad base64, compile error, fuel exhaustion, timeout
— returns a failed `ToolResult` with an explanatory message, never a
panic.

**Registration is host-side** — the `use_skill` (M6c) precedent:
hosts add the tool to the registry they build, never
`default_registry_with_policy` (which would drag wasmtime into
`amparo-tools`; a test pins that registry stays at its 17 tools). Four
sites: `amparo run` (cli/run.rs), the chat driver's per-task registry
(both tenant modes), `amparo chat dispatch` (one-shot dispatch), and
the MCP serve registry (where approval defaults to auto-deny, so
`eval_wasm` is refused until an operator allows — correct). The gate
flow is unchanged: ExternalEffector → human approval before execution,
exactly like `fetch_url`.

**Preflight honesty.** The tier-seeded classifier would label
`eval_wasm` *network* ("reaches the network") — wrong copy. One static
override in `classify`: `eval_wasm` → `read_only` (a pure computation,
bounded; it observes and modifies nothing outside its own sandbox).
This is a per-tool static override, not argument inspection — the
"argument checks only raise" rule is untouched — and it is display-only
(I1): the tier stays ExternalEffector, the approval still asks, and the
override feeds nothing back into the gate.

**The ledger interaction — none, and pinned.** `eval_wasm` never leaves
the machine, so the M7 `LedgerSink` correctly writes no row for it. A
unit test pins that an `eval_wasm` ToolExecuted event produces no
NetworkCall row: the ledger's "what left the machine" contract holds
under the new tool.

### 3.2 The ledger quota lever (`amparo-privacy` + hosts)

M7's `LedgerStore` is unbounded by design — an audit log; the reviewer
wants completeness. The lever is opt-in and records its own loss:

- **`LedgerQuota { max_bytes }`** and
  **`LedgerStore::open_with_quota(path, quota)`**. The existing `open`
  is unchanged (no quota — every current caller keeps M7 behavior; a
  test pins that the unbounded default never rotates).
- **Enforcement in `append`**: after write + flush, if the file
  exceeds `max_bytes`, rotate — read all rows, drop the oldest N so the
  file shrinks to ~`max_bytes / 2` (no thrash: one rotation per burst,
  not per append — pinned by a test), rewrite atomically (tmp +
  rename, the M7 checkpoint pattern; a test reads the directory
  mid-contract and never sees a tmp or partial line), then append a
  **rotation marker row**: `LedgerKind::Rotated` carrying
  `dropped_rows`. The marker is the newest row, so it survives the
  rotation it describes — the reviewer sees exactly what was lost and
  why. The newest data row is always kept, even when it alone exceeds
  the half-size target: the bound is on the file, never on one row's
  legibility.
- **Marker arithmetic, honestly stated.** Each marker records exactly
  how many rows *its* rotation dropped. A later rotation rotates the
  earlier marker off like any other row, so the counts are per-marker
  (the current window), not a cumulative lifetime total — `amparo
  privacy` sums the surviving markers. In steady state under a tiny
  quota the file holds [newest row, marker recording `dropped 2`] — the
  previous row and the previous marker.
- **The quota sidecar.** The bound itself is not a ledger header (the
  file stays pure rows). It lives in `<privacy>/quota` beside the
  ledger: a bounded open writes it, an unbounded open removes it — the
  reviewer surface never reports a bound that is not currently
  enforced. `amparo privacy` reads it via `recorded_quota` and the rows
  via the free `read_ledger`, *before* anything could touch the store —
  reading never opens a store, so it never creates, rewrites or
  rotates the ledger (the M7 read-only contract, still pinned by the
  `privacy_without_a_ledger_reports_empty` e2e).
- **Surfaces**:
  - **CLI**: `--ledger-max-bytes <N>` on `amparo run` — plain bytes or
    `K`/`M`/`G` suffixes (1024-based); garbage, zero, or overflow →
    usage error, exit 2. `amparo privacy` gains the usage line: `ledger:
    <bytes> bytes (quota <N> | unbounded)` and, when rotations
    exist, `rotations: <n> (rows dropped <m>)`; the tail renders marker
    rows as `rotated  dropped <m> rows`.
  - **Chat**: `UserProfile.ledger_max_bytes: Option<u64>` in the TOML
    chat config — per-profile → per-tenant (I2); zero is rejected at
    load (`None` is how a profile says "unbounded"). The driver opens
    the per-tenant ledger with `open_with_quota`; allowlist mode has no
    profile, so it stays unbounded.
  - **MCP serve** has no ledger (M7 wired run + chat only) — out of
    scope.
- **Single-writer posture.** Rotation is safe under one process per
  ledger: CLI runs are one process per workspace, chat is one driver
  process per workspace. The same assumption the notebook's `JsonlStore`
  already makes — not a new risk class.

**Landed**: W1 sandbox crate + runtime, W2 `eval_wasm` tool +
registration, W3 preflight override + gate e2e + no-ledger-row pin, W4
quota core + rotation, W5 CLI flag + chat wiring + both e2e proofs.

---

## 4. The audit posture, per feature

The acceptance test, made concrete for each feature — and how the
suite proves it:

- **Sandbox** — *an operator can ask the agent to run untrusted
  computation, the approval prompt names the consequence honestly, the
  run is fuel/memory/time-bounded, and failure modes are loud.* Proven
  by: unit tests (echo round-trip through the real ABI, fuel death on
  an infinite loop, timeout with huge fuel, import/export/oversize/
  bad-magic/bad-base64 rejections, input/output region refusals,
  concurrency cap) and e2e (`run_executes_approved_eval_wasm_with_
  read_only_radius` — the approval prompt carries the read_only radius
  and the sandbox limits, the approved run executes and the result
  feeds the loop; `run_denied_eval_wasm_executes_nothing_and_writes_
  no_row` — a denial executes nothing and lands no ledger row).
- **Quota** — *an operator who sets a quota gets a bounded ledger that
  records its own loss, while the default stays unbounded.* Proven by:
  unit tests (over-quota append rotates with the marker surviving as
  newest, dropped counts match, one-rotation-per-burst, atomic rewrite,
  unbounded default unchanged, sidecar write/remove/read-back, read
  without opening) and e2e (`run_with_tiny_ledger_quota_rotates_and_
  privacy_reports_it` — a 250-byte quota over 8 calls leaves [1 row +
  marker(dropped 2)] under 600 bytes, and `amparo privacy` reports
  `quota 250`, `rotations: 1`, `rows dropped 2`; `directory_mode_ledger_
  quotas_rotate_independently_per_tenant` — a bounded and an unbounded
  tenant rotate independently, and the sidecar exists only under the
  bounded one).

---

## 5. Storage layout

The quota lever adds one file to the M7 layout; the sandbox adds no
storage at all (per-eval engine, nothing persisted):

```
<workspace>/.amparo/
  privacy/ledger.jsonl           # M7: the ledger — append-only, always-on
  privacy/quota                  # M7b: the enforced bound in bytes —
                                 #   present iff a quota is set; removed by
                                 #   an unbounded open
  sessions/<tenant>/<task>.json  # M7: checkpoints
  notebook/                      # M6: cold archive, hot layer, rollup
  skills/                        # M6: adopted skills, logs, rechecks
```

---

## 6. Deliberately excluded

- **Language compilation** — Axiom's Python/JavaScript/Rust source
  compilers are stubs that fabricate success (Axiom-OS audit). Only
  WASM binaries run, base64-encoded on the way in.
- **An HTTP sandbox route** — Axiom's daemon endpoint; Amparo is a
  library + tool, and the tool face is the gate chain, not a raw
  route.
- **WASI** — no filesystem, no network, no clock inside the sandbox,
  ever.
- **wasm32-wasi compilation via `rustc`** — requires toolchain
  assumptions; modules arrive pre-compiled.
- **Age-based ledger retention** — a `--max-age`/days lever was
  considered and cut: size is the resource the operator actually
  bounds (disk), and a size quota already records its own loss.
  Nothing prevents a later retention lever riding the same marker-row
  contract.
- **A quota on the cold archive** — `records.jsonl` stays untouched by
  everything (M6e); the ledger quota is the ledger's lever only.
- **MCP ledger wiring** — M7's ledger covers `amparo run` and
  `amparo chat`; the MCP surface remains ledger-free, and its approval
  default (auto-deny) means `eval_wasm` is refused there until an
  operator allows.

---

## 7. I1–I6 audit

| Invariant | How M7b honors it |
|---|---|
| I1 — no auto-tuning | The `eval_wasm` → `read_only` preflight override is display-only: the tier stays ExternalEffector, the approval still asks, and the override feeds nothing back into the gate. The quota never changes a verdict — it bounds a log file. |
| I2 — per-tenant namespacing | The chat quota is per-profile in the tenant directory; two tenants' ledgers (each already per-workspace) rotate independently, and the marker rows carry the surviving row's tenant. |
| I3 — provenance | Rotation's loss is recorded by the marker row that survives it, with an exact `dropped_rows` count. The loss of audit completeness is itself an audited event — and only happens because the operator set the quota. |
| I4 — revocable | The sandbox has nothing to revoke (no cache, no persisted state, per-eval engine). The quota is a file the operator can delete; removing it restores unbounded behavior at the next open. |
| I5 — prompt immutable | Neither feature touches prompts: the `eval_wasm` schema description is the one new model-facing string, authored once and shipped, not tuned at runtime. |
| I6 — privacy at capture | `eval_wasm` writes no ledger row (nothing left the machine — pinned by test). The quota never changes *what* a row stores: host at most, counts never values, the same PII rule as M7. |

---

## 8. Test inventory

The M7b surface is covered by these suites, all green at the W6 gate
alongside the untouched M7 and M6 regression suites:

- `amparo-sandbox/src/lib.rs` — 19 runtime tests: validation accept/
  reject (magic, version, length, size), ABI round-trip, base64
  decode/garbage, zero-byte output, module error codes, import
  rejection, missing exports, fuel death on an infinite loop,
  input-beyond-memory and input-beyond-region refusals,
  output-beyond-cap refusal, wall-clock timeout with huge fuel,
  concurrency cap.
- `amparo-sandbox/src/tool.rs` — schema contract test +
  `default_registry_stays_at_17_without_eval_wasm` (host-side
  registration never touches the default registry).
- `amparo-agent/src/preflight.rs` —
  `eval_wasm_is_read_only_despite_the_effector_tier` (label read_only,
  tier unchanged).
- `amparo-agent/src/ledger_sink.rs` — `eval_wasm_writes_no_ledger_row`.
- `amparo-privacy/src/ledger.rs` — 8 new tests on top of the M7 seven:
  rotate-with-marker-surviving-newest, dropped counts, one-rotation-
  per-burst, atomic rewrite, unbounded-default-never-rotates, summary
  rotations/rows-dropped, sidecar write/remove/read-back, read-without-
  opening (and the M7 atomic test now asserts the sidecar too).
- `amparo-cli/src/run.rs` — `parse_bytes` units (plain + K/M/G +
  suffixes + `u64::MAX`; garbage table incl. `""`, `K`, `0`, `4KB`,
  `4.5K`, `12 K`, `-1`, overflow; flag parse + exit-2-shaped messages).
- `amparo-chat/src/config.rs` — quota parse/default-to-unbounded,
  zero-rejection (`ConfigError::Quota`).
- `amparo-chat/src/driver.rs` —
  `directory_mode_ledger_quotas_rotate_independently_per_tenant`
  (bounded tenant: 1 row + marker(dropped 2); unbounded tenant: all 4
  rows, no marker; sidecar only under the bounded tenant).
- `amparo-cli/tests/cli_e2e.rs` — 4 binary-level tests: approved
  `eval_wasm` with the read_only radius line, denied `eval_wasm`
  executing nothing, tiny-quota rotation + `amparo privacy` reporting
  (`quota 250`, `rotations: 1`, `rows dropped 2`), usage-error exit 2.

---

## 9. Risks and open questions

- **wasmtime is the heaviest dependency Amparo has taken** — deliberate
  and pre-screened (carryover screen); scoped to `amparo-sandbox` so
  `amparo-tools` consumers never pay for it. Compile time rises; the
  gates absorb it.
- **Sandbox escape** — wasmtime is the reference implementation; the
  posture is defense in depth (no imports, no WASI, per-eval engine,
  four bounds), and the tool remains approval-gated regardless. A
  sandbox flaw degrades to "same reach as any local tool", not to a
  new boundary.
- **Quota rotation races** — single-writer assumption, same as the
  notebook; marker rows make even a botched rotation legible. Quota is
  default-off, so the v0.5.0 guarantee stands unless opted into.
- **Marker counts are per-window, not cumulative.** A later rotation
  rotates the earlier marker off; the surviving marker states exactly
  what *its* rotation dropped. A reviewer wanting a lifetime dropped
  total should read the sidecar + current markers together and accept
  the window — or set a quota large enough that nothing rotates. This
  is the honest cost of a bounded audit log, stated rather than
  papered over.
- **ABI deviation from the plan draft** — the input lands at offset 0
  and the output at `OUTPUT_BASE` (256 KB), not the other way around;
  the two-region no-overlap contract is identical. The implemented
  contract is what the tool description and this doc state; modules
  written against the draft fail loudly (no output at the expected
  address), and the e2e pins the happy path.
- **Preflight override misread as gate change** — the override is
  display-only (I1); a test pins that `classify` returns `read_only`
  while the tier stays ExternalEffector and the approval still asks.
