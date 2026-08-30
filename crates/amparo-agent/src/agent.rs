//! The Amparo agent loop — native `tool_calls`, policy-gated execution.
//!
//! Ported from Axiom-OS `agent_loop.rs` (`run_agent_task`, MIT, Copyright
//! (c) Pixel Phantom AI); relicensed Apache-2.0 — see the repository NOTICE.
//!
//! The Axiom loop parsed text-format ReAct turns (`parse_react_turn` over
//! `Thought:`/`Action:` text) and collapsed a six-deep gate chain. Amparo
//! drives the model's **native function-calling protocol** (see `crate::sse`)
//! and keeps the gate chain it owns:
//!
//! ```text
//! model tool_calls → trust ceiling → policy gate → human approval → execute
//! ```
//!
//! Every call the model makes — including blocked ones — is answered with a
//! tool-role message carrying the same `tool_call_id`; providers reject the
//! next request otherwise. Loop mechanics preserved from Axiom: max steps,
//! conversation trimming, parallel batches with retry×2, the one-shot
//! shortcut, the same-tool loop guard, empty-turn recovery, and the
//! VERIFIED/INCOMPLETE self-verification pass.

use crate::approval::{ApprovalGate, ApprovalRequest, AutoDeny};
use crate::cases::{evidence_section, CaseLibrary};
use crate::events::{truncate, AgentEvent, EventSink, InMemoryEventSink};
use crate::preflight::classify;
use crate::session::{Checkpoint, CheckpointStore, LoopState, SessionStatus};
use crate::sse::accumulate_turn;
use crate::tokens::estimate_tokens;
use amparo_inference::{
    AssistantToolCall, ChatMessage, ChatRequest, FunctionCall, InferenceProvider, InferenceRequest,
    Tool,
};
use amparo_policy::{PolicyEngine, PolicyVerdict};
use amparo_privacy::{DataCategory, PiiPlaceholder};
use amparo_tools::{
    PathPolicy, SkillLibrary, ToolCall, ToolRegistry, ToolResult, ToolTrustTier, USE_SKILL,
};
use futures_util::future::join_all;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Keep the system prompt plus the last 30 messages.
const MAX_CONVERSATION_TAIL: usize = 30;
/// Retry a failed tool at most twice.
const MAX_TOOL_RETRIES: usize = 2;
/// Two consecutive same-tool calls → soft nudge toward a final answer.
const MAX_SAME_TOOL_CONSECUTIVE: u32 = 2;
/// Three consecutive same-tool calls → hard stop: answer now.
const HARD_SAME_TOOL_LIMIT: u32 = 3;
const DEFAULT_MAX_STEPS: usize = 12;

/// How a task ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    /// The task finished with a final answer.
    Complete,
    /// The task ended without one — a gate block, an error, or max steps.
    Failed,
}

/// One recorded step of the task, mirroring the event surface.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentStep {
    /// The model requested a tool call.
    ToolCall(ToolCall),
    /// A tool finished executing — or a gate blocked it.
    ToolResult(ToolResult),
    /// The model produced a final answer.
    FinalAnswer {
        /// The answer text.
        content: String,
    },
    /// The task failed with this message.
    Error {
        /// Why the task failed.
        message: String,
    },
}

/// The outcome of the self-verification pass.
#[derive(Debug, Clone, Serialize)]
pub struct Verification {
    /// `complete` | `incomplete`
    pub decision: String,
    /// The model's feedback on what is missing, when incomplete.
    pub feedback: Option<String>,
}

/// What the agent reports when a task ends.
#[derive(Debug, Clone, Serialize)]
pub struct AgentReport {
    /// How the task ended.
    pub status: TaskStatus,
    /// The final answer text, if the task produced one.
    pub final_answer: Option<String>,
    /// Every recorded step, in order.
    pub steps: Vec<AgentStep>,
    /// Loop iterations consumed (1 = one LLM turn).
    pub steps_used: usize,
    /// The self-verification outcome, when one ran.
    pub verification: Option<Verification>,
    /// Estimated tokens consumed across every inference call of the run —
    /// outgoing request text plus returned content and tool-call JSON
    /// (`chars / 4`; an estimate, not provider billing — the method is
    /// stated on the cost line).
    pub tokens_estimated: usize,
    /// Tool calls the model requested during the run, counted at
    /// dispatch — blocked calls included: the model asked, the gate
    /// answered, the count records the ask.
    pub tool_calls: usize,
}

/// Loop configuration.
#[derive(Debug, Clone)]
pub struct AgentConfig {
    /// Maximum loop iterations (one iteration = one LLM turn).
    pub max_steps: usize,
    /// Tools at tiers above this ceiling are blocked outright.
    pub trust_ceiling: ToolTrustTier,
    /// Override the provider's default model for every request.
    pub model: Option<String>,
    /// Override the provider's default completion token limit.
    pub max_tokens: Option<usize>,
    /// Override the provider's default sampling temperature.
    pub temperature: Option<f32>,
    /// Dollars per million tokens for the cost estimate; `None` turns the
    /// cost line off (counts still reported). Default `Some(3.0)` — a
    /// stated mid-range model assumption.
    pub cost_per_million_tokens: Option<f64>,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_steps: DEFAULT_MAX_STEPS,
            trust_ceiling: ToolTrustTier::SystemControl,
            model: None,
            max_tokens: None,
            temperature: None,
            cost_per_million_tokens: Some(3.0),
        }
    }
}

/// The system prompt. Native function calling carries the tool schemas on
/// the wire, so this only sets expectations: act through tools, answer with
/// a plain message when done, never invent tool results.
const SYSTEM_PROMPT: &str = "\
You are Amparo, a policy-governed AI agent. You act through tools, and every \
tool call you make is checked against a policy engine before it executes: \
denied calls are blocked, escalated calls need human approval, and audit-mode \
verdicts are recorded. Use your tools to gather information and take actions. \
When you have enough information to answer, respond with a plain assistant \
message containing your final answer — do not call tools when the task is \
done. If a tool result does not contain what was requested, say honestly what \
you see and what is missing. Never invent tool results.";

/// The outcome of gating one call.
enum GateOutcome {
    /// The call may execute; `reasons` are the gate reasons recorded on the
    /// `ToolGate` event.
    Ready { reasons: Vec<String> },
    /// A gate blocked the call; the caller records the failed `result`
    /// (for model calls, via [`Agent::block_call`] — for skill steps, into
    /// the expansion record).
    Blocked {
        result: ToolResult,
        decision: String,
        reasons: Vec<String>,
    },
}

/// The dry-run core of [`Agent::gate_call`], shared with [`dry_run_gate`]:
/// the same lookup → ceiling → policy chain, minus the approval gate.
struct GateCheck {
    /// What the policy engine judged — the skill name for a `use_skill`
    /// call, the extracted tool target otherwise.
    target: String,
    /// The core verdict.
    outcome: GateOutcomeCore,
}

/// The verdict of [`gate_check`], before the human-approval gate runs.
enum GateOutcomeCore {
    /// A gate blocked the call; `result` is the fully built failed
    /// [`ToolResult`] the caller records (the error strings live here —
    /// callers must not reconstruct them).
    Blocked {
        result: ToolResult,
        decision: String,
        reasons: Vec<String>,
    },
    /// The call may execute once the approval gate (tool tier ≥
    /// [`ToolTrustTier::ExternalEffector`] or a policy Escalate) has run.
    Ready {
        reasons: Vec<String>,
        escalate_pending: bool,
        tier: ToolTrustTier,
    },
}

/// The shared gate core: registry lookup → trust ceiling → policy engine.
/// Returns either a fully built failed [`ToolResult`] (model calls record
/// it via [`Agent::block_call`], skill steps into the expansion record) or
/// the reasons/escalation/tier the approval gate needs. [`Agent::gate_call`]
/// runs this then the approval block; [`dry_run_gate`] runs this alone.
async fn gate_check(
    registry: &ToolRegistry,
    ceiling: ToolTrustTier,
    policy: &dyn PolicyEngine,
    call: &ToolCall,
) -> GateCheck {
    let make_result = |success: bool, output: serde_json::Value, summary: &str| ToolResult {
        tool_call_id: call.id.clone(),
        tool_name: call.name.clone(),
        success,
        output,
        display_summary: summary.to_string(),
        duration_ms: 0,
    };

    // Registry lookup first — unknown tools get an honest error naming
    // what is available, not a trust verdict.
    if registry.get_executor(&call.name).is_none() {
        let available: Vec<String> = registry
            .list_schemas()
            .iter()
            .map(|s| s.name.clone())
            .collect();
        let error = format!(
            "Unknown tool: {}. Available: {}",
            call.name,
            available.join(", ")
        );
        return GateCheck {
            target: String::new(),
            outcome: GateOutcomeCore::Blocked {
                result: make_result(false, serde_json::json!({"error": error}), "Unknown tool"),
                decision: "unknown_tool".to_string(),
                reasons: Vec::new(),
            },
        };
    }

    // Trust ceiling
    let tier_ok = registry
        .get_tier(&call.name)
        .map(|t| t <= ceiling)
        .unwrap_or(false);
    if !tier_ok {
        return GateCheck {
            target: String::new(),
            outcome: GateOutcomeCore::Blocked {
                result: make_result(
                    false,
                    serde_json::json!({"error": "tool blocked by trust ceiling"}),
                    "Blocked",
                ),
                decision: "trust_blocked".to_string(),
                reasons: vec!["tool tier exceeds the trust ceiling".to_string()],
            },
        };
    }

    // Policy gate — the deny-by-default seam. For `use_skill` the engine
    // judges the skill name, matching the adoption check; its steps get
    // their own checks with their own targets when they expand.
    let (target, params) = if call.name == USE_SKILL {
        (
            call.arg_str("skill_name").unwrap_or("").to_string(),
            Vec::new(),
        )
    } else {
        extract_target(call)
    };
    let param_refs: Vec<(&str, &str)> = params
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let decision = policy.judge_tool(&call.name, &target, &param_refs).await;
    let mut escalate_pending = false;
    let reasons: Vec<String> = match decision.verdict {
        PolicyVerdict::Deny => {
            let error = format!("Policy denied {}: {}", call.name, decision.fired.join("; "));
            return GateCheck {
                target,
                outcome: GateOutcomeCore::Blocked {
                    result: make_result(
                        false,
                        serde_json::json!({"error": error}),
                        "Blocked by policy",
                    ),
                    decision: "policy_denied".to_string(),
                    reasons: decision.fired,
                },
            };
        }
        PolicyVerdict::Escalate => {
            // Escalate means ask — never execute silently.
            escalate_pending = true;
            let reasons = decision.fired;
            tracing::warn!(
                "[amparo-agent] policy escalate on {} (asking for approval): {}",
                call.name,
                reasons.join("; ")
            );
            reasons
        }
        PolicyVerdict::Allow => {
            // Audit-mode allows carry the engine's real verdict in
            // `fired` — keep it visible.
            decision.fired
        }
    };

    // The approval gate needs the tier; the core reports it so
    // `dry_run_gate` can compute `approval_required` without a second
    // registry lookup.
    let tier = registry
        .get_tier(&call.name)
        .unwrap_or(ToolTrustTier::Observational);
    GateCheck {
        target,
        outcome: GateOutcomeCore::Ready {
            reasons,
            escalate_pending,
            tier,
        },
    }
}

/// The verdict of a dry-run gate check: what [`dry_run_gate`] reports
/// without executing anything, emitting any event, or asking for approval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DryRunVerdict {
    /// `allowed` | `unknown_tool` | `trust_blocked` | `policy_denied`
    pub decision: String,
    /// The gate reasons (policy-fired rules, ceiling complaint, …).
    pub reasons: Vec<String>,
    /// True when the same call through the live gate chain would be
    /// blocked without human intervention.
    pub would_block: bool,
    /// True when the live chain would route the call to human approval:
    /// a policy Escalate or a tool tier ≥ [`ToolTrustTier::ExternalEffector`].
    /// Escalation is not drift — an escalated step is still allowed.
    pub approval_required: bool,
    /// What the policy engine judged: the `use_skill` skill name, or the
    /// extracted tool target.
    pub target: String,
}

/// Dry-run one call through the gate chain (registry lookup → trust
/// ceiling → policy engine) without executing it, emitting events, or
/// asking for approval. Used by the skill re-checker to detect
/// **policy drift**: a skill step the current policy would deny must
/// retire. A policy Escalate reports `approval_required: true` with
/// `would_block: false` — escalate is not drift.
pub async fn dry_run_gate(
    registry: &ToolRegistry,
    ceiling: ToolTrustTier,
    policy: &dyn PolicyEngine,
    call: &ToolCall,
) -> DryRunVerdict {
    let check = gate_check(registry, ceiling, policy, call).await;
    match check.outcome {
        GateOutcomeCore::Blocked {
            decision, reasons, ..
        } => DryRunVerdict {
            decision,
            reasons,
            would_block: true,
            approval_required: false,
            target: check.target,
        },
        GateOutcomeCore::Ready {
            reasons,
            escalate_pending,
            tier,
        } => DryRunVerdict {
            decision: "allowed".to_string(),
            reasons,
            would_block: false,
            approval_required: escalate_pending || tier >= ToolTrustTier::ExternalEffector,
            target: check.target,
        },
    }
}

/// One batch item, in model order: either a direct call still to execute,
/// or an already-expanded skill result.
enum ExecItem {
    /// A gated model call, executed concurrently with the rest of the batch.
    Call(ToolCall),
    /// A `use_skill` expansion result, already in hand.
    Skill(ToolResult),
}

/// The agent. `inference` supplies the model; `registry` the tools; `policy`
/// the deny-by-default gate. Everything else has a safe default and a
/// `with_*` builder.
pub struct Agent {
    inference: Arc<dyn InferenceProvider>,
    registry: ToolRegistry,
    policy: Arc<dyn PolicyEngine>,
    approval: Arc<dyn ApprovalGate>,
    events: Arc<dyn EventSink>,
    privacy: Option<Arc<amparo_privacy::PrivacyPolicy>>,
    /// The optional verification case library (M6b): retrieved prior cases
    /// appear as an evidence section in the self-verification prompt only.
    cases: Option<Arc<dyn CaseLibrary>>,
    /// The optional adopted-skill library (M6c): `use_skill` calls expand
    /// into their steps in the loop, each step gated individually.
    skills: Option<Arc<dyn SkillLibrary>>,
    /// The optional workspace path policy (M7): drives the preflight
    /// blast-radius classification. Display-only — its absence silences
    /// the `[preflight]` label, never changes the gate.
    path_policy: Option<Arc<PathPolicy>>,
    /// The optional checkpoint store (M7): when attached, every loop
    /// iteration and every terminal exit is snapshotted through it.
    /// Failures warn, never fatal.
    checkpoints: Option<Arc<dyn CheckpointStore>>,
    /// The tenant this agent's fresh tasks are saved under. A resume
    /// reuses the checkpoint's own tenant instead, so a restored task
    /// always lands back in its file.
    checkpoint_tenant: Option<String>,
    /// The optional task id the next fresh run uses (M8): when the host
    /// generated it — a sub-agent's chain id like `sess-123.1` — every
    /// artifact (checkpoint file, ledger row, approval copy) names the
    /// same task. `None` (the default) lets [`Agent::run`] generate
    /// `sess-<nanos>-<pid>` as before.
    task_id: Option<String>,
    /// The optional parent task id (M8): set on a spawned sub-agent so
    /// its approval copy, checkpoints and ledger rows carry the explicit
    /// delegation link. Provenance and display only — delegation never
    /// changes the gate (the one rule).
    parent_task_id: Option<String>,
    /// The one-shot continuity context the next fresh task receives
    /// (M7 W8): built by the host from the tenant's latest complete
    /// checkpoint, injected as one user-role message between the system
    /// prompt and the task prompt. A resume ignores it — its own
    /// conversation continues.
    continuity: Option<String>,
    config: AgentConfig,
}

/// The checkpoint handle a loop carries (M7): the task id and start time
/// are fixed for the task's whole life, including a resume. `tenant` is
/// `None` when no checkpoint store is attached — saves then no-op.
struct SessionHandle {
    task_id: String,
    /// The parent task's id when this task is a sub-agent (M8) —
    /// persisted into checkpoints so the chain survives a resume.
    parent_task_id: Option<String>,
    tenant: Option<String>,
    started_at: u64,
}

