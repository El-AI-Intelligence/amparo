# Amparo pre-reveal security audit — 2026-08-31

Clean-sheet audit of the Amparo workspace at v0.10.0 (0f5c8e8), run before the
GitHub release / public reveal. Four parallel auditors covered four surfaces;
every MED finding was independently verified against the code by the operator
before landing here. Nothing below overstates the code: VERIFIED = the whole
path was read; PLAUSIBLE = the path exists but an exploit was not demonstrated.

## Tally

| Surface | CRIT | HIGH | MED | LOW |
|---|---|---|---|---|
| Gate chain (agent/MCP/chat/policy) | 0 | 0 | 2 | 3 |
| Secrets & auth | 0 | 0 | 2 | 4 |
| Ledger / PII / persistence | 0 | 0 | 3 | 3 |
| Supply chain / web seam / assets | 0 | 0 | 5 | 5 |
| **Unique, deduplicated** | **0** | **0** | **11** | **14** |

## MED findings (all to be fixed before reveal)

1. **wire.rs accepts an `allow` verdict on HTTP 500.** The fail-safe
   passthrough meant to let a non-2xx carry the canonical `escalate` verdict
   accepts *any* verdict, including `allow` — a misbehaving engine converts a
   policy failure into an execution. VERIFIED. Fix: on non-2xx, only
   non-allow verdicts pass through; everything else maps to Escalate.
2. **`web_search` / `fetch_url` registered at `Observational`.** Network I/O
   classified read-only: policy `allow` executes with no human approval, and
   `fetch_url` takes arbitrary URLs with no host allowlist (SSRF probe
   surface). Contradicts the repo's own blast-radius vocabulary. VERIFIED.
   Fix: `fetch_url` → `ExternalEffector` (human approval for arbitrary
   fetches); `web_search` → new `Network` tier (honest classification,
   policy-decidable, approval only via Escalate).
3. **`git` and `run_build` subprocesses inherit the full process env** —
   every `AMPARO_*` key reaches git hooks and build scripts; `run_command` is
   the only spawn that clears env. VERIFIED. Fix: surgical secret-stripped
   env for these spawns (retain toolchain vars, remove the `AMPARO_*` key set).
4. **`memory_store` persists content with no PII strip** — the only
   agent-facing persistence path that forwards raw content (to Engram over the
   wire when the adapter is enabled) while every automatic path strips.
   VERIFIED. Fix: strip at the tool, matching the notebook's discipline.
5. **`blackboard_write` persists raw values** to `.amparo/blackboard/board.jsonl`
   with no strip. VERIFIED. Fix: same strip at the tool.
6. **All workspace state files are 0644/0755** — sessions (full
   PII-stripped transcripts), ledger, notebook, blackboard, schedule; no
   `set_permissions`/umask anywhere in the crates; a local user on a shared
   box can read them and can forge ledger rows. VERIFIED. Fix: 0600 files /
   0700 dirs at every creation site (Unix; Windows no-op).
7. **`Cargo.lock` contains a quinn HTTP/3 stack no manifest enables** —
   extra attack surface if it compiles in, or a stale lock. PLAUSIBLE. Fix:
   confirm with `cargo tree -i quinn` and prune the lock.
8. **wasmtime pinned at 16.0.0 (2024)** at the sandbox trust boundary — two
   years of sandbox-escape fixes missed (wrapper itself is hardened: fuel,
   stack cap, import rejection, safe APIs only). VERIFIED. Fix: bump.
9. **Deploy docs pin the box build at v0.7.0** while the approval seam ships
   in v0.9.0 — an operator following the binding doc ships a binary two
   releases stale, silently disabling web approvals. VERIFIED. Fix: repin
   docs + `web/deploy/` to v0.10.0.
10. **SSE carries the operator token in the URL query** (`web/public/app.js`)
    — the token lands in access logs and browser history. VERIFIED. Fix:
    one-shot `?ticket=` (the Engram `ws/ticket` pattern).
11. **The `/approvals` POST is unauthenticated and overwrites** a pending
    approval's display copy — a loopback-capable process could swap benign
    copy over a destructive call before the operator decides. Decisions are
    token-gated, so PLAUSIBLE not VERIFIED. Fix: reject duplicate
    `call_id` registrations; cap the pending map.

## LOW findings

