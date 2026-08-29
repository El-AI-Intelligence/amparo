//! [`LedgerSink`] — the EventSink consumer that appends privacy-ledger
//! rows for network-touching executions and PII strips.
//!
//! The ledger is an I6 instrument: always-on, and it records *evidence*
//! rather than content. For every execution attempt of a network tool —
//! or a human denial, which never reaches execution — the row carries the
//! tool name, the host at most (never a path, query or command), the
//! outcome, and whether a human approved or denied it — the "who allowed
//! this, and under what policy" answer. PII strips are recorded as
//! per-category counts, never values.
//!
//! Open or write failures never fail the task: the host decides what to
//! do when the store cannot be opened, and a write failure warns once per
//! task (`[ledger] …`) and continues.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use amparo_privacy::{site_host_only, LedgerKind, LedgerRow, LedgerStore};

use crate::events::{AgentEvent, EventSink};

/// Tools whose execution attempts reach beyond the machine and therefore
/// belong in the ledger. `run_command` is included even for purely local
/// commands — the operator cannot know without looking, and the row never
/// carries the command text.
pub const NETWORK_TOOLS: [&str; 3] = ["web_search", "fetch_url", "run_command"];

/// Per-call tracking between the request and its execution.
struct CallTracker {
    tool: String,
    site: Option<String>,
    gate: Option<String>,
}

/// The privacy ledger's [`EventSink`]: tracks network-tool calls from
/// request to execution, records the human gate's answer, and appends one
/// [`LedgerRow`] per execution attempt, human denial, or PII strip. Cheap
/// to clone behind an `Arc` for fanout.
pub struct LedgerSink {
    store: LedgerStore,
    tenant_id: String,
    calls: Mutex<HashMap<String, CallTracker>>,
    warn_on_write: AtomicBool,
}

impl LedgerSink {
    /// A sink appending rows tagged with `tenant_id` through `store`.
    pub fn new(store: LedgerStore, tenant_id: impl Into<String>) -> Self {
        Self {
            store,
            tenant_id: tenant_id.into(),
            calls: Mutex::new(HashMap::new()),
            warn_on_write: AtomicBool::new(false),
        }
    }

    /// Append one row; a write failure warns once per task and never
    /// propagates — the ledger is observational.
    fn append(&self, row: LedgerRow) {
        if let Err(error) = self.store.append(&row) {
            if !self.warn_on_write.swap(true, Ordering::Relaxed) {
                eprintln!("[ledger] row failed to persist: {error}");
            }
        }
    }
}

