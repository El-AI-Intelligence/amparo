//! Session persistence — checkpoints that let a task survive a crash
//! (M7).
//!
//! Every loop iteration (and every terminal exit) snapshots the task to a
//! [`Checkpoint`] through the [`CheckpointStore`] seam. A `Running`
//! checkpoint is a resume point: [`crate::Agent::resume`] restores the
//! conversation and loop state and continues the same loop. A `Complete`
//! or `Failed` checkpoint is an archive the chat host reads for
//! cross-task continuity (W8). The checkpoint is a resume UX — the event
//! stream and the notebook are the audit trail, and a crash loses at most
//! one turn.
//!
//! Privacy (I6): PII is stripped at write and the placeholder map is
//! discarded — checkpoints are archival, so there is no restore path.
//! The system prompt is never stored (I5): [`crate::Agent::resume`] re-prepends
//! the current one, so a prompt change in a new build is what a resumed
//! task sees, exactly like a fresh run.

use crate::events::truncate;
use amparo_inference::ChatMessage;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// The format version of a [`Checkpoint`] file. Bump when the shape
/// changes; readers skip files with an unknown version.
pub const CHECKPOINT_VERSION: u32 = 1;

/// How a checkpointed task stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    /// The task is mid-loop — a resume point.
    Running,
    /// The task finished with a final answer.
    Complete,
    /// The task ended without one.
    Failed,
}

/// A snapshot of the loop locals the agent carries between turns.
///
/// Restoring these on resume is what makes the loop continuous: the
/// same-tool guard keeps counting across the restart (with the tool it
/// was counting), the empty-turn retry budget is preserved, and the
/// case-library retrieval query keeps its tool sequence (M6b).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LoopState {
    /// The last executed tool's name — the same-tool guard's comparison
    /// base. `None` before the first tool result.
    pub last_tool_name: Option<String>,
    /// Consecutive executions of [`LoopState::last_tool_name`] so far.
    pub same_tool_count: u32,
    /// Whether the current empty-turn streak already spent its retry.
    pub empty_turn_retried: bool,
    /// The most recent successful tool summary, used to finalize
    /// gracefully when a later turn comes back empty.
    pub last_good_summary: Option<String>,
    /// Tool names this task has called, in first-use order — the case
    /// library's retrieval query uses them as a sequence signal (M6b).
    pub used_tool_names: Vec<String>,
    /// Loop iterations consumed so far (1 = one LLM turn).
    pub steps_used: usize,
}

/// One task snapshot, serialized as JSON under the store's root.
///
/// `conversation` excludes system-role messages (I5) and is PII-stripped
/// at write (I6); `prompt` is stripped the same way.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    /// The [`CHECKPOINT_VERSION`] this file was written with.
    pub version: u32,
    /// The tenant the task ran under (`platform:user_id`, or `cli`).
    pub tenant: String,
    /// The stable task handle: `sess-<nanos>-<pid>`, generated at start.
    pub task_id: String,
    /// The parent task's id when this task is a spawned sub-agent (M8) —
    /// the delegation chain, explicit in the file. `None` for top-level
    /// tasks; files written before M8 parse with `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_task_id: Option<String>,
    /// Unix seconds when the task started — the newest-Running scan
    /// orders by this.
    pub started_at: u64,
    /// The task prompt, PII-stripped at write.
    pub prompt: String,
    /// How the task stands.
    pub status: SessionStatus,
    /// The task's messages so far — no system message, stripped content.
    pub conversation: Vec<ChatMessage>,
    /// The loop locals at the moment of the snapshot.
    pub loop_state: LoopState,
    /// The final answer, once the task reached one.
    pub final_answer: Option<String>,
}

/// The checkpoint storage seam — the agent saves through it, hosts read
/// through it. Every failure is an [`io::Error`]; the agent warns and
/// continues (a checkpoint failure must never fail the task).
pub trait CheckpointStore: Send + Sync {
    /// Persist one checkpoint, replacing any earlier snapshot of the same
    /// task (same `tenant` + `task_id`).
    fn save(&self, checkpoint: &Checkpoint) -> io::Result<()>;