1. `site_host_only` retains URL userinfo (`https://user:pass@host` survives
   the row's `site` field) — VERIFIED. Fix: strip userinfo.
2. Policy timeout covers the send, not the body read — a stalled engine
   hangs the loop past the 60s budget (fail-hang, not fail-open). VERIFIED.
   Fix: time the body read too.
3. `WebApprovalGate` interpolates the model-supplied `call_id` into the poll
   URL unencoded — first-party endpoint verified fail-closed (strict charset),
   PLAUSIBLE against non-conforming endpoints. Fix: percent-encode.
4. Approval decisions keyed solely by `call_id` — replayable only against
   endpoints that keep stale decisions; shipped surfaces verified replay-safe.
   Documented; no code change.
5. Engram adapter defaults to plaintext `http://127.0.0.1:8787`; nothing
   constrains `AMPARO_ENGRAM_URL` to loopback, so a remote URL sends the key
   and unstripped content in the clear. VERIFIED. Fix: warn when the URL is
   non-loopback http.
6. The approval seam's loopback-by-convention has no host validation.
   Documented by design (the gate holds no secrets); the Caddy 403 is real.
   No code change; documented.
7. `auto_redact_pii: false` disables the inference-time strip (config by
   design; persistence strips stay unconditional). Informational.
8. PII strip patterns do not classify API-key shapes — a user-pasted key in a
   prompt persists verbatim through checkpoints/notebook. VERIFIED. Fix: add
   common key-shape patterns to the strip engine.
9. `deploy.sh` never checks the unit is active after restart (silent-failure
   mode). Fix: `systemctl is-active` check.
10. Published crates don't carry `rust-version` despite the workspace MSRV
    1.85 claim. Fix: add to workspace.package + inherit.
11. Contract-version stamp drift in `web/README.md` / deploy READMEs (v1 vs
    v3, "until v0.9.0 ships"). Fix: fold into finding 9's doc pass.
12. `--approval-endpoint` URL validation is a prefix check only; downstream
    reqwest fails closed, so no exploit. Fix: require a host segment.
13. No rate limit on `/approvals` POST or authed API (queue spam; token brute
    force infeasible at ~190 bits). Fix: cap the pending map.
14. `web/` is untracked and not gitignored — a plain `git add .` commits the
    whole web surface; the deploy README's `git checkout -- web/` rollback
    instruction cannot work for a never-committed directory. The ignore
    decision is the operator's (see note below).

## Clean areas (verified whole-path)

- **Gate chain**: every dispatch in every crate passes `gate_call` /
  `gate_and_dispatch`; no direct execute path exists outside a gate; children
  inherit policy/gate/privacy/ceiling and the budget fails closed; all four
  approval gates fail closed on timeout (60s); Escalate never auto-resolves;
  `--auto-approve` is explicit, default-off, mutually exclusive with the web
  seam; action-loop prompt is a const, immutable, tool output never mutates
  it; no auto-tuning of policy/gates/prompts.
- **Fail-closed policy client**: transport error, timeout, malformed body,
  unknown verdict all → Escalate; `limit_reached` hard-denies even in audit
  mode; `enforced:false` never stamped as authoritative.
- **Tool→tier mapping**: correct for every tool except finding MED-2.
- **Secrets**: env-only, no key in any argv, no key in any log/error path
  (Telegram/Discord redact), no hardcoded keys, git history clean (75 commits
  scanned), MCP is stdio-only with deny-all defaults.
- **Ledger**: counts-never-values holds — the only `LedgerRow` constructors
  in the repo carry no command text, payloads, or message bodies; rotation is
  itself audited; the cold archive `records.jsonl` is append-only/read-only
  (verified writers and the rollup reader).
- **Persistence stripping**: checkpoint, notebook, and Engram-facing paths
  strip PII unconditionally before write (the gaps are findings MED-4/5).
- **Supply chain**: rustls-only (zero OpenSSL/native-tls in the lockfile),
  no build scripts, zero `unsafe` in 76 first-party source files, curated
  tokio features, no git/vendored sources.
- **Web app** (Kimi build, in `web/`): loopback bind, token required to
  start, constant-time compare, `textContent`-only DOM (no XSS), path
  traversal guards, sandboxed systemd unit, Caddy blocks `/approvals*`.

## Remediation plan

All 11 MEDs and the code-able LOWs (1, 2, 3, 5, 8, 9, 10, 12, 13) fixed
before the reveal; both gates green after each batch; cold archive
byte-identical. The `web/` fixes edit the untracked tree on disk and deploy
to the box; the `.gitignore` decision for `web/` is the operator's.

### Fix log

- **2552ef5** — MED-1 (wire client: only an `escalate` verdict passes
  through failure statuses; the body read gets its own timeout), LOW-2
  (body-read timeout), LOW-3 (`call_id` percent-encoded into the poll
  URL), LOW-12 (`--approval-endpoint` requires scheme + host).
- **d1cbc2e** — MED-2 (`fetch_url` → ExternalEffector; `web_search` →
  new `Network` tier; `--trust-ceiling network`; preflight maps `Network`
  → `BlastRadius::Network`), MED-4 (`memory_store` strips PII), MED-5
  (`blackboard_write` strips PII), LOW-8 (API-key shapes in the strip
  engine, run before the digit patterns).
- **20793d2** — MED-3 (git and build spawns inherit a stripped
  environment via `amparo_tools::process_env::secret_free_env`:
  `env_clear` + re-inject the current environment minus `AMPARO_*` keys,
  so PATH/CARGO/RUSTUP toolchain resolution still works; the MCP client
  spawn keeps the operator environment — the audit scoped git+build, and
  an operator-configured MCP server may need its own env), MED-6
  (`amparo_privacy::perms::owner_only` enforces 0600 files / 0700
  directories at every persistence site: ledger + rotation sidecar,
  notebook store, skills append, rollup lock + hot layer, blackboard,
  session checkpoints, chat schedule. Directories are hardened only when
  the call itself created them — a pre-existing parent is not ours to
  re-permission; files we just opened are always hardened; mode carries
  to the final file through the write-tmp-then-rename pattern. Residual:
  state directories created by pre-fix versions keep their old modes
  until recreated; the cold archive's content bytes are untouched).
- **6f1886f** — MED-8 (wasmtime 16 → 34.0.2: the newest release whose
  `rust-version` (1.85) still matches the workspace MSRV — 35+ needs
  1.86 and 48.0.1 needs 1.95, so "latest" would break the standing MSRV
  claim; the embedder API is feature-gated behind `runtime` since v18,
  added beside `cranelift`; 26 sandbox tests green on 34), MED-7
  (verified and closed without a code change: the quinn entries in
  `Cargo.lock` are reqwest's optional `http3` feature dependencies
  recorded by cargo's feature-independent lockfile — no manifest
  enables the feature and `cargo tree -i quinn` is empty across all
  targets, so the HTTP/3 stack never compiles and the lock cannot be
  pruned; the entries stay byte-identical), LOW-10 (every crate
  manifest carries `rust-version.workspace = true` inheriting the 1.85
  MSRV), LOW-1 (`site_host_only` strips URL userinfo — only what
  follows the last `@` reaches a ledger row), LOW-5 (`EngramStore::new`
  warns on stderr when the engramd URL is non-loopback plaintext http;
  127/8, `localhost` and `[::1]` stay quiet).
- **e03bb43** — MED-10 (SSE auth: the operator token never rides a URL —
  `POST /api/tickets` issues a one-shot 30-second ticket over the authed
  API and the events endpoint consumes exactly one; the `?token=` path is
  gone; the SPA re-issues on drop with bounded retries), MED-11 (the
  `/approvals` POST rejects non-pollable `call_id`s with 400, duplicates
  with 409, and caps the pending queue at 64 with 429 — the gate fails
  closed on every refusal, verified in `web_approval.rs`), LOW-13 (POSTs
  on `/approvals` and the authed API rate-limited per source address,
  120/min → 429), MED-9 + LOW-11 (contract bumped to v4: the binary pin
  moves v0.7.0 → v0.10.0 in the contract and the deploy docs, the mock
  wording is retired, the contract stamps agree at v4, and the deploy
  README's rollback text no longer prescribes a `git checkout` that an
  untracked `web/` cannot serve), LOW-9 (`deploy.sh` aborts loudly when
  the unit is not active after restart). The `web/` tree itself is
  untracked by design and edited on disk only (server.mjs, app.js,
  deploy files, test harness) — verified by the local smoke acceptance
  plus a drill of the new semantics (ticket one-shot 401 on replay,
  `?token=` rejected, duplicate 409, rate limit 429).
- **box deploy (2026-08-31)** — the v4 web tree is live on the site box:
  `rsync` of `web/` to `/srv/amparo/src/web/` followed by the on-box
  `deploy.sh` (idempotent; the env file is preserved; the unit came back
  active — the LOW-9 `is-active` check passed). Verified live: the unit
  is active, `/api/health` reports `contract: 'web-surface.md v4'`, the
  one-shot ticket drill is green on the deployed server (first use
  passes auth, replay → 401, `?token=` → 401), and the full on-box
  acceptance ran **BOX ACCEPTANCE OK** (statics, auth, checkpoint +
  resume, task e2e, SSE, gate harness approve/deny/double/expire,
  ledger/sessions/schedule/notebook views, restart survival, Caddy
  valid). The audit's web-surface findings are closed in production.
- **filesystem append completion semantics (2026-08-31)** — the
  parallel-load flake in `write_file_append_backs_up_the_existing_file`
  (#174) traced to a real tool defect: the append arm's `tokio::fs::File`
  + `write_all` resolved before the write(2). Tokio's `File::poll_write`
  returns Ready the moment the write is *dispatched* to the blocking
  pool, and dropping the file detaches the task — `write_file` in append
  mode could report success before the append reached the file. The
  append is now a join-awaited `spawn_blocking` std write (the overwrite
  arm's `tokio::fs::write` is join-awaited by construction, which is why
  only the append path flaked); the hand-rolled tokio-File write pattern
  was grep-verified workspace-unique. Verified: 8-way stress 1600/1600
  green (pre-fix ~1%), both gates green.

## Operator note

The audit is the prerequisite, not the decision: making the repo public and
publishing the GitHub release stays gated on explicit operator instruction.
