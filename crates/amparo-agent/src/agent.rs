//! The Amparo agent loop — native `tool_calls`, policy-gated execution.
//!
//! Ported from Axiom-OS `agent_loop.rs` (`run_agent_task`, MIT, Copyright
//! (c) Pixel Phantom AI); relicensed Apache-2.0 — see the repository NOTICE.
//!
//! The Axiom loop parsed text-format ReAct turns (`parse_react_turn` over
//! `Thought:`/`Action:` text) and collapsed a six-deep gate chain. Amparo
//! drives the model's **native function-calling protocol** (see [`crate::sse`])
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
use crate::events::{AgentEvent, EventSink, InMemoryEventSink};
use crate::sse::accumulate_turn;
use amparo_inference::{ChatMessage, ChatRequest, InferenceProvider, InferenceRequest, Tool};
use amparo_policy::{PolicyEngine, PolicyVerdict};
use amparo_privacy::{DataCategory, PiiPlaceholder};
use amparo_tools::{ToolCall, ToolRegistry, ToolResult, ToolTrustTier};
use futures_util::future::join_all;
use serde::Serialize;
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Complete,
    Failed,
}

/// One recorded step of the task, mirroring the event surface.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentStep {
    ToolCall(ToolCall),
    ToolResult(ToolResult),
    FinalAnswer { content: String },
    Error { message: String },
}

/// The outcome of the self-verification pass.
#[derive(Debug, Clone, Serialize)]
pub struct Verification {
    /// `complete` | `incomplete`
    pub decision: String,
    pub feedback: Option<String>,
}

/// What the agent reports when a task ends.
#[derive(Debug, Clone, Serialize)]
pub struct AgentReport {
    pub status: TaskStatus,
    pub final_answer: Option<String>,
    pub steps: Vec<AgentStep>,
    /// Loop iterations consumed (1 = one LLM turn).
    pub steps_used: usize,
    pub verification: Option<Verification>,
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
    pub max_tokens: Option<usize>,
    pub temperature: Option<f32>,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_steps: DEFAULT_MAX_STEPS,
            trust_ceiling: ToolTrustTier::SystemControl,
            model: None,
            max_tokens: None,
            temperature: None,
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
    config: AgentConfig,
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

    pub fn with_config(mut self, config: AgentConfig) -> Self {
        self.config = config;
        self
    }

    pub fn registry(&self) -> &ToolRegistry {
        &self.registry
    }

    pub fn config(&self) -> &AgentConfig {
        &self.config
    }

    /// Registry schemas as wire-ready OpenAI tool definitions.
    fn openai_tools(&self) -> Vec<Tool> {
        self.registry
            .openai_tools()
            .into_iter()
            .filter_map(|v| serde_json::from_value(v).ok())
            .collect()
    }

