//! Blackboard — the workspace-scoped coordination board (M10).
//!
//! The blackboard is the event bus in its deferred, hand-rolled shape: a
//! shared, append-only JSONL board under the workspace root
//! (`.amparo/blackboard/board.jsonl`). Sub-agents coordinate through it —
//! a write appends one row, a read folds the log with the last write per
//! key winning (the skills `adopted.jsonl` precedent). The store is cheap
//! to clone and travels with the registry: a sub-agent's registry is the
//! parent's clone, so every member of the delegation chain reads and
//! writes the same file.
//!
//! Deliberately hand-rolled JSONL, no database: writes open the file
//! append-only, write one line and flush; reads re-fold the whole file —
//! O(file), acceptable at board scale. Rows carry no writer identity:
//! tool arguments are model-chosen and cannot be trusted as provenance —
//! the trusted writer (the loop's task id) rides in the agent's `[bus]`
//! event instead (I4).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use super::{ToolCall, ToolExecutor, ToolParam, ToolResult, ToolSchema, ToolTrustTier};

/// The blackboard directory under the workspace root.
pub const BLACKBOARD_DIR: &str = ".amparo/blackboard";

/// The board filename inside [`BLACKBOARD_DIR`].
pub const BLACKBOARD_FILE: &str = "board.jsonl";

/// The registry name of the blackboard read tool.
pub const BLACKBOARD_READ: &str = "blackboard_read";

/// The registry name of the blackboard write tool.
pub const BLACKBOARD_WRITE: &str = "blackboard_write";

fn make_result(call: &ToolCall, success: bool, output: Value, summary: String) -> ToolResult {
    ToolResult {
        tool_call_id: call.id.clone(),
        tool_name: call.name.clone(),
        success,
        output,
        display_summary: summary,
        duration_ms: 0,
    }
}

/// One board row. The log is append-only — writing a key again is a new
/// row, and the last row per key wins on read (the audit trail keeps
/// every row, I4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BlackboardEntry {
    /// The coordination key.
    pub key: String,
    /// The value written, stored as a string.
    pub value: String,
    /// RFC 3339 write timestamp.
    pub written_at: String,
}

/// The board store: one path, append-only writes, last-wins folds on read.
///
/// Cheap to clone — every blackboard tool in a task's registry shares one
/// `Arc`, and the registry clone handed to sub-agents (M8) carries the
/// same `Arc`, so the whole delegation chain shares the file. Each write
/// opens, appends one line and flushes — no held handle, so a shared
/// store never contends on a lock and a broken board can only fail the
/// operation that touches it.
#[derive(Debug, Clone)]
pub struct BlackboardStore {
    path: PathBuf,
}

impl BlackboardStore {
    /// A board rooted at `workspace_root`:
    /// `<workspace_root>/.amparo/blackboard/board.jsonl`.
    pub fn new(workspace_root: impl AsRef<Path>) -> Self {
        Self {
            path: workspace_root
                .as_ref()
                .join(BLACKBOARD_DIR)
                .join(BLACKBOARD_FILE),
        }
    }

    /// The board file this store appends to.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one row, creating the file and parent directories as
    /// needed — a task may write the board before anything else in the
    /// workspace exists, so the store must not depend on it existing.
    pub fn write(&self, key: &str, value: &str) -> Result<BlackboardEntry, String> {
        if key.trim().is_empty() {
            return Err("blackboard key must not be empty".to_string());
        }
        let entry = BlackboardEntry {
            key: key.to_string(),
            // Strip PII before persistence (audit 2026-08-31 MED-5) — the
            // board is shared across the whole delegation chain, so it
            // follows the same discipline as the notebook: placeholders
            // persist, originals never do.
            value: amparo_privacy::secure_minions_strip(value).sanitised_text,
            written_at: chrono::Utc::now().to_rfc3339(),
        };
        if let Some(parent) = self.path.parent() {
            let created = !parent.exists();
            std::fs::create_dir_all(parent).map_err(|e| {
                format!(
                    "cannot create blackboard directory {}: {e}",
                    parent.display()
                )
            })?;
            // Harden only a directory this call created (audit
            // 2026-08-31 MED-6).
            if created {
                amparo_privacy::perms::owner_only(parent).map_err(|e| {
                    format!("cannot lock blackboard directory {}: {e}", parent.display())
                })?;
            }
        }
        let line = serde_json::to_string(&entry).map_err(|e| e.to_string())?;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|e| format!("cannot open blackboard {}: {e}", self.path.display()))?;
        amparo_privacy::perms::owner_only(&self.path)
            .map_err(|e| format!("cannot lock blackboard {}: {e}", self.path.display()))?;
        writeln!(file, "{line}").map_err(|e| format!("cannot write blackboard: {e}"))?;
        file.flush()
            .map_err(|e| format!("cannot flush blackboard: {e}"))?;
        Ok(entry)
    }

    /// Fold the board: the last value per key, in the log's order. A
    /// missing file is an empty board and unparseable lines are skipped —
    /// the same tolerance as the skills adoption log.
    pub fn read(&self) -> Result<Map<String, Value>, String> {
        let Ok(text) = std::fs::read_to_string(&self.path) else {
            return Ok(Map::new());
        };
        let mut board: Map<String, Value> = Map::new();
        for line in text.lines() {
            let Ok(entry) = serde_json::from_str::<BlackboardEntry>(line) else {
                continue;
            };
            board.insert(entry.key, Value::String(entry.value));
        }
        Ok(board)
    }
}

