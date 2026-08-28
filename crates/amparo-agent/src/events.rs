//! The agent's event surface — every decision the loop makes is observable.
//!
//! `AgentEvent`s flow through the [`EventSink`] seam: the default
//! [`InMemoryEventSink`] keeps a log and fans out on a broadcast channel, so
//! a host application can render live progress, an audit log can persist
//! it, or a test can assert on it.

use amparo_tools::{ToolCall, ToolResult};
use serde::Serialize;
use tokio::sync::broadcast;

/// Everything the agent does, in order. Serializable for audit trails.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum AgentEvent {
    /// The task began.
    TaskStarted {
        /// The original prompt.
        prompt: String,
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
    /// The self-verification turn decided.
    Verification {
        /// `complete` | `incomplete`
        decision: String,
        /// The model's feedback, when incomplete.
        feedback: Option<String>,
    },
    /// The task completed with this final answer.
    TaskComplete {
        /// The final answer text.
        final_answer: String,
    },
    /// The task failed.
    TaskFailed {
        /// Why the task failed.
        message: String,
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
        Self { events: std::sync::Mutex::new(Vec::new()), tx }
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
        AgentEvent::TaskStarted { prompt } => format!("[task] {}", truncate(prompt)),
        AgentEvent::AssistantTurn { step, content, tool_calls } => {
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
        AgentEvent::ToolGate { tool_name, decision, reasons, .. } => {
            let why = if reasons.is_empty() {
                String::new()
            } else {
                format!(" ({})", truncate(&reasons.join("; ")))
            };
            format!("[gate] {tool_name}: {decision}{why}")
        }
        AgentEvent::ApprovalRequested { tool_name, reasons, .. } => {
            format!("[approval] {tool_name}: {}", truncate(&reasons.join("; ")))
        }
        AgentEvent::ApprovalResolved { approved, .. } => {
            format!("[approval] {}", if *approved { "granted" } else { "denied" })
        }
        AgentEvent::ToolExecuted { result } => {
            format!("[exec] {} ({}ms)", result.tool_name, result.duration_ms)
        }
        AgentEvent::FinalAnswer { content } => format!("[answer] {}", truncate(content)),
        AgentEvent::Verification { decision, feedback } => {
            let detail = feedback
                .as_ref()
                .map(|f| format!(": {}", truncate(f)))
                .unwrap_or_default();
            format!("[verify] {decision}{detail}")
        }
        AgentEvent::TaskComplete { final_answer } => {
            format!("[complete] {}", truncate(final_answer))
        }
        AgentEvent::TaskFailed { message } => format!("[failed] {}", truncate(message)),
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
        sink.emit(&AgentEvent::TaskStarted { prompt: "hi".into() });
        sink.emit(&AgentEvent::ToolExecuted { result: result("read_file") });
        sink.emit(&AgentEvent::TaskComplete { final_answer: "done".into() });
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
        sink.emit(&AgentEvent::TaskStarted { prompt: "hi".into() });
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
            ("[task]", format_event(&AgentEvent::TaskStarted { prompt: "p".into() })),
            (
                "[turn",
                format_event(&AgentEvent::AssistantTurn {
                    step: 2,
                    content: "hi".into(),
                    tool_calls: 1,
                }),
            ),
            ("[call]", format_event(&AgentEvent::ToolCallRequested { call: make_call() })),
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
                format_event(&AgentEvent::ApprovalResolved { call_id: "c1".into(), approved: true }),
            ),
            ("[exec] run_command", format_event(&AgentEvent::ToolExecuted { result: make_result() })),
            ("[answer]", format_event(&AgentEvent::FinalAnswer { content: "a".into() })),
            (
                "[verify]",
                format_event(&AgentEvent::Verification {
                    decision: "complete".into(),
                    feedback: Some("f".into()),
                }),
            ),
            ("[complete]", format_event(&AgentEvent::TaskComplete { final_answer: "a".into() })),
            ("[failed]", format_event(&AgentEvent::TaskFailed { message: "m".into() })),
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
        assert_eq!(line.chars().count(), "[answer] ".chars().count() + TRUNCATE + 1);

        let line = format_event(&AgentEvent::FinalAnswer { content: "short".into() });
        assert_eq!(line, "[answer] short");
    }

    #[test]
    fn denied_approvals_say_denied() {
        let line = format_event(&AgentEvent::ApprovalResolved { call_id: "c1".into(), approved: false });
        assert_eq!(line, "[approval] denied");
    }
}
