# Local models for Amparo: feasibility and bolt-on assessment

Status: research assessment (2026-08-28). Inputs for docs and roadmap;
nothing here is built. Sources at the end. Sister doc:
`m6-controlled-growth.md` (§4, §6 reference this one).

## Verdict

1. **Local models can be officially recommended as of mid-2026 — but
   tier-gated**: a self-host/fallback tier, never the default over
   frontier APIs.
2. **Recommend one primary family: Qwen (3.5/3.6 Instruct,
   Apache-2.0)** — proven native tool calling, permissive license,
   universal serving-engine parser support. GLM-4.7-Flash (MIT) is the
   24 GB-GPU performance pick; Phi-4-Mini or Qwen3-4B for the low-power
   tier.
3. **State hardware honestly.** "Capable" = 16 GB minimum; the real
   recommendation is a 24 GB GPU (GLM-4.7-Flash Q4_K_M, ~8–10 tool
   steps) or 32 GB unified-memory Mac (Qwen3.5-27B / Qwen3.6-35B-A3B via
   MLX). Below 16 GB, the honest ceiling is 2–3-step tasks.
4. **Q4_K_M is safe for the recommended models** (Qwen shows no
   significant tool-call loss measured against fp16); serve via
   llama.cpp/vLLM with model-matched tool-call parsers, or Ollama/LM
   Studio as the easy path — and raise the context size before blaming
   the model.
5. **Frame honestly:** local is ~1–1.5 generations behind frontier APIs
   for long-horizon autonomy, and the multi-turn reliability gap is
   real. Recommend local for privacy/sovereignty, a hard cost cap, and
   well-scoped 3–10-step tasks — not for open-ended autonomous coding.

## What Amparo already accepts

`AMPARO_INFERENCE_URL` takes any OpenAI-compatible endpoint, so the
bolt-on is already true for every mainstream local serving stack:
**Ollama** (as of v0.32.x its `/v1/chat/completions` returns structured
`tool_calls`, with per-family parsers), **vLLM**, **llama.cpp**
`llama-server`, **LM Studio**, **EXO** (multi-Mac clusters). The
Anthropic provider covers API endpoints. There is nothing to build to
"open the agent to any model" — there is documentation to write, and the
model-allowlist env (`AMPARO_INFERENCE_MODEL_ALLOWLIST`) already exists
for pinning.

## Capability tiers (mid-2026)

| Hardware | Recommended model | Quant | Expected tool-loop depth |
|---|---|---|---|
| 24 GB GPU (RTX 4090-class) | GLM-4.7-Flash, or Qwen3.6-27B / Qwen3.5-35B-A3B | Q4_K_M | 8–10 steps, ~52 tok/s — the serious tier |
| 32 GB unified (Mac, MLX) | Qwen3.5-27B, Qwen3.6-35B-A3B, Qwen3-Coder-30B-A3B | Q4 | 6–10 steps, ~39–95 tok/s |
| 16 GB | Apriel-1.6-15B-Thinker, Qwen3.5-14B-class | Q4_K_M | 5–6 steps |
| 8–16 GB | Qwen3.5-9B, LFM2.5-8B-A1B | Q4_K_M | 2–3 steps — document this ceiling |
| low-power / phone | Qwen3-4B, Phi-4-Mini (3.8B) | Q4_K_M | small, single-focus tasks |

## Quantization

Measured (QuantCall, BFCL, bootstrap CI): **Qwen3-1.7B shows zero
significant degradation at Q4_K_M**; the same quant makes a weak base
(Llama-3.2-1B) collapse — on hard tiers its success rate fell 0.57 →
0.34. The rule for our docs: quantizing a strong tool-calling family
costs little; quantizing a weak one makes it worse; and size does not
predict tool-calling ability (a 3.4 GB Qwen3.5-4B out-scored a 25 GB MoE
in one all-Q4 bakeoff).

## Serving gotchas — the operator errors our docs must preempt

- **vLLM** requires `--enable-auto-tool-choice --tool-call-parser
  <model>` (e.g. `qwen3_coder`). Omitting the parser is a *silent*
  failure: the model emits tool JSON, vLLM won't parse it.
- **llama.cpp** needs `--jinja` with the model's correct chat template;
  wrong or missing templates silently break tool calls.
- **Ollama**: the most common reported "tool calling broken" cause is a
  too-small context buffer — raise `num_ctx` first.
- **DeepSeek-R1 and its distills are not reliable tool callers**
  (reasoning-tag interference with parsers); exclude them from the
  recommended list.
- **xLAM-2** tops open BFCL but uses a proprietary tool format that
  breaks through generic OpenAI-compatible layers; skip it in
  recommendations.

## Bolt-on gaps (what could be built later, what will not be)

1. Anthropic-protocol-only local harnesses need a translation proxy —
   not built; documented as out of scope.
2. Server-side misconfiguration is invisible to the client; the
   mitigation is the gotcha documentation above.
3. **Candidate feature:** a `amparo doctor`-style inference probe — send
   one tool-call request and report whether structured `tool_calls` come
   back. Small, valuable, not yet planned.

## Environmental note

Local inference removes cloud round-trips and, for small models on
modern hardware, is comparable or better per token than datacenter
serving plus network. The strongest environmental claim Amparo can make
honestly: an on-device deployment — local model, local memory, no sync
relay — is a fully self-contained configuration, and Amparo supports it
end to end. The memory design's dedupe and rollup (`m6-controlled-growth.md`
§4) keep the relay small when sync *is* used.

## Sources

- Quantization measurement (QuantCall): dev.to/happynood — "Does
  quantization break tool calling?" (BFCL, 3 seeds, bootstrap CI)
- Model bakeoffs: intelligibberish.com (Mar 2026 LM Studio 13-model
  Q4_K_M), zenn.dev/plasmon (Apr 2026), d-central.tech local-agent
  survey, ertas.ai on-device tool-calling (Qwen3/Gemma4/Phi4)
- Serving engines: docs.vllm.ai tool-calling (v0.20+), Ollama changelog
  (openSUSE mirror, v0.32.x July 2026), llama.cpp MCP/agent support
  (PR #26062), LM Studio changelog v0.4.7, Apple WWDC26 MLX sessions
- Multi-turn reliability: ACM CF'26 poster (single vs multi-turn BFCL
  collapse)
