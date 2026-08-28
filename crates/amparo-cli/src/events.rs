//! Terminal event rendering: one `[tag]` line per agent event on stderr.
//!
//! stdout is reserved for the final answer, so every observability line
//! (turn, gate, approval, execution, verification, report) goes to stderr.

use amparo_agent::{AgentEvent, EventSink};

/// Long content is truncated to this many chars, plus an ellipsis.
pub const TRUNCATE: usize = 200;

/// Sink that renders each event as a stderr line.
pub struct PrintingSink;

impl EventSink for PrintingSink {
    fn emit(&self, event: &AgentEvent) {
        eprintln!("{}", format_event(event));
    }
}

/// Truncate `s` to [`TRUNCATE`] chars, appending `…` when anything was cut.
fn truncate(s: &str) -> String {
    let mut chars = s.chars();
    let head: String = chars.by_ref().take(TRUNCATE).collect();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

/// Render one event as a stderr line. Pure — unit-testable without a sink.
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

    fn call() -> amparo_tools::ToolCall {
        amparo_tools::ToolCall {
            id: "c1".into(),
            name: "run_command".into(),
            arguments: json!({"command": "echo hi"}),
        }
    }

    fn result() -> amparo_tools::ToolResult {
        amparo_tools::ToolResult {
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
            ("[call]", format_event(&AgentEvent::ToolCallRequested { call: call() })),
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
            ("[exec] run_command", format_event(&AgentEvent::ToolExecuted { result: result() })),
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
