//! The schedule queue (M8 W5) — a persisted promise, not an execution.
//!
//! Scheduling is how a chat task asks for work *later*: the model commits
//! to a concrete RFC 3339 instant, the promise is written PII-stripped to
//! the queue dir, and when the instant arrives the chat driver's ticker
//! re-enters the gate chain as the original requester — the same policy
//! engine, the same human-approval gate, the same ledger and checkpoints.
//! A promise whose instant passes while the host is down (or beyond the
//! grace window) is marked [`ScheduledStatus::Missed`] and the requester
//! is told — fail-closed: late firing would be doing work the operator
//! forgot, and re-scheduling is the operator's call. Executing a
//! schedule is never the model's shortcut: the firing path is the same
//! gate chain a live task gets, and a fire while nobody is present runs
//! to the approval gate and auto-denies on timeout.
//!
//! The queue lives at `<workspace>/.amparo/schedule/<id>.json` — one
//! driver-wide dir, entries tenant-tagged (I2: the firing task resolves
//! the recorded tenant's own parts). The CLI gets inspection, not a
//! daemon: `amparo schedule list|cancel` reads the same dir and never
//! deletes (cancel is a status change).

use amparo_tools::{ToolCall, ToolExecutor, ToolParam, ToolResult, ToolSchema, ToolTrustTier};
use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The registry name of the schedule tool.
pub const SCHEDULE: &str = "schedule";

/// The queue directory under a workspace root: `<root>/.amparo/schedule/`.
pub fn schedule_dir(root: &Path) -> PathBuf {
    root.join(".amparo/schedule")
}

/// How a scheduled promise stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScheduledStatus {
    /// Waiting for its instant (or for a free chat).
    Pending,
    /// Fired: the ticker re-entered the gate chain and the task ran.
    Fired,
    /// Missed: the instant passed beyond the grace window (the host was
    /// down, or the chat stayed busy) — fail-closed, never fired late.
    Missed,
    /// Cancelled by the operator before it fired (`amparo schedule
    /// cancel`).
    Cancelled,
}

impl std::fmt::Display for ScheduledStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pending => write!(f, "pending"),
            Self::Fired => write!(f, "fired"),
            Self::Missed => write!(f, "missed"),
            Self::Cancelled => write!(f, "cancelled"),
        }
    }
}

/// One persisted promise: the task the model committed to, tagged with
/// its requester, to re-enter the gate chain at `at`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduledTask {
    /// The schedule id — `sched-<nanos>-<pid>`.
    pub id: String,
    /// The requester's tenant key — `platform:user_id` (I2: the firing
    /// task resolves the same per-tenant parts as a live message).
    pub tenant: String,
    /// The platform adapter name — where the fired task's approvals and
    /// report are routed.
    pub platform: String,
    /// The chat the promise was made in.
    pub chat_id: String,
    /// The requester's platform user id (the approval gate's owner).
    pub requester: String,
    /// The scheduled task text, PII-stripped at write (I6).
    pub task: String,
    /// The RFC 3339 instant the model committed to.
    pub at: String,
    /// Where the promise stands.
    pub status: ScheduledStatus,
    /// The report head (or the fail-closed note) once fired/missed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
}

