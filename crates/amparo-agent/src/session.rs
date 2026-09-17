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
//!
//! Famulus OS Track 2: the store also carries the task's lifecycle
//! journal — one append-only JSONL file per task
//! (`<task_id>.journal.jsonl`) holding the same marker names the kernel
//! journals (`agent_started`, `agent_suspended`, `agent_resumed`,
//! `agent_killed`, `budget_exhausted`). A suspend flips the snapshot to
//! [`SessionStatus::Suspended`] and journals the marker (snapshot
//! pointer + journal, like the kernel's `AgentHandle`); a resume
//! reproduces the journal tail after the last suspend; a kill is a
//! terminal `agent_killed` marker with the task's file retained for
//! audit. The journal is the source of truth for lifecycle transitions —
//! checkpoints are snapshots, *not* Engram memories (the
//! session-boundaries sidecar decision stands).

use crate::events::truncate;
use amparo_inference::ChatMessage;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// The format version of a [`Checkpoint`] file. Bump when the shape
/// changes; readers skip files with an unknown version.
pub const CHECKPOINT_VERSION: u32 = 1;

/// Lifecycle event names shared with the Famulus OS kernel (Track 2) —
/// the same marker strings the kernel's journal uses, so one reader can
/// consume amparo sessions and kernel agents alike.
pub const AGENT_EVENT_STARTED: &str = "agent_started";
/// Journaled by [`JsonCheckpointStore::suspend`] — the snapshot pointer.
pub const AGENT_EVENT_SUSPENDED: &str = "agent_suspended";
/// Journaled by [`crate::Agent::resume`] — carries the reproduced tail.
pub const AGENT_EVENT_RESUMED: &str = "agent_resumed";
/// Journaled by [`JsonCheckpointStore::kill`] — terminal, id retained.
pub const AGENT_EVENT_KILLED: &str = "agent_killed";
/// Journaled at the max-steps hard stop — exhaustion, not a crash.
pub const AGENT_EVENT_BUDGET_EXHAUSTED: &str = "budget_exhausted";

/// How a checkpointed task stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    /// The task is mid-loop — a resume point.
    Running,
    /// The task was parked mid-loop by an explicit suspend (Famulus OS
    /// Track 2). A `Suspended` checkpoint is a resume point exactly like
    /// a `Running` one — the journal's `agent_suspended` marker records
    /// the snapshot pointer.
    Suspended,
    /// The task finished with a final answer.
    Complete,
    /// The task ended without one.
    Failed,
    /// The task was terminated by an operator kill (Track 2) — terminal
    /// like `Complete`/`Failed`, but the `agent_killed` marker
    /// distinguishes it in the journal and the file is retained for
    /// audit.
    Killed,
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
    /// Tool names that actually executed this task (M9 W1 QC) — the
    /// requested set minus gate blocks, plus skill steps that ran. A
    /// resume restores it so the QC council never flags pre-resume
    /// executions as unexecuted.
    #[serde(default)]
    pub executed_tools: Vec<String>,
    /// Tool calls that actually executed (M9 W1 QC) — the council's
    /// evidence rule compares this against tool results in context.
    #[serde(default)]
    pub executed_calls: usize,
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