impl EventSink for LedgerSink {
    fn emit(&self, event: &AgentEvent) {
        match event {
            AgentEvent::TaskStarted { .. } => {
                // Fresh task — reset the once-per-task write warning.
                self.warn_on_write.store(false, Ordering::Relaxed);
            }
            AgentEvent::ToolCallRequested { call } => {
                if NETWORK_TOOLS.contains(&call.name.as_str()) {
                    let site = if call.name == "fetch_url" {
                        call.arg_str("url").and_then(site_host_only)
                    } else {
                        None
                    };
                    self.calls.lock().unwrap().insert(
                        call.id.clone(),
                        CallTracker { tool: call.name.clone(), site, gate: None },
                    );
                }
            }
            AgentEvent::ApprovalResolved { call_id, approved } => {
                if *approved {
                    // Execution follows — remember the verdict for the
                    // row written at `ToolExecuted`.
                    if let Some(tracker) = self.calls.lock().unwrap().get_mut(call_id) {
                        tracker.gate = Some("human_approved".to_string());
                    }
                } else {
                    // Denied: the execution never happens, but the
                    // denial is itself the answer the ledger exists to
                    // record — write the row now.
                    let denied = self.calls.lock().unwrap().remove(call_id);
                    if let Some(tracker) = denied {
                        self.append(LedgerRow {
                            ts: chrono::Utc::now().to_rfc3339(),
                            tenant: self.tenant_id.clone(),
                            kind: LedgerKind::NetworkCall,
                            tool: Some(tracker.tool),
                            site: tracker.site,
                            outcome: Some("denied".to_string()),
                            gate: Some("human_denied".to_string()),
                            pii_counts: Vec::new(),
                        });
                    }
                }
            }
            AgentEvent::ToolExecuted { result } => {
                if let Some(tracker) = self.calls.lock().unwrap().remove(&result.tool_call_id) {
                    self.append(LedgerRow {
                        ts: chrono::Utc::now().to_rfc3339(),
                        tenant: self.tenant_id.clone(),
                        kind: LedgerKind::NetworkCall,
                        tool: Some(tracker.tool),
                        site: tracker.site,
                        outcome: Some(if result.success { "ok" } else { "error" }.to_string()),
                        gate: tracker.gate,
                        pii_counts: Vec::new(),
                    });
                }
            }
            AgentEvent::PrivacyStripped { categories } => {
                self.append(LedgerRow {
                    ts: chrono::Utc::now().to_rfc3339(),
                    tenant: self.tenant_id.clone(),
                    kind: LedgerKind::PiiStrip,
                    tool: None,
                    site: None,
                    outcome: None,
                    gate: None,
                    pii_counts: categories.clone(),
                });
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use amparo_tools::{ToolCall, ToolResult};
    use serde_json::json;
    use std::path::PathBuf;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "amparo-ledger-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sink(dir: &PathBuf) -> LedgerSink {
        let store = LedgerStore::open(dir.join("ledger.jsonl")).unwrap();
        LedgerSink::new(store, "cli")
    }

    fn request(sink: &LedgerSink, id: &str, name: &str, args: serde_json::Value) {
        sink.emit(&AgentEvent::ToolCallRequested {
            call: ToolCall { id: id.to_string(), name: name.to_string(), arguments: args },
        });
    }

    fn executed(sink: &LedgerSink, id: &str, name: &str, success: bool) {
        sink.emit(&AgentEvent::ToolExecuted {
            result: ToolResult {
                tool_call_id: id.to_string(),
                tool_name: name.to_string(),
                success,
                output: json!({}),
                display_summary: "ok".to_string(),
                duration_ms: 1,
            },
        });
    }

    fn rows(dir: &PathBuf) -> Vec<LedgerRow> {
        LedgerStore::open(dir.join("ledger.jsonl")).unwrap().read_all().unwrap()
    }

    #[test]
    fn fetch_url_execution_writes_a_row_with_host_only_site() {
        let dir = temp_dir();
        let sink = sink(&dir);
        sink.emit(&AgentEvent::TaskStarted { prompt: "fetch it".into() });
        request(
            &sink,
            "c1",
            "fetch_url",
            json!({"url": "https://example.com/a/b?q=secret@tracking"}),
        );
        executed(&sink, "c1", "fetch_url", true);

        let rows = rows(&dir);
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.tenant, "cli");
        assert_eq!(row.kind, LedgerKind::NetworkCall);
        assert_eq!(row.tool.as_deref(), Some("fetch_url"));
        assert_eq!(row.site.as_deref(), Some("https://example.com"));
        assert_eq!(row.outcome.as_deref(), Some("ok"));
        assert_eq!(row.gate, None);
    }

    #[test]
    fn human_denied_is_recorded_on_the_row() {
        let dir = temp_dir();
        let sink = sink(&dir);
        request(&sink, "c1", "web_search", json!({"query": "weather"}));
        sink.emit(&AgentEvent::ApprovalResolved { call_id: "c1".into(), approved: false });

        // The denial alone is the row — the execution never happens.
        let written = rows(&dir);
        assert_eq!(written.len(), 1);
        assert_eq!(written[0].gate.as_deref(), Some("human_denied"));
        assert_eq!(written[0].outcome.as_deref(), Some("denied"));
        assert_eq!(written[0].tool.as_deref(), Some("web_search"));
        assert_eq!(written[0].site, None);
        // A spurious later execution (the loop never sends one after a
        // denial) finds no tracker and writes nothing.
        executed(&sink, "c1", "web_search", true);
        assert_eq!(rows(&dir).len(), 1);
    }

    #[test]
    fn human_approved_is_recorded_on_the_row() {
        let dir = temp_dir();
        let sink = sink(&dir);
        request(&sink, "c1", "run_command", json!({"command": "sudo rm -rf /tmp/x"}));
        sink.emit(&AgentEvent::ApprovalResolved { call_id: "c1".into(), approved: true });
        executed(&sink, "c1", "run_command", true);

        let rows = rows(&dir);
        assert_eq!(rows[0].gate.as_deref(), Some("human_approved"));
        // The command text never reaches the ledger (the tool name itself
        // legitimately contains the word "command" — assert on payload).
        let line = serde_json::to_string(&rows[0]).unwrap();
        assert!(!line.contains("sudo"));
        assert!(!line.contains("rm -rf"));
    }

    #[test]
    fn observational_local_tools_write_no_rows() {
        let dir = temp_dir();
        let sink = sink(&dir);
        request(&sink, "c1", "read_file", json!({"path": "notes.txt"}));
        request(&sink, "c2", "write_file", json!({"path": "notes.txt"}));
        executed(&sink, "c1", "read_file", true);
        executed(&sink, "c2", "write_file", true);
        assert!(rows(&dir).is_empty());
    }

    #[test]
    fn blocked_network_calls_write_no_rows() {
        // A call the gate blocked never executes — no execution attempt,
        // no ledger row. (Its denial lives in the notebook's gate log.)
        let dir = temp_dir();
        let sink = sink(&dir);
        request(&sink, "c1", "run_command", json!({"command": "curl example.com"}));
        assert!(rows(&dir).is_empty());
    }

    #[test]
    fn pii_strip_writes_a_counts_only_row() {
        let dir = temp_dir();
        let sink = sink(&dir);
        sink.emit(&AgentEvent::PrivacyStripped {
            categories: vec![("email".to_string(), 2), ("phone".to_string(), 1)],
        });

        let rows = rows(&dir);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kind, LedgerKind::PiiStrip);
        assert_eq!(rows[0].tool, None);
        assert_eq!(
            rows[0].pii_counts,
            vec![("email".to_string(), 2), ("phone".to_string(), 1)]
        );
    }

    #[test]
    fn failed_execution_records_error_outcome() {
        let dir = temp_dir();
        let sink = sink(&dir);
        request(&sink, "c1", "fetch_url", json!({"url": "https://down.example.com/x"}));
        executed(&sink, "c1", "fetch_url", false);

        let rows = rows(&dir);
        assert_eq!(rows[0].outcome.as_deref(), Some("error"));
        assert_eq!(rows[0].site.as_deref(), Some("https://down.example.com"));
    }
}