/// The loop locals that move between turns — what a checkpoint snapshots
/// and a resume restores, plus the in-run steps/answer history, which a
/// resume starts fresh (the event stream is the full trail).
struct LoopVars {
    steps: Vec<AgentStep>,
    last_tool_name: Option<String>,
    same_tool_count: u32,
    empty_turn_retried: bool,
    last_good_summary: Option<String>,
    final_answer: Option<String>,
    verification: Option<Verification>,
    steps_used: usize,
    used_tool_names: Vec<String>,
    tokens_estimated: usize,
    tool_calls: usize,
}

impl LoopVars {
    /// A fresh run's starting locals.
    fn fresh() -> Self {
        Self {
            steps: Vec::new(),
            last_tool_name: None,
            same_tool_count: 0,
            empty_turn_retried: false,
            last_good_summary: None,
            final_answer: None,
            verification: None,
            steps_used: 0,
            used_tool_names: Vec::new(),
            tokens_estimated: 0,
            tool_calls: 0,
        }
    }
}

/// Everything a spawned sub-agent inherits from its parent (M8 W3): the
/// same inference provider, registry, gate chain, event sink and optional
/// instruments. A child runs the same loop under the same gates — the one
/// rule. `skills` and `cases` are deliberately NOT carried: a child
/// inherits no adopted skills and no case evidence (a `use_skill` call in
/// the child hits the registry's defensive executor, which fails loudly).
#[derive(Clone)]
pub(crate) struct SwarmParts {
    /// The shared model — child turns interleave on the same provider.
    pub inference: Arc<dyn InferenceProvider>,
    /// The parent's tool set; the child's registry is this clone with
    /// `spawn_agent` re-registered for the child's own chain id.
    pub registry: ToolRegistry,
    /// The deny-by-default engine — the child's calls are judged by it.
    pub policy: Arc<dyn PolicyEngine>,
    /// The human-approval gate — the child asks the same human.
    pub approval: Arc<dyn ApprovalGate>,
    /// The shared event sink — child events interleave into the one stream.
    pub events: Arc<dyn EventSink>,
    /// The privacy policy, when the parent had one: the child's prompt is
    /// stripped before inference exactly like the parent's (I6 — the child
    /// persists its own prompt in checkpoints).
    pub privacy: Option<Arc<amparo_privacy::PrivacyPolicy>>,
    /// The workspace path policy, when the parent had one: the child's
    /// preflight labels work identically (display-only, I1).
    pub path_policy: Option<Arc<PathPolicy>>,
    /// The checkpoint store and tenant, when the parent attached them:
    /// the child checkpoints into the same store under the same tenant.
    pub checkpoints: Option<Arc<dyn CheckpointStore>>,
    /// The tenant paired with `checkpoints` — `None` only when the store is.
    pub checkpoint_tenant: Option<String>,
    /// The loop configuration the child inherits (rate knob included).
    pub config: AgentConfig,
}

impl Agent {
    /// The only gates required are the inference provider and the policy
    /// engine — with a deny-all engine the agent will refuse every call.
    /// The approval gate defaults to [`AutoDeny`] and the event sink to
    /// [`InMemoryEventSink`].
    pub fn new(
        inference: Arc<dyn InferenceProvider>,
        registry: ToolRegistry,
        policy: Arc<dyn PolicyEngine>,
    ) -> Self {
        Self {
            inference,
            registry,
            policy,
            approval: Arc::new(AutoDeny),
            events: Arc::new(InMemoryEventSink::new()),
            privacy: None,
            cases: None,
            skills: None,
            path_policy: None,
            checkpoints: None,
            checkpoint_tenant: None,
            task_id: None,
            parent_task_id: None,
            continuity: None,
            config: AgentConfig::default(),
        }
    }

    /// Replace the human-approval gate (default: deny everything).
    pub fn with_approval(mut self, gate: Arc<dyn ApprovalGate>) -> Self {
        self.approval = gate;
        self
    }

    /// Replace the event sink (default: in-memory log + broadcast).
    pub fn with_events(mut self, sink: Arc<dyn EventSink>) -> Self {
        self.events = sink;
        self
    }

    /// Attach a privacy policy — every turn is privacy-checked, and PII is
    /// stripped before inference and restored after (Secure Minions).
    pub fn with_privacy(mut self, policy: Arc<amparo_privacy::PrivacyPolicy>) -> Self {
        self.privacy = Some(policy);
        self
    }

    /// Attach the verification case library — retrieved prior cases appear
    /// as an evidence section in the self-verification prompt only (never
    /// in the action loop; a case can never execute anything).
    pub fn with_case_library(mut self, library: Arc<dyn CaseLibrary>) -> Self {
        self.cases = Some(library);
        self
    }

    /// Attach the adopted-skill library (M6c). `use_skill` calls expand
    /// into their ordered steps in the loop, and every step runs the full
    /// gate chain individually — a skill can never grant its steps an
    /// exemption.
    pub fn with_skills(mut self, library: Arc<dyn SkillLibrary>) -> Self {
        self.skills = Some(library);
        self
    }

    /// Attach the workspace path policy (M7) so the preflight blast-radius
    /// classification can run. Display-only: the label decorates the
    /// approval copy and never feeds the gate (I1).
    pub fn with_path_policy(mut self, policy: Arc<PathPolicy>) -> Self {
        self.path_policy = Some(policy);
        self
    }

    /// Attach a checkpoint store (M7) and the tenant this agent's fresh
    /// tasks are saved under — `platform:user_id` in chat hosts, `cli`
    /// in the CLI. Without this the agent runs unpersisted; a resume
    /// still works but saves nothing further.
    pub fn with_checkpoints(
        mut self,
        store: Arc<dyn CheckpointStore>,
        tenant: impl Into<String>,
    ) -> Self {
        self.checkpoints = Some(store);
        self.checkpoint_tenant = Some(tenant.into());
        self
    }

    /// Fix the task id the next fresh run uses (M8 W2): a host that
    /// generated the id (a sub-agent's `{parent}.{n}` chain id) passes
    /// it here so the checkpoint file, ledger rows and approval copy
    /// all name the same task. Without it, [`Agent::run`] generates
    /// `sess-<nanos>-<pid>` exactly as before.
    pub fn with_task_id(mut self, task_id: impl Into<String>) -> Self {
        self.task_id = Some(task_id.into());
        self
    }

    /// Mark this agent as a sub-agent of `parent_task_id` (M8 W2): the
    /// approval copy names the chain (`sub-agent sess-123.1 of task
    /// sess-123`), and checkpoints and ledger rows carry the explicit
    /// parent link. Delegation is not exemption — the child runs the
    /// same loop, the same gate chain, the same ceiling (the one rule).
    pub fn with_parent_task_id(mut self, parent_task_id: impl Into<String>) -> Self {
        self.parent_task_id = Some(parent_task_id.into());
        self
    }

    /// Hand the next fresh task a one-shot context message (M7 W8): the
    /// host builds it from the tenant's latest complete checkpoint (see
    /// [`crate::session::continuity_context`]) and it arrives as one
    /// user-role message between the system prompt and the task prompt —
    /// the loop reads it as prior conversation, never as instruction
    /// (I5 holds: the system prompt stays byte-identical). A resume
    /// ignores it. The string must already be PII-stripped (I6); the
    /// loop strips it again before every inference anyway.
    pub fn with_continuity(mut self, context: Option<String>) -> Self {
        self.continuity = context;
        self
    }

    /// Replace the whole loop configuration (see [`AgentConfig`]).
    pub fn with_config(mut self, config: AgentConfig) -> Self {
        self.config = config;
        self
    }

    /// The tool registry this agent executes against.
    pub fn registry(&self) -> &ToolRegistry {
        &self.registry
    }

    /// The agent's loop configuration.
    pub fn config(&self) -> &AgentConfig {
        &self.config
    }

    /// The inheritance a spawned sub-agent receives (M8 W3): the same
    /// provider, gate chain, sink and optional instruments, with the
    /// child's registry rooted at `registry` (the host passes the parent's
    /// tool set *without* `spawn_agent` registered — the spawn tool itself
    /// re-registers it for each child, and the base must stay spawn-free so
    /// the tool never holds an `Arc` back to itself).
    pub(crate) fn swarm_parts(&self, registry: ToolRegistry) -> SwarmParts {
        SwarmParts {
            inference: Arc::clone(&self.inference),
            registry,
            policy: Arc::clone(&self.policy),
            approval: Arc::clone(&self.approval),
            events: Arc::clone(&self.events),
            privacy: self.privacy.clone(),
            path_policy: self.path_policy.clone(),
            checkpoints: self.checkpoints.clone(),
            checkpoint_tenant: self.checkpoint_tenant.clone(),
            config: self.config.clone(),
        }
    }

    /// Registry schemas as wire-ready OpenAI tool definitions.
    fn openai_tools(&self) -> Vec<Tool> {
        self.registry
            .openai_tools()
            .into_iter()
            .filter_map(|v| serde_json::from_value(v).ok())
            .collect()
    }

    /// Emit a [`AgentEvent::PrivacyStripped`] when a strip found anything —
    /// per-category counts only, never the values. The strip sites call
    /// this before discarding their placeholder maps (I6).
    fn emit_privacy_stripped(&self, pii_map: &[PiiPlaceholder]) {
        if pii_map.is_empty() {
            return;
        }
        let mut categories: Vec<(String, usize)> = Vec::new();
        for placeholder in pii_map {
            match categories
                .iter_mut()
                .find(|(category, _)| category == &placeholder.category)
            {
                Some((_, count)) => *count += 1,
                None => categories.push((placeholder.category.clone(), 1)),
            }
        }
        self.events
            .emit(&AgentEvent::PrivacyStripped { categories });
    }