/// One lifecycle transition in a task's journal (Famulus OS Track 2).
///
/// Appended one line per event to `<task_id>.journal.jsonl` next to the
/// checkpoint file. The journal is the source of truth for lifecycle
/// transitions — the checkpoint is the snapshot it annotates.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LifecycleEvent {
    /// 1-based position within the task's journal.
    pub sequence: u64,
    /// One of the [`AGENT_EVENT_*`] marker names.
    pub event_type: String,
    /// The task the marker belongs to — the agent handle; maps to the
    /// kernel's `agent_id`.
    pub task_id: String,
    /// Marker-specific extras: a suspend pointer carries `steps_used`,
    /// exhaustion carries `steps_used`/`max_steps`, a resume carries the
    /// reproduced `tail_event_types`. Free-form so marker shapes can
    /// evolve without a schema bump.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<serde_json::Value>,
    /// Unix seconds when the marker was appended.
    pub created_at: u64,
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

    /// Every `Running` checkpoint for `tenant`, newest first — the
    /// resume picker's list. `Suspended` tasks are resume points too
    /// (Track 2), so they appear here as well. The default returns at
    /// most one entry ([`CheckpointStore::latest_incomplete`]); stores
    /// that can enumerate their own files (the on-disk store) override
    /// it.
    fn list_incomplete(&self, tenant: &str) -> Vec<Checkpoint> {
        self.latest_incomplete(tenant).into_iter().collect()
    }

    /// Append one lifecycle marker to the task's journal (Famulus OS
    /// Track 2). Returns the marker's 1-based journal sequence, or
    /// `None` when this store has no journal surface — the default, so
    /// in-memory doubles skip journaling exactly like a missing store
    /// skips checkpoints.
    fn journal_event(
        &self,
        tenant: &str,
        task_id: &str,
        event_type: &str,
        payload: Option<serde_json::Value>,
    ) -> io::Result<Option<u64>> {
        let _ = (tenant, task_id, event_type, payload);
        Ok(None)
    }

    /// The task's lifecycle journal, oldest first (Track 2). Empty for
    /// stores without a journal surface.
    fn read_journal(&self, tenant: &str, task_id: &str) -> Vec<LifecycleEvent> {
        let _ = (tenant, task_id);
        Vec::new()
    }
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
        self.root
            .join(".amparo")
            .join("sessions")
            .join(tenant.replace(':', "-"))
    }

    /// Scan `tenant`'s directory for the newest checkpoint with one of
    /// the `wanted` statuses; corrupt or foreign files are skipped.
    fn latest(&self, tenant: &str, wanted: &[SessionStatus]) -> Option<Checkpoint> {
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
                    tracing::warn!(
                        "[amparo-agent] skipping corrupt checkpoint {}: {error}",
                        path.display()
                    );
                    continue;
                }
            };
            if !wanted.contains(&checkpoint.status) {
                continue;
            }
            let better = newest
                .as_ref()
                .map(|current: &Checkpoint| {
                    (checkpoint.started_at, &checkpoint.task_id)
                        > (current.started_at, &current.task_id)
                })
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
        let created = !dir.exists();
        fs::create_dir_all(&dir)?;
        // Harden only a directory this call created — a pre-existing
        // parent is not ours to re-permission; owner-only before any
        // content lands (audit 2026-08-31 MED-6).
        if created {
            amparo_privacy::perms::owner_only(&dir)?;
        }
        let json = serde_json::to_vec_pretty(checkpoint)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let tmp = dir.join(format!("{}.tmp", checkpoint.task_id));
        let final_path = dir.join(format!("{}.json", checkpoint.task_id));
        fs::write(&tmp, json)?;
        amparo_privacy::perms::owner_only(&tmp)?;
        fs::rename(&tmp, &final_path)?;
        Ok(())
    }

    /// The task's journal file: `<task_id>.journal.jsonl` next to the
    /// checkpoint (Famulus OS Track 2).
    fn journal_file(&self, tenant: &str, task_id: &str) -> PathBuf {
        self.dir_for(tenant).join(format!("{task_id}.journal.jsonl"))
    }

    /// Append one marker: count the lines already there (each line is
    /// one event — a corrupt tail still counts, so sequences stay
    /// monotonic), write the serialized event plus a newline with
    /// `O_APPEND`, and harden the file owner-only on creation. A marker
    /// is one short line, so a crash mid-append corrupts at most the
    /// tail line — readers skip it.
    fn append_journal(
        &self,
        tenant: &str,
        task_id: &str,
        event_type: &str,
        payload: Option<serde_json::Value>,
    ) -> io::Result<u64> {
        use std::io::Write;

        let path = self.journal_file(tenant, task_id);
        let dir = self.dir_for(tenant);
        let created_dir = !dir.exists();
        fs::create_dir_all(&dir)?;
        // Harden only a directory this call created — a pre-existing
        // parent is not ours to re-permission (audit 2026-08-31 MED-6),
        // same rule as the checkpoint writer.
        if created_dir {
            amparo_privacy::perms::owner_only(&dir)?;
        }
        let sequence = match fs::read_to_string(&path) {
            Ok(text) => text.lines().count() as u64 + 1,
            Err(error) if error.kind() == io::ErrorKind::NotFound => 1,
            Err(error) => return Err(error),
        };
        let event = LifecycleEvent {
            sequence,
            event_type: event_type.to_string(),
            task_id: task_id.to_string(),
            payload,
            created_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        };
        let mut line = serde_json::to_vec(&event)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        line.push(b'\n');
        let created = !path.exists();
        let mut file = fs::OpenOptions::new().create(true).append(true).open(&path)?;
        file.write_all(&line)?;
        if created {
            amparo_privacy::perms::owner_only(&path)?;
        }
        Ok(sequence)
    }

    /// Read the task's journal, oldest first. A corrupt or truncated
    /// line (a crash mid-append) is skipped with a warn, never fatal.
    fn read_journal_file(&self, tenant: &str, task_id: &str) -> Vec<LifecycleEvent> {
        let path = self.journal_file(tenant, task_id);
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(_) => return Vec::new(), // no journal file — no events
        };
        let mut events = Vec::new();
        for line in text.lines() {
            if line.is_empty() {
                continue;
            }
            match serde_json::from_str::<LifecycleEvent>(line) {
                Ok(event) => events.push(event),
                Err(error) => {
                    tracing::warn!(
                        "[amparo-agent] skipping corrupt journal line {}: {error}",
                        path.display()
                    );
                }
            }
        }
        events
    }

    /// Suspend a task mid-loop (Famulus OS Track 2): the task's
    /// checkpoint is re-saved with status `Suspended` — the snapshot
    /// pointer — and the `agent_suspended` marker is journaled carrying
    /// the snapshot's `steps_used`. A suspend of a task with no
    /// checkpoint fails with [`io::ErrorKind::NotFound`], like the
    /// kernel's unknown-agent error.
    pub fn suspend(&self, tenant: &str, task_id: &str) -> io::Result<u64> {
        let path = checkpoint_path(&self.root, tenant, task_id);
        let text = fs::read_to_string(&path)?; // NotFound propagates
        let mut checkpoint: Checkpoint = serde_json::from_str(&text)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        checkpoint.status = SessionStatus::Suspended;
        self.write(&checkpoint)?;
        self.append_journal(
            tenant,
            task_id,
            AGENT_EVENT_SUSPENDED,
            Some(serde_json::json!({ "steps_used": checkpoint.loop_state.steps_used })),
        )
    }

    /// Kill a task (Famulus OS Track 2): journal the terminal
    /// `agent_killed` marker and re-save the checkpoint with status
    /// `Killed`, so the snapshot stays on disk for audit — the id is
    /// never reclaimed. Idempotent: a task whose journal already ends
    /// in `agent_killed` is already dead and the call returns `None`
    /// without appending. A kill of a task with no checkpoint fails
    /// with [`io::ErrorKind::NotFound`], like the kernel's
    /// unknown-agent error.
    pub fn kill(&self, tenant: &str, task_id: &str) -> io::Result<Option<u64>> {
        let path = checkpoint_path(&self.root, tenant, task_id);
        let text = fs::read_to_string(&path)?; // NotFound propagates
        if self
            .read_journal_file(tenant, task_id)
            .last()
            .is_some_and(|event| event.event_type == AGENT_EVENT_KILLED)
        {
            return Ok(None); // already dead — no marker appended
        }
        let mut checkpoint: Checkpoint = serde_json::from_str(&text)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        checkpoint.status = SessionStatus::Killed;
        self.write(&checkpoint)?;
        self.append_journal(tenant, task_id, AGENT_EVENT_KILLED, None).map(Some)
    }
}

