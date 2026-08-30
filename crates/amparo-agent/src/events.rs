//! The agent's event surface — every decision the loop makes is observable.
//!
//! `AgentEvent`s flow through the [`EventSink`] seam: the default
//! [`InMemoryEventSink`] keeps a log and fans out on a broadcast channel, so
//! a host application can render live progress, an audit log can persist
//! it, or a test can assert on it.

use std::sync::Arc;

use amparo_tools::{ToolCall, ToolResult};
use serde::Serialize;
use tokio::sync::broadcast;

use crate::qc::{QcFinding, QcVerdict};

/// Everything the agent does, in order. Serializable for audit trails.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum AgentEvent {
    /// The task began.
    TaskStarted {
        /// The original prompt.
        prompt: String,
        /// The task's id when the host fixed one (M8) — a sub-agent's
        /// chain id like `sess-123.1` — so the `[task]` line names the
        /// delegation chain. `None` for a host that let the agent
        /// generate the id.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task_id: Option<String>,
    },
    /// One LLM turn arrived (content plus how many tool calls it made).
    AssistantTurn {
        /// The loop step number.
        step: usize,
        /// The assistant's text for this turn.
        content: String,
        /// How many tool calls the turn requested.
        tool_calls: usize,
    },
    /// The model requested a tool call.
    ToolCallRequested {
        /// The requested call.
        call: ToolCall,
    },
    /// A gate decided on a tool call.
    /// `decision` is one of `allowed`, `trust_blocked`, `policy_denied`,
    /// `approval_denied`, `unknown_tool`.
    ToolGate {
        /// The call's id.
        call_id: String,
        /// The tool's name.
        tool_name: String,
        /// The gate's decision: `allowed` or a block reason.
        decision: String,
        /// The reasons behind it — policy rules that fired, or the block reason.
        reasons: Vec<String>,
    },
    /// The loop asked a human whether a call may run.
    ApprovalRequested {
        /// The call's id.
        call_id: String,
        /// The tool's name.
        tool_name: String,
        /// Why approval is required — policy escalation, tier, or both.
        reasons: Vec<String>,
    },
    /// The human (or gate) answered.
    ApprovalResolved {
        /// The call's id.
        call_id: String,
        /// Whether the call may execute.
        approved: bool,
    },
    /// PII was stripped before inference — per-category counts, never the
    /// values themselves. Emitted only when something was found.
    PrivacyStripped {
        /// `(category, count)` pairs in first-seen order (`email`, `phone`,
        /// `ssn`, `credit_card`, `password`, `address`, `medical`).
        categories: Vec<(String, usize)>,
    },
    /// A tool call finished executing.
    ToolExecuted {
        /// The tool's result.
        result: ToolResult,
    },
    /// The model produced a candidate final answer.
    FinalAnswer {
        /// The candidate answer text.
        content: String,
    },
    /// The QC council audited the candidate final answer (M9): advisory
    /// findings appended to the verification prompt. The verdict never
    /// overrides the verification turn's decision.
    QcAudit {
        /// `approved` | `with_findings`
        verdict: QcVerdict,
        /// The findings, empty when approved.
        findings: Vec<QcFinding>,
    },
    /// The self-verification turn decided.
    Verification {
        /// `complete` | `incomplete`
        decision: String,
        /// The model's feedback, when incomplete.
        feedback: Option<String>,
    },
    /// A sub-agent was spawned (M8): the child's chain id, its parent
    /// task, and the sub-task prompt it was given. Emitted by the
    /// `spawn_agent` tool after the budget check — a budget-exhausted
    /// spawn emits nothing, because no agent exists.
    SubAgentSpawned {
        /// The child's task id — `{parent}.{n}`.
        task_id: String,
        /// The parent task's id.
        parent_task_id: String,
        /// The sub-task prompt.
        prompt: String,
    },
    /// The task completed with this final answer.
    TaskComplete {
        /// The final answer text.
        final_answer: String,
        /// The task's id when the host fixed one (M8) — mirrors
        /// [`AgentEvent::TaskStarted::task_id`] so the `[complete]` line
        /// names the same chain.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task_id: Option<String>,
    },
    /// The task failed.
    TaskFailed {
        /// Why the task failed.
        message: String,
    },
    /// A checkpointed task resumed (M7) instead of starting fresh — the
    /// loop continues from the stored conversation and loop state.
    TaskResumed {
        /// The resumed checkpoint's task id.
        task_id: String,
        /// Loop iterations already used when the checkpoint was written.
        steps_used: usize,
    },
}