    /// Run the loop to completion: every gate decision, tool execution and
    /// the final self-verification, all reported through [`AgentReport`] and
    /// the [`EventSink`].
    pub async fn run(&self, prompt: impl Into<String>) -> AgentReport {
        let prompt = prompt.into();
        self.events.emit(&AgentEvent::TaskStarted {
            prompt: prompt.clone(),
            // The host-fixed id, when one exists (M8) — a sub-agent's
            // chain id names the `[task]` line. `None` for an unhosted
            // run keeps the v0.6.0 plain tag.
            task_id: self.task_id.clone(),
        });

        // The prompt appears in nudge and verification messages sent to the
        // model — strip it once so the original never leaks there.
        let safe_prompt: String = match &self.privacy {
            Some(policy) if policy.auto_redact_pii => {
                let stripped = amparo_privacy::secure_minions_strip(&prompt);
                self.emit_privacy_stripped(&stripped.pii_map);
                stripped.sanitised_text
            }
            _ => prompt.clone(),
        };

        let mut conversation: Vec<ChatMessage> = vec![ChatMessage::system(SYSTEM_PROMPT)];
        // Continuity (M7 W8): a fresh task in a long-lived chat carries
        // the prior task's tail as one user-role context message before
        // the task prompt. The host built it from a PII-stripped
        // checkpoint; a resume never reaches here.
        if let Some(context) = &self.continuity {
            conversation.push(ChatMessage::user(context.clone()));
        }
        conversation.push(ChatMessage::user(prompt.clone()));

        // The task id is generated even without a store — it is the stable
        // handle a later resume would use.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        let session = SessionHandle {
            task_id: self
                .task_id
                .clone()
                .unwrap_or_else(|| format!("sess-{}-{}", now.as_nanos(), std::process::id())),
            parent_task_id: self.parent_task_id.clone(),
            tenant: self.checkpoint_tenant.clone(),
            started_at: now.as_secs(),
        };
        self.run_loop(
            prompt,
            safe_prompt,
            conversation,
            LoopVars::fresh(),
            session,
            0,
        )
        .await
    }

    /// Resume a checkpointed task (M7): restore the conversation and loop
    /// state, re-prepend the current `SYSTEM_PROMPT` (I5 — checkpoints
    /// never store the system message), and run the same loop. The stored
    /// conversation is already PII-stripped (I6), so the checkpoint's
    /// prompt doubles as the safe prompt. The iteration budget is the
    /// remainder: `max_steps - steps_used`; an exhausted budget fails
    /// with "Max steps reached" like any run.
    pub async fn resume(&self, checkpoint: Checkpoint) -> AgentReport {
        let conversation = std::iter::once(ChatMessage::system(SYSTEM_PROMPT))
            .chain(checkpoint.conversation.into_iter())
            .collect();
        let starting_steps = checkpoint.loop_state.steps_used;
        self.events.emit(&AgentEvent::TaskResumed {
            task_id: checkpoint.task_id.clone(),
            steps_used: starting_steps,
        });
        let vars = LoopVars {
            steps: Vec::new(),
            last_tool_name: checkpoint.loop_state.last_tool_name,
            same_tool_count: checkpoint.loop_state.same_tool_count,
            empty_turn_retried: checkpoint.loop_state.empty_turn_retried,
            last_good_summary: checkpoint.loop_state.last_good_summary,
            final_answer: None,
            verification: None,
            steps_used: starting_steps,
            used_tool_names: checkpoint.loop_state.used_tool_names,
            // A resume's report covers the resumed segment only — the
            // checkpoint carries no counters (they are not part of its
            // schema), so accounting starts fresh here.
            tokens_estimated: 0,
            tool_calls: 0,
        };
        let session = SessionHandle {
            task_id: checkpoint.task_id,
            parent_task_id: checkpoint.parent_task_id,
            tenant: Some(checkpoint.tenant),
            started_at: checkpoint.started_at,
        };
        self.run_loop(
            checkpoint.prompt.clone(),
            checkpoint.prompt,
            conversation,
            vars,
            session,
            starting_steps,
        )
        .await
    }

    /// The shared loop — a fresh run and a resume execute the same body.
    /// `vars` carry the loop locals, `session` the checkpoint handle,
    /// `starting_steps` the iteration offset (0 for a fresh run).
    ///
    /// M8 W2: the loop computes one `session_label` from the session's
    /// task id and the agent's parent link — `sub-agent sess-123.1 of
    /// task sess-123` — and every approval inside (model calls and
    /// skill steps alike) carries it. `None` for a top-level agent.
    async fn run_loop(
        &self,
        prompt: String,
        safe_prompt: String,
        mut conversation: Vec<ChatMessage>,
        vars: LoopVars,
        session: SessionHandle,
        starting_steps: usize,
    ) -> AgentReport {
        let LoopVars {
            mut steps,
            mut last_tool_name,
            mut same_tool_count,
            mut empty_turn_retried,
            mut last_good_summary,
            mut final_answer,
            mut verification,
            mut steps_used,
            mut used_tool_names,
            mut tokens_estimated,
            mut tool_calls,
        } = vars;

        // Who is asking (M8 W2): a sub-agent's approvals name its
        // delegation chain. Computed once from the session's task id
        // and the agent's parent link; every gate_call below — model
        // calls and skill steps — carries the same label. Display-only
        // (I1): the label decorates the approval copy, never the gate.
        let session_label: Option<String> = self
            .parent_task_id
            .as_ref()
            .map(|parent| format!("sub-agent {} of task {}", session.task_id, parent));

        for step in 0..self.config.max_steps.saturating_sub(starting_steps) {
            steps_used = starting_steps + step + 1;

            // ── Checkpoint (M7): once per iteration, before the LLM call —
            // a crash loses at most one turn. Failures warn, never fatal.
            self.persist_checkpoint(
                &session,
                SessionStatus::Running,
                &prompt,
                &conversation,
                LoopState {
                    last_tool_name: last_tool_name.clone(),
                    same_tool_count,
                    empty_turn_retried,
                    last_good_summary: last_good_summary.clone(),
                    used_tool_names: used_tool_names.clone(),
                    steps_used,
                },
                None,
            );

            // ── Trim conversation to prevent unbounded growth ───────────────
            if conversation.len() > MAX_CONVERSATION_TAIL + 1 {
                let tail = conversation.split_off(conversation.len() - MAX_CONVERSATION_TAIL);
                let system = conversation.remove(0);
                conversation.clear();
                conversation.push(system);
                conversation.extend(tail);
            }

            // ── Privacy check ───────────────────────────────────────────────
            if let Some(policy) = &self.privacy {
                let decision = amparo_privacy::evaluate(policy, None, DataCategory::Chat, None);
                if !decision.allowed {
                    let message = format!("Privacy policy blocked inference: {}", decision.reason);
                    steps.push(AgentStep::Error {
                        message: message.clone(),
                    });
                    self.events.emit(&AgentEvent::TaskFailed {
                        message: message.clone(),
                    });
                    self.persist_checkpoint(
                        &session,
                        SessionStatus::Failed,
                        &prompt,
                        &conversation,
                        LoopState {
                            last_tool_name: last_tool_name.clone(),
                            same_tool_count,
                            empty_turn_retried,
                            last_good_summary: last_good_summary.clone(),
                            used_tool_names: used_tool_names.clone(),
                            steps_used,
                        },
                        None,
                    );
                    return AgentReport {
                        status: TaskStatus::Failed,
                        final_answer: None,
                        steps,
                        steps_used,
                        verification: None,
                        tokens_estimated,
                        tool_calls,
                    };
                }
            }

            // ── Secure Minions PII strip ────────────────────────────────────
            let (send_messages, pii_map) = match &self.privacy {
                Some(policy) if policy.auto_redact_pii => {
                    let (messages, map) = strip_messages(&conversation);
                    self.emit_privacy_stripped(&map);
                    (messages, map)
                }
                _ => (conversation.clone(), Vec::new()),
            };

            // ── Token accounting (M8 W1) ────────────────────────────────────
            // The outgoing request is billed whether the stream fails or
            // not, so it is counted up front. chars/4 — the method is
            // stated on the cost line.
            let request_json = serde_json::to_string(&send_messages).unwrap_or_default();
            tokens_estimated = tokens_estimated.saturating_add(estimate_tokens(&request_json));

            // ── LLM call (native tool calling) ──────────────────────────────
            let request = ChatRequest {
                messages: send_messages,
                tools: Some(self.openai_tools()),
                max_tokens: self.config.max_tokens,
                temperature: self.config.temperature,
                stream: Some(true),
                model: self.config.model.clone(),
                privacy_level: None,
                json_schema: None,
                thinking: None,
                web_search: None,
                challenge_level: None,
            };
            let turn = match self.inference.complete_chat_stream(request).await {
                Ok(stream) => match accumulate_turn(stream).await {
                    Ok(turn) => turn,
                    Err(e) => {
                        let message = format!("Inference stream failed: {}", e);
                        steps.push(AgentStep::Error {
                            message: message.clone(),
                        });
                        self.events.emit(&AgentEvent::TaskFailed {
                            message: message.clone(),
                        });
                        self.persist_checkpoint(
                            &session,
                            SessionStatus::Failed,
                            &prompt,
                            &conversation,
                            LoopState {
                                last_tool_name: last_tool_name.clone(),
                                same_tool_count,
                                empty_turn_retried,
                                last_good_summary: last_good_summary.clone(),
                                used_tool_names: used_tool_names.clone(),
                                steps_used,
                            },
                            None,
                        );
                        return AgentReport {
                            status: TaskStatus::Failed,
                            final_answer: None,
                            steps,
                            steps_used,
                            verification: None,
                            tokens_estimated,
                            tool_calls,
                        };
                    }
                },
                Err(e) => {
                    let message = format!("Inference request failed: {}", e);
                    steps.push(AgentStep::Error {
                        message: message.clone(),
                    });
                    self.events.emit(&AgentEvent::TaskFailed {
                        message: message.clone(),
                    });
                    self.persist_checkpoint(
                        &session,
                        SessionStatus::Failed,
                        &prompt,
                        &conversation,
                        LoopState {
                            last_tool_name: last_tool_name.clone(),
                            same_tool_count,
                            empty_turn_retried,
                            last_good_summary: last_good_summary.clone(),
                            used_tool_names: used_tool_names.clone(),
                            steps_used,
                        },
                        None,
                    );
                    return AgentReport {
                        status: TaskStatus::Failed,
                        final_answer: None,
                        steps,
                        steps_used,
                        verification: None,
                        tokens_estimated,
                        tool_calls,
                    };
                }
            };

            // The returned content and tool-call JSON join the count only
            // when the stream actually delivered a turn.
            tokens_estimated = tokens_estimated.saturating_add(estimate_tokens(&turn.content));
            if !turn.tool_calls.is_empty() {
                let calls_json = serde_json::to_string(&turn.tool_calls).unwrap_or_default();
                tokens_estimated = tokens_estimated.saturating_add(estimate_tokens(&calls_json));
            }

            // ── Empty-turn recovery ─────────────────────────────────────────
            // A reasoning model can burn its whole budget on hidden tokens and
            // return no visible text. Retry once with a nudge, then finalize
            // from real tool work if any exists — or fail honestly.
            if turn.is_empty() {
                if !empty_turn_retried {
                    empty_turn_retried = true;
                    tracing::warn!("[amparo-agent] empty LLM turn — retrying with a format nudge");
                    conversation.push(ChatMessage::user(
                        "SYSTEM: Your previous response was empty. You MUST respond now. \
                         If you have enough information, reply with your final answer as a \
                         plain message. Otherwise call the tools you need and then answer.",
                    ));
                    continue;
                }
                tracing::warn!(
                    "[amparo-agent] empty LLM turn persisted after retry — finalizing gracefully"
                );
                if let Some(summary) = last_good_summary.take() {
                    let content = format!(
                        "Task completed. Based on the actions performed: {}",
                        summary
                    );
                    steps.push(AgentStep::FinalAnswer {
                        content: content.clone(),
                    });
                    self.events.emit(&AgentEvent::FinalAnswer {
                        content: content.clone(),
                    });
                    self.events.emit(&AgentEvent::TaskComplete {
                        final_answer: content.clone(),
                        task_id: self.task_id.clone(),
                    });
                    self.persist_checkpoint(
                        &session,
                        SessionStatus::Complete,
                        &prompt,
                        &conversation,
                        LoopState {
                            last_tool_name: last_tool_name.clone(),
                            same_tool_count,
                            empty_turn_retried,
                            // The take above emptied the local — the summary
                            // lives on as `content`, which is what continuity
                            // reads from complete checkpoints.
                            last_good_summary: Some(content.clone()),
                            used_tool_names: used_tool_names.clone(),
                            steps_used,
                        },
                        Some(&content),
                    );
                    return AgentReport {
                        status: TaskStatus::Complete,
                        final_answer: Some(content),
                        steps,
                        steps_used,
                        verification: None,
                        tokens_estimated,
                        tool_calls,
                    };
                }
                let message = "The model produced no output after a retry, and no prior tool \
                               results were available to finalize from"
                    .to_string();
                steps.push(AgentStep::Error {
                    message: message.clone(),
                });
                self.events.emit(&AgentEvent::TaskFailed {
                    message: message.clone(),
                });
                self.persist_checkpoint(
                    &session,
                    SessionStatus::Failed,
                    &prompt,
                    &conversation,
                    LoopState {
                        last_tool_name: last_tool_name.clone(),
                        same_tool_count,
                        empty_turn_retried,
                        last_good_summary: last_good_summary.clone(),
                        used_tool_names: used_tool_names.clone(),
                        steps_used,
                    },
                    None,
                );
                return AgentReport {
                    status: TaskStatus::Failed,
                    final_answer: None,
                    steps,
                    steps_used,
                    verification: None,
                    tokens_estimated,
                    tool_calls,
                };
            }
            // A non-empty turn arrived — reset the retry budget so a later
            // empty streak gets its own chance.
            empty_turn_retried = false;

            // Restore any PII placeholders stripped before inference.
            let assistant_content = if pii_map.is_empty() {
                turn.content
            } else {
                amparo_privacy::secure_minions_restore(&turn.content, &pii_map)
            };

            conversation.push(ChatMessage {
                role: "assistant".to_string(),
                content: assistant_content.clone(),
                tool_calls: if turn.tool_calls.is_empty() {
                    None
                } else {
                    Some(turn.tool_calls.clone())
                },
                tool_call_id: None,
            });
            self.events.emit(&AgentEvent::AssistantTurn {
                step,
                content: assistant_content.clone(),
                tool_calls: turn.tool_calls.len(),
            });

            // ── Tool batch ──────────────────────────────────────────────────
            if !turn.tool_calls.is_empty() {
                let calls: Vec<ToolCall> = turn
                    .tool_calls
                    .iter()
                    .map(|tc| ToolCall {
                        id: tc.id.clone(),
                        name: tc.function.name.clone(),
                        arguments: serde_json::from_str(&tc.function.arguments)
                            .unwrap_or(serde_json::Value::Null),
                    })
                    .collect();

                // Counted at dispatch (M8 W1) — every call the model
                // asked for, blocked or not: the gate answers, the count
                // records the ask.
                tool_calls = tool_calls.saturating_add(calls.len());

                for call in &calls {
                    steps.push(AgentStep::ToolCall(call.clone()));
                    self.events
                        .emit(&AgentEvent::ToolCallRequested { call: call.clone() });
                    if !used_tool_names.contains(&call.name) {
                        used_tool_names.push(call.name.clone());
                    }
                }

                // Pre-flight gates run serially; each call ends up either
                // ready to execute or answered with a blocked result. Every
                // call — executed or not — must be answered by a tool-role
                // message carrying the same tool_call_id, or providers reject
                // the next request.
                let mut ready: Vec<ExecItem> = Vec::new();
                let mut blocked_obs: Vec<String> = Vec::new();
                let mut tool_messages: Vec<ChatMessage> = Vec::new();

                for call in &calls {
                    match self.gate_call(call, session_label.as_deref()).await {
                        GateOutcome::Blocked {
                            result,
                            decision,
                            reasons,
                        } => {
                            self.block_call(
                                &result,
                                &decision,
                                reasons,
                                &mut steps,
                                &mut blocked_obs,
                                &mut tool_messages,
                            );
                        }
                        GateOutcome::Ready { reasons } => {
                            self.events.emit(&AgentEvent::ToolGate {
                                call_id: call.id.clone(),
                                tool_name: call.name.clone(),
                                decision: "allowed".to_string(),
                                reasons,
                            });
                            if call.name == USE_SKILL {
                                // A skill expands in the loop: every step runs
                                // the same gate chain, serially, before the
                                // rest of the batch executes. Steps never add
                                // conversation messages — the model asked for
                                // `use_skill` and gets ONE tool-role answer.
                                let (result, step_tools) =
                                    self.expand_skill(call, session_label.as_deref()).await;
                                for tool in step_tools {
                                    if !used_tool_names.contains(&tool) {
                                        used_tool_names.push(tool);
                                    }
                                }
                                ready.push(ExecItem::Skill(result));
                            } else {
                                ready.push(ExecItem::Call(call.clone()));
                            }
                        }
                    }
                }

                // Execute all gated tools concurrently, retrying failures.
                // Skill results are already in hand; the ordered walk below
                // rebuilds `exec_results` in model order so everything
                // downstream sees one result per call, batch position intact.
                let exec_futures = ready.iter().filter_map(|item| match item {
                    ExecItem::Call(call) => {
                        let call = call.clone();
                        Some(async move { self.execute_call(&call).await })
                    }
                    ExecItem::Skill(_) => None,
                });
                let mut exec_results: Vec<ToolResult> = Vec::with_capacity(ready.len());
                let mut call_results = join_all(exec_futures).await.into_iter();
                for item in &ready {
                    match item {
                        ExecItem::Call(_) => {
                            exec_results.push(call_results.next().expect("one result per call"))
                        }
                        ExecItem::Skill(result) => exec_results.push(result.clone()),
                    }
                }

                // One-shot action detection: a simple "open X" request whose
                // single batch all succeeded needs no further turns.
                // Compound requests ("… and …", "then") are NOT one-shot —
                // the agent must continue to fulfill the second part.
                let is_one_shot = {
                    let lc = prompt.to_lowercase();
                    let is_simple_action = lc.starts_with("open ")
                        || lc.starts_with("launch ")
                        || lc.starts_with("start ");
                    let is_compound = lc.contains(" and ")
                        || lc.contains(" then ")
                        || lc.contains(" also ")
                        || lc.contains(" after ")
                        || lc.contains(" while ");
                    is_simple_action
                        && !is_compound
                        && exec_results.iter().all(|r| r.success)
                        && blocked_obs.is_empty()
                };

                if is_one_shot {
                    let summary = exec_results
                        .iter()
                        .map(|r| r.display_summary.clone())
                        .collect::<Vec<_>>()
                        .join(". ");
                    for result in exec_results {
                        steps.push(AgentStep::ToolResult(result.clone()));
                        self.events.emit(&AgentEvent::ToolExecuted {
                            result: result.clone(),
                        });
                    }
                    steps.push(AgentStep::FinalAnswer {
                        content: summary.clone(),
                    });
                    self.events.emit(&AgentEvent::FinalAnswer {
                        content: summary.clone(),
                    });
                    self.events.emit(&AgentEvent::TaskComplete {
                        final_answer: summary.clone(),
                        task_id: self.task_id.clone(),
                    });
                    self.persist_checkpoint(
                        &session,
                        SessionStatus::Complete,
                        &prompt,
                        &conversation,
                        LoopState {
                            last_tool_name: last_tool_name.clone(),
                            same_tool_count,
                            empty_turn_retried,
                            // One-shot returns before the summary-update
                            // loop — the joined summaries are the good
                            // summary continuity reads.
                            last_good_summary: Some(summary.clone()),
                            used_tool_names: used_tool_names.clone(),
                            steps_used,
                        },
                        Some(&summary),
                    );
                    return AgentReport {
                        status: TaskStatus::Complete,
                        final_answer: Some(summary),
                        steps,
                        steps_used,
                        verification: None,
                        tokens_estimated,
                        tool_calls,
                    };
                }

                let mut all_obs: Vec<String> = blocked_obs;
                for result in &exec_results {
                    let result_step = AgentStep::ToolResult(result.clone());
                    steps.push(result_step);
                    self.events.emit(&AgentEvent::ToolExecuted {
                        result: result.clone(),
                    });
                    tool_messages.push(ChatMessage::tool(
                        result.tool_call_id.clone(),
                        serde_json::to_string(&result.output).unwrap_or_default(),
                    ));

                    all_obs.push(format!(
                        "Observation ({}) [{}]: {}",
                        if result.success { "ok" } else { "error" },
                        result.tool_name,
                        serde_json::to_string_pretty(&result.output).unwrap_or_default()
                    ));

                    // Remember the most recent successful tool summary so
                    // that, if a later LLM turn comes back empty, the task
                    // can finalize from real work instead of hard-failing.
                    if result.success && !result.display_summary.trim().is_empty() {
                        last_good_summary =
                            Some(format!("{}: {}", result.tool_name, result.display_summary));
                    }

                    // Same-tool loop detection.
                    if last_tool_name.as_deref() == Some(&result.tool_name) {
                        same_tool_count = same_tool_count.saturating_add(1);
                    } else {
                        last_tool_name = Some(result.tool_name.clone());
                        same_tool_count = 1;
                    }
                }

                // Hard stop: 3+ consecutive same-tool calls → answer now.
                // Soft nudge: 2 consecutive → use what you already have.
                if same_tool_count >= HARD_SAME_TOOL_LIMIT {
                    let tool_name = last_tool_name.as_deref().unwrap_or("unknown");
                    all_obs.push(format!(
                        "SYSTEM: You have called {} {} times in a row. You MUST answer NOW \
                         with whatever information you have. Do NOT call any more tools. If \
                         the tool results don't contain what was requested, tell the user \
                         honestly what you see and what's missing.",
                        tool_name, same_tool_count
                    ));
                    all_obs.push(
                        "SYSTEM: A final answer is your ONLY option now. Output it immediately."
                            .to_string(),
                    );
                } else if same_tool_count >= MAX_SAME_TOOL_CONSECUTIVE {
                    let tool_name = last_tool_name.as_deref().unwrap_or("unknown");
                    all_obs.push(format!(
                        "SYSTEM: You've called {} {} times. If the results above answer the \
                         request \"{}\", answer NOW. Do NOT call {} again — use what you \
                         already have.",
                        tool_name,
                        same_tool_count,
                        safe_prompt.chars().take(120).collect::<String>(),
                        tool_name
                    ));
                } else {
                    all_obs.push(format!(
                        "SYSTEM: You now have the tool results above. If these results answer \
                         the original request \"{}\", respond with your final answer \
                         IMMEDIATELY. Do NOT call the same tool again unless it explicitly \
                         failed.",
                        safe_prompt.chars().take(120).collect::<String>()
                    ));
                }

                // Tool-role answers first, then the wrap-up nudge.
                conversation.extend(tool_messages);
                if !all_obs.is_empty() {
                    conversation.push(ChatMessage::user(all_obs.join("\n\n")));
                }
                continue;
            }

            // ── Final answer + self-verification ────────────────────────────
            final_answer = Some(assistant_content.clone());
            steps.push(AgentStep::FinalAnswer {
                content: assistant_content.clone(),
            });
            self.events.emit(&AgentEvent::FinalAnswer {
                content: assistant_content.clone(),
            });

            // One extra turn at near-zero cost: ask the model whether its
            // answer fully addresses the original request. INCOMPLETE
            // re-enters the loop with the model's own feedback; an inference
            // error defaults to VERIFIED so a transient failure never wedges
            // an otherwise-finished task. The prompt includes the candidate
            // answer because this verification runs as a standalone
            // completion — and it is PII-stripped like every other message.
            let mut verify_prompt = format!(
                "You just completed this task: \"{}\"\nYour final answer was: \"{}\"\n\
                 Verification check: Does the final answer fully and correctly address \
                 the task?\n\
                 Reply with exactly one of:\n\
                 VERIFIED — answer is complete and correct.\n\
                 INCOMPLETE: <brief description of what is missing or wrong>",
                safe_prompt.chars().take(200).collect::<String>(),
                assistant_content
            );
            // M6b: retrieved prior cases — observation-format evidence —
            // appended to the verification prompt only. An empty result
            // leaves the prompt byte-identical to the no-library build.
            if let Some(library) = &self.cases {
                let cases = library.retrieve(&safe_prompt, &used_tool_names, 3).await;
                if let Some(section) = evidence_section(&cases) {
                    verify_prompt.push('\n');
                    verify_prompt.push('\n');
                    verify_prompt.push_str(&section);
                }
            }
            let verify_prompt = match &self.privacy {
                Some(policy) if policy.auto_redact_pii => {
                    let stripped = amparo_privacy::secure_minions_strip(&verify_prompt);
                    self.emit_privacy_stripped(&stripped.pii_map);
                    stripped.sanitised_text
                }
                _ => verify_prompt,
            };
            conversation.push(ChatMessage::user(verify_prompt.clone()));

            // The verification completion is billed like any call — count
            // the prompt up front, the reply when it arrives.
            tokens_estimated = tokens_estimated.saturating_add(estimate_tokens(&verify_prompt));

            let verify_text = match self
                .inference
                .complete(InferenceRequest {
                    prompt: verify_prompt,
                    max_tokens: Some(256),
                    temperature: Some(0.0),
                    model: self.config.model.clone(),
                    ..Default::default()
                })
                .await
            {
                Ok(resp) => {
                    let t = strip_think_tags(resp.text.trim());
                    if t.is_empty() {
                        "VERIFIED".to_string()
                    } else {
                        t
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        "[amparo-agent] self-verification inference failed: {e} — defaulting to VERIFIED"
                    );
                    "VERIFIED".to_string()
                }
            };
            tokens_estimated = tokens_estimated.saturating_add(estimate_tokens(&verify_text));

            match interpret_verification(&verify_text) {
                VerificationDecision::Complete => {
                    self.events.emit(&AgentEvent::Verification {
                        decision: "complete".to_string(),
                        feedback: None,
                    });
                    verification = Some(Verification {
                        decision: "complete".to_string(),
                        feedback: None,
                    });
                }
                VerificationDecision::Incomplete(feedback) => {
                    self.events.emit(&AgentEvent::Verification {
                        decision: "incomplete".to_string(),
                        feedback: Some(feedback.clone()),
                    });
                    verification = Some(Verification {
                        decision: "incomplete".to_string(),
                        feedback: Some(feedback.clone()),
                    });
                    // The model found a gap — feed its feedback back and
                    // keep the loop going.
                    conversation.push(ChatMessage::assistant(verify_text.trim()));
                    conversation.push(ChatMessage::user(format!(
                        "Your answer was incomplete. Please continue and address: {}",
                        feedback
                    )));
                    continue;
                }
            }

            // VERIFIED — the task is complete.
            conversation.push(ChatMessage::assistant("VERIFIED"));
            let content = final_answer.take().unwrap_or_default();
            self.events.emit(&AgentEvent::TaskComplete {
                final_answer: content.clone(),
                task_id: self.task_id.clone(),
            });
            self.persist_checkpoint(
                &session,
                SessionStatus::Complete,
                &prompt,
                &conversation,
                LoopState {
                    last_tool_name: last_tool_name.clone(),
                    same_tool_count,
                    empty_turn_retried,
                    last_good_summary: last_good_summary.clone(),
                    used_tool_names: used_tool_names.clone(),
                    steps_used,
                },
                Some(&content),
            );
            return AgentReport {
                status: TaskStatus::Complete,
                final_answer: Some(content),
                steps,
                steps_used,
                verification,
                tokens_estimated,
                tool_calls,
            };
        }

        // Exhausted max steps — fail honestly.
        let message = "Max steps reached".to_string();
        steps.push(AgentStep::Error {
            message: message.clone(),
        });
        self.events.emit(&AgentEvent::TaskFailed {
            message: message.clone(),
        });
        self.persist_checkpoint(
            &session,
            SessionStatus::Failed,
            &prompt,
            &conversation,
            LoopState {
                last_tool_name: last_tool_name.clone(),
                same_tool_count,
                empty_turn_retried,
                last_good_summary: last_good_summary.clone(),
                used_tool_names: used_tool_names.clone(),
                steps_used,
            },
            final_answer.as_deref(),
        );
        AgentReport {
            status: TaskStatus::Failed,
            final_answer,
            steps,
            steps_used,
            verification,
            tokens_estimated,
            tool_calls,
        }
    }

    /// Snapshot the task through the attached checkpoint store (M7):
    /// PII-stripped at write with the placeholder map discarded (I6) —
    /// the prompt, every message, the last tool summary and the final
    /// answer — and system-role messages excluded (I5). With no store
    /// attached this is a no-op; a failing save only warns — persistence
    /// must never fail the task.
    fn persist_checkpoint(
        &self,
        session: &SessionHandle,
        status: SessionStatus,
        prompt: &str,
        conversation: &[ChatMessage],
        loop_state: LoopState,
        final_answer: Option<&str>,
    ) {
        let (Some(store), Some(tenant)) = (&self.checkpoints, &session.tenant) else {
            return;
        };
        let checkpoint = Checkpoint {
            version: crate::session::CHECKPOINT_VERSION,
            tenant: tenant.clone(),
            task_id: session.task_id.clone(),
            parent_task_id: session.parent_task_id.clone(),
            started_at: session.started_at,
            prompt: amparo_privacy::secure_minions_strip(prompt).sanitised_text,
            status,
            conversation: conversation
                .iter()
                .filter(|message| message.role != "system")
                .map(|message| ChatMessage {
                    role: message.role.clone(),
                    content: amparo_privacy::secure_minions_strip(&message.content).sanitised_text,
                    // Tool-call arguments are user data too (I6) — strip
                    // them like any other persisted field.
                    tool_calls: message.tool_calls.as_ref().map(|calls| {
                        calls
                            .iter()
                            .map(|call| AssistantToolCall {
                                id: call.id.clone(),
                                call_type: call.call_type.clone(),
                                function: FunctionCall {
                                    name: call.function.name.clone(),
                                    arguments: amparo_privacy::secure_minions_strip(
                                        &call.function.arguments,
                                    )
                                    .sanitised_text,
                                },
                            })
                            .collect()
                    }),
                    tool_call_id: message.tool_call_id.clone(),
                })
                .collect(),
            loop_state: LoopState {
                last_tool_name: loop_state.last_tool_name,
                same_tool_count: loop_state.same_tool_count,
                empty_turn_retried: loop_state.empty_turn_retried,
                // The summary is tool-output-derived — strip it like
                // every other persisted field, or continuity would
                // carry a live PII string into the next task (I6).
                last_good_summary: loop_state
                    .last_good_summary
                    .map(|summary| amparo_privacy::secure_minions_strip(&summary).sanitised_text),
                used_tool_names: loop_state.used_tool_names,
                steps_used: loop_state.steps_used,
            },
            final_answer: final_answer
                .map(|answer| amparo_privacy::secure_minions_strip(answer).sanitised_text),
        };
        if let Err(error) = store.save(&checkpoint) {
            tracing::warn!("[amparo-agent] checkpoint save failed: {error}");
        }
    }

    /// Record a call that a gate blocked: a failed ToolResult step, a
    /// ToolGate event, a legible observation for the model, and the
    /// mandatory tool-role answer so the provider accepts the next request.
    fn block_call(
        &self,
        result: &ToolResult,
        decision: &str,
        reasons: Vec<String>,
        steps: &mut Vec<AgentStep>,
        blocked_obs: &mut Vec<String>,
        tool_messages: &mut Vec<ChatMessage>,
    ) {
        steps.push(AgentStep::ToolResult(result.clone()));
        self.events.emit(&AgentEvent::ToolGate {
            call_id: result.tool_call_id.clone(),
            tool_name: result.tool_name.clone(),
            decision: decision.to_string(),
            reasons,
        });
        blocked_obs.push(format!(
            "Observation (error) [{}]: {}",
            result.tool_name,
            result
                .output
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("blocked")
        ));
        tool_messages.push(ChatMessage::tool(
            result.tool_call_id.clone(),
            serde_json::to_string(&result.output).unwrap_or_default(),
        ));
    }

    /// Gate one call: registry lookup → trust ceiling → policy engine →
    /// human approval. Returns [`GateOutcome::Ready`] with the gate reasons,
    /// or [`GateOutcome::Blocked`] with the failed result the caller must
    /// record (model calls via [`Self::block_call`]; skill steps into the
    /// expansion record). Emits `ApprovalRequested`/`ApprovalResolved` when
    /// the approval gate runs; never emits `ToolGate` — the caller decides.
    ///
    /// `session_label` (M8) names who is asking on the approval copy —
    /// `sub-agent sess-123.1 of task sess-123` — or `None` for a
    /// top-level agent. The loop computes it once and every call inside
    /// shares it. Display-only (I1).
    async fn gate_call(&self, call: &ToolCall, session_label: Option<&str>) -> GateOutcome {
        let check = gate_check(
            &self.registry,
            self.config.trust_ceiling,
            self.policy.as_ref(),
            call,
        )
        .await;
        let (reasons, escalate_pending, tier) = match check.outcome {
            GateOutcomeCore::Blocked {
                result,
                decision,
                reasons,
            } => {
                return GateOutcome::Blocked {
                    result,
                    decision,
                    reasons,
                };
            }
            GateOutcomeCore::Ready {
                reasons,
                escalate_pending,
                tier,
            } => (reasons, escalate_pending, tier),
        };

        // Preflight (M7): classify the call's blast radius for the
        // approval copy. Display-only — the gate has already decided;
        // this label tells the human what they are approving, never what
        // may run (I1). No path policy attached → no classification.
        let blast_radius = self
            .path_policy
            .as_ref()
            .map(|policy| classify(&self.registry, policy, call));

        // Human-approval gate — tier ≥ ExternalEffector or a policy
        // Escalate. The gate decides how a human is asked and when to
        // auto-deny; Amparo's built-ins auto-deny.
        if escalate_pending || tier >= ToolTrustTier::ExternalEffector {
            let mut ask_reasons = reasons.clone();
            if escalate_pending {
                ask_reasons.push("policy escalated this call for human review".to_string());
            }
            if tier >= ToolTrustTier::ExternalEffector {
                ask_reasons.push(format!("tool tier {:?} requires human approval", tier));
            }
            let approval_request = ApprovalRequest {
                call_id: call.id.clone(),
                tool_name: call.name.clone(),
                arguments: call.arguments.clone(),
                reasons: ask_reasons.clone(),
                blast_radius,
                session_label: session_label.map(str::to_string),
            };
            self.events.emit(&AgentEvent::ApprovalRequested {
                call_id: call.id.clone(),
                tool_name: call.name.clone(),
                reasons: ask_reasons.clone(),
            });
            let approved = self.approval.request(&approval_request).await;
            self.events.emit(&AgentEvent::ApprovalResolved {
                call_id: call.id.clone(),
                approved,
            });
            if !approved {
                return GateOutcome::Blocked {
                    result: ToolResult {
                        tool_call_id: call.id.clone(),
                        tool_name: call.name.clone(),
                        success: false,
                        output: serde_json::json!({"error": "User denied the action or approval timed out"}),
                        display_summary: "Denied by user".to_string(),
                        duration_ms: 0,
                    },
                    decision: "approval_denied".to_string(),
                    reasons: vec!["human approval denied".to_string()],
                };
            }
        }

        GateOutcome::Ready { reasons }
    }

    /// Execute one already-gated call through its executor, retrying failed
    /// executions up to [`MAX_TOOL_RETRIES`] times.
    async fn execute_call(&self, call: &ToolCall) -> ToolResult {
        let execute = || async {
            match self.registry.get_executor(&call.name) {
                Some(executor) => executor.execute(call).await,
                // Pre-checked by the gate; keep an honest fallback.
                None => ToolResult {
                    tool_call_id: call.id.clone(),
                    tool_name: call.name.clone(),
                    success: false,
                    output: serde_json::json!({"error": "unknown tool"}),
                    display_summary: "Unknown tool".to_string(),
                    duration_ms: 0,
                },
            }
        };
        let mut result = execute().await;
        let mut retry = 0usize;
        while !result.success && retry < MAX_TOOL_RETRIES {
            retry += 1;
            result = execute().await;
        }
        result
    }

    /// Expand a `use_skill` call into its ordered steps, running each one
    /// through the full gate chain individually (registry → ceiling →
    /// policy → approval) before executing it. A blocked step aborts the
    /// skill: remaining steps are skipped and the overall result fails with
    /// the per-step record including the block reason.
    ///
    /// Steps emit the standard `ToolCallRequested`/`ToolGate`/`ToolExecuted`
    /// events (so the notebook records the expansion) but add no
    /// conversation messages and do not feed the same-tool counter — an
    /// authored finite sequence is not a loop symptom. The returned
    /// `Vec<String>` is the step tool names in first-use order, feeding the
    /// case-library retrieval query.
    ///
    /// `session_label` (M8) is the loop's who-is-asking label; every
    /// step's approval carries it, exactly like a model call.
    async fn expand_skill(
        &self,
        call: &ToolCall,
        session_label: Option<&str>,
    ) -> (ToolResult, Vec<String>) {
        let skill_name = call.arg_str("skill_name").unwrap_or("").to_string();
        let names = self
            .skills
            .as_ref()
            .map(|library| library.names())
            .unwrap_or_default();
        let spec = match self.skills.as_ref().and_then(|l| l.get(&skill_name)) {
            Some(spec) => spec,
            None => {
                // Defensive: the tool is only registered with a library
                // attached. An honest failure names what was available.
                let listed = if names.is_empty() {
                    "(none)".to_string()
                } else {
                    names.join(", ")
                };
                let error = format!("Unknown skill: {}. Adopted skills: {}", skill_name, listed);
                return (
                    ToolResult {
                        tool_call_id: call.id.clone(),
                        tool_name: call.name.clone(),
                        success: false,
                        output: serde_json::json!({"error": error}),
                        display_summary: "Unknown skill".to_string(),
                        duration_ms: 0,
                    },
                    Vec::new(),
                );
            }
        };

        let total = spec.steps.len();
        let mut done: Vec<serde_json::Value> = Vec::new();
        let mut step_tools: Vec<String> = Vec::new();
        let mut duration_ms: u64 = 0;
        let mut blocked = false;

        for (k, step) in spec.steps.iter().enumerate() {
            let step_call = ToolCall {
                id: format!("{}-step-{}", call.id, k),
                name: step.tool.clone(),
                arguments: step.arguments.clone(),
            };
            if !step_tools.contains(&step_call.name) {
                step_tools.push(step_call.name.clone());
            }
            self.events.emit(&AgentEvent::ToolCallRequested {
                call: step_call.clone(),
            });
            match self.gate_call(&step_call, session_label).await {
                GateOutcome::Blocked {
                    result,
                    decision,
                    reasons,
                } => {
                    self.events.emit(&AgentEvent::ToolGate {
                        call_id: step_call.id.clone(),
                        tool_name: step_call.name.clone(),
                        decision: decision.clone(),
                        reasons: reasons.clone(),
                    });
                    done.push(serde_json::json!({
                        "step": k + 1,
                        "tool": step_call.name,
                        "success": false,
                        "blocked": true,
                        "decision": decision,
                        "reasons": reasons,
                        "error": result.output.get("error").cloned().unwrap_or(serde_json::Value::Null),
                    }));
                    blocked = true;
                    break;
                }
                GateOutcome::Ready { reasons } => {
                    self.events.emit(&AgentEvent::ToolGate {
                        call_id: step_call.id.clone(),
                        tool_name: step_call.name.clone(),
                        decision: "allowed".to_string(),
                        reasons,
                    });
                    let result = self.execute_call(&step_call).await;
                    duration_ms = duration_ms.saturating_add(result.duration_ms);
                    done.push(serde_json::json!({
                        "step": k + 1,
                        "tool": step_call.name,
                        "success": result.success,
                        "display_summary": result.display_summary,
                        "output": result.output,
                    }));
                    self.events.emit(&AgentEvent::ToolExecuted { result });
                }
            }
        }

        let output = serde_json::json!({
            "skill": skill_name,
            "success": !blocked,
            "steps": done,
            "skipped": total - done.len(),
            "expected_outcome": spec.expected_outcome,
        });
        let (success, summary) = if blocked {
            (
                false,
                format!(
                    "skill {}: blocked at step {} of {} — remaining steps skipped",
                    skill_name,
                    done.len(),
                    total
                ),
            )
        } else {
            (
                true,
                format!(
                    "skill {}: {}/{} steps succeeded — {}",
                    skill_name,
                    done.len(),
                    total,
                    truncate(&spec.expected_outcome)
                ),
            )
        };
        (
            ToolResult {
                tool_call_id: call.id.clone(),
                tool_name: call.name.clone(),
                success,
                output,
                display_summary: summary,
                duration_ms,
            },
            step_tools,
        )
    }
}