/// The pure due-scan — the scheduler's clock seam. `tasks` are the queue
/// contents, `now` is the ticker's clock (a parameter, so tests drive a
/// fire without sleeping), `grace` is how long after its instant a task
/// stays firable (host-down recovery). Returns the indices of the tasks
/// to fire and the tasks to mark missed:
///
/// - a `pending` task whose `at` is in the future → neither;
/// - a `pending` task with `at <= now <= at + grace` → due (fires);
/// - a `pending` task with `now > at + grace` → missed (fail-closed —
///   late firing would be doing work the operator forgot);
/// - a `pending` task whose `at` does not parse → missed (a hand-edited
///   or corrupt time can never cause a fire);
/// - any non-pending task → neither (fired/missed/cancelled are final).
pub fn due_scan(
    tasks: &[ScheduledTask],
    now: DateTime<Utc>,
    grace: Duration,
) -> (Vec<usize>, Vec<usize>) {
    let mut due = Vec::new();
    let mut missed = Vec::new();
    for (index, task) in tasks.iter().enumerate() {
        if task.status != ScheduledStatus::Pending {
            continue;
        }
        let Ok(at) = DateTime::parse_from_rfc3339(&task.at) else {
            missed.push(index);
            continue;
        };
        let at = at.with_timezone(&Utc);
        if now > at + grace {
            missed.push(index);
        } else if at <= now {
            due.push(index);
        }
    }
    (due, missed)
}

/// The schedule store seam — how scheduled promises are persisted.
pub trait ScheduleStore: Send + Sync {
    /// Persist one task. Writes are atomic (tmp file + rename), the M7
    /// checkpoint pattern: a reader never sees a half-written promise.
    fn save(&self, task: &ScheduledTask) -> io::Result<()>;
    /// Load every task in the queue dir, newest file first. Corrupt
    /// entries are skipped with a warn line — one bad file never blocks
    /// the queue.
    fn load_all(&self) -> Vec<ScheduledTask>;
    /// Load one task by id, or `None` when it does not exist (or cannot
    /// be read).
    fn load(&self, id: &str) -> io::Result<Option<ScheduledTask>>;
}

/// The JSON schedule store at `<root>/.amparo/schedule/<id>.json`.
#[derive(Debug, Clone)]
pub struct JsonScheduleStore {
    /// The queue directory.
    dir: PathBuf,
}

impl JsonScheduleStore {
    /// A store rooted at `dir` (the [`schedule_dir`] of a workspace root).
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// The queue directory this store reads and writes.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The on-disk path for one task id.
    fn path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }
}

impl ScheduleStore for JsonScheduleStore {
    fn save(&self, task: &ScheduledTask) -> io::Result<()> {
        let created = !self.dir.exists();
        std::fs::create_dir_all(&self.dir)?;
        // Harden only a directory this call created (audit
        // 2026-08-31 MED-6).
        if created {
            amparo_privacy::perms::owner_only(&self.dir)?;
        }
        let json = serde_json::to_vec(task).map_err(io::Error::other)?;
        let tmp = self.dir.join(format!("{}.tmp", task.id));
        std::fs::write(&tmp, json)?;
        amparo_privacy::perms::owner_only(&tmp)?;
        std::fs::rename(&tmp, self.path(&task.id))
    }

    fn load_all(&self) -> Vec<ScheduledTask> {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        let mut tasks = entries
            .flatten()
            .filter(|entry| entry.path().extension().is_some_and(|e| e == "json"))
            .filter_map(|entry| match std::fs::read(entry.path()) {
                Ok(bytes) => match serde_json::from_slice::<ScheduledTask>(&bytes) {
                    Ok(task) => Some(task),
                    Err(e) => {
                        eprintln!(
                            "[schedule] skipping corrupt queue file {}: {e}",
                            entry.path().display()
                        );
                        None
                    }
                },
                Err(e) => {
                    eprintln!(
                        "[schedule] skipping unreadable queue file {}: {e}",
                        entry.path().display()
                    );
                    None
                }
            })
            .collect::<Vec<_>>();
        tasks.sort_by(|a, b| a.at.cmp(&b.at).then_with(|| a.id.cmp(&b.id)));
        tasks
    }

    fn load(&self, id: &str) -> io::Result<Option<ScheduledTask>> {
        let bytes = match std::fs::read(self.path(id)) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(io::Error::other)
    }
}

