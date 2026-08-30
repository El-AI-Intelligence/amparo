//! Amparo Agent — the loop that turns a prompt into a completed task.
//!
//! Originally part of Axiom-OS (MIT, Copyright (c) Pixel Phantom AI);
//! ported to Amparo and relicensed Apache-2.0. See NOTICE at the repo root.
//!
//! # The loop
//!
//! Axiom's `run_agent_task` drove text-format ReAct turns (`parse_react_turn`
//! over `Thought:`/`Action:`/`Action Input:` text). Amparo drives the model's
//! **native function-calling protocol** instead: the registry's tool schemas
//! go out as OpenAI-shaped `tools`, and the model's `tool_calls` come back
//! already structured. The loop mechanics Amparo keeps are the ones that made
//! Axiom reliable, all preserved:
//!
//! - **max steps** with conversation trimming (system prompt + last 30
//!   messages),
//! - **parallel tool batches** with `MAX_TOOL_RETRIES = 2` per tool,
//! - the **one-shot shortcut** (simple "open X" actions with no compound
//!   words complete immediately when every call succeeds),
//! - the **same-tool loop guard** (2 consecutive = soft nudge, 3 = hard stop),
//! - **empty-turn recovery** (one retry with a format nudge, then finalize
//!   from the last successful tool summary — or fail honestly),
//! - **self-verification** (one extra VERIFIED/INCOMPLETE turn; INCOMPLETE
//!   re-enters the loop, and an inference error defaults to VERIFIED so a
//!   finished task never wedges).
//!
//! # The gate chain
//!
//! The Axiom gate chain (trust ceiling → QC council → capability store →
//! ELLM constitutional → policy → human approval) collapses to the pieces
//! Amparo actually owns — the QC council, capability store, FrameGraph
//! ingestion, screen-state injection and ELLM constitutional audit are
//! axiom-daemon specifics and stay behind:
//!
//! 1. **Trust ceiling** — tools at tiers above [`AgentConfig::trust_ceiling`]
//!    are blocked outright.
//! 2. **Policy gate** — the deny-by-default seam
//!    ([`amparo_policy::PolicyEngine`]). Deny hard-blocks, Escalate routes to
//!    human approval with the fired reasons attached, Allow proceeds. A
//!    remote engine speaks the open wire protocol
//!    ([`amparo_policy::wire::WirePolicyEngine`]); with no engine configured
//!    the agent refuses every call.
//! 3. **Human approval** ([`ApprovalGate`]) — required for tools at tier ≥
//!    `ExternalEffector` *or* a policy Escalate. Amparo's built-in gates
//!    auto-deny: no wired human means no execution. Embedders implement the
//!    trait to ask a real person (console UI, CLI prompt, remote approver).
//!
//! Axiom's 60-second approval timeout is the *gate's* responsibility — a
//! human-facing gate should auto-deny on timeout, exactly as Axiom did.
//!
//! # Observability and privacy
//!
//! Every decision the loop makes is emitted as an [`AgentEvent`] through the
//! [`EventSink`] seam (an in-memory log with a broadcast channel by
//! default). When a [`amparo_privacy::PrivacyPolicy`] is attached, each turn
//! is privacy-checked, and PII is stripped before inference and restored in
//! the response (Secure Minions), with per-message placeholder namespaces so
//! tokens from different messages cannot collide.

#![warn(missing_docs)]

pub mod agent;
pub mod approval;
pub mod cases;
pub mod events;
pub mod ledger_sink;
pub mod preflight;
pub mod session;
pub mod tokens;
mod sse;

pub use agent::{
    dry_run_gate, extract_target, Agent, AgentConfig, AgentReport, AgentStep, DryRunVerdict,
    TaskStatus, Verification,
};
pub use approval::{ApprovalGate, ApprovalRequest, AutoApprove, AutoDeny};
pub use cases::{evidence_section, CaseLibrary, EvidenceCase};
pub use events::{format_event, truncate, AgentEvent, EventSink, FanoutSink, InMemoryEventSink, TRUNCATE};
pub use ledger_sink::{LedgerSink, NETWORK_TOOLS};
pub use preflight::{classify, BlastRadius};
pub use session::{
    continuity_context, Checkpoint, CheckpointStore, JsonCheckpointStore, LoopState,
    SessionStatus, CONTINUITY_TAIL,
};
pub use tokens::{estimate_tokens, format_cost_line};