impl CheckpointStore for JsonCheckpointStore {
    fn save(&self, checkpoint: &Checkpoint) -> io::Result<()> {
        self.write(checkpoint)
    }

    fn latest_incomplete(&self, tenant: &str) -> Option<Checkpoint> {
        // Suspended tasks are resume points too (Track 2) — the resume
        // picker lists them alongside Running ones.
        self.latest(
            tenant,
            &[SessionStatus::Running, SessionStatus::Suspended],
        )
    }

    fn latest_complete(&self, tenant: &str) -> Option<Checkpoint> {
        self.latest(tenant, &[SessionStatus::Complete])
    }

    fn list_incomplete(&self, tenant: &str) -> Vec<Checkpoint> {
        let dir = self.dir_for(tenant);
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => return Vec::new(), // no directory — no checkpoints
        };
        let mut running: Vec<Checkpoint> = Vec::new();
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
                    tracing::warn!(
                        "[amparo-agent] skipping corrupt checkpoint {}: {error}",
                        path.display()
                    );
                    continue;
                }
            };
            if matches!(
                checkpoint.status,
                SessionStatus::Running | SessionStatus::Suspended
            ) {
                running.push(checkpoint);
            }
        }
        // Newest first — the resume picker lists the most recent on top.
        running.sort_by(|a: &Checkpoint, b: &Checkpoint| {
            (b.started_at, &b.task_id).cmp(&(a.started_at, &a.task_id))
        });
        running
    }

    fn journal_event(
        &self,
        tenant: &str,
        task_id: &str,
        event_type: &str,
        payload: Option<serde_json::Value>,
    ) -> io::Result<Option<u64>> {
        self.append_journal(tenant, task_id, event_type, payload)
            .map(Some)
    }

    fn read_journal(&self, tenant: &str, task_id: &str) -> Vec<LifecycleEvent> {
        self.read_journal_file(tenant, task_id)
    }
}