/// Observability seam — everything the loop emits goes through here.
pub trait EventSink: Send + Sync {
    /// Record one event — the loop calls this for every decision.
    fn emit(&self, event: &AgentEvent);
}

/// Default sink: an in-memory log plus a broadcast channel for live views.
pub struct InMemoryEventSink {
    events: std::sync::Mutex<Vec<AgentEvent>>,
    tx: broadcast::Sender<AgentEvent>,
}

impl InMemoryEventSink {
    /// An empty log with a fresh broadcast channel.
    pub fn new() -> Self {
        let (tx, _rx) = broadcast::channel(64);
        Self {
            events: std::sync::Mutex::new(Vec::new()),
            tx,
        }
    }

    /// A snapshot of everything emitted so far, in order.
    pub fn snapshot(&self) -> Vec<AgentEvent> {
        self.events.lock().unwrap().clone()
    }

    /// A live subscription for progress rendering. Lagging receivers are
    /// dropped by the broadcast channel (capacity 64) rather than blocking
    /// the agent.
    pub fn subscribe(&self) -> broadcast::Receiver<AgentEvent> {
        self.tx.subscribe()
    }
}

impl Default for InMemoryEventSink {
    fn default() -> Self {
        Self::new()
    }
}

impl EventSink for InMemoryEventSink {
    fn emit(&self, event: &AgentEvent) {
        self.events.lock().unwrap().push(event.clone());
        // No receivers is fine — the log above still holds the event.
        let _ = self.tx.send(event.clone());
    }
}

/// A sink that fans every event out to several other sinks.
///
/// Useful when a host wants its own rendering sink *and* an additional
/// consumer (an audit writer, the lab notebook) to observe the same event
/// stream — each sink sees the identical event, in the same order.
pub struct FanoutSink {
    sinks: Vec<Arc<dyn EventSink>>,
}

impl FanoutSink {
    /// Fan events out to `sinks`, in the order given.
    pub fn new(sinks: Vec<Arc<dyn EventSink>>) -> Self {
        Self { sinks }
    }
}

impl EventSink for FanoutSink {
    fn emit(&self, event: &AgentEvent) {
        for sink in &self.sinks {
            sink.emit(event);
        }
    }
}

/// Long content is truncated to this many chars, plus an ellipsis.
pub const TRUNCATE: usize = 200;