/// Reads a key from the board (or the whole board). Trusted at
/// [`ToolTrustTier::Observational`] — a read never mutates the board.
pub struct BlackboardReadTool {
    store: Arc<BlackboardStore>,
}

impl BlackboardReadTool {
    /// A read tool over `store`.
    pub fn new(store: Arc<BlackboardStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl ToolExecutor for BlackboardReadTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: BLACKBOARD_READ.to_string(),
            description: "Read a key from the shared blackboard — the \
workspace-scoped board every member of the delegation chain reads and \
writes. Omit `key` to read the whole board (the last write per key wins)."
                .to_string(),
            parameters: vec![ToolParam {
                name: "key".to_string(),
                description: "The key to read; omit to read the whole board".to_string(),
                param_type: "string".to_string(),
                enum_values: None,
                required: false,
            }],
            trust_tier: ToolTrustTier::Observational,
        }
    }

    async fn execute(&self, call: &ToolCall) -> ToolResult {
        match self.store.read() {
            Ok(board) => match call.arg_str("key") {
                Some(key) => {
                    let value = board.get(key).cloned().unwrap_or(Value::Null);
                    let summary = if value.is_null() {
                        format!("blackboard '{key}': unset")
                    } else {
                        format!("blackboard '{key}': set")
                    };
                    make_result(call, true, json!({ "key": key, "value": value }), summary)
                }
                None => {
                    let count = board.len();
                    make_result(
                        call,
                        true,
                        json!({ "board": Value::Object(board) }),
                        format!("blackboard: {count} key(s)"),
                    )
                }
            },
            Err(e) => make_result(
                call,
                false,
                json!({ "error": e }),
                "blackboard read failed".to_string(),
            ),
        }
    }
}

/// Writes a key on the board. Trusted at [`ToolTrustTier::LocalMutating`]
/// — the board is a workspace file, and the write goes through the same
/// gate chain as every other local mutation.
pub struct BlackboardWriteTool {
    store: Arc<BlackboardStore>,
}