impl JsonCheckpointStore {
    /// The newest `Killed` checkpoint for `tenant`, if any — the audit
    /// path for terminated tasks (Famulus OS Track 2): a killed task's
    /// snapshot stays readable, its id never reclaimed.
    pub fn latest_killed(&self, tenant: &str) -> Option<Checkpoint> {
        self.latest(tenant, &[SessionStatus::Killed])
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

/// The journal tail a resume reproduces (Famulus OS Track 2): the event
/// types from the last `agent_suspended` marker onward — the marker
/// trails the snapshot pointer, exactly like the kernel's journal, where
/// a suspend captures the checkpoint first and the marker lands after
/// it, inside the replay window. The kernel's resume reports the same
/// tail from its event log; here the journal is the log. When the task
/// was never suspended (a crash-resume from a `Running` snapshot), the
/// whole journal is the tail — the marker list a reader replays to reach
/// the current state.
pub fn tail_after_last_suspend(events: &[LifecycleEvent]) -> Vec<String> {
    let last_suspend = events
        .iter()
        .rposition(|event| event.event_type == AGENT_EVENT_SUSPENDED);
    events[last_suspend.map_or(0, |index| index)..]
        .iter()
        .map(|event| event.event_type.clone())
        .collect()
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

    fn checkpoint(
        tenant: &str,
        task_id: &str,
        started_at: u64,
        status: SessionStatus,
    ) -> Checkpoint {
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
        let got = store
            .latest_incomplete("cli")
            .expect("the Running checkpoint");
        assert_eq!(got.task_id, "sess-1");
        assert_eq!(got.tenant, "cli");
        assert_eq!(got.status, SessionStatus::Running);
        assert_eq!(got.loop_state.same_tool_count, 2);
        assert_eq!(
            got.loop_state.last_good_summary.as_deref(),
            Some("read_file: ok")
        );
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
            store
                .save(&checkpoint("cli", task, started, SessionStatus::Running))
                .unwrap();
        }
        let got = store
            .latest_incomplete("cli")
            .expect("a Running checkpoint");
        assert_eq!(got.task_id, "sess-new");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn latest_incomplete_ignores_complete_and_failed() {
        let root = temp_root("status-filter");
        let _ = fs::remove_dir_all(&root);
        let store = JsonCheckpointStore::new(&root);
        store
            .save(&checkpoint(
                "cli",
                "sess-done",
                200,
                SessionStatus::Complete,
            ))
            .unwrap();
        store
            .save(&checkpoint(
                "cli",
                "sess-failed",
                150,
                SessionStatus::Failed,
            ))
            .unwrap();
        assert!(store.latest_incomplete("cli").is_none());
        store
            .save(&checkpoint(
                "cli",
                "sess-running",
                100,
                SessionStatus::Running,
            ))
            .unwrap();
        assert_eq!(
            store.latest_incomplete("cli").expect("Running").task_id,
            "sess-running"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn latest_complete_returns_the_newest_complete_only() {
        let root = temp_root("complete");
        let _ = fs::remove_dir_all(&root);
        let store = JsonCheckpointStore::new(&root);
        store
            .save(&checkpoint("cli", "sess-a", 100, SessionStatus::Complete))
            .unwrap();
        store
            .save(&checkpoint("cli", "sess-b", 300, SessionStatus::Complete))
            .unwrap();
        store
            .save(&checkpoint("cli", "sess-c", 200, SessionStatus::Running))
            .unwrap();
        let got = store.latest_complete("cli").expect("a Complete checkpoint");
        assert_eq!(got.task_id, "sess-b");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn corrupt_files_are_skipped_without_error() {
        let root = temp_root("corrupt");
        let _ = fs::remove_dir_all(&root);
        let store = JsonCheckpointStore::new(&root);
        store
            .save(&checkpoint("cli", "sess-good", 100, SessionStatus::Running))
            .unwrap();
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
        store
            .save(&checkpoint(
                "telegram:12345",
                "sess-1",
                100,
                SessionStatus::Running,
            ))
            .unwrap();
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

    #[cfg(unix)]
    #[test]
    fn checkpoints_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let root = temp_root("perms");
        let _ = fs::remove_dir_all(&root);
        let store = JsonCheckpointStore::new(&root);
        store
            .save(&checkpoint("cli", "sess-p", 100, SessionStatus::Running))
            .unwrap();
        let dir = root.join(".amparo").join("sessions").join("cli");
        assert_eq!(
            fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700,
            "sessions dir must be owner-only"
        );
        assert_eq!(
            fs::metadata(dir.join("sess-p.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600,
            "checkpoint file must be owner-only"
        );
        let _ = fs::remove_dir_all(&root);
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
        let got = store
            .latest_incomplete("cli")
            .expect("the Running checkpoint");
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
        store
            .save(&checkpoint("cli", "sess-old", 100, SessionStatus::Running))
            .unwrap();
        let path = checkpoint_path(&root, "cli", "sess-old");
        fs::write(
            &path,
            r#"{"version":1,"tenant":"cli","task_id":"sess-old","started_at":100,"prompt":"hello","status":"running","conversation":[],"loop_state":{"last_tool_name":null,"same_tool_count":0,"empty_turn_retried":false,"last_good_summary":null,"used_tool_names":[],"steps_used":0},"final_answer":null}"#,
        )
        .unwrap();
        let got = store
            .latest_incomplete("cli")
            .expect("the old-shape checkpoint");
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
        let got = store
            .latest_complete("cli")
            .expect("the terminal checkpoint");
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
                reasoning_content: None,
            })
            .chain([ChatMessage::tool("call-1", "tool output")])
            .collect();
        let context = continuity_context(&c).expect("a context");
        // The last six user/assistant messages appear, in order; older
        // messages and the tool message do not.
        let first = context
            .find("message 4")
            .expect("the tail's oldest kept message");
        let last = context.find("message 9").expect("the newest message");
        assert!(first < last, "tail keeps its order: {context}");
        assert!(
            !context.contains("message 3"),
            "older messages fall off: {context}"
        );
        assert!(
            !context.contains("call-1"),
            "tool messages are skipped: {context}"
        );
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

    // ── Famulus OS Track 2: lifecycle journal, suspend, kill ────────────────

    fn lifecycle(sequence: u64, event_type: &str) -> LifecycleEvent {
        LifecycleEvent {
            sequence,
            event_type: event_type.to_string(),
            task_id: "sess-1".to_string(),
            payload: None,
            created_at: 100,
        }
    }

    #[test]
    fn journal_appends_in_order_and_reads_back() {
        let root = temp_root("journal");
        let _ = fs::remove_dir_all(&root);
        let store = JsonCheckpointStore::new(&root);
        let first = store
            .journal_event(
                "cli",
                "sess-1",
                AGENT_EVENT_STARTED,
                Some(serde_json::json!({ "steps_total": 12 })),
            )
            .unwrap()
            .expect("a journaled store returns the sequence");
        assert_eq!(first, 1);
        store
            .journal_event("cli", "sess-1", AGENT_EVENT_SUSPENDED, None)
            .unwrap();
        store
            .journal_event("cli", "sess-1", AGENT_EVENT_RESUMED, None)
            .unwrap();

        let events = store.read_journal("cli", "sess-1");
        assert_eq!(
            events
                .iter()
                .map(|event| event.sequence)
                .collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert_eq!(
            events
                .iter()
                .map(|event| event.event_type.as_str())
                .collect::<Vec<_>>(),
            vec![AGENT_EVENT_STARTED, AGENT_EVENT_SUSPENDED, AGENT_EVENT_RESUMED]
        );
        assert_eq!(
            events[0].payload.as_ref().and_then(|p| p.get("steps_total")),
            Some(&serde_json::json!(12))
        );
        assert_eq!(events[0].task_id, "sess-1");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn suspend_flips_the_checkpoint_and_journals_the_pointer() {
        let root = temp_root("suspend");
        let _ = fs::remove_dir_all(&root);
        let store = JsonCheckpointStore::new(&root);
        let mut c = checkpoint("cli", "sess-1", 100, SessionStatus::Running);
        c.loop_state.steps_used = 2;
        store.save(&c).unwrap();

        let sequence = store.suspend("cli", "sess-1").unwrap();
        assert_eq!(sequence, 1);

        // The parked snapshot is a resume point — with the suspend status
        // visible, not a silent Running rewrite.
        let got = store.latest_incomplete("cli").expect("a resume point");
        assert_eq!(got.task_id, "sess-1");
        assert_eq!(got.status, SessionStatus::Suspended);
        let journal = store.read_journal("cli", "sess-1");
        assert_eq!(journal.len(), 1);
        assert_eq!(journal[0].event_type, AGENT_EVENT_SUSPENDED);
        assert_eq!(
            journal[0].payload.as_ref().and_then(|p| p.get("steps_used")),
            Some(&serde_json::json!(2)),
            "the marker carries the snapshot pointer"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn suspend_of_an_unknown_task_errors() {
        let store = JsonCheckpointStore::new(temp_root("suspend-missing"));
        let error = store
            .suspend("cli", "nobody")
            .expect_err("an unknown task cannot suspend");
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn kill_is_terminal_idempotent_and_retains_the_snapshot() {
        let root = temp_root("kill");
        let _ = fs::remove_dir_all(&root);
        let store = JsonCheckpointStore::new(&root);
        store
            .save(&checkpoint("cli", "sess-1", 100, SessionStatus::Running))
            .unwrap();

        let first = store.kill("cli", "sess-1").unwrap().expect("first kill journals");
        assert_eq!(first, 1);
        // Terminal: no resume point remains, the audit snapshot does.
        assert!(store.latest_incomplete("cli").is_none());
        let killed = store.latest_killed("cli").expect("the killed snapshot");
        assert_eq!(killed.status, SessionStatus::Killed);
        assert_eq!(killed.task_id, "sess-1");

        // A second kill is a no-op — no duplicate marker.
        assert!(store.kill("cli", "sess-1").unwrap().is_none());
        let journal = store.read_journal("cli", "sess-1");
        assert_eq!(journal.len(), 1);
        assert_eq!(journal[0].event_type, AGENT_EVENT_KILLED);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn kill_after_a_suspend_keeps_the_whole_trail() {
        let root = temp_root("kill-suspended");
        let _ = fs::remove_dir_all(&root);
        let store = JsonCheckpointStore::new(&root);
        store
            .save(&checkpoint("cli", "sess-1", 100, SessionStatus::Running))
            .unwrap();
        store.suspend("cli", "sess-1").unwrap();
        store.kill("cli", "sess-1").unwrap();

        let journal = store.read_journal("cli", "sess-1");
        assert_eq!(
            journal
                .iter()
                .map(|event| event.event_type.as_str())
                .collect::<Vec<_>>(),
            vec![AGENT_EVENT_SUSPENDED, AGENT_EVENT_KILLED]
        );
        assert!(store.latest_incomplete("cli").is_none());
        assert!(store.latest_killed("cli").is_some());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn kill_of_an_unknown_task_errors() {
        let store = JsonCheckpointStore::new(temp_root("kill-missing"));
        let error = store
            .kill("cli", "nobody")
            .expect_err("an unknown task cannot be killed");
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn a_corrupt_journal_tail_is_skipped_on_read() {
        let root = temp_root("corrupt-journal");
        let _ = fs::remove_dir_all(&root);
        let store = JsonCheckpointStore::new(&root);
        store
            .journal_event("cli", "sess-1", AGENT_EVENT_STARTED, None)
            .unwrap();
        // A crash mid-append leaves a truncated line — it must not hide
        // the valid markers before it.
        let path = root
            .join(".amparo")
            .join("sessions")
            .join("cli")
            .join("sess-1.journal.jsonl");
        let mut text = fs::read_to_string(&path).unwrap();
        text.push_str("{\"sequence\":2,\"event_ty");
        fs::write(&path, text).unwrap();

        let events = store.read_journal("cli", "sess-1");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, AGENT_EVENT_STARTED);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn tail_after_last_suspend_reports_events_after_the_pointer() {
        let events = vec![
            lifecycle(1, AGENT_EVENT_STARTED),
            lifecycle(2, AGENT_EVENT_SUSPENDED),
            lifecycle(3, AGENT_EVENT_RESUMED),
            lifecycle(4, AGENT_EVENT_SUSPENDED),
            lifecycle(5, AGENT_EVENT_BUDGET_EXHAUSTED),
        ];
        // The pointer is the LAST suspend, and the tail starts at the
        // marker itself — it trails the snapshot, like the kernel's
        // journal where the suspend marker lands after the capture.
        assert_eq!(
            tail_after_last_suspend(&events),
            vec![AGENT_EVENT_SUSPENDED, AGENT_EVENT_BUDGET_EXHAUSTED]
        );

        // Never suspended: the whole journal is the tail (crash-resume).
        let no_suspend = vec![lifecycle(1, AGENT_EVENT_STARTED)];
        assert_eq!(
            tail_after_last_suspend(&no_suspend),
            vec![AGENT_EVENT_STARTED]
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_journal_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let root = temp_root("journal-perms");
        let _ = fs::remove_dir_all(&root);
        let store = JsonCheckpointStore::new(&root);
        store
            .journal_event("cli", "sess-1", AGENT_EVENT_STARTED, None)
            .unwrap();
        let path = root
            .join(".amparo")
            .join("sessions")
            .join("cli")
            .join("sess-1.journal.jsonl");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600,
            "journal file must be owner-only"
        );
        let _ = fs::remove_dir_all(&root);
    }
}