/// Truncate `s` to [`TRUNCATE`] chars, appending `…` when anything was cut.
pub fn truncate(s: &str) -> String {
    let mut chars = s.chars();
    let head: String = chars.by_ref().take(TRUNCATE).collect();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

/// Render one event as one `[tag]` line. Pure — unit-testable without a sink.
///
/// This is the canonical human-readable form of an [`AgentEvent`]: the CLI
/// prints it to stderr and chat adapters forward it to the chat.
pub fn format_event(event: &AgentEvent) -> String {
    match event {
        AgentEvent::TaskStarted { prompt, task_id } => match task_id {
            Some(id) => format!("[task {id}] {}", truncate(prompt)),
            None => format!("[task] {}", truncate(prompt)),
        },
        AgentEvent::AssistantTurn {
            step,
            content,
            tool_calls,
        } => {
            let calls = if *tool_calls > 0 {
                format!(" ({tool_calls} tool call(s))")
            } else {
                String::new()
            };
            format!("[turn {step}] {}{calls}", truncate(content))
        }
        AgentEvent::ToolCallRequested { call } => format!(
            "[call] {} {}",
            call.name,
            truncate(&serde_json::to_string(&call.arguments).unwrap_or_default())
        ),
        AgentEvent::ToolGate {
            tool_name,
            decision,
            reasons,
            ..
        } => {
            let why = if reasons.is_empty() {
                String::new()
            } else {
                format!(" ({})", truncate(&reasons.join("; ")))
            };
            format!("[gate] {tool_name}: {decision}{why}")
        }
        AgentEvent::ApprovalRequested {
            tool_name, reasons, ..
        } => {
            format!("[approval] {tool_name}: {}", truncate(&reasons.join("; ")))
        }
        AgentEvent::ApprovalResolved { approved, .. } => {
            format!(
                "[approval] {}",
                if *approved { "granted" } else { "denied" }
            )
        }
        AgentEvent::PrivacyStripped { categories } => {
            if categories.is_empty() {
                "[privacy] stripped: nothing".to_string()
            } else {
                let parts: Vec<String> = categories
                    .iter()
                    .map(|(c, n)| format!("{c} x{n}"))
                    .collect();
                format!("[privacy] stripped: {}", parts.join(", "))
            }
        }
        AgentEvent::ToolExecuted { result } => {
            format!("[exec] {} ({}ms)", result.tool_name, result.duration_ms)
        }
        AgentEvent::FinalAnswer { content } => format!("[answer] {}", truncate(content)),
        AgentEvent::QcAudit { verdict, findings } => match verdict {
            QcVerdict::Approved => "[qc] approved".to_string(),
            QcVerdict::WithFindings => {
                let list: Vec<String> = findings
                    .iter()
                    .map(|f| format!("{}: {}", f.rule, truncate(&f.message)))
                    .collect();
                format!("[qc] with_findings: {}", list.join("; "))
            }
        },
        AgentEvent::Verification { decision, feedback } => {
            let detail = feedback
                .as_ref()
                .map(|f| format!(": {}", truncate(f)))
                .unwrap_or_default();
            format!("[verify] {decision}{detail}")
        }
        AgentEvent::SubAgentSpawned {
            task_id,
            parent_task_id,
            prompt,
        } => {
            format!(
                "[spawn] {task_id} under {parent_task_id}: {}",
                truncate(prompt)
            )
        }
        AgentEvent::TaskComplete {
            final_answer,
            task_id,
        } => match task_id {
            Some(id) => format!("[complete {id}] {}", truncate(final_answer)),
            None => format!("[complete] {}", truncate(final_answer)),
        },
        AgentEvent::TaskFailed { message } => format!("[failed] {}", truncate(message)),
        AgentEvent::TaskResumed {
            task_id,
            steps_used,
        } => {
            format!("[session] resumed {task_id} (step {steps_used})")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn result(name: &str) -> ToolResult {
        ToolResult {
            tool_call_id: "c1".into(),
            tool_name: name.into(),
            success: true,
            output: json!({}),
            display_summary: "ok".into(),
            duration_ms: 0,
        }
    }

    #[test]
    fn snapshot_holds_emitted_events_in_order() {
        let sink = InMemoryEventSink::new();
        sink.emit(&AgentEvent::TaskStarted {
            prompt: "hi".into(),
            task_id: None,
        });
        sink.emit(&AgentEvent::ToolExecuted {
            result: result("read_file"),
        });
        sink.emit(&AgentEvent::TaskComplete {
            final_answer: "done".into(),
            task_id: None,
        });
        let events = sink.snapshot();
        assert_eq!(events.len(), 3);
        assert!(matches!(events[0], AgentEvent::TaskStarted { .. }));
        assert!(matches!(events[1], AgentEvent::ToolExecuted { .. }));
        assert!(matches!(events[2], AgentEvent::TaskComplete { .. }));
    }

    #[test]
    fn broadcast_fans_out_to_subscribers() {
        let sink = InMemoryEventSink::new();
        let mut rx = sink.subscribe();
        sink.emit(&AgentEvent::TaskStarted {
            prompt: "hi".into(),
            task_id: None,
        });
        // The channel holds the event even if no one awaited it yet.
        let received = rx.blocking_recv().unwrap();
        assert!(matches!(received, AgentEvent::TaskStarted { .. }));
    }

    #[test]
    fn events_serialize_with_discriminating_tag() {
        let v = serde_json::to_value(&AgentEvent::ToolGate {
            call_id: "c1".into(),
            tool_name: "run_command".into(),
            decision: "policy_denied".into(),
            reasons: vec!["rm with destructive flags".into()],
        })
        .unwrap();
        assert_eq!(v["event"], "tool_gate");
        assert_eq!(v["decision"], "policy_denied");
        assert_eq!(v["reasons"][0], "rm with destructive flags");
    }

    fn make_call() -> ToolCall {
        ToolCall {
            id: "c1".into(),
            name: "run_command".into(),
            arguments: json!({"command": "echo hi"}),
        }
    }

    fn make_result() -> ToolResult {
        ToolResult {
            tool_call_id: "c1".into(),
            tool_name: "run_command".into(),
            success: true,
            output: json!({}),
            display_summary: "ok".into(),
            duration_ms: 4,
        }
    }

    #[test]
    fn every_variant_renders_as_one_tagged_line() {
        let cases: Vec<(&str, String)> = vec![
            (
                "[task]",
                format_event(&AgentEvent::TaskStarted {
                    prompt: "p".into(),
                    task_id: None,
                }),
            ),
            (
                "[turn",
                format_event(&AgentEvent::AssistantTurn {
                    step: 2,
                    content: "hi".into(),
                    tool_calls: 1,
                }),
            ),
            (
                "[call]",
                format_event(&AgentEvent::ToolCallRequested { call: make_call() }),
            ),
            (
                "[gate]",
                format_event(&AgentEvent::ToolGate {
                    call_id: "c1".into(),
                    tool_name: "run_command".into(),
                    decision: "policy_denied".into(),
                    reasons: vec!["rm".into()],
                }),
            ),
            (
                "[approval]",
                format_event(&AgentEvent::ApprovalRequested {
                    call_id: "c1".into(),
                    tool_name: "run_command".into(),
                    reasons: vec!["r".into()],
                }),
            ),
            (
                "[approval] granted",
                format_event(&AgentEvent::ApprovalResolved {
                    call_id: "c1".into(),
                    approved: true,
                }),
            ),
            (
                "[privacy] stripped:",
                format_event(&AgentEvent::PrivacyStripped {
                    categories: vec![("email".into(), 2)],
                }),
            ),
            (
                "[exec] run_command",
                format_event(&AgentEvent::ToolExecuted {
                    result: make_result(),
                }),
            ),
            (
                "[answer]",
                format_event(&AgentEvent::FinalAnswer {
                    content: "a".into(),
                }),
            ),
            (
                "[qc] approved",
                format_event(&AgentEvent::QcAudit {
                    verdict: QcVerdict::Approved,
                    findings: vec![],
                }),
            ),
            (
                "[qc] with_findings:",
                format_event(&AgentEvent::QcAudit {
                    verdict: QcVerdict::WithFindings,
                    findings: vec![QcFinding {
                        rule: "cost_honesty",
                        message: "the figure diverges".into(),
                    }],
                }),
            ),
            (
                "[verify]",
                format_event(&AgentEvent::Verification {
                    decision: "complete".into(),
                    feedback: Some("f".into()),
                }),
            ),
            (
                "[task sess-123.1]",
                format_event(&AgentEvent::TaskStarted {
                    prompt: "p".into(),
                    task_id: Some("sess-123.1".into()),
                }),
            ),
            (
                "[spawn]",
                format_event(&AgentEvent::SubAgentSpawned {
                    task_id: "sess-123.1".into(),
                    parent_task_id: "sess-123".into(),
                    prompt: "research X".into(),
                }),
            ),
            (
                "[complete]",
                format_event(&AgentEvent::TaskComplete {
                    final_answer: "a".into(),
                    task_id: None,
                }),
            ),
            (
                "[complete sess-123.1]",
                format_event(&AgentEvent::TaskComplete {
                    final_answer: "a".into(),
                    task_id: Some("sess-123.1".into()),
                }),
            ),
            (
                "[failed]",
                format_event(&AgentEvent::TaskFailed {
                    message: "m".into(),
                }),
            ),
            (
                "[session] resumed",
                format_event(&AgentEvent::TaskResumed {
                    task_id: "sess-1".into(),
                    steps_used: 3,
                }),
            ),
        ];
        for (tag, line) in cases {
            assert!(line.starts_with(tag), "{tag} vs {line}");
        }
    }

    #[test]
    fn content_truncates_to_the_limit_with_an_ellipsis() {
        let long = "x".repeat(500);
        let line = format_event(&AgentEvent::FinalAnswer { content: long });
        assert!(line.ends_with('…'));
        // Count chars, not bytes — the ellipsis is multi-byte.
        assert_eq!(
            line.chars().count(),
            "[answer] ".chars().count() + TRUNCATE + 1
        );

        let line = format_event(&AgentEvent::FinalAnswer {
            content: "short".into(),
        });
        assert_eq!(line, "[answer] short");
    }

    #[test]
    fn denied_approvals_say_denied() {
        let line = format_event(&AgentEvent::ApprovalResolved {
            call_id: "c1".into(),
            approved: false,
        });
        assert_eq!(line, "[approval] denied");
    }

    #[test]
    fn task_ids_name_the_chain_and_absent_ids_keep_the_plain_tag() {
        assert_eq!(
            format_event(&AgentEvent::TaskStarted {
                prompt: "p".into(),
                task_id: None
            }),
            "[task] p"
        );
        assert_eq!(
            format_event(&AgentEvent::TaskStarted {
                prompt: "p".into(),
                task_id: Some("sess-123.1".into())
            }),
            "[task sess-123.1] p"
        );
        assert_eq!(
            format_event(&AgentEvent::TaskComplete {
                final_answer: "a".into(),
                task_id: Some("sess-123.1".into())
            }),
            "[complete sess-123.1] a"
        );
        assert_eq!(
            format_event(&AgentEvent::SubAgentSpawned {
                task_id: "sess-123.1".into(),
                parent_task_id: "sess-123".into(),
                prompt: "research X".into(),
            }),
            "[spawn] sess-123.1 under sess-123: research X"
        );
    }

    #[test]
    fn fanout_delivers_every_event_to_every_sink() {
        let a = Arc::new(InMemoryEventSink::new());
        let b = Arc::new(InMemoryEventSink::new());
        let fanout = FanoutSink::new(vec![
            a.clone() as Arc<dyn EventSink>,
            b.clone() as Arc<dyn EventSink>,
        ]);
        fanout.emit(&AgentEvent::TaskStarted {
            prompt: "hi".into(),
            task_id: None,
        });
        fanout.emit(&AgentEvent::TaskComplete {
            final_answer: "done".into(),
            task_id: None,
        });
        // Both sinks saw both events, in order.
        assert_eq!(a.snapshot().len(), 2);
        assert_eq!(b.snapshot().len(), 2);
        assert!(matches!(a.snapshot()[0], AgentEvent::TaskStarted { .. }));
        assert!(matches!(b.snapshot()[0], AgentEvent::TaskStarted { .. }));
        assert!(matches!(a.snapshot()[1], AgentEvent::TaskComplete { .. }));
        assert!(matches!(b.snapshot()[1], AgentEvent::TaskComplete { .. }));
    }
}
