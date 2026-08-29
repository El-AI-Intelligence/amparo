//! [`NotebookSink`] — the EventSink consumer that turns an agent run's
//! event stream into one persisted [`RunRecord`].
//!
//! The loop already emits everything the record needs: `TaskStarted` (the
//! prompt), `ToolCallRequested` (the call and, via the pure
//! `extract_target`, its target), `ToolGate` (decision + reasons),
//! `ApprovalRequested`/`ApprovalResolved` (escalation and outcome),
//! `ToolExecuted` (success, summary, duration), `Verification` (the
//! self-check) and `TaskComplete`/`TaskFailed` (the terminal event that
//! finalizes the record). This crate therefore changes nothing in the loop.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use amparo_agent::{extract_target, AgentEvent, EventSink};
use amparo_tools::Memory;
use tokio::task::JoinHandle;

use crate::record::{
    sequence_hash, strip, truncate_to, RunRecord, ToolStep, VerificationRecord, FINAL_ANSWER_CAP,
    TRUNCATE,
};

/// Per-call accumulator, fed by the call's events in order.
#[derive(Default, Clone)]
struct CallState {
    tool_name: String,
    /// Raw at first; stripped when the record is built.
    target: String,
    decision: Option<String>,
    reasons: Vec<String>,
    escalated: bool,
    approved: Option<bool>,
    success: Option<bool>,
    summary: Option<String>,
    duration_ms: Option<u64>,
}

/// Everything buffered between `TaskStarted` and the terminal event.
struct SinkState {
    started_at: String,
    started_instant: Instant,
    task_text: String,
    /// Call ids in first-requested order.
    order: Vec<String>,
    calls: HashMap<String, CallState>,
    verification: Option<VerificationRecord>,
}

/// The lab notebook's [`EventSink`]: buffers one task's events and, on the
/// terminal event, assembles a PII-stripped [`RunRecord`] and hands it to a
/// [`Memory::store`] on a spawned task.
///
/// The write is fire-and-forget by design — growth is observational and must
/// never fail or block the task. Persistence errors are reported on stderr
/// (`[notebook] …`), never silently dropped. Call [`flush`](NotebookSink::flush)
/// after a run in short-lived hosts (the CLI) so the pending write completes
/// before process exit.
pub struct NotebookSink {
    store: Arc<dyn Memory>,
    tenant_id: String,
    state: Mutex<Option<SinkState>>,
    pending: Mutex<Option<JoinHandle<()>>>,
}

impl NotebookSink {
    /// A sink that writes records for `tenant_id` through `store`.
    ///
    /// The host must run the agent loop inside a tokio runtime — the record
    /// write is spawned onto one.
    pub fn new(store: Arc<dyn Memory>, tenant_id: impl Into<String>) -> Self {
        Self {
            store,
            tenant_id: tenant_id.into(),
            state: Mutex::new(None),
            pending: Mutex::new(None),
        }
    }

    /// Await the pending record write, if any. Idempotent — after the
    /// terminal event the record exists on disk once this returns.
    pub async fn flush(&self) {
        let handle = self.pending.lock().unwrap().take();
        if let Some(handle) = handle {
            let _ = handle.await;
        }
    }

    /// Assemble and persist the record for the buffered task.
    fn finalize(&self, state: SinkState, status: &str, final_answer: Option<&str>) {
        let stripped_task = truncate_to(&strip(&state.task_text), TRUNCATE);
        let mut hash_pairs = Vec::with_capacity(state.order.len());
        let mut tool_calls = Vec::with_capacity(state.order.len());
        for id in state.order {
            let call = state.calls.get(&id).cloned().unwrap_or_default();
            let target = strip(&call.target);
            hash_pairs.push((call.tool_name.clone(), target.clone()));
            tool_calls.push(ToolStep {
                call_id: id,
                tool_name: call.tool_name,
                target: truncate_to(&target, TRUNCATE),
                decision: call.decision.unwrap_or_else(|| "unknown".to_string()),
                reasons: call.reasons.iter().map(|r| strip(r)).collect(),
                escalated: call.escalated,
                approved: call.approved,
                success: call.success,
                summary: call
                    .summary
                    .as_deref()
                    .map(|s| truncate_to(&strip(s), TRUNCATE)),
                duration_ms: call.duration_ms,
            });
        }
        let stripped_answer = final_answer.map(|a| truncate_to(&strip(a), FINAL_ANSWER_CAP));
        let record = RunRecord {
            version: 1,
            tenant_id: self.tenant_id.clone(),
            started_at: state.started_at,
            duration_ms: state.started_instant.elapsed().as_millis() as u64,
            task_text: stripped_task.clone(),
            tool_sequence_hash: sequence_hash(&hash_pairs),
            tool_calls,
            verification: state.verification,
            status: status.to_string(),
            token_cost_estimate: (stripped_task.chars().count()
                + stripped_answer.as_deref().map_or(0, |a| a.chars().count())
                + 3)
                / 4,
            final_answer: stripped_answer,
        };

        let json = serde_json::to_string(&record).unwrap_or_default();
        let store = Arc::clone(&self.store);
        let handle = tokio::spawn(async move {
            if let Err(error) = store.store(json).await {
                eprintln!("[notebook] run record failed to persist: {error}");
            }
        });
        *self.pending.lock().unwrap() = Some(handle);
    }
}