impl BlackboardWriteTool {
    /// A write tool over `store`.
    pub fn new(store: Arc<BlackboardStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl ToolExecutor for BlackboardWriteTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: BLACKBOARD_WRITE.to_string(),
            description: "Write a key on the shared blackboard — the \
workspace-scoped board every member of the delegation chain reads and \
writes. Writing an existing key appends a new row; the last write wins."
                .to_string(),
            parameters: vec![
                ToolParam {
                    name: "key".to_string(),
                    description: "The coordination key to write".to_string(),
                    param_type: "string".to_string(),
                    enum_values: None,
                    required: true,
                },
                ToolParam {
                    name: "value".to_string(),
                    description: "The value to store under the key".to_string(),
                    param_type: "string".to_string(),
                    enum_values: None,
                    required: true,
                },
            ],
            trust_tier: ToolTrustTier::LocalMutating,
        }
    }

    async fn execute(&self, call: &ToolCall) -> ToolResult {
        let key = call.arg_str("key").unwrap_or_default();
        let value = call.arg_str("value").unwrap_or_default();
        match self.store.write(key, value) {
            Ok(entry) => make_result(
                call,
                true,
                json!({
                    "key": entry.key,
                    "value": entry.value,
                    "written_at": entry.written_at,
                }),
                format!("blackboard '{}' set", entry.key),
            ),
            Err(e) => make_result(
                call,
                false,
                json!({ "error": e }),
                "blackboard write failed".to_string(),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A unique board root per test — the sequence counter makes the
    /// directory unique by construction: concurrent tests can draw the
    /// same clock reading, and the counter cannot collide within the
    /// process (M10 W6 hardening).
    fn temp_root() -> PathBuf {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "amparo-blackboard-{}-{}-{}",
            std::process::id(),
            n,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ))
    }

    fn call(tool: &str, arguments: Value) -> ToolCall {
        ToolCall {
            id: "call_1".to_string(),
            name: tool.to_string(),
            arguments,
        }
    }

    #[test]
    fn write_then_read_round_trips() {
        let store = BlackboardStore::new(temp_root());
        store.write("status", "in progress").unwrap();
        let board = store.read().unwrap();
        assert_eq!(
            board.get("status"),
            Some(&Value::String("in progress".into()))
        );
        assert!(store.path().ends_with(".amparo/blackboard/board.jsonl"));
    }

    #[cfg(unix)]
    #[test]
    fn writes_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let root = temp_root();
        let store = BlackboardStore::new(&root);
        store.write("k", "v").unwrap();
        let dir = store.path().parent().unwrap();
        assert_eq!(
            std::fs::metadata(dir).unwrap().permissions().mode() & 0o777,
            0o700,
            "blackboard dir must be owner-only"
        );
        assert_eq!(
            std::fs::metadata(store.path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600,
            "board file must be owner-only"
        );
    }

    #[test]
    fn write_strips_pii_before_persisting() {
        // Audit 2026-08-31 MED-5: the board is shared across the whole
        // delegation chain, so raw values never land on disk.
        let store = BlackboardStore::new(temp_root());
        store.write("contact", "call 555-123-4567").unwrap();
        let board = store.read().unwrap();
        let value = board
            .get("contact")
            .and_then(|v| v.as_str())
            .expect("contact value present");
        assert!(!value.contains("555-123-4567"));
        assert!(value.contains("[PHONE_"));
    }

    #[test]
    fn last_write_per_key_wins_and_the_audit_trail_keeps_every_row() {
        let store = BlackboardStore::new(temp_root());
        store.write("k", "first").unwrap();
        store.write("k", "second").unwrap();
        let board = store.read().unwrap();
        assert_eq!(board.get("k"), Some(&Value::String("second".into())));
        let rows = std::fs::read_to_string(store.path()).unwrap();
        assert_eq!(rows.lines().count(), 2, "both rows survive the rewrite");
    }

    #[test]
    fn a_missing_board_reads_as_empty() {
        let store = BlackboardStore::new(temp_root());
        assert!(store.read().unwrap().is_empty());
    }

    #[test]
    fn unparseable_lines_are_skipped() {
        let store = BlackboardStore::new(temp_root());
        store.write("k", "v").unwrap();
        std::fs::OpenOptions::new()
            .append(true)
            .open(store.path())
            .unwrap()
            .write_all(b"not json\n")
            .unwrap();
        store.write("k2", "v2").unwrap();
        let board = store.read().unwrap();
        assert_eq!(board.len(), 2, "garbage line skipped, good rows folded");
        assert_eq!(board.get("k2"), Some(&Value::String("v2".into())));
    }

    #[test]
    fn an_empty_key_is_rejected() {
        let store = BlackboardStore::new(temp_root());
        assert!(store.write("", "v").is_err());
        assert!(store.write("   ", "v").is_err());
    }

    #[tokio::test]
    async fn tools_round_trip_through_the_schemas() {
        let store = Arc::new(BlackboardStore::new(temp_root()));
        let read = BlackboardReadTool::new(Arc::clone(&store));
        let write = BlackboardWriteTool::new(store);

        assert_eq!(read.schema().name, BLACKBOARD_READ);
        assert_eq!(read.schema().trust_tier, ToolTrustTier::Observational);
        assert_eq!(write.schema().name, BLACKBOARD_WRITE);
        assert_eq!(write.schema().trust_tier, ToolTrustTier::LocalMutating);

        let w = write
            .execute(&call(
                BLACKBOARD_WRITE,
                json!({ "key": "answer", "value": "42" }),
            ))
            .await;
        assert!(w.success, "{w:?}");
        assert_eq!(w.output["key"], "answer");

        let r = read
            .execute(&call(BLACKBOARD_READ, json!({ "key": "answer" })))
            .await;
        assert!(r.success);
        assert_eq!(r.output["value"], "42");
        assert_eq!(r.display_summary, "blackboard 'answer': set");

        let all = read.execute(&call(BLACKBOARD_READ, json!({}))).await;
        assert_eq!(all.output["board"]["answer"], "42");
        assert!(all.display_summary.contains("1 key(s)"));

        let missing = read
            .execute(&call(BLACKBOARD_READ, json!({ "key": "nope" })))
            .await;
        assert!(missing.success, "an unset key is a value, not an error");
        assert!(missing.output["value"].is_null());
        assert_eq!(missing.display_summary, "blackboard 'nope': unset");

        let bad = write
            .execute(&call(BLACKBOARD_WRITE, json!({ "key": "", "value": "x" })))
            .await;
        assert!(!bad.success);
        assert!(bad.output["error"].is_string());
    }
}