/// The `schedule` tool — persists a promise that the task re-enters the
/// gate chain at a concrete future instant.
///
/// Tier [`ToolTrustTier::ExternalEffector`]: committing to a future
/// action always asks a human. Preflight labels it
/// [`amparo_agent::BlastRadius::SystemWide`] — the promise outlives the
/// session, so its reach is wider than the instant's. Executing a
/// schedule is never the model's shortcut: the ticker's firing path is
/// the same gate chain a live task gets.
pub struct ScheduleTool {
    /// Where promises are persisted (the driver-wide queue dir).
    store: Arc<dyn ScheduleStore>,
    /// The tenant key the promise is tagged with.
    tenant: String,
    /// The platform adapter name the fired task routes through.
    platform: String,
    /// The chat the promise was made in.
    chat_id: String,
    /// The requester's platform user id (the approval gate's owner).
    requester: String,
    /// The schema's firing-semantics copy — the chat and the CLI report
    /// a fire to different faces, and the model is told which.
    fire_copy: String,
}

impl ScheduleTool {
    /// A schedule tool bound to one chat: promises written by it fire as
    /// that chat's requester and the result reports back to the chat.
    pub fn new(
        store: Arc<dyn ScheduleStore>,
        tenant: impl Into<String>,
        platform: impl Into<String>,
        chat_id: impl Into<String>,
        requester: impl Into<String>,
    ) -> Self {
        Self {
            store,
            tenant: tenant.into(),
            platform: platform.into(),
            chat_id: chat_id.into(),
            requester: requester.into(),
            fire_copy: "When it fires, it re-enters the same policy and approval gate chain as \
                        a live task and reports back to this chat."
                .to_string(),
        }
    }

    /// A schedule tool for the CLI (M10 W5): the process is short-lived
    /// — there is no ticker — so a promise fires at the next
    /// `amparo run` start (within the grace window), and the result is
    /// recorded on the promise and printed to stderr. Tenant, platform
    /// and requester are all the run's task id: there is no chat to
    /// report back to.
    pub fn for_cli(store: Arc<dyn ScheduleStore>, task_id: impl Into<String>) -> Self {
        let task_id = task_id.into();
        Self {
            store,
            tenant: "cli".to_string(),
            platform: "cli".to_string(),
            chat_id: task_id.clone(),
            requester: task_id,
            fire_copy: "The promise re-enters the same policy and approval gate chain when it \
                        fires — at the next `amparo run` start, within the grace window — and \
                        the result is recorded on the promise and printed to stderr."
                .to_string(),
        }
    }

    /// A fresh schedule id, the `sess-` shape's sibling.
    fn new_schedule_id() -> String {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        format!("sched-{nanos}-{}", std::process::id())
    }
}

#[async_trait]
impl ToolExecutor for ScheduleTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: SCHEDULE.to_string(),
            description: format!(
                "Schedule a task to run at a specific future time. {}",
                self.fire_copy
            ),
            parameters: vec![
                ToolParam {
                    name: "at".to_string(),
                    description: "The RFC 3339 instant to fire at, e.g. \
                                  2026-08-30T17:00:00Z — a concrete future time."
                        .to_string(),
                    param_type: "string".to_string(),
                    enum_values: None,
                    required: true,
                },
                ToolParam {
                    name: "task".to_string(),
                    description: "The task text to run at that time.".to_string(),
                    param_type: "string".to_string(),
                    enum_values: None,
                    required: true,
                },
            ],
            trust_tier: ToolTrustTier::ExternalEffector,
        }
    }

    async fn execute(&self, call: &ToolCall) -> ToolResult {
        let fail = |message: String| ToolResult {
            tool_call_id: call.id.clone(),
            tool_name: SCHEDULE.to_string(),
            success: false,
            output: serde_json::json!({}),
            display_summary: message,
            duration_ms: 0,
        };
        let Some(at) = call.arg_str("at") else {
            return fail("schedule requires an \"at\" instant (RFC 3339)".to_string());
        };
        let Some(task) = call.arg_str("task") else {
            return fail("schedule requires a \"task\" text".to_string());
        };
        let Ok(instant) = DateTime::parse_from_rfc3339(at) else {
            return fail(format!("\"{at}\" is not a valid RFC 3339 instant"));
        };
        if instant <= Utc::now() {
            return fail(format!("{at} is in the past — schedule a future instant"));
        }
        // I6: the persisted promise carries placeholders, never the
        // values — and the promise itself names its stripped form.
        let stripped = amparo_privacy::secure_minions_strip(task).sanitised_text;
        if stripped.trim().is_empty() {
            return fail("the task is empty after PII stripping".to_string());
        }
        let scheduled = ScheduledTask {
            id: Self::new_schedule_id(),
            tenant: self.tenant.clone(),
            platform: self.platform.clone(),
            chat_id: self.chat_id.clone(),
            requester: self.requester.clone(),
            task: stripped,
            at: at.to_string(),
            status: ScheduledStatus::Pending,
            result: None,
        };
        if let Err(e) = self.store.save(&scheduled) {
            return fail(format!("cannot persist the schedule: {e}"));
        }
        ToolResult {
            tool_call_id: call.id.clone(),
            tool_name: SCHEDULE.to_string(),
            success: true,
            output: serde_json::json!({ "id": scheduled.id, "at": scheduled.at }),
            display_summary: format!("scheduled {} at {}", scheduled.id, scheduled.at),
            duration_ms: 0,
        }
    }
}

