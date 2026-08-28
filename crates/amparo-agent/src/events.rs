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
}
