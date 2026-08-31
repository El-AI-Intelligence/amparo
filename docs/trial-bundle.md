# Trial bundle — Amparo download, one month of Engram + Guardrail, then graceful degradation

Status: plan (2026-08-28); Amparo side executed 2026-08-31 (M11 W2,
v0.10.0) — see the execution note in the Amparo section below.
Cross-product: the Amparo side is thin by design; the real work lives
in Engram and Guardrail. Facts verified against the repos on
2026-08-28.

## The offer

Someone downloads Amparo. With the download they get a **one-month trial
of the paid tiers of both companions**:

- **Engram** — personal-tier allowances (5 devices / 10 GiB, sync relay
  included) instead of the free tier (1 device / 1 GiB).
- **Guardrail** — policy **enforcement** (the paid capability) instead of
  the audit-only free tier.

No credit card for the trial. After one month, each product degrades to
its existing free tier — nothing breaks, nothing is silently waived, and
the user is told in advance, in plain terms.

## Why the degradation is genuinely graceful (the seams already exist)

Amparo was built so that both companions are recommended, never
required — and the degradation path is exactly the "never required"
path, already exercised:

- **Guardrail → audit-only free tier.** The wire contract's caller rule
  is *never block on `enforced: false`*. Audit-only mode returns
  advisory verdicts; Amparo keeps running, with the human approval gate
  as the remaining check. The agent goes from "policy enforced" to
  "policy observed" — a real, visible change, and a safe one. One line
  on stderr at startup makes the mode visible ("policy engine is in
  audit mode; verdicts are advisory").
- **Engram → free tier.** The vault keeps working (1 device / 1 GiB);
  multi-device sync stops being available. Amparo's memory trait falls
  back to its built-in default store whenever Engram is absent — the
  same seam. Data already in the vault stays readable; quota is about
  what comes next.
- **Both → absent.** Even if the user uninstalls both, Amparo runs:
  built-in memory store, local policy engine (DenyAll default /
  explicit `--allow-all`). The trial is additive; its expiry cannot
  strand the user.

This is the anti-bait-and-switch design: the trial advertises exactly
what expires, the expiry lands on tiers that already exist, and the
agent's own voice (§7 of `m6-controlled-growth.md`) says it plainly at
signup — "the trial includes policy enforcement and multi-device sync;
after the month, checks continue in audit mode and sync falls back to
one device."

## What each product must ship

### Guardrail

1. **Trial entitlement** — a no-card, 30-day enforcement entitlement
   granted by redemption (the `trialing` subscription status already
   exists in the schema; the entitlement path avoids requiring a
   payment method up front).
2. **Expiry behavior** — entitlement end → audit-only free tier, with
   the degradation notice in the console (the `trialing` badge CSS
   already exists in BillingTab).
3. **Copy** — trial description on the billing tab: what is on, what
   turns advisory, when.

### Engram

1. **Trial entitlement** — a no-card, 30-day personal-tier allowance
   (5 devices / 10 GiB) granted by redemption; billing.rs already
   env-gates Stripe and maps quota columns directly, so the entitlement
   is a quota bump with an expiry timestamp.
2. **Expiry behavior** — allowance drop → free tier (1 device / 1 GiB);
   over-quota data stays readable (read-only, never deleted).
3. **Copy** — vault shows trial status and the expiry date.

### Amparo

Mostly documentation and first-run UX, no degradation code:

1. First-run output points at the bundle when it detects the conditions
   (Engram daemon probe on `127.0.0.1:8787`, Guardrail key env) — the
   probes are already in the UX directives.
2. A `docs/trial.md` or README section describing the offer and the
   expiry behavior, in the voice of §7.
3. The one-line stderr notice when a wire policy engine reports
   audit-mode verdicts (visible mode, not silent).

Execution status (2026-08-31):

- **Item 3 landed first** (M9 W3, v0.8.0): the audit-mode stderr notice
  — one line, once per process, in `amparo run`, `amparo chat`, and
  `mcp-serve` when a wire engine first returns `enforced:false`.
- **Items 1 and 2 landed in M11 W2 (v0.10.0)**: the probes now live in
  `amparo doctor` — the Engram check does a real `GET /health`
  (`--engram-url`, or `AMPARO_ENGRAM_URL`, or the default
  `127.0.0.1:8787` when `AMPARO_MEMORY_BACKEND=engram`), and the
  Guardrail check's `--probe` does a real `/check` that prints the
  audit mode when the engine reports `enforced:false`. The README
  "Native integrations" section describes the offer, the degradation,
  and the expiry. A trial-pointer banner at `amparo run` startup is
  not shipped — the doctor probes are the first-run surface, and the
  bundle itself launches at the reveal.
- **The Engram/Guardrail entitlement work above is unchanged** — still
  plan, still launches at the reveal, not before.

## Redemption flow

One link from the Amparo download page redeems **both** entitlements in
one step (both products are ours; a single redemption endpoint can mint
an Engram trial entitlement and a Guardrail trial entitlement together,
keyed by the same passkey account where accounts already exist). The
download flow reads: install → first run prints the trial pointer →
redeem → both companions light up within the same minute.

## Sequencing

The bundle launches at Amparo's public reveal — not before. Until then
the Engram/Guardrail entitlement work can land independently (both are
internal improvements to existing billing surfaces), and the Amparo-side
docs ship with the reveal.

## Risks and decisions needed

- **Perceived bait-and-switch** — the mitigation is the advance notice
  and the fact that degradation lands on real, working free tiers. This
  is a marketing-line decision too: "one month of the full stack, then
  the free tier — nothing breaks."
- **Trial abuse** (repeated redemptions) — one trial per account; the
  passkey account is the identity anchor.
- **What about self-hosters?** Engram is FSL source-available and
  self-hostable; the trial is for the hosted relay and Guardrail's
  hosted engine. Self-hosted deployments are not degraded by anything —
  that is a feature to state, not hide.
- **User decisions:** no-card vs card trial; 30 days vs 14; whether the
  bundle also includes a discounted upgrade path (e.g. a one-time trial
  discount on the Engram personal plan).