impl EventSink for NotebookSink {
    fn emit(&self, event: &AgentEvent) {
        let mut slot = self.state.lock().unwrap();
        match event {
            AgentEvent::TaskStarted { prompt } => {
                *slot = Some(SinkState {
                    started_at: chrono::Utc::now().to_rfc3339(),
                    started_instant: Instant::now(),
                    task_text: prompt.clone(),
                    order: Vec::new(),
                    calls: HashMap::new(),
                    verification: None,
                });
            }
            AgentEvent::ToolCallRequested { call } => {
                if let Some(state) = slot.as_mut() {
                    let (target, _) = extract_target(call);
                    state.order.push(call.id.clone());
                    let entry = state.calls.entry(call.id.clone()).or_default();
                    entry.tool_name = call.name.clone();
                    entry.target = target;
                }
            }
            AgentEvent::ToolGate {
                call_id,
                decision,
                reasons,
                ..
            } => {
                if let Some(state) = slot.as_mut() {
                    if let Some(call) = state.calls.get_mut(call_id) {
                        call.decision = Some(decision.clone());
                        call.reasons = reasons.clone();
                    }
                }
            }
            AgentEvent::ApprovalRequested { call_id, .. } => {
                if let Some(state) = slot.as_mut() {
                    if let Some(call) = state.calls.get_mut(call_id) {
                        call.escalated = true;
                    }
                }
            }
            AgentEvent::ApprovalResolved { call_id, approved } => {
                if let Some(state) = slot.as_mut() {
                    if let Some(call) = state.calls.get_mut(call_id) {
                        call.approved = Some(*approved);
                    }
                }
            }
            AgentEvent::ToolExecuted { result } => {
                if let Some(state) = slot.as_mut() {
                    if let Some(call) = state.calls.get_mut(&result.tool_call_id) {
                        call.success = Some(result.success);
                        call.summary = Some(result.display_summary.clone());
                        call.duration_ms = Some(result.duration_ms);
                    }
                }
            }
            AgentEvent::Verification { decision, feedback } => {
                if let Some(state) = slot.as_mut() {
                    state.verification = Some(VerificationRecord {
                        decision: decision.clone(),
                        feedback: feedback
                            .as_deref()
                            .map(|f| truncate_to(&strip(f), TRUNCATE)),
                    });
                }
            }
            AgentEvent::TaskComplete { final_answer } => {
                if let Some(state) = slot.take() {
                    self.finalize(state, "complete", Some(final_answer));
                }
            }
            AgentEvent::TaskFailed { message } => {
                if let Some(state) = slot.take() {
                    self.finalize(state, "failed", Some(message));
                }
            }
            // AssistantTurn, FinalAnswer: rendered by the event formatter, not
            // part of the record's shape.
            AgentEvent::AssistantTurn { .. } | AgentEvent::FinalAnswer { .. } => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use amparo_tools::{InMemoryStore, MemoryEntry, ToolCall, ToolResult};
    use serde_json::json;

    fn sink(tenant: &str) -> (Arc<NotebookSink>, Arc<InMemoryStore>) {
        let store = Arc::new(InMemoryStore::new());
        let sink = Arc::new(NotebookSink::new(
            store.clone() as Arc<dyn Memory>,
            tenant,
        ));
        (sink, store)
    }

    /// Wait-free read of the stored records for this test's tenant: flush
    /// first, then search on a term every record contains (its tenant id).
    async fn stored_records(store: &Arc<InMemoryStore>) -> Vec<RunRecord> {
        store
            .search("cli", usize::MAX)
            .await
            .into_iter()
            .filter_map(|e| serde_json::from_str::<RunRecord>(&e.content).ok())
            .collect()
    }

    fn start(sink: &NotebookSink, prompt: &str) {
        sink.emit(&AgentEvent::TaskStarted {
            prompt: prompt.to_string(),
        });
    }

    fn request(sink: &NotebookSink, id: &str, name: &str, args: serde_json::Value) {
        sink.emit(&AgentEvent::ToolCallRequested {
            call: ToolCall {
                id: id.to_string(),
                name: name.to_string(),
                arguments: args,
            },
        });
    }

    fn result(sink: &NotebookSink, id: &str, name: &str, success: bool, summary: &str) {
        sink.emit(&AgentEvent::ToolExecuted {
            result: ToolResult {
                tool_call_id: id.to_string(),
                tool_name: name.to_string(),
                success,
                output: json!({}),
                display_summary: summary.to_string(),
                duration_ms: 42,
            },
        });
    }

    #[tokio::test]
    async fn allowed_call_with_approval_becomes_a_full_record() {
        let (sink, store) = sink("cli");
        start(&sink, "list the directory");
        request(&sink, "call_1", "run_command", json!({"command": "ls"}));
        sink.emit(&AgentEvent::ToolGate {
            call_id: "call_1".into(),
            tool_name: "run_command".into(),
            decision: "allowed".into(),
            reasons: vec!["rule-a".into()],
        });
        sink.emit(&AgentEvent::ApprovalRequested {
            call_id: "call_1".into(),
            tool_name: "run_command".into(),
            reasons: vec!["policy escalated".into()],
        });
        sink.emit(&AgentEvent::ApprovalResolved {
            call_id: "call_1".into(),
            approved: true,
        });
        result(&sink, "call_1", "run_command", true, "listed 3 entries");
        sink.emit(&AgentEvent::Verification {
            decision: "complete".into(),
            feedback: None,
        });
        sink.emit(&AgentEvent::TaskComplete {
            final_answer: "the directory has 3 entries".into(),
        });
        sink.flush().await;

        let records = stored_records(&store).await;
        assert_eq!(records.len(), 1);
        let record = &records[0];
        assert_eq!(record.tenant_id, "cli");
        assert_eq!(record.status, "complete");
        assert_eq!(record.task_text, "list the directory");
        assert_eq!(record.tool_sequence_hash, sequence_hash(&[(
            "run_command".to_string(),
            "ls".to_string()
        )]));
        assert_eq!(record.tool_calls.len(), 1);
        let call = &record.tool_calls[0];
        assert_eq!(call.call_id, "call_1");
        assert_eq!(call.tool_name, "run_command");
        assert_eq!(call.target, "ls");
        assert_eq!(call.decision, "allowed");
        assert_eq!(call.reasons, vec!["rule-a".to_string()]);
        assert!(call.escalated);
        assert_eq!(call.approved, Some(true));
        assert_eq!(call.success, Some(true));
        assert_eq!(call.summary.as_deref(), Some("listed 3 entries"));
        assert_eq!(call.duration_ms, Some(42));
        assert_eq!(record.verification.as_ref().unwrap().decision, "complete");
        assert_eq!(record.final_answer.as_deref(), Some("the directory has 3 entries"));
        assert!(record.token_cost_estimate > 0);
        assert!(record.duration_ms < 60_000);
    }

    #[tokio::test]
    async fn blocked_call_has_no_execution_or_approval() {
        let (sink, store) = sink("cli");
        start(&sink, "try to write outside");
        request(&sink, "call_1", "write_file", json!({"path": "/etc/hosts"}));
        sink.emit(&AgentEvent::ToolGate {
            call_id: "call_1".into(),
            tool_name: "write_file".into(),
            decision: "trust_blocked".into(),
            reasons: vec!["tool tier exceeds the trust ceiling".into()],
        });
        sink.emit(&AgentEvent::TaskFailed {
            message: "no tool ran".into(),
        });
        sink.flush().await;

        let records = stored_records(&store).await;
        let record = &records[0];
        assert_eq!(record.status, "failed");
        assert_eq!(record.final_answer.as_deref(), Some("no tool ran"));
        let call = &record.tool_calls[0];
        assert_eq!(call.decision, "trust_blocked");
        assert_eq!(call.reasons, vec!["tool tier exceeds the trust ceiling".to_string()]);
        assert!(!call.escalated);
        assert_eq!(call.approved, None);
        assert_eq!(call.success, None);
        assert_eq!(call.summary, None);
        assert_eq!(call.duration_ms, None);
        assert_eq!(record.verification, None);
    }

    #[tokio::test]
    async fn pii_is_stripped_from_task_text_target_summary_and_answer() {
        let (sink, store) = sink("cli");
        start(&sink, "email alice@example.com the report");
        request(
            &sink,
            "call_1",
            "run_command",
            json!({"command": "mail alice@example.com"}),
        );
        sink.emit(&AgentEvent::ToolGate {
            call_id: "call_1".into(),
            tool_name: "run_command".into(),
            decision: "allowed".into(),
            reasons: vec![],
        });
        result(&sink, "call_1", "run_command", true, "mailed alice@example.com");
        sink.emit(&AgentEvent::Verification {
            decision: "complete".into(),
            feedback: None,
        });
        sink.emit(&AgentEvent::TaskComplete {
            final_answer: "sent to alice@example.com".into(),
        });
        sink.flush().await;

        let records = stored_records(&store).await;
        let record = &records[0];
        let everything = format!(
            "{}{}{}{}",
            record.task_text,
            record.tool_calls[0].target,
            record.tool_calls[0].summary.as_deref().unwrap_or(""),
            record.final_answer.as_deref().unwrap_or("")
        );
        assert!(!everything.contains("alice@example.com"));
        assert!(record.task_text.contains("[EMAIL_1]"));
        assert!(record.tool_calls[0].target.contains("[EMAIL_1]"));
        assert!(record
            .tool_calls[0]
            .summary
            .as_deref()
            .unwrap()
            .contains("[EMAIL_1]"));
        assert!(record.final_answer.as_deref().unwrap().contains("[EMAIL_1]"));
    }

    #[tokio::test]
    async fn last_verification_wins_and_feedback_is_stripped() {
        let (sink, store) = sink("cli");
        start(&sink, "do a thing");
        sink.emit(&AgentEvent::Verification {
            decision: "complete".into(),
            feedback: None,
        });
        sink.emit(&AgentEvent::Verification {
            decision: "incomplete".into(),
            feedback: Some("recheck bob@example.com".into()),
        });
        sink.emit(&AgentEvent::TaskFailed {
            message: "stopped".into(),
        });
        sink.flush().await;

        let records = stored_records(&store).await;
        let verification = records[0].verification.as_ref().unwrap();
        assert_eq!(verification.decision, "incomplete");
        let feedback = verification.feedback.as_deref().unwrap();
        assert!(feedback.contains("[EMAIL_1]"));
        assert!(!feedback.contains("bob@example.com"));
    }

    #[tokio::test]
    async fn one_shot_path_has_no_verification() {
        let (sink, store) = sink("cli");
        start(&sink, "open the file");
        request(&sink, "call_1", "read_file", json!({"path": "notes.txt"}));
        sink.emit(&AgentEvent::ToolGate {
            call_id: "call_1".into(),
            tool_name: "read_file".into(),
            decision: "allowed".into(),
            reasons: vec![],
        });
        result(&sink, "call_1", "read_file", true, "read 2 lines");
        sink.emit(&AgentEvent::TaskComplete {
            final_answer: "opened".into(),
        });
        sink.flush().await;

        let records = stored_records(&store).await;
        assert_eq!(records[0].verification, None);
        assert_eq!(records[0].status, "complete");
    }

    #[tokio::test]
    async fn long_text_is_truncated() {
        let (sink, store) = sink("cli");
        let long_prompt = "x".repeat(500);
        start(&sink, &long_prompt);
        sink.emit(&AgentEvent::TaskComplete {
            final_answer: "y".repeat(900),
        });
        sink.flush().await;

        let records = stored_records(&store).await;
        let record = &records[0];
        assert_eq!(record.task_text, format!("{}…", "x".repeat(TRUNCATE)));
        assert_eq!(
            record.final_answer.as_deref(),
            Some(format!("{}…", "y".repeat(FINAL_ANSWER_CAP)).as_str())
        );
    }

    #[tokio::test]
    async fn events_before_task_started_are_ignored() {
        let (sink, store) = sink("cli");
        // Terminal events with no task buffered must not persist anything.
        sink.emit(&AgentEvent::TaskComplete {
            final_answer: "orphan".into(),
        });
        sink.flush().await;
        assert!(stored_records(&store).await.is_empty());
    }

    #[tokio::test]
    async fn multiple_tasks_each_produce_one_record() {
        let (sink, store) = sink("cli");
        start(&sink, "first");
        sink.emit(&AgentEvent::TaskComplete {
            final_answer: "one".into(),
        });
        start(&sink, "second");
        sink.emit(&AgentEvent::TaskFailed {
            message: "two".into(),
        });
        sink.flush().await;

        let records = stored_records(&store).await;
        assert_eq!(records.len(), 2);
        assert!(records.iter().any(|r| r.status == "complete"));
        assert!(records.iter().any(|r| r.status == "failed"));
    }

    #[tokio::test]
    async fn store_failure_is_reported_not_fatal() {
        // A Memory impl whose store always fails: emit a full task, flush —
        // the sink must not panic and must leave no pending handle.
        struct FailingStore;
        #[async_trait::async_trait]
        impl Memory for FailingStore {
            async fn search(&self, _query: &str, _limit: usize) -> Vec<MemoryEntry> {
                Vec::new()
            }
            async fn store(&self, _content: String) -> Result<String, String> {
                Err("disk full".into())
            }
        }
        let sink = Arc::new(NotebookSink::new(
            Arc::new(FailingStore) as Arc<dyn Memory>,
            "cli",
        ));
        start(&sink, "task");
        sink.emit(&AgentEvent::TaskComplete {
            final_answer: "done".into(),
        });
        sink.flush().await;
        // Reached here: the failure did not panic; the task itself is done.
    }
}