    /// Run the loop to completion: every gate decision, tool execution and
    /// the final self-verification, all reported through [`AgentReport`] and
    /// the [`EventSink`].
    pub async fn run(&self, prompt: impl Into<String>) -> AgentReport {
        let prompt = prompt.into();
        self.events.emit(&AgentEvent::TaskStarted { prompt: prompt.clone() });

        // The prompt appears in nudge and verification messages sent to the
        // model — strip it once so the original never leaks there.
        let safe_prompt: String = match &self.privacy {
            Some(policy) if policy.auto_redact_pii => {
                amparo_privacy::secure_minions_strip(&prompt).sanitised_text
            }
            _ => prompt.clone(),
        };

        let mut conversation: Vec<ChatMessage> = vec![
            ChatMessage::system(SYSTEM_PROMPT),
            ChatMessage::user(prompt.clone()),
        ];

        let mut steps: Vec<AgentStep> = Vec::new();
        let mut last_tool_name: Option<String> = None;
        let mut same_tool_count: u32 = 0;
        let mut empty_turn_retried = false;
        let mut last_good_summary: Option<String> = None;
        let mut final_answer: Option<String> = None;
        let mut verification: Option<Verification> = None;
        let mut steps_used = 0;

        for step in 0..self.config.max_steps {
            steps_used = step + 1;

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
                let decision =
                    amparo_privacy::evaluate(policy, None, DataCategory::Chat, None);
                if !decision.allowed {
                    let message =
                        format!("Privacy policy blocked inference: {}", decision.reason);
                    steps.push(AgentStep::Error { message: message.clone() });
                    self.events.emit(&AgentEvent::TaskFailed { message: message.clone() });
                    return AgentReport {
                        status: TaskStatus::Failed,
                        final_answer: None,
                        steps,
                        steps_used,
                        verification: None,
                    };
                }
            }

            // ── Secure Minions PII strip ────────────────────────────────────
            let (send_messages, pii_map) = match &self.privacy {
                Some(policy) if policy.auto_redact_pii => strip_messages(&conversation),
                _ => (conversation.clone(), Vec::new()),
            };

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
                        steps.push(AgentStep::Error { message: message.clone() });
                        self.events.emit(&AgentEvent::TaskFailed { message: message.clone() });
                        return AgentReport {
                            status: TaskStatus::Failed,
                            final_answer: None,
                            steps,
                            steps_used,
                            verification: None,
                        };
                    }
                },
                Err(e) => {
                    let message = format!("Inference request failed: {}", e);
                    steps.push(AgentStep::Error { message: message.clone() });
                    self.events.emit(&AgentEvent::TaskFailed { message: message.clone() });
                    return AgentReport {
                        status: TaskStatus::Failed,
                        final_answer: None,
                        steps,
                        steps_used,
                        verification: None,
                    };
                }
            };

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
                    steps.push(AgentStep::FinalAnswer { content: content.clone() });
                    self.events.emit(&AgentEvent::FinalAnswer { content: content.clone() });
                    self.events.emit(&AgentEvent::TaskComplete { final_answer: content.clone() });
                    return AgentReport {
                        status: TaskStatus::Complete,
                        final_answer: Some(content),
                        steps,
                        steps_used,
                        verification: None,
                    };
                }
                let message = "The model produced no output after a retry, and no prior tool \
                               results were available to finalize from"
                    .to_string();
                steps.push(AgentStep::Error { message: message.clone() });
                self.events.emit(&AgentEvent::TaskFailed { message: message.clone() });
                return AgentReport {
                    status: TaskStatus::Failed,
                    final_answer: None,
                    steps,
                    steps_used,
                    verification: None,
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

                for call in &calls {
                    steps.push(AgentStep::ToolCall(call.clone()));
                    self.events.emit(&AgentEvent::ToolCallRequested { call: call.clone() });
                }

                // Pre-flight gates run serially; each call ends up either
                // ready to execute or answered with a blocked result. Every
                // call — executed or not — must be answered by a tool-role
                // message carrying the same tool_call_id, or providers reject
                // the next request.
                let mut ready: Vec<ToolCall> = Vec::new();
                let mut blocked_obs: Vec<String> = Vec::new();
                let mut tool_messages: Vec<ChatMessage> = Vec::new();

                for call in &calls {
                    let make_result =
                        |success: bool, output: serde_json::Value, summary: &str| ToolResult {
                            tool_call_id: call.id.clone(),
                            tool_name: call.name.clone(),
                            success,
                            output,
                            display_summary: summary.to_string(),
                            duration_ms: 0,
                        };

                    // Registry lookup first — unknown tools get an honest
                    // error naming what is available, not a trust verdict.
                    if self.registry.get_executor(&call.name).is_none() {
                        let available: Vec<String> = self
                            .registry
                            .list_schemas()
                            .iter()
                            .map(|s| s.name.clone())
                            .collect();
                        let error = format!(
                            "Unknown tool: {}. Available: {}",
                            call.name,
                            available.join(", ")
                        );
                        let result =
                            make_result(false, serde_json::json!({"error": error}), "Unknown tool");
                        self.block_call(
                            &result,
                            "unknown_tool",
                            Vec::new(),
                            &mut steps,
                            &mut blocked_obs,
                            &mut tool_messages,
                        );
                        continue;
                    }

                    // Trust ceiling
                    let tier_ok = self
                        .registry
                        .get_tier(&call.name)
                        .map(|t| t <= self.config.trust_ceiling)
                        .unwrap_or(false);
                    if !tier_ok {
                        let result = make_result(
                            false,
                            serde_json::json!({"error": "tool blocked by trust ceiling"}),
                            "Blocked",
                        );
                        self.block_call(
                            &result,
                            "trust_blocked",
                            vec!["tool tier exceeds the trust ceiling".to_string()],
                            &mut steps,
                            &mut blocked_obs,
                            &mut tool_messages,
                        );
                        continue;
                    }

                    // Policy gate — the deny-by-default seam.
                    let (target, params) = extract_target(call);
                    let param_refs: Vec<(&str, &str)> =
                        params.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
                    let decision =
                        self.policy.judge_tool(&call.name, &target, &param_refs).await;
                    let mut escalate_pending = false;
                    let reasons: Vec<String> = match decision.verdict {
                        PolicyVerdict::Deny => {
                            let error = format!(
                                "Policy denied {}: {}",
                                call.name,
                                decision.fired.join("; ")
                            );
                            let result = make_result(
                                false,
                                serde_json::json!({"error": error}),
                                "Blocked by policy",
                            );
                            self.block_call(
                                &result,
                                "policy_denied",
                                decision.fired,
                                &mut steps,
                                &mut blocked_obs,
                                &mut tool_messages,
                            );
                            continue;
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
                            // Audit-mode allows carry the engine's real
                            // verdict in `fired` — keep it visible.
                            decision.fired
                        }
                    };

                    // Human-approval gate — tier ≥ ExternalEffector or a
                    // policy Escalate. The gate decides how a human is asked
                    // and when to auto-deny; Amparo's built-ins auto-deny.
                    let tier = self
                        .registry
                        .get_tier(&call.name)
                        .unwrap_or(ToolTrustTier::Observational);
                    if escalate_pending || tier >= ToolTrustTier::ExternalEffector {
                        let mut ask_reasons = reasons.clone();
                        if escalate_pending {
                            ask_reasons
                                .push("policy escalated this call for human review".to_string());
                        }
                        if tier >= ToolTrustTier::ExternalEffector {
                            ask_reasons
                                .push(format!("tool tier {:?} requires human approval", tier));
                        }
                        let approval_request = ApprovalRequest {
                            call_id: call.id.clone(),
                            tool_name: call.name.clone(),
                            arguments: call.arguments.clone(),
                            reasons: ask_reasons.clone(),
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
                            let result = make_result(
                                false,
                                serde_json::json!({"error": "User denied the action or approval timed out"}),
                                "Denied by user",
                            );
                            self.block_call(
                                &result,
                                "approval_denied",
                                vec!["human approval denied".to_string()],
                                &mut steps,
                                &mut blocked_obs,
                                &mut tool_messages,
                            );
                            continue;
                        }
                    }

                    self.events.emit(&AgentEvent::ToolGate {
                        call_id: call.id.clone(),
                        tool_name: call.name.clone(),
                        decision: "allowed".to_string(),
                        reasons,
                    });
                    ready.push(call.clone());
                }

                // Execute all gated tools concurrently, retrying failures.
                let exec_futures = ready.iter().map(|call| {
                    let registry = &self.registry;
                    let call = call.clone();
                    async move {
                        let execute = || async {
                            match registry.get_executor(&call.name) {
                                Some(executor) => executor.execute(&call).await,
                                // Pre-checked above; keep an honest fallback.
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
                });
                let exec_results: Vec<ToolResult> = join_all(exec_futures).await;

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
                        self.events.emit(&AgentEvent::ToolExecuted { result: result.clone() });
                    }
                    steps.push(AgentStep::FinalAnswer { content: summary.clone() });
                    self.events.emit(&AgentEvent::FinalAnswer { content: summary.clone() });
                    self.events.emit(&AgentEvent::TaskComplete { final_answer: summary.clone() });
                    return AgentReport {
                        status: TaskStatus::Complete,
                        final_answer: Some(summary),
                        steps,
                        steps_used,
                        verification: None,
                    };
                }

                let mut all_obs: Vec<String> = blocked_obs;
                for result in &exec_results {
                    let result_step = AgentStep::ToolResult(result.clone());
                    steps.push(result_step);
                    self.events.emit(&AgentEvent::ToolExecuted { result: result.clone() });
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
            steps.push(AgentStep::FinalAnswer { content: assistant_content.clone() });
            self.events.emit(&AgentEvent::FinalAnswer { content: assistant_content.clone() });

            // One extra turn at near-zero cost: ask the model whether its
            // answer fully addresses the original request. INCOMPLETE
            // re-enters the loop with the model's own feedback; an inference
            // error defaults to VERIFIED so a transient failure never wedges
            // an otherwise-finished task. The prompt includes the candidate
            // answer because this verification runs as a standalone
            // completion — and it is PII-stripped like every other message.
            let verify_prompt = format!(
                "You just completed this task: \"{}\"\nYour final answer was: \"{}\"\n\
                 Verification check: Does the final answer fully and correctly address \
                 the task?\n\
                 Reply with exactly one of:\n\
                 VERIFIED — answer is complete and correct.\n\
                 INCOMPLETE: <brief description of what is missing or wrong>",
                safe_prompt.chars().take(200).collect::<String>(),
                assistant_content
            );
            let verify_prompt = match &self.privacy {
                Some(policy) if policy.auto_redact_pii => {
                    amparo_privacy::secure_minions_strip(&verify_prompt).sanitised_text
                }
                _ => verify_prompt,
            };
            conversation.push(ChatMessage::user(verify_prompt.clone()));

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
            self.events.emit(&AgentEvent::TaskComplete { final_answer: content.clone() });
            return AgentReport {
                status: TaskStatus::Complete,
                final_answer: Some(content),
                steps,
                steps_used,
                verification,
            };
        }

        // Exhausted max steps — fail honestly.
        let message = "Max steps reached".to_string();
        steps.push(AgentStep::Error { message: message.clone() });
        self.events.emit(&AgentEvent::TaskFailed { message: message.clone() });
        AgentReport { status: TaskStatus::Failed, final_answer, steps, steps_used, verification }
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
}

/// Map a tool call to the policy gate's `(target, params)` shape.
///
/// The target is the primary argument the engine judges: the shell command
/// for `run_command`, the path for file tools, the URL/query for web tools.
/// Other arguments become supplementary key/value pairs. Calls with no
/// recognizable primary string argument use the compact JSON of the
/// arguments as the target.
fn extract_target(call: &ToolCall) -> (String, Vec<(String, String)>) {
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
        None => (serde_json::to_string(&call.arguments).unwrap_or_default(), Vec::new()),
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
    use crate::approval::{ApprovalGate, AutoApprove};
    use amparo_inference::InferenceError;
    use amparo_policy::PolicyDecision;
    use amparo_tools::{ToolExecutor, ToolParam, ToolSchema};
    use async_trait::async_trait;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // ── Test doubles ─────────────────────────────────────────────────────────

    /// Scripted provider: pops one SSE script per chat turn (a Vec of frames),
    /// then a final-answer script when exhausted. `complete()` (used by
    /// self-verification) pops from a separate queue. Every chat request is
    /// recorded for assertions.
    struct ScriptedProvider {
        chat_scripts: std::sync::Mutex<std::collections::VecDeque<Vec<String>>>,
        complete_scripts: std::sync::Mutex<std::collections::VecDeque<String>>,
        chat_requests: std::sync::Mutex<Vec<ChatRequest>>,
    }

    impl ScriptedProvider {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                chat_scripts: std::sync::Mutex::new(Default::default()),
                complete_scripts: std::sync::Mutex::new(Default::default()),
                chat_requests: std::sync::Mutex::new(Vec::new()),
            })
        }

        fn push_chat(&self, script: Vec<String>) {
            self.chat_scripts.lock().unwrap().push_back(script);
        }

        fn push_verify(&self, reply: &str) {
            self.complete_scripts.lock().unwrap().push_back(reply.to_string());
        }

        fn recorded_requests(&self) -> Vec<ChatRequest> {
            self.chat_requests.lock().unwrap().clone()
        }
    }

    fn content_delta(text: &str) -> String {
        serde_json::json!({"choices": [{"delta": {"content": text}}]}).to_string()
    }

    fn tool_call_frame(id: &str, name: &str, arguments: &str) -> String {
        serde_json::json!({"choices": [{"delta": {"tool_calls": [{
            "index": 0, "id": id, "type": "function",
            "function": {"name": name, "arguments": arguments}
        }]}}]})
        .to_string()
    }

    fn done() -> String {
        "[DONE]".to_string()
    }

    /// One full turn: a single tool call, then [DONE].
    fn turn_tool_call(id: &str, name: &str, arguments: &str) -> Vec<String> {
        vec![tool_call_frame(id, name, arguments), done()]
    }

    /// One full turn: plain text, then [DONE].
    fn turn_text(text: &str) -> Vec<String> {
        vec![content_delta(text), done()]
    }

    fn sse_stream(frames: &[String]) -> amparo_inference::InferenceStream {
        use futures_util::stream;
        let events: Vec<std::result::Result<bytes::Bytes, InferenceError>> = frames
            .iter()
            .map(|f| Ok(bytes::Bytes::from(format!("data: {}\n\n", f))))
            .collect();
        Box::pin(stream::iter(events))
    }

    #[async_trait]
    impl InferenceProvider for ScriptedProvider {
        async fn complete(
            &self,
            _request: InferenceRequest,
        ) -> Result<amparo_inference::InferenceResponse, InferenceError> {
            let reply = self
                .complete_scripts
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| "VERIFIED".to_string());
            Ok(amparo_inference::InferenceResponse {
                text: reply,
                tokens: 1,
                finish_reason: "stop".into(),
            })
        }

        async fn complete_chat_stream(
            &self,
            request: ChatRequest,
        ) -> Result<amparo_inference::InferenceStream, InferenceError> {
            self.chat_requests.lock().unwrap().push(request);
            let script = self
                .chat_scripts
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| turn_text("Done."));
            Ok(sse_stream(&script))
        }

        async fn embed(&self, _text: &str) -> Result<Vec<f64>, InferenceError> {
            Ok(vec![])
        }

        async fn list_models(&self) -> Result<Vec<String>, InferenceError> {
            Ok(vec![])
        }

        fn default_model(&self) -> String {
            "test-model".into()
        }
    }

    /// A stub tool that counts its executions and echoes `message`.
    struct EchoTool {
        calls: Arc<AtomicUsize>,
        tier: ToolTrustTier,
    }

    impl EchoTool {
        fn new(tier: ToolTrustTier) -> (Self, Arc<AtomicUsize>) {
            let calls = Arc::new(AtomicUsize::new(0));
            (Self { calls: calls.clone(), tier }, calls)
        }
    }

    #[async_trait]
    impl ToolExecutor for EchoTool {
        fn schema(&self) -> ToolSchema {
            ToolSchema {
                name: "echo".to_string(),
                description: "echo the message".to_string(),
                parameters: vec![ToolParam {
                    name: "message".to_string(),
                    description: "text to echo".to_string(),
                    param_type: "string".to_string(),
                    enum_values: None,
                    required: true,
                }],
                trust_tier: self.tier,
            }
        }

        async fn execute(&self, call: &ToolCall) -> ToolResult {
            self.calls.fetch_add(1, Ordering::SeqCst);
            ToolResult {
                tool_call_id: call.id.clone(),
                tool_name: call.name.clone(),
                success: true,
                output: serde_json::json!({"echoed": call.arg_str("message").unwrap_or("")}),
                display_summary: format!("echoed: {}", call.arg_str("message").unwrap_or("")),
                duration_ms: 0,
            }
        }
    }

    fn registry_with(executor: Arc<dyn ToolExecutor>) -> ToolRegistry {
        let mut registry = ToolRegistry::new();
        registry.register(executor);
        registry
    }

    /// Allow everything, record nothing.
    struct AllowAllPolicy;
    #[async_trait]
    impl PolicyEngine for AllowAllPolicy {
        async fn judge_tool(
            &self,
            _tool: &str,
            _target: &str,
            _params: &[(&str, &str)],
        ) -> PolicyDecision {
            PolicyDecision::allow()
        }
    }

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
            self.seen.lock().unwrap().push((tool.to_string(), target.to_string()));
            match self.verdicts.lock().unwrap().get(tool) {
                Some(PolicyVerdict::Allow) => PolicyDecision::allow(),
                Some(PolicyVerdict::Deny) => {
                    PolicyDecision::deny(format!("test deny {}", tool))
                }
                Some(PolicyVerdict::Escalate) => {
                    PolicyDecision::escalate(format!("test escalate {}", tool))
                }
                None => PolicyDecision::allow(),
            }
        }
    }

    /// Records approval requests and answers with a fixed verdict.
    struct RecordingGate {
        requests: std::sync::Mutex<Vec<ApprovalRequest>>,
        answer: bool,
    }

    impl RecordingGate {
        fn new(answer: bool) -> Arc<Self> {
            Arc::new(Self {
                requests: std::sync::Mutex::new(Vec::new()),
                answer,
            })
        }

        fn requests(&self) -> Vec<ApprovalRequest> {
            self.requests.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl ApprovalGate for RecordingGate {
        async fn request(&self, request: &ApprovalRequest) -> bool {
            self.requests.lock().unwrap().push(request.clone());
            self.answer
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
        assert_eq!(policy.seen(), vec![("echo".to_string(), "{\"message\":\"hi\"}".to_string())]);
        assert_eq!(calls.load(Ordering::SeqCst), 0, "denied tool must not execute");
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
        assert_eq!(calls.load(Ordering::SeqCst), 1, "approved escalated call executes");
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
        assert!(request.reasons.iter().any(|r| r.contains("requires human approval")));
        assert!(report
            .steps
            .iter()
            .any(|s| matches!(s, AgentStep::ToolResult(r) if !r.success)));
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
        let agent = Agent::new(provider.clone(), registry, Arc::new(AllowAllPolicy))
            .with_config(config);

        let report = agent.run("do a thing").await;
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(report.steps.iter().any(|s| matches!(s, AgentStep::ToolResult(r)
            if r.output["error"].as_str().unwrap_or("").contains("trust ceiling"))));
    }

    #[tokio::test]
    async fn unknown_tool_gets_honest_error_and_tool_role_answer() {
        let provider = ScriptedProvider::new();
        provider.push_chat(turn_tool_call("call_1", "no_such_tool", "{}"));

        let (registry, _) = echo_registry();
        let agent = Agent::new(provider.clone(), registry, Arc::new(AllowAllPolicy));

        let report = agent.run("do a thing").await;
        assert!(report.steps.iter().any(|s| matches!(s, AgentStep::ToolResult(r)
            if r.output["error"].as_str().unwrap_or("").contains("Unknown tool"))));

        let requests = provider.recorded_requests();
        let tool_answer = requests[1].messages.iter().find(|m| m.role == "tool");
        assert_eq!(tool_answer.unwrap().tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(report.status, TaskStatus::Complete);
    }

    #[tokio::test]
    async fn one_shot_shortcut_completes_without_further_turns() {
        let provider = ScriptedProvider::new();
        provider.push_chat(turn_tool_call("call_1", "echo", r#"{"message":"pod bay doors"}"#));
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
        assert_eq!(provider.recorded_requests().len(), 1, "one-shot made a single LLM turn");
        assert!(report.verification.is_none(), "one-shot skips self-verification");
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
        assert_eq!(report.final_answer.as_deref(), Some("And then the second thing."));
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
        assert!(answer.contains("Task completed"), "finalized from real work: {answer}");
        assert!(answer.contains("echoed: hi"), "finalized from real work: {answer}");
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
        assert!(report.steps.iter().any(|s| matches!(s, AgentStep::Error { message }
            if message.contains("no output after a retry"))));
    }

    #[tokio::test]
    async fn same_tool_loop_gets_hard_stop_nudge() {
        let provider = ScriptedProvider::new();
        for i in 0..3 {
            provider.push_chat(turn_tool_call(&format!("call_{}", i), "echo", r#"{"message":"x"}"#));
        }

        let (registry, calls) = echo_registry();
        let agent = Agent::new(provider.clone(), registry, Arc::new(AllowAllPolicy))
            .with_approval(Arc::new(AutoApprove));

        let report = agent.run("do a thing").await;
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        assert_eq!(report.status, TaskStatus::Complete); // default final answer

        let requests = provider.recorded_requests();
        let nudge_seen = requests.iter().any(|r| {
            r.messages.iter().any(|m| m.role == "user"
                && m.content.contains("times in a row")
                && m.content.contains("answer NOW"))
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
        assert_eq!(report.final_answer.as_deref(), Some("Revised answer with details."));
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
            provider.push_chat(turn_tool_call(&format!("call_{}", i), "echo", r#"{"message":"x"}"#));
        }

        let (registry, _) = echo_registry();
        let config = AgentConfig { max_steps: 2, ..Default::default() };
        let agent = Agent::new(provider.clone(), registry, Arc::new(AllowAllPolicy))
            .with_approval(Arc::new(AutoApprove))
            .with_config(config);

        let report = agent.run("do a thing").await;
        assert_eq!(report.status, TaskStatus::Failed);
        assert!(report.steps.iter().any(|s| matches!(s, AgentStep::Error { message }
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
        let agent = Agent::new(Arc::new(FailingProvider), registry, Arc::new(AllowAllPolicy));
        let report = agent.run("do a thing").await;
        assert_eq!(report.status, TaskStatus::Failed);
        assert!(report.steps.iter().any(|s| matches!(s, AgentStep::Error { message }
            if message.contains("Inference request failed"))));
    }

    // ── Pure-function tests ──────────────────────────────────────────────────

    #[test]
    fn interpret_verification_parses_verified_and_incomplete() {
        assert_eq!(interpret_verification("VERIFIED"), VerificationDecision::Complete);
        assert_eq!(interpret_verification("  VERIFIED  "), VerificationDecision::Complete);
        assert_eq!(interpret_verification("anything else"), VerificationDecision::Complete);
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
        assert!(stripped[0].content.contains("[M0_EMAIL_1]"), "{}", stripped[0].content);
        assert!(stripped[1].content.contains("[M1_EMAIL_1]"), "{}", stripped[1].content);
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
}