/// Map a tool call to the policy gate's `(target, params)` shape.
///
/// The target is the primary argument the engine judges: the shell command
/// for `run_command`, the path for file tools, the URL/query for web tools.
/// Other arguments become supplementary key/value pairs. Calls with no
/// recognizable primary string argument use the compact JSON of the
/// arguments as the target.
///
/// Public because the same contract is shared by every surface that routes
/// calls to a [`amparo_policy::PolicyEngine`] — the agent loop and the MCP
/// server both use it.
pub fn extract_target(call: &ToolCall) -> (String, Vec<(String, String)>) {
    let primary = ["command", "path", "url", "query"].iter().find_map(|key| {
        call.arguments
            .get(*key)
            .and_then(|v| v.as_str())
            .map(|s| (*key, s.to_string()))
    });
    match primary {
        Some((key, target)) => {
            let params = call
                .arguments
                .as_object()
                .map(|obj| {
                    obj.iter()
                        .filter(|(k, _)| k.as_str() != key)
                        .map(|(k, v)| (k.clone(), value_to_string(v)))
                        .collect()
                })
                .unwrap_or_default();
            (target, params)
        }
        None => (
            serde_json::to_string(&call.arguments).unwrap_or_default(),
            Vec::new(),
        ),
    }
}

fn value_to_string(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Strip PII from every message's content (Secure Minions), giving each
/// message its own placeholder namespace — tokens are prefixed with the
/// message index — so placeholders from different messages cannot collide
/// during restore.
fn strip_messages(messages: &[ChatMessage]) -> (Vec<ChatMessage>, Vec<PiiPlaceholder>) {
    let mut out = Vec::with_capacity(messages.len());
    let mut merged: Vec<PiiPlaceholder> = Vec::new();
    for (i, msg) in messages.iter().enumerate() {
        let stripped = amparo_privacy::secure_minions_strip(&msg.content);
        let mut sanitised = stripped.sanitised_text.clone();
        let mut map = stripped.pii_map;
        for p in &mut map {
            let inner = p.token.trim_matches(['[', ']']);
            let unique = format!("[M{}_{}]", i, inner);
            sanitised = sanitised.replace(&p.token, &unique);
            p.token = unique;
        }
        merged.extend(map);
        out.push(ChatMessage {
            role: msg.role.clone(),
            content: sanitised,
            tool_calls: msg.tool_calls.clone(),
            tool_call_id: msg.tool_call_id.clone(),
        });
    }
    (out, merged)
}

/// Strip `<think>…</think>` reasoning tags from a reply.
fn strip_think_tags(raw: &str) -> String {
    if let (Some(s), Some(e)) = (raw.find("<think>"), raw.find("</think>")) {
        if s < e {
            return raw[e + 8..].trim().to_string();
        }
    }
    raw.trim().to_string()
}

/// Outcome of the self-verification pass over a Final Answer.
///
/// `Complete` — the model certified the answer (VERIFIED, or any
/// non-INCOMPLETE reply); the task finishes.
/// `Incomplete(feedback)` — the model reported a gap; the loop continues
/// with `feedback` (the text after the `INCOMPLETE:` prefix, or the whole
/// reply if the prefix is absent) fed back to the model.
#[derive(Debug, Clone, PartialEq, Eq)]
enum VerificationDecision {
    Complete,
    Incomplete(String),
}

/// Pure interpretation of a self-verification reply, kept separate from the
/// loop so both branches are unit-testable without a live backend.
fn interpret_verification(verify_text: &str) -> VerificationDecision {
    let trimmed = verify_text.trim();
    if trimmed.starts_with("INCOMPLETE") {
        let feedback = trimmed
            .strip_prefix("INCOMPLETE:")
            .unwrap_or(trimmed)
            .trim()
            .to_string();
        VerificationDecision::Incomplete(feedback)
    } else {
        VerificationDecision::Complete
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approval::AutoApprove;
    use amparo_inference::InferenceError;
    use amparo_policy::PolicyDecision;
    use amparo_tools::{SkillLibrary, SkillOrigin, SkillSpec, SkillStep, UseSkillTool};
    use async_trait::async_trait;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // ── Test doubles ─────────────────────────────────────────────────────────

    use crate::test_support::{
        registry_with, turn_text, turn_tool_call, AllowAllPolicy, EchoTool, RecordingGate,
        ScriptedProvider,
    };

    /// Returns a configured verdict per tool name and records what it saw.
    struct RecordingPolicy {
        verdicts: std::sync::Mutex<HashMap<String, PolicyVerdict>>,
        seen: std::sync::Mutex<Vec<(String, String)>>,
    }

    impl RecordingPolicy {
        fn new(verdicts: &[(&str, PolicyVerdict)]) -> Arc<Self> {
            Arc::new(Self {
                verdicts: std::sync::Mutex::new(
                    verdicts.iter().map(|(k, v)| (k.to_string(), *v)).collect(),
                ),
                seen: std::sync::Mutex::new(Vec::new()),
            })
        }

        fn seen(&self) -> Vec<(String, String)> {
            self.seen.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl PolicyEngine for RecordingPolicy {
        async fn judge_tool(
            &self,
            tool: &str,
            target: &str,
            _params: &[(&str, &str)],
        ) -> PolicyDecision {
            self.seen
                .lock()
                .unwrap()
                .push((tool.to_string(), target.to_string()));
            match self.verdicts.lock().unwrap().get(tool) {
                Some(PolicyVerdict::Allow) => PolicyDecision::allow(),
                Some(PolicyVerdict::Deny) => PolicyDecision::deny(format!("test deny {}", tool)),
                Some(PolicyVerdict::Escalate) => {
                    PolicyDecision::escalate(format!("test escalate {}", tool))
                }
                None => PolicyDecision::allow(),
            }
        }
    }

    fn echo_registry() -> (ToolRegistry, Arc<AtomicUsize>) {
        let (echo, calls) = EchoTool::new(ToolTrustTier::Observational);
        (registry_with(Arc::new(echo)), calls)
    }

    // ── Loop tests ───────────────────────────────────────────────────────────

    #[tokio::test]
    async fn tool_call_round_trip_reaches_verified_final_answer() {
        let provider = ScriptedProvider::new();
        provider.push_chat(turn_tool_call("call_1", "echo", r#"{"message":"hi"}"#));
        provider.push_chat(turn_text("The answer is 42."));
        provider.push_verify("VERIFIED");

        let (registry, calls) = echo_registry();
        let agent = Agent::new(provider.clone(), registry, Arc::new(AllowAllPolicy))
            .with_approval(Arc::new(AutoApprove));

        let report = agent.run("what is the answer?").await;
        assert_eq!(report.status, TaskStatus::Complete);
        assert_eq!(report.final_answer.as_deref(), Some("The answer is 42."));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let v = report.verification.expect("verification recorded");
        assert_eq!(v.decision, "complete");

        // Steps record the whole chain.
        assert!(matches!(&report.steps[0], AgentStep::ToolCall(c) if c.name == "echo"));
        assert!(report
            .steps
            .iter()
            .any(|s| matches!(s, AgentStep::ToolResult(r) if r.success)));
        assert!(report
            .steps
            .iter()
            .any(|s| matches!(s, AgentStep::FinalAnswer { .. })));
    }

    #[tokio::test]
    async fn report_accumulates_token_estimate_and_tool_calls() {
        let provider = ScriptedProvider::new();
        provider.push_chat(turn_tool_call("call_1", "echo", r#"{"message":"hi"}"#));
        provider.push_chat(turn_text("The answer is 42."));
        provider.push_verify("VERIFIED");

        let (registry, calls) = echo_registry();
        let agent = Agent::new(provider.clone(), registry, Arc::new(AllowAllPolicy))
            .with_approval(Arc::new(AutoApprove));

        let report = agent.run("what is the answer?").await;
        assert_eq!(report.status, TaskStatus::Complete);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // One tool call dispatched → counted once.
        assert_eq!(report.tool_calls, 1);

        // Recompute the expected estimate from what the provider recorded —
        // this pins the counting contract, not a magic number: outgoing
        // request JSON per chat turn, the returned content and tool-call
        // JSON, then the verification prompt and reply.
        let mut expected = 0usize;
        let add = |acc: &mut usize, text: &str| *acc += estimate_tokens(text);

        let chat_requests = provider.recorded_requests();
        assert_eq!(chat_requests.len(), 2);
        for req in &chat_requests {
            add(
                &mut expected,
                &serde_json::to_string(&req.messages).unwrap(),
            );
        }
        // Turn 1: empty content (zero), plus the tool call reconstructed
        // exactly as the SSE accumulator assembles it.
        let assembled = vec![AssistantToolCall {
            id: "call_1".to_string(),
            call_type: "function".to_string(),
            function: FunctionCall {
                name: "echo".to_string(),
                arguments: r#"{"message":"hi"}"#.to_string(),
            },
        }];
        add(&mut expected, &serde_json::to_string(&assembled).unwrap());
        // Turn 2: content only.
        add(&mut expected, "The answer is 42.");
        // The verification completion: the prompt as sent, the reply as
        // received ("VERIFIED" — the scripted reply, unchanged).
        let complete_requests = provider.recorded_complete_requests();
        assert_eq!(complete_requests.len(), 1);
        add(&mut expected, &complete_requests[0].prompt);
        add(&mut expected, "VERIFIED");

        assert_eq!(report.tokens_estimated, expected);
        assert!(report.tokens_estimated > 0);
    }

    // ── Case library (M6b) tests ────────────────────────────────────────────

    fn evidence_case(id: &str) -> crate::cases::EvidenceCase {
        crate::cases::EvidenceCase {
            id: id.to_string(),
            date: "2026-08-14".to_string(),
            verdict: "VERIFIED".to_string(),
            task_text: "run the tests".to_string(),
            tool_names: vec!["run_command".to_string()],
            outcome: Some("all passed".to_string()),
        }
    }

    /// Returns scripted cases and records every retrieval for assertions.
    struct RecordingCaseLibrary {
        cases: std::sync::Mutex<Vec<crate::cases::EvidenceCase>>,
        seen: std::sync::Mutex<Vec<(String, Vec<String>, usize)>>,
    }

    impl RecordingCaseLibrary {
        fn new(cases: Vec<crate::cases::EvidenceCase>) -> Arc<Self> {
            Arc::new(Self {
                cases: std::sync::Mutex::new(cases),
                seen: std::sync::Mutex::new(Vec::new()),
            })
        }

        fn seen(&self) -> Vec<(String, Vec<String>, usize)> {
            self.seen.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl CaseLibrary for RecordingCaseLibrary {
        async fn retrieve(
            &self,
            task_text: &str,
            tool_names: &[String],
            limit: usize,
        ) -> Vec<crate::cases::EvidenceCase> {
            self.seen
                .lock()
                .unwrap()
                .push((task_text.to_string(), tool_names.to_vec(), limit));
            self.cases.lock().unwrap().clone()
        }
    }

    #[tokio::test]
    async fn case_library_feeds_verification_prompt_only() {
        let provider = ScriptedProvider::new();
        provider.push_chat(turn_text("The answer."));
        provider.push_verify("VERIFIED");

        let library = RecordingCaseLibrary::new(vec![evidence_case("rec-1")]);
        let (registry, _calls) = echo_registry();
        let agent = Agent::new(provider.clone(), registry, Arc::new(AllowAllPolicy))
            .with_approval(Arc::new(AutoApprove))
            .with_case_library(library.clone());

        let report = agent.run("run the tests").await;
        assert_eq!(report.status, TaskStatus::Complete);

        // The evidence section reached the verification completion…
        let verify_prompt = provider
            .recorded_complete_requests()
            .into_iter()
            .map(|r| r.prompt)
            .find(|p| p.contains("Verification check"))
            .expect("verification completion ran");
        assert!(verify_prompt.contains("Prior cases in this tenant resembling the current task:"));
        assert!(verify_prompt.contains("Case rec-1 (2026-08-14, VERIFIED)"));

        // …and nowhere in the action-loop chat requests.
        for req in provider.recorded_requests() {
            for msg in req.messages {
                assert!(
                    !msg.content.contains("Prior cases"),
                    "evidence must never reach the action loop"
                );
            }
        }

        // Retrieval saw the stripped task and the host's limit.
        assert_eq!(library.seen().len(), 1);
        assert_eq!(library.seen()[0].0, "run the tests");
        assert_eq!(library.seen()[0].2, 3);
    }

    #[tokio::test]
    async fn empty_case_library_leaves_verify_prompt_unchanged() {
        let provider = ScriptedProvider::new();
        provider.push_chat(turn_text("The answer."));
        provider.push_verify("VERIFIED");

        let library = RecordingCaseLibrary::new(vec![]);
        let (registry, _calls) = echo_registry();
        let agent = Agent::new(provider.clone(), registry, Arc::new(AllowAllPolicy))
            .with_approval(Arc::new(AutoApprove))
            .with_case_library(library.clone());

        let report = agent.run("run the tests").await;
        assert_eq!(report.status, TaskStatus::Complete);
        assert_eq!(library.seen().len(), 1, "retrieval still ran");
        let verify_prompt = provider
            .recorded_complete_requests()
            .into_iter()
            .map(|r| r.prompt)
            .find(|p| p.contains("Verification check"))
            .expect("verification completion ran");
        assert!(!verify_prompt.contains("Prior cases"));
    }

    #[tokio::test]
    async fn retrieval_receives_stripped_task_and_tool_names() {
        let provider = ScriptedProvider::new();
        provider.push_chat(turn_tool_call("call_1", "echo", r#"{"message":"hi"}"#));
        provider.push_chat(turn_text("The answer."));
        provider.push_verify("VERIFIED");

        let library = RecordingCaseLibrary::new(vec![]);
        let (registry, _calls) = echo_registry();
        let agent = Agent::new(provider.clone(), registry, Arc::new(AllowAllPolicy))
            .with_approval(Arc::new(AutoApprove))
            .with_privacy(Arc::new(amparo_privacy::PrivacyPolicy::default()))
            .with_case_library(library.clone());

        let report = agent.run("email user@example.com please").await;
        assert_eq!(report.status, TaskStatus::Complete);

        let seen = library.seen();
        assert_eq!(seen.len(), 1, "one retrieval per verification round");
        assert!(
            seen[0].0.contains("[EMAIL_1]"),
            "retrieval sees the PII-stripped task, got: {}",
            seen[0].0
        );
        assert!(!seen[0].0.contains("user@example.com"));
        assert_eq!(
            seen[0].1,
            vec!["echo".to_string()],
            "tool names reach retrieval"
        );
    }

    #[tokio::test]
    async fn one_shot_path_never_consults_case_library() {
        let provider = ScriptedProvider::new();
        provider.push_chat(turn_tool_call("call_1", "echo", r#"{"message":"hi"}"#));

        let library = RecordingCaseLibrary::new(vec![evidence_case("rec-1")]);
        let (registry, _calls) = echo_registry();
        let agent = Agent::new(provider.clone(), registry, Arc::new(AllowAllPolicy))
            .with_approval(Arc::new(AutoApprove))
            .with_case_library(library.clone());

        let report = agent.run("open the thing").await;
        assert_eq!(report.status, TaskStatus::Complete);
        assert!(report.verification.is_none(), "one-shot skips verification");
        assert!(
            library.seen().is_empty(),
            "no verification ran, so no retrieval may run"
        );
        assert!(provider.recorded_complete_requests().is_empty());
    }

    // ── Skill expansion (M6c) tests ──────────────────────────────────────────

    /// A test skill library over owned specs.
    struct SpecLibrary(Vec<SkillSpec>);

    impl SkillLibrary for SpecLibrary {
        fn names(&self) -> Vec<String> {
            self.0.iter().map(|s| s.name.clone()).collect()
        }
        fn get(&self, name: &str) -> Option<SkillSpec> {
            self.0.iter().find(|s| s.name == name).cloned()
        }
    }

    /// A skill whose `n` steps all echo a fixed message.
    fn skill_spec(name: &str, n: usize) -> SkillSpec {
        SkillSpec {
            name: name.to_string(),
            description: "echoes a greeting".to_string(),
            preconditions: vec![],
            steps: (0..n)
                .map(|i| SkillStep {
                    tool: "echo".to_string(),
                    arguments: serde_json::json!({"message": format!("hi {i}")}),
                })
                .collect(),
            expected_outcome: "the greeting is echoed".to_string(),
            origin: SkillOrigin::Operator,
            source_run_ids: vec![],
            adopted_at: None,
        }
    }

    /// A registry with `echo` plus a `use_skill` tool over the given specs.
    /// Returns the echo counter and the library for `with_skills`.
    fn skill_registry(
        specs: Vec<SkillSpec>,
    ) -> (ToolRegistry, Arc<AtomicUsize>, Arc<dyn SkillLibrary>) {
        let (echo, calls) = EchoTool::new(ToolTrustTier::Observational);
        let mut registry = registry_with(Arc::new(echo));
        let library: Arc<dyn SkillLibrary> = Arc::new(SpecLibrary(specs));
        registry.register(Arc::new(UseSkillTool::new(Arc::clone(&library))));
        (registry, calls, library)
    }

    #[tokio::test]
    async fn skill_expansion_runs_every_step_and_answers_once() {
        let provider = ScriptedProvider::new();
        provider.push_chat(turn_tool_call(
            "call_1",
            USE_SKILL,
            r#"{"skill_name":"greet"}"#,
        ));
        provider.push_chat(turn_text("Done."));
        provider.push_verify("VERIFIED");

        let (registry, calls, library) = skill_registry(vec![skill_spec("greet", 2)]);
        let agent = Agent::new(provider.clone(), registry, Arc::new(AllowAllPolicy))
            .with_approval(Arc::new(AutoApprove))
            .with_skills(library);

        let report = agent.run("greet me").await;
        assert_eq!(report.status, TaskStatus::Complete);
        assert_eq!(calls.load(Ordering::SeqCst), 2, "both steps executed");

        // The synthesized skill result is recorded like any batch result.
        let skill_result = report
            .steps
            .iter()
            .find_map(|s| match s {
                AgentStep::ToolResult(r) if r.tool_name == "use_skill" => Some(r),
                _ => None,
            })
            .expect("use_skill result recorded");
        assert!(skill_result.success);
        assert_eq!(skill_result.output["steps"].as_array().unwrap().len(), 2);
        assert_eq!(skill_result.output["skipped"], 0);
        assert!(
            skill_result
                .display_summary
                .contains("skill greet: 2/2 steps succeeded"),
            "got: {}",
            skill_result.display_summary
        );

        // Exactly ONE tool-role message answers the model's use_skill call —
        // the steps never become conversation messages.
        let requests = provider.recorded_requests();
        let tool_answers: Vec<_> = requests[1]
            .messages
            .iter()
            .filter(|m| m.role == "tool")
            .collect();
        assert_eq!(tool_answers.len(), 1);
        assert_eq!(tool_answers[0].tool_call_id.as_deref(), Some("call_1"));
    }

    #[tokio::test]
    async fn unknown_skill_fails_naming_available_skills() {
        let provider = ScriptedProvider::new();
        provider.push_chat(turn_tool_call(
            "call_1",
            USE_SKILL,
            r#"{"skill_name":"nope"}"#,
        ));
        provider.push_chat(turn_text("Done."));
        provider.push_verify("VERIFIED");

        let (registry, calls, library) = skill_registry(vec![skill_spec("greet", 1)]);
        let agent = Agent::new(provider.clone(), registry, Arc::new(AllowAllPolicy))
            .with_approval(Arc::new(AutoApprove))
            .with_skills(library);

        let report = agent.run("use a skill").await;
        assert_eq!(calls.load(Ordering::SeqCst), 0, "no steps run");
        let skill_result = report
            .steps
            .iter()
            .find_map(|s| match s {
                AgentStep::ToolResult(r) if r.tool_name == "use_skill" => Some(r),
                _ => None,
            })
            .expect("use_skill result recorded");
        assert!(!skill_result.success);
        let error = skill_result.output["error"].as_str().unwrap();
        assert!(error.contains("Unknown skill: nope"), "got: {error}");
        assert!(
            error.contains("greet"),
            "must list available skills: {error}"
        );
    }

    #[tokio::test]
    async fn blocked_step_aborts_skill_and_skips_the_rest() {
        let provider = ScriptedProvider::new();
        provider.push_chat(turn_tool_call(
            "call_1",
            USE_SKILL,
            r#"{"skill_name":"greet"}"#,
        ));
        provider.push_chat(turn_text("Done."));
        provider.push_verify("VERIFIED");

        let (registry, calls, library) = skill_registry(vec![skill_spec("greet", 3)]);
        // The engine allows use_skill itself but denies echo — the first
        // step is blocked, so no step may run.
        let policy = RecordingPolicy::new(&[("echo", PolicyVerdict::Deny)]);
        let sink = Arc::new(InMemoryEventSink::new());
        let agent = Agent::new(provider.clone(), registry, policy)
            .with_approval(Arc::new(AutoApprove))
            .with_skills(library)
            .with_events(sink.clone());

        let report = agent.run("greet me").await;
        assert_eq!(calls.load(Ordering::SeqCst), 0, "no step may execute");

        let skill_result = report
            .steps
            .iter()
            .find_map(|s| match s {
                AgentStep::ToolResult(r) if r.tool_name == "use_skill" => Some(r),
                _ => None,
            })
            .expect("use_skill result recorded");
        assert!(!skill_result.success);
        let steps = skill_result.output["steps"].as_array().unwrap();
        assert_eq!(steps.len(), 1, "the blocked step is recorded");
        assert_eq!(steps[0]["blocked"], true);
        assert_eq!(steps[0]["decision"], "policy_denied");
        assert_eq!(skill_result.output["skipped"], 2, "remaining steps skipped");
        assert!(
            skill_result
                .display_summary
                .contains("blocked at step 1 of 3"),
            "got: {}",
            skill_result.display_summary
        );

        // The step's gate decision was emitted as an event.
        let events = sink.snapshot();
        assert!(events.iter().any(|e| matches!(e,
            AgentEvent::ToolGate { call_id, decision, .. }
                if call_id == "call_1-step-0" && decision == "policy_denied")));
    }

    #[tokio::test]
    async fn skill_steps_emit_the_standard_event_trio() {
        let provider = ScriptedProvider::new();
        provider.push_chat(turn_tool_call(
            "call_1",
            USE_SKILL,
            r#"{"skill_name":"greet"}"#,
        ));
        provider.push_chat(turn_text("Done."));
        provider.push_verify("VERIFIED");

        let (registry, calls, library) = skill_registry(vec![skill_spec("greet", 2)]);
        let sink = Arc::new(InMemoryEventSink::new());
        let agent = Agent::new(provider.clone(), registry, Arc::new(AllowAllPolicy))
            .with_approval(Arc::new(AutoApprove))
            .with_skills(library)
            .with_events(sink.clone());

        let report = agent.run("greet me").await;
        assert_eq!(report.status, TaskStatus::Complete);
        assert_eq!(calls.load(Ordering::SeqCst), 2);

        let events = sink.snapshot();
        for k in 0..2 {
            let id = format!("call_1-step-{k}");
            assert!(events.iter().any(|e| matches!(e,
                AgentEvent::ToolCallRequested { call } if call.id == id)));
            assert!(events.iter().any(|e| matches!(e,
                AgentEvent::ToolGate { call_id, decision, .. }
                    if call_id == &id && decision == "allowed")));
            assert!(events.iter().any(|e| matches!(e,
                AgentEvent::ToolExecuted { result } if result.tool_call_id == id)));
        }
    }

    #[tokio::test]
    async fn use_skill_policy_target_is_the_skill_name() {
        let provider = ScriptedProvider::new();
        provider.push_chat(turn_tool_call(
            "call_1",
            USE_SKILL,
            r#"{"skill_name":"greet"}"#,
        ));
        provider.push_chat(turn_text("Done."));
        provider.push_verify("VERIFIED");

        let (registry, _calls, library) = skill_registry(vec![skill_spec("greet", 1)]);
        let policy = RecordingPolicy::new(&[]);
        let agent = Agent::new(provider.clone(), registry, policy.clone())
            .with_approval(Arc::new(AutoApprove))
            .with_skills(library);

        agent.run("greet me").await;
        // The use_skill check judged the skill name (not the JSON fallback);
        // the echo step then got its own check with the echo target.
        assert_eq!(
            policy.seen()[0],
            ("use_skill".to_string(), "greet".to_string())
        );
        assert!(policy.seen().iter().any(|(tool, _)| tool == "echo"));
    }

    #[tokio::test]
    async fn same_tool_steps_do_not_trip_the_loop_guard() {
        let provider = ScriptedProvider::new();
        provider.push_chat(turn_tool_call(
            "call_1",
            USE_SKILL,
            r#"{"skill_name":"greet"}"#,
        ));
        provider.push_chat(turn_text("Done."));
        provider.push_verify("VERIFIED");

        let (registry, calls, library) = skill_registry(vec![skill_spec("greet", 3)]);
        let agent = Agent::new(provider.clone(), registry, Arc::new(AllowAllPolicy))
            .with_approval(Arc::new(AutoApprove))
            .with_skills(library);

        let report = agent.run("greet me").await;
        assert_eq!(report.status, TaskStatus::Complete);
        assert_eq!(calls.load(Ordering::SeqCst), 3, "all three steps ran");

        // The steps must not count as same-tool repetition — no same-tool
        // nudge (soft or hard) may mention them.
        let requests = provider.recorded_requests();
        for req in &requests {
            for msg in &req.messages {
                assert!(
                    !msg.content.contains("times in a row")
                        && !msg.content.contains("You've called"),
                    "loop guard must not fire on skill steps: {}",
                    msg.content
                );
            }
        }
    }

    #[tokio::test]
    async fn policy_deny_on_use_skill_blocks_before_expansion() {
        let provider = ScriptedProvider::new();
        provider.push_chat(turn_tool_call(
            "call_1",
            USE_SKILL,
            r#"{"skill_name":"greet"}"#,
        ));

        let (registry, calls, library) = skill_registry(vec![skill_spec("greet", 2)]);
        let policy = RecordingPolicy::new(&[("use_skill", PolicyVerdict::Deny)]);
        let agent = Agent::new(provider.clone(), registry, policy).with_skills(library);

        let report = agent.run("greet me").await;
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "denied skill must not expand"
        );
        assert!(report
            .steps
            .iter()
            .any(|s| matches!(s, AgentStep::ToolResult(r)
            if r.tool_name == "use_skill" && !r.success
                && r.output["error"].as_str().unwrap_or("").contains("Policy denied use_skill"))));

        // The blocked call is still answered with a tool-role message.
        let requests = provider.recorded_requests();
        let tool_answer = requests[1].messages.iter().find(|m| m.role == "tool");
        assert_eq!(tool_answer.unwrap().tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(report.status, TaskStatus::Complete);
    }

    #[tokio::test]
    async fn policy_deny_blocks_execution_and_answers_the_call() {
        let provider = ScriptedProvider::new();
        provider.push_chat(turn_tool_call("call_1", "echo", r#"{"message":"hi"}"#));
        // exhausted → default "Done." final answer → default VERIFIED

        let (registry, calls) = echo_registry();
        let policy = RecordingPolicy::new(&[("echo", PolicyVerdict::Deny)]);
        let agent = Agent::new(provider.clone(), registry, policy.clone());

        let report = agent.run("do a thing").await;
        // The engine judged the call: no primary argument key, so the
        // compact JSON of the arguments was the target.
        assert_eq!(
            policy.seen(),
            vec![("echo".to_string(), "{\"message\":\"hi\"}".to_string())]
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "denied tool must not execute"
        );
        assert!(report
            .steps
            .iter()
            .any(|s| matches!(s, AgentStep::ToolResult(r) if !r.success
                && r.output["error"].as_str().unwrap_or("").contains("Policy denied"))));

        // The blocked call is still answered with a tool-role message.
        let requests = provider.recorded_requests();
        let tool_answer = requests[1].messages.iter().find(|m| m.role == "tool");
        let answer = tool_answer.expect("blocked call answered with tool-role message");
        assert_eq!(answer.tool_call_id.as_deref(), Some("call_1"));

        // The agent recovered and finished with the default answer.
        assert_eq!(report.status, TaskStatus::Complete);
    }

    #[tokio::test]
    async fn policy_escalate_asks_human_and_respects_denial() {
        let provider = ScriptedProvider::new();
        provider.push_chat(turn_tool_call("call_1", "echo", r#"{"message":"hi"}"#));

        let (registry, calls) = echo_registry();
        let policy = RecordingPolicy::new(&[("echo", PolicyVerdict::Escalate)]);
        let gate = RecordingGate::new(false);
        let sink = Arc::new(InMemoryEventSink::new());
        let agent = Agent::new(provider.clone(), registry, policy)
            .with_approval(gate.clone())
            .with_events(sink.clone());

        let report = agent.run("do a thing").await;
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(gate.requests().len(), 1);
        let request = &gate.requests()[0];
        assert_eq!(request.tool_name, "echo");
        assert!(request.reasons.iter().any(|r| r.contains("test escalate")));
        assert!(request
            .reasons
            .iter()
            .any(|r| r.contains("policy escalated")));

        let events = sink.snapshot();
        assert!(events.iter().any(|e| matches!(e,
            AgentEvent::ApprovalRequested { call_id, .. } if call_id == "call_1")));
        assert!(events.iter().any(|e| matches!(e,
            AgentEvent::ToolGate { decision, .. } if decision == "approval_denied")));
        assert!(report
            .steps
            .iter()
            .any(|s| matches!(s, AgentStep::ToolResult(r) if !r.success)));
    }

    #[tokio::test]
    async fn policy_escalate_with_approval_executes() {
        let provider = ScriptedProvider::new();
        provider.push_chat(turn_tool_call("call_1", "echo", r#"{"message":"hi"}"#));

        let (registry, calls) = echo_registry();
        let policy = RecordingPolicy::new(&[("echo", PolicyVerdict::Escalate)]);
        let gate = RecordingGate::new(true);
        let agent = Agent::new(provider.clone(), registry, policy).with_approval(gate.clone());

        let report = agent.run("do a thing").await;
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "approved escalated call executes"
        );
        assert_eq!(gate.requests().len(), 1);
        assert_eq!(report.status, TaskStatus::Complete);
    }

    #[tokio::test]
    async fn external_effector_tier_requires_approval_even_when_policy_allows() {
        let provider = ScriptedProvider::new();
        provider.push_chat(turn_tool_call("call_1", "echo", r#"{"message":"hi"}"#));

        let (echo, calls) = EchoTool::new(ToolTrustTier::ExternalEffector);
        let registry = registry_with(Arc::new(echo));

        // Default gate (AutoDeny): blocked without any explicit gate wiring.
        let gate = RecordingGate::new(false);
        let agent = Agent::new(provider.clone(), registry, Arc::new(AllowAllPolicy))
            .with_approval(gate.clone());
        let report = agent.run("do a thing").await;
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let request = &gate.requests()[0];
        assert!(request
            .reasons
            .iter()
            .any(|r| r.contains("requires human approval")));
        assert!(report
            .steps
            .iter()
            .any(|s| matches!(s, AgentStep::ToolResult(r) if !r.success)));
    }

    #[tokio::test]
    async fn sub_agent_approval_copy_names_the_delegation_chain() {
        let provider = ScriptedProvider::new();
        provider.push_chat(turn_tool_call("call_1", "echo", r#"{"message":"hi"}"#));

        let (echo, calls) = EchoTool::new(ToolTrustTier::ExternalEffector);
        let registry = registry_with(Arc::new(echo));
        let gate = RecordingGate::new(true);
        let agent = Agent::new(provider.clone(), registry, Arc::new(AllowAllPolicy))
            .with_approval(gate.clone())
            .with_task_id("sess-123.1")
            .with_parent_task_id("sess-123");
        let report = agent.run("do a thing").await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let request = &gate.requests()[0];
        assert_eq!(
            request.session_label.as_deref(),
            Some("sub-agent sess-123.1 of task sess-123")
        );
        assert!(report
            .steps
            .iter()
            .any(|s| matches!(s, AgentStep::ToolResult(r) if r.success)));
    }

    #[tokio::test]
    async fn trust_ceiling_blocks_higher_tiers() {
        let provider = ScriptedProvider::new();
        provider.push_chat(turn_tool_call("call_1", "echo", r#"{"message":"hi"}"#));

        let (echo, calls) = EchoTool::new(ToolTrustTier::LocalMutating);
        let registry = registry_with(Arc::new(echo));
        let config = AgentConfig {
            trust_ceiling: ToolTrustTier::Observational,
            ..Default::default()
        };
        let agent =
            Agent::new(provider.clone(), registry, Arc::new(AllowAllPolicy)).with_config(config);

        let report = agent.run("do a thing").await;
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(report
            .steps
            .iter()
            .any(|s| matches!(s, AgentStep::ToolResult(r)
            if r.output["error"].as_str().unwrap_or("").contains("trust ceiling"))));
    }

    #[tokio::test]
    async fn unknown_tool_gets_honest_error_and_tool_role_answer() {
        let provider = ScriptedProvider::new();
        provider.push_chat(turn_tool_call("call_1", "no_such_tool", "{}"));

        let (registry, _) = echo_registry();
        let agent = Agent::new(provider.clone(), registry, Arc::new(AllowAllPolicy));

        let report = agent.run("do a thing").await;
        assert!(report
            .steps
            .iter()
            .any(|s| matches!(s, AgentStep::ToolResult(r)
            if r.output["error"].as_str().unwrap_or("").contains("Unknown tool"))));

        let requests = provider.recorded_requests();
        let tool_answer = requests[1].messages.iter().find(|m| m.role == "tool");
        assert_eq!(tool_answer.unwrap().tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(report.status, TaskStatus::Complete);
    }

    #[tokio::test]
    async fn one_shot_shortcut_completes_without_further_turns() {
        let provider = ScriptedProvider::new();
        provider.push_chat(turn_tool_call(
            "call_1",
            "echo",
            r#"{"message":"pod bay doors"}"#,
        ));
        // A second script must never be consumed.
        provider.push_chat(turn_text("should not be used"));

        let (registry, calls) = echo_registry();
        let agent = Agent::new(provider.clone(), registry, Arc::new(AllowAllPolicy))
            .with_approval(Arc::new(AutoApprove));

        let report = agent.run("open the pod bay doors").await;
        assert_eq!(report.status, TaskStatus::Complete);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(report
            .final_answer
            .as_deref()
            .unwrap_or("")
            .contains("echoed: pod bay doors"));
        assert_eq!(
            provider.recorded_requests().len(),
            1,
            "one-shot made a single LLM turn"
        );
        assert!(
            report.verification.is_none(),
            "one-shot skips self-verification"
        );
    }

    #[tokio::test]
    async fn compound_request_is_not_one_shot() {
        let provider = ScriptedProvider::new();
        provider.push_chat(turn_tool_call("call_1", "echo", r#"{"message":"first"}"#));
        provider.push_chat(turn_text("And then the second thing."));
        provider.push_verify("VERIFIED");

        let (registry, calls) = echo_registry();
        let agent = Agent::new(provider.clone(), registry, Arc::new(AllowAllPolicy))
            .with_approval(Arc::new(AutoApprove));

        let report = agent.run("open the doors and close the windows").await;
        assert_eq!(report.status, TaskStatus::Complete);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // The loop continued past the first batch: more than one chat turn.
        assert!(provider.recorded_requests().len() >= 2);
        assert_eq!(
            report.final_answer.as_deref(),
            Some("And then the second thing.")
        );
    }

    #[tokio::test]
    async fn empty_turn_recovers_from_last_good_summary() {
        let provider = ScriptedProvider::new();
        provider.push_chat(turn_tool_call("call_1", "echo", r#"{"message":"hi"}"#));
        provider.push_chat(vec![]); // empty turn → nudge
        provider.push_chat(vec![]); // second empty turn → finalize from work

        let (registry, calls) = echo_registry();
        let agent = Agent::new(provider.clone(), registry, Arc::new(AllowAllPolicy))
            .with_approval(Arc::new(AutoApprove));

        let report = agent.run("do a thing").await;
        assert_eq!(report.status, TaskStatus::Complete);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let answer = report.final_answer.as_deref().unwrap_or("");
        assert!(
            answer.contains("Task completed"),
            "finalized from real work: {answer}"
        );
        assert!(
            answer.contains("echoed: hi"),
            "finalized from real work: {answer}"
        );
    }

    #[tokio::test]
    async fn empty_turn_without_prior_work_fails_honestly() {
        let provider = ScriptedProvider::new();
        provider.push_chat(vec![]); // empty turn → nudge
        provider.push_chat(vec![]); // second empty turn → no work to finalize from

        let (registry, _) = echo_registry();
        let agent = Agent::new(provider.clone(), registry, Arc::new(AllowAllPolicy));

        let report = agent.run("do a thing").await;
        assert_eq!(report.status, TaskStatus::Failed);
        assert!(report
            .steps
            .iter()
            .any(|s| matches!(s, AgentStep::Error { message }
            if message.contains("no output after a retry"))));
    }

    #[tokio::test]
    async fn same_tool_loop_gets_hard_stop_nudge() {
        let provider = ScriptedProvider::new();
        for i in 0..3 {
            provider.push_chat(turn_tool_call(
                &format!("call_{}", i),
                "echo",
                r#"{"message":"x"}"#,
            ));
        }

        let (registry, calls) = echo_registry();
        let agent = Agent::new(provider.clone(), registry, Arc::new(AllowAllPolicy))
            .with_approval(Arc::new(AutoApprove));

        let report = agent.run("do a thing").await;
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        assert_eq!(report.status, TaskStatus::Complete); // default final answer

        let requests = provider.recorded_requests();
        let nudge_seen = requests.iter().any(|r| {
            r.messages.iter().any(|m| {
                m.role == "user"
                    && m.content.contains("times in a row")
                    && m.content.contains("answer NOW")
            })
        });
        assert!(nudge_seen, "hard-stop nudge must reach the model");
    }

    #[tokio::test]
    async fn self_verification_incomplete_reenters_the_loop() {
        let provider = ScriptedProvider::new();
        provider.push_chat(turn_text("First answer."));
        provider.push_verify("INCOMPLETE: forgot the details");
        provider.push_chat(turn_text("Revised answer with details."));
        provider.push_verify("VERIFIED");

        let (registry, _) = echo_registry();
        let agent = Agent::new(provider.clone(), registry, Arc::new(AllowAllPolicy));

        let report = agent.run("explain it fully").await;
        assert_eq!(report.status, TaskStatus::Complete);
        assert_eq!(
            report.final_answer.as_deref(),
            Some("Revised answer with details.")
        );
        assert_eq!(
            report
                .steps
                .iter()
                .filter(|s| matches!(s, AgentStep::FinalAnswer { .. }))
                .count(),
            2,
            "both candidate answers were recorded"
        );
        let v = report.verification.unwrap();
        assert_eq!(v.decision, "complete");
    }

    #[tokio::test]
    async fn self_verification_inference_error_defaults_to_verified() {
        let provider = ScriptedProvider::new();
        provider.push_chat(turn_text("Fine answer."));
        // complete_scripts exhausted → mock returns "VERIFIED" (the default),
        // which exercises the same path as a defaulted verification.

        let (registry, _) = echo_registry();
        let agent = Agent::new(provider.clone(), registry, Arc::new(AllowAllPolicy));

        let report = agent.run("answer me").await;
        assert_eq!(report.status, TaskStatus::Complete);
        assert_eq!(report.verification.unwrap().decision, "complete");
    }

    #[tokio::test]
    async fn max_steps_reached_fails_with_error_step() {
        let provider = ScriptedProvider::new();
        for i in 0..10 {
            provider.push_chat(turn_tool_call(
                &format!("call_{}", i),
                "echo",
                r#"{"message":"x"}"#,
            ));
        }

        let (registry, _) = echo_registry();
        let config = AgentConfig {
            max_steps: 2,
            ..Default::default()
        };
        let agent = Agent::new(provider.clone(), registry, Arc::new(AllowAllPolicy))
            .with_approval(Arc::new(AutoApprove))
            .with_config(config);

        let report = agent.run("do a thing").await;
        assert_eq!(report.status, TaskStatus::Failed);
        assert!(report
            .steps
            .iter()
            .any(|s| matches!(s, AgentStep::Error { message }
            if message == "Max steps reached")));
        assert_eq!(report.steps_used, 2);
    }

    #[tokio::test]
    async fn privacy_redaction_strips_pii_and_restores_the_answer() {
        // The user message is message index 1 (system is 0), so the model
        // sees the email as [M1_EMAIL_1] and echoes that token back.
        let provider = ScriptedProvider::new();
        provider.push_chat(turn_text("write to [M1_EMAIL_1]"));
        provider.push_verify("VERIFIED");

        let (registry, _) = echo_registry();
        let agent = Agent::new(provider.clone(), registry, Arc::new(AllowAllPolicy))
            .with_privacy(Arc::new(amparo_privacy::PrivacyPolicy::default()));

        let report = agent.run("Email me at a@b.com").await;
        assert_eq!(report.status, TaskStatus::Complete);
        // The answer's placeholder was restored to the original PII.
        assert_eq!(report.final_answer.as_deref(), Some("write to a@b.com"));

        // The request that reached the provider was sanitised.
        let requests = provider.recorded_requests();
        let user_msg = &requests[0].messages[1];
        assert!(
            user_msg.content.contains("[M1_EMAIL_1]"),
            "PII must be stripped before inference: {}",
            user_msg.content
        );
        assert!(!user_msg.content.contains("a@b.com"));
    }

    #[tokio::test]
    async fn inference_request_error_fails_the_task() {
        // A provider whose stream always errors.
        struct FailingProvider;
        #[async_trait]
        impl InferenceProvider for FailingProvider {
            async fn complete(
                &self,
                _request: InferenceRequest,
            ) -> Result<amparo_inference::InferenceResponse, InferenceError> {
                Err(InferenceError::Provider("boom".into()))
            }
            async fn complete_chat_stream(
                &self,
                _request: ChatRequest,
            ) -> Result<amparo_inference::InferenceStream, InferenceError> {
                Err(InferenceError::Provider("boom".into()))
            }
            async fn embed(&self, _text: &str) -> Result<Vec<f64>, InferenceError> {
                Ok(vec![])
            }
            async fn list_models(&self) -> Result<Vec<String>, InferenceError> {
                Ok(vec![])
            }
            fn default_model(&self) -> String {
                "failing".into()
            }
        }

        let (registry, _) = echo_registry();
        let agent = Agent::new(
            Arc::new(FailingProvider),
            registry,
            Arc::new(AllowAllPolicy),
        );
        let report = agent.run("do a thing").await;
        assert_eq!(report.status, TaskStatus::Failed);
        assert!(report
            .steps
            .iter()
            .any(|s| matches!(s, AgentStep::Error { message }
            if message.contains("Inference request failed"))));
    }

    // ── Pure-function tests ──────────────────────────────────────────────────

    #[test]
    fn interpret_verification_parses_verified_and_incomplete() {
        assert_eq!(
            interpret_verification("VERIFIED"),
            VerificationDecision::Complete
        );
        assert_eq!(
            interpret_verification("  VERIFIED  "),
            VerificationDecision::Complete
        );
        assert_eq!(
            interpret_verification("anything else"),
            VerificationDecision::Complete
        );
        assert_eq!(
            interpret_verification("INCOMPLETE: missing the date"),
            VerificationDecision::Incomplete("missing the date".to_string())
        );
        assert_eq!(
            interpret_verification("INCOMPLETE"),
            VerificationDecision::Incomplete("INCOMPLETE".to_string())
        );
    }

    #[test]
    fn strip_think_tags_removes_reasoning() {
        assert_eq!(strip_think_tags("<think>hmm</think>VERIFIED"), "VERIFIED");
        assert_eq!(strip_think_tags("VERIFIED"), "VERIFIED");
    }

    #[test]
    fn extract_target_maps_primary_argument() {
        let call = ToolCall {
            id: "1".into(),
            name: "run_command".into(),
            arguments: serde_json::json!({"command": "ls -la", "timeout_secs": 10}),
        };
        let (target, params) = extract_target(&call);
        assert_eq!(target, "ls -la");
        assert!(params.contains(&("timeout_secs".to_string(), "10".to_string())));

        let call = ToolCall {
            id: "2".into(),
            name: "write_file".into(),
            arguments: serde_json::json!({"path": "/tmp/x", "content": "hi"}),
        };
        let (target, params) = extract_target(&call);
        assert_eq!(target, "/tmp/x");
        assert!(params.contains(&("content".to_string(), "hi".to_string())));
    }

    #[test]
    fn extract_target_falls_back_to_compact_json() {
        let call = ToolCall {
            id: "3".into(),
            name: "odd_tool".into(),
            arguments: serde_json::json!({"nested": {"k": 1}}),
        };
        let (target, params) = extract_target(&call);
        assert!(target.contains("nested"));
        assert!(params.is_empty());
    }

    #[test]
    fn strip_messages_uses_unique_namespaces_and_restores() {
        let messages = vec![
            ChatMessage::user("Email me at a@b.com"),
            ChatMessage::user("Email me at c@d.com"),
        ];
        let (stripped, map) = strip_messages(&messages);
        assert!(
            stripped[0].content.contains("[M0_EMAIL_1]"),
            "{}",
            stripped[0].content
        );
        assert!(
            stripped[1].content.contains("[M1_EMAIL_1]"),
            "{}",
            stripped[1].content
        );
        assert!(!stripped[0].content.contains("a@b.com"));
        assert!(!stripped[1].content.contains("c@d.com"));

        // Each placeholder restores its own original — no cross-message bleed.
        let restored =
            amparo_privacy::secure_minions_restore("send to [M0_EMAIL_1] and [M1_EMAIL_1]", &map);
        assert_eq!(restored, "send to a@b.com and c@d.com");
    }

    #[test]
    fn config_defaults_match_axiom_loop() {
        let config = AgentConfig::default();
        assert_eq!(config.max_steps, 12);
        assert_eq!(config.trust_ceiling, ToolTrustTier::SystemControl);
        assert!(config.model.is_none());
    }

    #[test]
    fn policy_sees_the_shell_command_as_target() {
        // Integration-shaped check of extract_target for the real registry:
        // run_command's "command" argument is what the engine judges.
        let call = ToolCall {
            id: "9".into(),
            name: "run_command".into(),
            arguments: serde_json::json!({"command": "rm -rf /"}),
        };
        let (target, _) = extract_target(&call);
        assert_eq!(target, "rm -rf /");
    }

    // ── Dry-run gate (M6d) tests ─────────────────────────────────────────────

    fn call(id: &str, name: &str, arguments: serde_json::Value) -> ToolCall {
        ToolCall {
            id: id.to_string(),
            name: name.to_string(),
            arguments,
        }
    }

    /// An empty skill library — a `use_skill` call can still be gated.
    struct EmptySkills;
    impl SkillLibrary for EmptySkills {
        fn get(&self, _name: &str) -> Option<SkillSpec> {
            None
        }
        fn names(&self) -> Vec<String> {
            Vec::new()
        }
    }

    #[tokio::test]
    async fn dry_run_unknown_tool_blocks() {
        let (registry, _) = echo_registry();
        let verdict = dry_run_gate(
            &registry,
            ToolTrustTier::SystemControl,
            &AllowAllPolicy,
            &call("c1", "nope", serde_json::json!({})),
        )
        .await;
        assert_eq!(verdict.decision, "unknown_tool");
        assert!(verdict.would_block);
        assert!(!verdict.approval_required);
    }

    #[tokio::test]
    async fn dry_run_ceiling_block() {
        let (echo, _) = EchoTool::new(ToolTrustTier::ExternalEffector);
        let registry = registry_with(Arc::new(echo));
        let verdict = dry_run_gate(
            &registry,
            ToolTrustTier::Observational,
            &AllowAllPolicy,
            &call("c1", "echo", serde_json::json!({"message": "hi"})),
        )
        .await;
        assert_eq!(verdict.decision, "trust_blocked");
        assert!(verdict.would_block);
        assert!(!verdict.approval_required);
    }

    #[tokio::test]
    async fn dry_run_policy_deny() {
        let (registry, _) = echo_registry();
        let policy = RecordingPolicy::new(&[("echo", PolicyVerdict::Deny)]);
        let verdict = dry_run_gate(
            &registry,
            ToolTrustTier::SystemControl,
            policy.as_ref(),
            &call("c1", "echo", serde_json::json!({"message": "hi"})),
        )
        .await;
        assert_eq!(verdict.decision, "policy_denied");
        assert!(verdict.would_block);
        assert!(!verdict.approval_required);
        assert_eq!(verdict.reasons, vec!["test deny echo".to_string()]);
    }

    #[tokio::test]
    async fn dry_run_allow_observational() {
        let (registry, _) = echo_registry();
        let verdict = dry_run_gate(
            &registry,
            ToolTrustTier::SystemControl,
            &AllowAllPolicy,
            &call("c1", "echo", serde_json::json!({"message": "hi"})),
        )
        .await;
        assert_eq!(verdict.decision, "allowed");
        assert!(!verdict.would_block);
        assert!(!verdict.approval_required);
        // No primary key → extract_target falls back to the compact-JSON form.
        assert_eq!(verdict.target, r#"{"message":"hi"}"#);
    }

    #[tokio::test]
    async fn dry_run_escalate_is_not_drift() {
        let (registry, _) = echo_registry();
        let policy = RecordingPolicy::new(&[("echo", PolicyVerdict::Escalate)]);
        let verdict = dry_run_gate(
            &registry,
            ToolTrustTier::SystemControl,
            policy.as_ref(),
            &call("c1", "echo", serde_json::json!({"message": "hi"})),
        )
        .await;
        assert_eq!(verdict.decision, "allowed");
        assert!(!verdict.would_block, "escalation is not drift");
        assert!(verdict.approval_required);
        assert_eq!(verdict.reasons, vec!["test escalate echo".to_string()]);
    }

    #[tokio::test]
    async fn dry_run_external_effector_requires_approval() {
        let (echo, _) = EchoTool::new(ToolTrustTier::ExternalEffector);
        let registry = registry_with(Arc::new(echo));
        let verdict = dry_run_gate(
            &registry,
            ToolTrustTier::ExternalEffector,
            &AllowAllPolicy,
            &call("c1", "echo", serde_json::json!({"message": "hi"})),
        )
        .await;
        assert_eq!(verdict.decision, "allowed");
        assert!(!verdict.would_block);
        assert!(verdict.approval_required);
    }

    #[tokio::test]
    async fn dry_run_use_skill_judges_the_skill_name() {
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(UseSkillTool::new(Arc::new(EmptySkills))));
        let policy = RecordingPolicy::new(&[("use_skill", PolicyVerdict::Allow)]);
        let verdict = dry_run_gate(
            &registry,
            ToolTrustTier::SystemControl,
            policy.as_ref(),
            &call("c1", USE_SKILL, serde_json::json!({"skill_name": "greet"})),
        )
        .await;
        assert_eq!(verdict.decision, "allowed");
        assert_eq!(verdict.target, "greet");
        assert_eq!(
            policy.seen(),
            vec![("use_skill".to_string(), "greet".to_string())]
        );
    }

    #[tokio::test]
    async fn dry_run_never_emits_events_or_asks_approval() {
        let (echo, _) = EchoTool::new(ToolTrustTier::ExternalEffector);
        let registry = registry_with(Arc::new(echo));
        let gate = RecordingGate::new(false);
        let sink = Arc::new(InMemoryEventSink::new());
        let _agent = Agent::new(
            ScriptedProvider::new(),
            registry.clone(),
            Arc::new(AllowAllPolicy),
        )
        .with_approval(gate.clone())
        .with_events(sink.clone());

        let verdict = dry_run_gate(
            &registry,
            ToolTrustTier::ExternalEffector,
            &AllowAllPolicy,
            &call("c1", "echo", serde_json::json!({"message": "hi"})),
        )
        .await;
        assert!(verdict.approval_required, "the live chain would ask");
        assert!(
            gate.requests().is_empty(),
            "the dry run never asks approval"
        );
        assert!(
            sink.snapshot()
                .iter()
                .all(|e| !matches!(e, AgentEvent::ApprovalRequested { .. })),
            "the dry run never emits approval events"
        );
    }

    // ── M7 W6: checkpoints and resume ────────────────────────────────────────

    use crate::JsonCheckpointStore;

    fn running_checkpoint(task_id: &str, steps_used: usize) -> Checkpoint {
        Checkpoint {
            version: crate::session::CHECKPOINT_VERSION,
            tenant: "cli".to_string(),
            task_id: task_id.to_string(),
            parent_task_id: None,
            started_at: 100,
            prompt: "echo hi".to_string(),
            status: SessionStatus::Running,
            conversation: vec![
                ChatMessage::user("echo hi"),
                ChatMessage::assistant("working"),
            ],
            loop_state: LoopState {
                steps_used,
                ..Default::default()
            },
            final_answer: None,
        }
    }

    #[tokio::test]
    async fn with_task_id_names_the_checkpoint_file() {
        let root = std::env::temp_dir().join(format!("amparo-agent-w2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (echo, _) = EchoTool::new(ToolTrustTier::Observational);
        let provider = ScriptedProvider::new();
        provider.push_chat(turn_text("Done."));
        provider.push_verify("VERIFIED");
        let store = Arc::new(JsonCheckpointStore::new(&root));
        let agent = Agent::new(
            provider,
            registry_with(Arc::new(echo)),
            Arc::new(AllowAllPolicy),
        )
        .with_approval(Arc::new(AutoApprove))
        .with_checkpoints(store.clone(), "cli")
        .with_task_id("sess-host-1");
        agent.run("echo hi").await;
        // The host's id — not a generated one — named the checkpoint
        // file, so every artifact agrees on the task's identity.
        assert!(crate::session::checkpoint_path(&root, "cli", "sess-host-1").exists());
        let complete = store
            .latest_complete("cli")
            .expect("the terminal checkpoint");
        assert_eq!(complete.task_id, "sess-host-1");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn run_writes_a_terminal_checkpoint_without_system_messages_or_pii() {
        let root = std::env::temp_dir().join(format!("amparo-agent-w6-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (echo, _) = EchoTool::new(ToolTrustTier::Observational);
        let provider = ScriptedProvider::new();
        provider.push_chat(turn_text("Done."));
        provider.push_verify("VERIFIED");
        let store = Arc::new(JsonCheckpointStore::new(&root));
        let agent = Agent::new(
            provider,
            registry_with(Arc::new(echo)),
            Arc::new(AllowAllPolicy),
        )
        .with_approval(Arc::new(AutoApprove))
        .with_checkpoints(store.clone(), "cli");
        let report = agent.run("email me at alice@example.com please").await;
        assert_eq!(report.status, TaskStatus::Complete);
        let complete = store
            .latest_complete("cli")
            .expect("the terminal checkpoint");
        assert!(
            store.latest_incomplete("cli").is_none(),
            "Running must transition to Complete at the terminal exit"
        );
        // I5: the system message is never stored.
        assert!(complete.conversation.iter().all(|m| m.role != "system"));
        // I6: PII is stripped at write, even without a privacy policy.
        let file = crate::session::checkpoint_path(&root, "cli", &complete.task_id);
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(
            !text.contains("alice@example.com"),
            "stored checkpoint leaks PII: {text}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn resume_reprepends_the_system_prompt_and_restores_the_conversation() {
        let (echo, _) = EchoTool::new(ToolTrustTier::Observational);
        let provider = ScriptedProvider::new();
        provider.push_chat(turn_text("Done."));
        provider.push_verify("VERIFIED");
        let events = Arc::new(InMemoryEventSink::new());
        let agent = Agent::new(
            provider.clone(),
            registry_with(Arc::new(echo)),
            Arc::new(AllowAllPolicy),
        )
        .with_approval(Arc::new(AutoApprove))
        .with_events(events.clone());

        let report = agent.resume(running_checkpoint("sess-1", 2)).await;
        assert_eq!(report.status, TaskStatus::Complete);
        // I5: the current system prompt is re-prepended, byte-identical —
        // a resumed task sees the prompt of THIS build, not the stored one.
        let messages = &provider.recorded_requests()[0].messages;
        assert_eq!(messages[0].role, "system");
        assert_eq!(messages[0].content, SYSTEM_PROMPT);
        // The stored conversation follows in order.
        assert_eq!(messages[1].content, "echo hi");
        assert_eq!(messages[2].content, "working");
        // TaskResumed — not TaskStarted — marks the resume.
        let resumed = events.snapshot().iter().any(|e| {
            matches!(e, AgentEvent::TaskResumed { task_id, steps_used }
                if task_id == "sess-1" && *steps_used == 2)
        });
        assert!(resumed, "the resume emits TaskResumed");
        assert!(
            events
                .snapshot()
                .iter()
                .all(|e| !matches!(e, AgentEvent::TaskStarted { .. })),
            "a resume never emits TaskStarted"
        );
    }

    #[tokio::test]
    async fn resume_restores_the_same_tool_guard_across_the_restart() {
        let (echo, calls) = EchoTool::new(ToolTrustTier::Observational);
        let provider = ScriptedProvider::new();
        provider.push_chat(turn_tool_call("c1", "echo", r#"{"message":"again"}"#));
        provider.push_chat(turn_text("Done."));
        provider.push_verify("VERIFIED");
        let agent = Agent::new(
            provider.clone(),
            registry_with(Arc::new(echo)),
            Arc::new(AllowAllPolicy),
        )
        .with_approval(Arc::new(AutoApprove));

        // The killed run had already called `echo` twice in a row.
        let mut checkpoint = running_checkpoint("sess-2", 1);
        checkpoint.loop_state.last_tool_name = Some("echo".to_string());
        checkpoint.loop_state.same_tool_count = 2;

        let report = agent.resume(checkpoint).await;
        assert_eq!(report.status, TaskStatus::Complete);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // 2 + 1 = 3 consecutive — the hard-stop nudge must fire on the
        // next request, exactly as it would have without the restart.
        let nudged = provider.recorded_requests().iter().any(|request| {
            request
                .messages
                .iter()
                .any(|m| m.content.contains("MUST answer NOW"))
        });
        assert!(nudged, "the hard-stop nudge must fire across a resume");
    }

    /// A checkpoint store whose writes always fail — the task must still
    /// run to completion (persistence warns, never fatal).
    struct FailingStore;
    impl CheckpointStore for FailingStore {
        fn save(&self, _checkpoint: &Checkpoint) -> std::io::Result<()> {
            Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "disk on fire",
            ))
        }

        fn latest_incomplete(&self, _tenant: &str) -> Option<Checkpoint> {
            None
        }

        fn latest_complete(&self, _tenant: &str) -> Option<Checkpoint> {
            None
        }
    }

    #[tokio::test]
    async fn a_failing_checkpoint_store_never_fails_the_task() {
        let (echo, _) = EchoTool::new(ToolTrustTier::Observational);
        let provider = ScriptedProvider::new();
        provider.push_chat(turn_text("Done."));
        provider.push_verify("VERIFIED");
        let agent = Agent::new(
            provider,
            registry_with(Arc::new(echo)),
            Arc::new(AllowAllPolicy),
        )
        .with_approval(Arc::new(AutoApprove))
        .with_checkpoints(Arc::new(FailingStore), "cli");
        let report = agent.run("hi").await;
        assert_eq!(report.status, TaskStatus::Complete);
    }

    #[tokio::test]
    async fn resume_with_an_exhausted_budget_fails_like_a_fresh_run() {
        let (echo, _) = EchoTool::new(ToolTrustTier::Observational);
        let provider = ScriptedProvider::new();
        let events = Arc::new(InMemoryEventSink::new());
        let agent = Agent::new(
            provider,
            registry_with(Arc::new(echo)),
            Arc::new(AllowAllPolicy),
        )
        .with_approval(Arc::new(AutoApprove))
        .with_events(events.clone());
        // steps_used == max_steps: no iterations remain.
        let report = agent
            .resume(running_checkpoint("sess-3", DEFAULT_MAX_STEPS))
            .await;
        assert_eq!(report.status, TaskStatus::Failed);
        assert!(events.snapshot().iter().any(
            |e| matches!(e, AgentEvent::TaskFailed { message } if message == "Max steps reached")
        ));
    }

    // ── M7 W8: chat continuity ─────────────────────────────────────────────

    #[tokio::test]
    async fn continuity_arrives_as_one_user_message_before_the_prompt() {
        let (echo, _) = EchoTool::new(ToolTrustTier::Observational);
        let provider = ScriptedProvider::new();
        provider.push_chat(turn_text("Done."));
        provider.push_verify("VERIFIED");
        let agent = Agent::new(
            provider.clone(),
            registry_with(Arc::new(echo)),
            Arc::new(AllowAllPolicy),
        )
        .with_approval(Arc::new(AutoApprove))
        .with_continuity(Some("[earlier context]".into()));
        let report = agent.run("do the thing").await;
        assert_eq!(report.status, TaskStatus::Complete);
        // One user-role message, between the system prompt and the task
        // prompt — the loop reads it as prior conversation, never as
        // instruction (I5: the system prompt stays byte-identical).
        let messages = &provider.recorded_requests()[0].messages;
        assert_eq!(messages[0].role, "system");
        assert_eq!(messages[1].role, "user");
        assert_eq!(messages[1].content, "[earlier context]");
        assert_eq!(messages[2].content, "do the thing");
    }

    #[tokio::test]
    async fn checkpoints_strip_the_tool_summary_and_final_answer_too() {
        let root = std::env::temp_dir().join(format!("amparo-agent-w8-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (echo, _) = EchoTool::new(ToolTrustTier::Observational);
        let provider = ScriptedProvider::new();
        // Turn 1: a tool call whose echoed output carries an email.
        // Turns 2+3: empty — the loop finalizes from the last good
        // summary, which means both the summary AND the final answer
        // hold the email.
        provider.push_chat(turn_tool_call(
            "c1",
            "echo",
            r#"{"message": "mail alice@example.com"}"#,
        ));
        provider.push_chat(vec![]);
        provider.push_chat(vec![]);
        let store = Arc::new(JsonCheckpointStore::new(&root));
        let agent = Agent::new(
            provider,
            registry_with(Arc::new(echo)),
            Arc::new(AllowAllPolicy),
        )
        .with_approval(Arc::new(AutoApprove))
        .with_checkpoints(store.clone(), "cli");
        let report = agent.run("echo the message").await;
        assert_eq!(report.status, TaskStatus::Complete);
        let complete = store
            .latest_complete("cli")
            .expect("the terminal checkpoint");
        // I6: the summary and the final answer are stripped at write —
        // continuity reads both, so neither may carry a live PII string.
        assert!(
            complete
                .loop_state
                .last_good_summary
                .as_deref()
                .is_none_or(|s| !s.contains("alice@example.com")),
            "stored summary leaks PII: {:?}",
            complete.loop_state.last_good_summary
        );
        let file = crate::session::checkpoint_path(&root, "cli", &complete.task_id);
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(
            !text.contains("alice@example.com"),
            "stored checkpoint leaks PII anywhere: {text}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