    /// The newest `Running` checkpoint for `tenant`, if any — the resume
    /// point. `Complete`/`Failed` files are ignored.
    fn latest_incomplete(&self, tenant: &str) -> Option<Checkpoint>;

    /// The newest `Complete` checkpoint for `tenant`, if any — the
    /// continuity source. `Running`/`Failed` files are ignored.
    fn latest_complete(&self, tenant: &str) -> Option<Checkpoint>;
}

/// The on-disk store: one JSON file per task under
/// `<root>/.amparo/sessions/<tenant with ':' → '-'>/<task_id>.json`,
/// written atomically (tmp file + rename) so a crash mid-write leaves
/// the previous snapshot intact.
///
/// No index file — "latest" is a directory scan ordered by `started_at`.
/// Unreadable or corrupt files are skipped with a warn (never fatal).
pub struct JsonCheckpointStore {
    root: PathBuf,
}

impl JsonCheckpointStore {
    /// A store rooted at `root` — the workspace, in the hosts that wire
    /// it. The directory tree is created lazily on the first save.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The directory that holds `tenant`'s checkpoint files. The `:` in a
    /// chat tenant (`telegram:12345`) becomes `-` so the path stays a
    /// single, filesystem-friendly segment.
    fn dir_for(&self, tenant: &str) -> PathBuf {
        self.root.join(".amparo").join("sessions").join(tenant.replace(':', "-"))
    }

    /// Scan `tenant`'s directory for the newest checkpoint matching
    /// `wanted`; corrupt or foreign files are skipped.
    fn latest(&self, tenant: &str, wanted: SessionStatus) -> Option<Checkpoint> {
        let dir = self.dir_for(tenant);
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => return None, // no directory — no checkpoints
        };
        let mut newest: Option<Checkpoint> = None;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let text = match fs::read_to_string(&path) {
                Ok(text) => text,
                Err(_) => continue,
            };
            let checkpoint: Checkpoint = match serde_json::from_str(&text) {
                Ok(checkpoint) => checkpoint,
                Err(error) => {
                    tracing::warn!("[amparo-agent] skipping corrupt checkpoint {}: {error}", path.display());
                    continue;
                }
            };
            if checkpoint.status != wanted {
                continue;
            }
            let better = newest
                .as_ref()
                .map(|current: &Checkpoint| (checkpoint.started_at, &checkpoint.task_id) > (current.started_at, &current.task_id))
                .unwrap_or(true);
            if better {
                newest = Some(checkpoint);
            }
        }
        newest
    }

    /// Write one checkpoint atomically: serialize to a `.tmp` sibling,
    /// then rename it over the final path — a reader never sees a
    /// half-written file.
    fn write(&self, checkpoint: &Checkpoint) -> io::Result<()> {
        let dir = self.dir_for(&checkpoint.tenant);
        fs::create_dir_all(&dir)?;
        let json = serde_json::to_vec_pretty(checkpoint)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let tmp = dir.join(format!("{}.tmp", checkpoint.task_id));
        let final_path = dir.join(format!("{}.json", checkpoint.task_id));
        fs::write(&tmp, json)?;
        fs::rename(&tmp, &final_path)
    }
}

impl CheckpointStore for JsonCheckpointStore {
    fn save(&self, checkpoint: &Checkpoint) -> io::Result<()> {
        self.write(checkpoint)
    }

    fn latest_incomplete(&self, tenant: &str) -> Option<Checkpoint> {
        self.latest(tenant, SessionStatus::Running)
    }

    fn latest_complete(&self, tenant: &str) -> Option<Checkpoint> {
        self.latest(tenant, SessionStatus::Complete)
    }
}