// ───────────────────────────────────────────────────────────── Tests ─────────

#[cfg(test)]
mod tests {
    use super::*;

    /// An RFC 3339 instant an hour out — the schedule tool compares
    /// against the wall clock, so hardcoded dates expire.
    fn future_at() -> String {
        (Utc::now() + Duration::hours(1)).to_rfc3339()
    }

    #[cfg(unix)]
    #[test]
    fn saves_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!(
            "amparo-schedule-perms-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let store = JsonScheduleStore::new(&dir);
        store
            .save(&task("sched-p", &future_at(), ScheduledStatus::Pending))
            .unwrap();
        assert_eq!(
            std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700,
            "schedule dir must be owner-only"
        );
        assert_eq!(
            std::fs::metadata(dir.join("sched-p.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600,
            "schedule file must be owner-only"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn task(id: &str, at: &str, status: ScheduledStatus) -> ScheduledTask {
        ScheduledTask {
            id: id.to_string(),
            tenant: "mock:user_1".to_string(),
            platform: "mock".to_string(),
            chat_id: "chat_1".to_string(),
            requester: "user_1".to_string(),
            task: "a task".to_string(),
            at: at.to_string(),
            status,
            result: None,
        }
    }

    #[test]
    fn due_scan_partitions_due_missed_and_waiting() {
        let now = DateTime::parse_from_rfc3339("2026-08-30T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let grace = Duration::seconds(60);
        let tasks = vec![
            task("future", "2026-08-30T13:00:00Z", ScheduledStatus::Pending),
            task("due", "2026-08-30T11:59:30Z", ScheduledStatus::Pending),
            task(
                "late_within_grace",
                "2026-08-30T11:59:00Z",
                ScheduledStatus::Pending,
            ),
            task("missed", "2026-08-30T11:00:00Z", ScheduledStatus::Pending),
            task("unparsable", "not-a-time", ScheduledStatus::Pending),
            task(
                "already_fired",
                "2026-08-30T11:00:00Z",
                ScheduledStatus::Fired,
            ),
            task(
                "already_missed",
                "2026-08-30T11:00:00Z",
                ScheduledStatus::Missed,
            ),
            task(
                "cancelled",
                "2026-08-30T11:00:00Z",
                ScheduledStatus::Cancelled,
            ),
        ];
        let (due, missed) = due_scan(&tasks, now, grace);
        let names = |indices: &[usize]| -> Vec<&str> {
            indices.iter().map(|&i| tasks[i].id.as_str()).collect()
        };
        assert_eq!(names(&due), vec!["due", "late_within_grace"]);
        assert_eq!(names(&missed), vec!["missed", "unparsable"]);
    }

    #[test]
    fn store_round_trips_and_leaves_no_tmp() {
        let dir =
            std::env::temp_dir().join(format!("amparo-schedule-roundtrip-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = JsonScheduleStore::new(dir.clone());
        let scheduled = task("sched-1", "2026-08-30T13:00:00Z", ScheduledStatus::Pending);
        store.save(&scheduled).unwrap();
        assert!(dir.join("sched-1.json").exists(), "the file landed");
        assert!(!dir.join("sched-1.tmp").exists(), "no tmp lingers");
        let loaded = store.load("sched-1").unwrap().expect("the task loads");
        assert_eq!(loaded.id, "sched-1");
        assert_eq!(loaded.status, ScheduledStatus::Pending);
        assert_eq!(
            store.load("nope").unwrap().is_none(),
            true,
            "absent id is None"
        );
        let all = store.load_all();
        assert_eq!(all.len(), 1, "one queue entry: {all:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn store_skips_corrupt_files_and_keeps_the_rest() {
        let dir =
            std::env::temp_dir().join(format!("amparo-schedule-corrupt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("good.json"),
            serde_json::to_vec(&task(
                "good",
                "2026-08-30T13:00:00Z",
                ScheduledStatus::Pending,
            ))
            .unwrap(),
        )
        .unwrap();
        std::fs::write(dir.join("bad.json"), b"{not json").unwrap();
        std::fs::write(dir.join("notes.txt"), b"not a task").unwrap();
        let store = JsonScheduleStore::new(dir.clone());
        let all = store.load_all();
        assert_eq!(all.len(), 1, "only the good entry loads: {all:?}");
        assert_eq!(all[0].id, "good");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn schedule_writes_a_stripped_pending_promise() {
        let dir = std::env::temp_dir().join(format!("amparo-schedule-tool-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Arc::new(JsonScheduleStore::new(dir.clone()));
        let tool = ScheduleTool::new(store, "mock:user_1", "mock", "chat_1", "user_1");
        let call = ToolCall {
            id: "call_1".to_string(),
            name: SCHEDULE.to_string(),
            arguments: serde_json::json!({
                "at": future_at(),
                "task": "remind me to email alice@example.com"
            }),
        };
        let result = tool.execute(&call).await;
        assert!(result.success, "the promise persists: {result:?}");
        let id = result.output["id"]
            .as_str()
            .expect("the result names the id")
            .to_string();
        let saved = JsonScheduleStore::new(dir.clone())
            .load(&id)
            .unwrap()
            .expect("the file exists");
        assert_eq!(saved.status, ScheduledStatus::Pending);
        assert_eq!(saved.tenant, "mock:user_1");
        assert_eq!(saved.chat_id, "chat_1");
        assert_eq!(saved.requester, "user_1");
        assert!(
            !saved.task.contains("alice@example.com"),
            "the promise is PII-stripped: {}",
            saved.task
        );
        assert!(
            saved.task.contains("[EMAIL_1]"),
            "the placeholder survives: {}",
            saved.task
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn schedule_rejects_past_unparsable_and_missing() {
        let store = Arc::new(JsonScheduleStore::new(
            std::env::temp_dir().join(format!("amparo-schedule-reject-{}", std::process::id())),
        ));
        let tool = ScheduleTool::new(store, "mock:user_1", "mock", "chat_1", "user_1");
        let call = |arguments: serde_json::Value| ToolCall {
            id: "call_1".to_string(),
            name: SCHEDULE.to_string(),
            arguments,
        };
        let past = tool
            .execute(&call(
                serde_json::json!({"at": "2020-01-01T00:00:00Z", "task": "x"}),
            ))
            .await;
        assert!(!past.success);
        assert!(past.display_summary.contains("past"), "{past:?}");
        let unparsable = tool
            .execute(&call(serde_json::json!({"at": "tomorrow", "task": "x"})))
            .await;
        assert!(!unparsable.success);
        assert!(
            unparsable.display_summary.contains("RFC 3339"),
            "{unparsable:?}"
        );
        let missing_at = tool.execute(&call(serde_json::json!({"task": "x"}))).await;
        assert!(!missing_at.success);
        assert!(
            missing_at.display_summary.contains("\"at\""),
            "{missing_at:?}"
        );
        let missing_task = tool
            .execute(&call(serde_json::json!({"at": future_at()})))
            .await;
        assert!(!missing_task.success);
        assert!(
            missing_task.display_summary.contains("\"task\""),
            "{missing_task:?}"
        );
    }

    /// A store that always fails to write — the tool reports it, never
    /// panics, never claims success.
    struct FailingStore;

    impl ScheduleStore for FailingStore {
        fn save(&self, _task: &ScheduledTask) -> io::Result<()> {
            Err(io::Error::new(io::ErrorKind::Other, "disk gone"))
        }
        fn load_all(&self) -> Vec<ScheduledTask> {
            Vec::new()
        }
        fn load(&self, _id: &str) -> io::Result<Option<ScheduledTask>> {
            Ok(None)
        }
    }

    #[tokio::test]
    async fn schedule_reports_a_store_failure_as_a_failure() {
        let tool = ScheduleTool::new(
            Arc::new(FailingStore),
            "mock:user_1",
            "mock",
            "chat_1",
            "user_1",
        );
        let call = ToolCall {
            id: "call_1".to_string(),
            name: SCHEDULE.to_string(),
            arguments: serde_json::json!({"at": future_at(), "task": "x"}),
        };
        let result = tool.execute(&call).await;
        assert!(!result.success);
        assert!(
            result.display_summary.contains("cannot persist"),
            "{result:?}"
        );
    }

    #[tokio::test]
    async fn cli_tool_names_the_run_start_fire_and_tags_the_promise_cli() {
        let dir = std::env::temp_dir().join(format!("amparo-schedule-cli-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = JsonScheduleStore::new(dir.clone());
        let tool = ScheduleTool::for_cli(Arc::new(store.clone()), "sess-1");
        let schema = tool.schema();
        assert!(
            schema.description.contains("next `amparo run` start"),
            "the CLI copy names the run-start fire: {}",
            schema.description
        );
        assert!(
            schema.description.contains("stderr"),
            "the CLI copy names where the result lands: {}",
            schema.description
        );
        // The promise is tagged cli end to end — the run-start scan
        // fires only promises it wrote (I2).
        let call = ToolCall {
            id: "call_1".to_string(),
            name: SCHEDULE.to_string(),
            arguments: serde_json::json!({
                "at": "2099-01-01T00:00:00Z",
                "task": "a cli promise"
            }),
        };
        let result = tool.execute(&call).await;
        assert!(result.success, "the promise persists: {result:?}");
        let saved = store
            .load(
                result.output["id"]
                    .as_str()
                    .expect("the result names the id"),
            )
            .unwrap()
            .expect("the cli promise file exists");
        assert_eq!(saved.tenant, "cli");
        assert_eq!(saved.platform, "cli");
        assert_eq!(saved.chat_id, "sess-1");
        assert_eq!(saved.requester, "sess-1");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn status_names_are_snake_case() {
        assert_eq!(ScheduledStatus::Pending.to_string(), "pending");
        assert_eq!(ScheduledStatus::Fired.to_string(), "fired");
        assert_eq!(ScheduledStatus::Missed.to_string(), "missed");
        assert_eq!(ScheduledStatus::Cancelled.to_string(), "cancelled");
        // The serde round-trip is by name, like every Amparo wire type.
        let json = serde_json::to_string(&ScheduledStatus::Fired).unwrap();
        assert_eq!(json, "\"fired\"");
    }
}