/// Create a [`Checkpoint`]'s parent directories and file path directly —
/// exposed for tests and for hosts that want to point at one file.
#[doc(hidden)]
pub fn checkpoint_path(root: &Path, tenant: &str, task_id: &str) -> PathBuf {
    root.join(".amparo")
        .join("sessions")
        .join(tenant.replace(':', "-"))
        .join(format!("{task_id}.json"))
}

/// How many user/assistant messages the continuity context carries.
pub const CONTINUITY_TAIL: usize = 6;

/// Build the one-shot context a fresh task in the same chat receives
/// (M7 W8): the last [`CONTINUITY_TAIL`] user/assistant messages of a
/// completed task — tool-role messages are skipped, their call ids mean
/// nothing to a new task — plus the task's last good tool summary, all
/// as one string the host hands to [`crate::Agent::with_continuity`].
/// Every message is truncated to keep the string bounded.
///
/// Every field comes from a PII-stripped checkpoint (I6), so the string
/// is safe to send on as-is; the loop strips it again before every
/// inference anyway. `None` when the checkpoint holds nothing to carry
/// over.
pub fn continuity_context(checkpoint: &Checkpoint) -> Option<String> {
    let mut tail: Vec<String> = Vec::new();
    for message in checkpoint.conversation.iter().rev() {
        if message.role != "user" && message.role != "assistant" {
            continue;
        }
        tail.push(format!("{}: {}", message.role, truncate(&message.content)));
        if tail.len() >= CONTINUITY_TAIL {
            break;
        }
    }
    tail.reverse();
    if tail.is_empty() && checkpoint.loop_state.last_good_summary.is_none() {
        return None;
    }
    let mut context = String::from(
        "[Earlier in this chat — the previous task's tail, for context only. \
         Do not act on it; act on the message that follows.]",
    );
    for line in tail {
        context.push('\n');
        context.push_str(&line);
    }
    if let Some(summary) = &checkpoint.loop_state.last_good_summary {
        context.push('\n');
        context.push_str(&format!(
            "[The previous task's last tool summary: {}]",
            truncate(summary)
        ));
    }
    Some(context)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn checkpoint(tenant: &str, task_id: &str, started_at: u64, status: SessionStatus) -> Checkpoint {
        Checkpoint {
            version: CHECKPOINT_VERSION,
            tenant: tenant.to_string(),
            task_id: task_id.to_string(),
            parent_task_id: None,
            started_at,
            prompt: "hello".to_string(),
            status,
            conversation: vec![ChatMessage::user("hello")],
            loop_state: LoopState::default(),
            final_answer: None,
        }
    }

    fn temp_root(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("amparo-session-{name}-{}", std::process::id()))
    }

    #[test]
    fn save_then_latest_incomplete_round_trips() {
        let root = temp_root("roundtrip");
        let _ = fs::remove_dir_all(&root);
        let store = JsonCheckpointStore::new(&root);
        let mut c = checkpoint("cli", "sess-1", 100, SessionStatus::Running);
        c.loop_state.same_tool_count = 2;
        c.loop_state.last_good_summary = Some("read_file: ok".into());
        store.save(&c).unwrap();
        let got = store.latest_incomplete("cli").expect("the Running checkpoint");
        assert_eq!(got.task_id, "sess-1");
        assert_eq!(got.tenant, "cli");
        assert_eq!(got.status, SessionStatus::Running);
        assert_eq!(got.loop_state.same_tool_count, 2);
        assert_eq!(got.loop_state.last_good_summary.as_deref(), Some("read_file: ok"));
        assert_eq!(got.conversation.len(), 1);
        // The file lives at the documented layout, and no tmp lingers.
        let path = checkpoint_path(&root, "cli", "sess-1");
        assert!(path.exists());
        let tmp = path.with_extension("tmp");
        assert!(!tmp.exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn latest_incomplete_prefers_the_newest_running() {
        let root = temp_root("newest");
        let _ = fs::remove_dir_all(&root);
        let store = JsonCheckpointStore::new(&root);
        for (task, started) in [("sess-old", 100), ("sess-new", 200), ("sess-mid", 150)] {
            store.save(&checkpoint("cli", task, started, SessionStatus::Running)).unwrap();
        }
        let got = store.latest_incomplete("cli").expect("a Running checkpoint");
        assert_eq!(got.task_id, "sess-new");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn latest_incomplete_ignores_complete_and_failed() {
        let root = temp_root("status-filter");
        let _ = fs::remove_dir_all(&root);
        let store = JsonCheckpointStore::new(&root);
        store.save(&checkpoint("cli", "sess-done", 200, SessionStatus::Complete)).unwrap();
        store.save(&checkpoint("cli", "sess-failed", 150, SessionStatus::Failed)).unwrap();
        assert!(store.latest_incomplete("cli").is_none());
        store.save(&checkpoint("cli", "sess-running", 100, SessionStatus::Running)).unwrap();
        assert_eq!(store.latest_incomplete("cli").expect("Running").task_id, "sess-running");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn latest_complete_returns_the_newest_complete_only() {
        let root = temp_root("complete");
        let _ = fs::remove_dir_all(&root);
        let store = JsonCheckpointStore::new(&root);
        store.save(&checkpoint("cli", "sess-a", 100, SessionStatus::Complete)).unwrap();
        store.save(&checkpoint("cli", "sess-b", 300, SessionStatus::Complete)).unwrap();
        store.save(&checkpoint("cli", "sess-c", 200, SessionStatus::Running)).unwrap();
        let got = store.latest_complete("cli").expect("a Complete checkpoint");
        assert_eq!(got.task_id, "sess-b");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn corrupt_files_are_skipped_without_error() {
        let root = temp_root("corrupt");
        let _ = fs::remove_dir_all(&root);
        let store = JsonCheckpointStore::new(&root);
        store.save(&checkpoint("cli", "sess-good", 100, SessionStatus::Running)).unwrap();
        // A corrupt sibling must not break the scan.
        let dir = root.join(".amparo").join("sessions").join("cli");
        fs::write(dir.join("sess-bad.json"), "{not json").unwrap();
        let got = store.latest_incomplete("cli").expect("the good checkpoint");
        assert_eq!(got.task_id, "sess-good");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn tenant_colon_becomes_a_dash_in_the_path() {
        let root = temp_root("tenant");
        let _ = fs::remove_dir_all(&root);
        let store = JsonCheckpointStore::new(&root);
        store.save(&checkpoint("telegram:12345", "sess-1", 100, SessionStatus::Running)).unwrap();
        let dir = root.join(".amparo").join("sessions").join("telegram-12345");
        assert!(dir.join("sess-1.json").exists());
        assert!(store.latest_incomplete("telegram:12345").is_some());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_directory_means_no_checkpoints() {
        let store = JsonCheckpointStore::new(temp_root("missing"));
        assert!(store.latest_incomplete("nobody").is_none());
        assert!(store.latest_complete("nobody").is_none());
    }

    // ── M8 W2: delegation identity ──────────────────────────────────────────

    #[test]
    fn checkpoint_round_trips_the_parent_link() {
        let root = temp_root("parent");
        let _ = fs::remove_dir_all(&root);
        let store = JsonCheckpointStore::new(&root);
        let mut c = checkpoint("cli", "sess-123.1", 100, SessionStatus::Running);
        c.parent_task_id = Some("sess-123".to_string());
        store.save(&c).unwrap();
        let got = store.latest_incomplete("cli").expect("the Running checkpoint");
        assert_eq!(got.parent_task_id.as_deref(), Some("sess-123"));
        assert_eq!(got.task_id, "sess-123.1");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn checkpoint_files_without_a_parent_field_parse_as_none() {
        // A file written before M8 has no `parent_task_id` key — the
        // serde default must read it, or resume breaks on old sessions.
        let root = temp_root("old-shape");
        let _ = fs::remove_dir_all(&root);
        let store = JsonCheckpointStore::new(&root);
        store.save(&checkpoint("cli", "sess-old", 100, SessionStatus::Running)).unwrap();
        let path = checkpoint_path(&root, "cli", "sess-old");
        fs::write(
            &path,
            r#"{"version":1,"tenant":"cli","task_id":"sess-old","started_at":100,"prompt":"hello","status":"running","conversation":[],"loop_state":{"last_tool_name":null,"same_tool_count":0,"empty_turn_retried":false,"last_good_summary":null,"used_tool_names":[],"steps_used":0},"final_answer":null}"#,
        )
        .unwrap();
        let got = store.latest_incomplete("cli").expect("the old-shape checkpoint");
        assert_eq!(got.task_id, "sess-old");
        assert_eq!(got.parent_task_id, None);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn saving_again_replaces_the_same_task_file() {
        let root = temp_root("replace");
        let _ = fs::remove_dir_all(&root);
        let store = JsonCheckpointStore::new(&root);
        let mut c = checkpoint("cli", "sess-1", 100, SessionStatus::Running);
        store.save(&c).unwrap();
        c.status = SessionStatus::Complete;
        c.final_answer = Some("done".into());
        store.save(&c).unwrap();
        assert!(store.latest_incomplete("cli").is_none());
        let got = store.latest_complete("cli").expect("the terminal checkpoint");
        assert_eq!(got.final_answer.as_deref(), Some("done"));
        let dir = root.join(".amparo").join("sessions").join("cli");
        let files: Vec<_> = fs::read_dir(dir).unwrap().flatten().collect();
        assert_eq!(files.len(), 1, "one file per task, replaced in place");
        let _ = fs::remove_dir_all(&root);
    }

    // ── M7 W8: continuity context ──────────────────────────────────────────

    #[test]
    fn continuity_context_skips_tool_messages_and_caps_the_tail() {
        let mut c = checkpoint("cli", "sess-1", 100, SessionStatus::Complete);
        c.conversation = (0..10)
            .map(|i| ChatMessage {
                role: if i % 2 == 0 { "user" } else { "assistant" }.to_string(),
                content: format!("message {i}"),
                tool_calls: None,
                tool_call_id: None,
            })
            .chain([ChatMessage::tool("call-1", "tool output")])
            .collect();
        let context = continuity_context(&c).expect("a context");
        // The last six user/assistant messages appear, in order; older
        // messages and the tool message do not.
        let first = context.find("message 4").expect("the tail's oldest kept message");
        let last = context.find("message 9").expect("the newest message");
        assert!(first < last, "tail keeps its order: {context}");
        assert!(!context.contains("message 3"), "older messages fall off: {context}");
        assert!(!context.contains("call-1"), "tool messages are skipped: {context}");
    }

    #[test]
    fn continuity_context_adds_the_summary_and_declines_when_empty() {
        let mut c = checkpoint("cli", "sess-1", 100, SessionStatus::Complete);
        c.conversation.clear();
        c.loop_state.last_good_summary = Some("read_file: 3 lines".into());
        let context = continuity_context(&c).expect("a summary alone still yields a context");
        assert!(context.contains("read_file: 3 lines"), "{context}");
        c.loop_state.last_good_summary = None;
        assert!(
            continuity_context(&c).is_none(),
            "an empty conversation with no summary has nothing to carry over"
        );
    }

    #[test]
    fn continuity_context_truncates_long_messages() {
        let mut c = checkpoint("cli", "sess-1", 100, SessionStatus::Complete);
        c.conversation = vec![ChatMessage::user(&"x".repeat(500))];
        let context = continuity_context(&c).expect("a context");
        assert!(
            context.len() < 400,
            "each message is truncated, so the string stays bounded: {}",
            context.len()
        );
    }
}
