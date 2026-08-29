//! Rollup and archival — the hot layer over the cold archive (M6e).
//!
//! `records.jsonl` is the **cold archive**: the full record for every task,
//! append-only, never touched here. The **hot layer** (`hot.jsonl`) is the
//! *derived, informative subset* it feeds (`docs/m6-controlled-growth.md`
//! §4): records that survive dedupe by tool-sequence hash, records carrying
//! gate events of interest (approvals, denials, escalations), and
//! operator-promoted cases. Hot rows keep the cold row's `MemoryEntry` id
//! and `created_at` — the same record has one identity on both layers.
//!
//! Two motions keep the hot layer fresh, and both are cheap:
//!
//! - **Tail promotion** — `records.jsonl` is scanned from a byte offset
//!   (`rollup.json`) that advances with every scan, so only rows written
//!   since the last scan are considered. Runs at every `--growth` task
//!   start via [`auto_rollup`], and on demand via [`rollup`].
//! - **The 90-day fold** — hot rows older than `days` fold back into the
//!   already-complete cold archive (they are pruned from hot, never
//!   deleted from cold); operator-promoted rows are exempt. Hot is
//!   derived, so the fold *rewrites* `hot.jsonl` and its hash sidecar —
//!   the append-only discipline applies to the cold archive alone.
//!
//! Cross-process mutations (a CLI run and a chat process can share a
//! workspace) are serialized by a lockfile; a crash leaves a stale lock
//! that is reclaimed after [`STALE_LOCK_MINUTES`].

use std::collections::HashSet;
use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use amparo_tools::MemoryEntry;

use crate::record::truncate_to;
use crate::skills::append_line;
use crate::RunRecord;

/// The hot-layer filename, under the notebook directory.
pub const HOT_FILE: &str = "hot.jsonl";
/// The dedupe sidecar filename: one `(tenant, hash)` line per promoted row.
pub const HOT_HASHES_FILE: &str = "hot-hashes.jsonl";
/// The rollup state filename (cold scan offset + last-fold timestamp).
pub const ROLLUP_STATE_FILE: &str = "rollup.json";
/// The operator-promotion log filename.
pub const PROMOTED_FILE: &str = "promoted.jsonl";
/// The mutation lockfile filename.
pub(crate) const LOCK_FILE: &str = "rollup.lock";

/// The default fold window in days — hot rows older than this fold.
pub const DEFAULT_ROLLUP_DAYS: u64 = 90;
/// The default payload cap per hot record, in bytes.
pub const DEFAULT_MAX_BYTES: usize = 4096;
/// The floor for the payload cap — below it the [`hot_record`] size
/// guarantee no longer holds for pathological input.
pub const MAX_BYTES_FLOOR: usize = 1024;
/// The automatic fold gate: a task-start [`auto_rollup`] folds only when
/// the last fold is at least this many hours old.
pub(crate) const AUTO_FOLD_HOURS: i64 = 24;
/// Locks older than this are considered crash debris and reclaimed.
pub(crate) const STALE_LOCK_MINUTES: u64 = 10;

/// The workspace notebook directory: `<workspace_root>/.amparo/notebook`.
pub fn notebook_dir(workspace_root: &Path) -> PathBuf {
    workspace_root.join(".amparo").join("notebook")
}

/// Promotion/fold bookkeeping, persisted to [`ROLLUP_STATE_FILE`]. The
/// cold scan offset is a byte offset into `records.jsonl` — the archive is
/// append-only, so everything before it has been scanned already.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct RollupState {
    /// Bytes of `records.jsonl` already promoted (or skipped).
    pub last_offset: u64,
    /// RFC 3339 timestamp of the last fold — `None` when the hot layer has
    /// never been folded.
    pub last_fold_at: Option<String>,
    /// RFC 3339 timestamp of the last state write.
    pub updated_at: String,
}

/// One dedupe-sidecar row: this tenant has already promoted this tool
/// sequence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HashRow {
    /// The record's tenant (I2 — dedupe is per tenant).
    pub tenant_id: String,
    /// The tool-sequence hash.
    pub tool_sequence_hash: String,
}

/// One operator promotion: a cold record pinned into the hot layer,
/// exempt from the fold. The operator review path from spec §3.2.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromotedRecord {
    /// RFC 3339 promotion timestamp.
    pub promoted_at: String,
    /// The record's tenant (I2).
    pub tenant_id: String,
    /// The cold `MemoryEntry` id of the promoted record.
    pub record_id: String,
}

/// The outcome of a tail-promotion scan.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PromoteReport {
    /// Cold lines read since the last offset.
    pub scanned: u64,
    /// Lines promoted into the hot layer.
    pub promoted: u64,
    /// Lines read but not promoted (duplicate hashes, uninteresting).
    pub skipped: u64,
}

/// The outcome of a fold.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FoldReport {
    /// Hot rows folded back into the cold archive.
    pub folded: u64,
    /// Hot rows kept (recent, or operator-promoted).
    pub kept: u64,
}

/// The outcome of a task-start [`auto_rollup`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AutoReport {
    /// Tail records promoted.
    pub promoted: u64,
    /// Hot rows folded (zero when the 24-hour gate skipped the fold).
    pub folded: u64,
    /// Hot rows kept by the fold (zero when the fold was skipped).
    pub kept: u64,
}

/// The outcome of a forced [`rollup`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RollupReport {
    /// Tail records promoted.
    pub promoted: u64,
    /// Tail records skipped (duplicate hashes, uninteresting).
    pub skipped: u64,
    /// Hot rows folded back into the cold archive.
    pub folded: u64,
    /// Hot rows kept.
    pub kept: u64,
}

/// The outcome of an operator [`promote_record`] call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromoteOutcome {
    /// The record was pinned into the hot layer.
    Promoted,
    /// The record was already pinned — nothing written.
    AlreadyPromoted,
}

/// One cold-archive row as shown by `amparo notebook list`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RecordSummary {
    /// The cold `MemoryEntry` id.
    pub id: String,
    /// RFC 3339 task start.
    pub started_at: String,
    /// The record's tenant.
    pub tenant_id: String,
    /// `complete` | `failed`.
    pub status: String,
    /// The verification decision, when the task verified.
    pub verification: Option<String>,
    /// The tool-sequence hash.
    pub tool_sequence_hash: String,
    /// The PII-stripped, truncated task text.
    pub task_text: String,
    /// Whether the operator has pinned this record (fold-exempt).
    pub promoted: bool,
}

/// The capped hot copy of a record.
///
/// Returns the record unchanged when it already fits `max_bytes`; otherwise
/// applies a deterministic truncation ladder — answer → 300 chars, per-step
/// reasons/summaries → 120, first 12 calls, task text → 120, then skeleton
/// rungs (no reasons/summaries, 6 calls, no calls, no answer, task text →
/// 40, feedback → 40). The final rung is guaranteed to fit for
/// `max_bytes >= `[`MAX_BYTES_FLOOR`]; callers clamp below the floor. The
/// cold archive always keeps the full record — this shapes only the hot
/// copy.
pub fn hot_record(record: &RunRecord, max_bytes: usize) -> RunRecord {
    if fits(record, max_bytes) {
        return record.clone();
    }
    let mut r = record.clone();
    r.final_answer = r
        .final_answer
        .as_ref()
        .map(|answer| truncate_to(answer, 300));
    if fits(&r, max_bytes) {
        return r;
    }
    for step in &mut r.tool_calls {
        step.reasons = step
            .reasons
            .iter()
            .map(|reason| truncate_to(reason, 120))
            .collect();
        step.summary = step
            .summary
            .as_ref()
            .map(|summary| truncate_to(summary, 120));
    }
    if fits(&r, max_bytes) {
        return r;
    }
    r.tool_calls.truncate(12);
    if fits(&r, max_bytes) {
        return r;
    }
    r.task_text = truncate_to(&r.task_text, 120);
    if fits(&r, max_bytes) {
        return r;
    }
    for step in &mut r.tool_calls {
        step.reasons.clear();
        step.summary = None;
    }
    if fits(&r, max_bytes) {
        return r;
    }
    r.tool_calls.truncate(6);
    if fits(&r, max_bytes) {
        return r;
    }
    r.tool_calls.clear();
    if fits(&r, max_bytes) {
        return r;
    }
    r.final_answer = None;
    if fits(&r, max_bytes) {
        return r;
    }
    r.task_text = truncate_to(&r.task_text, 40);
    if let Some(verification) = &mut r.verification {
        verification.feedback = verification
            .feedback
            .as_ref()
            .map(|feedback| truncate_to(feedback, 40));
    }
    r
}

/// Whether the record's serialized size is within `max_bytes`.
fn fits(record: &RunRecord, max_bytes: usize) -> bool {
    serde_json::to_string(record)
        .map(|json| json.len() <= max_bytes)
        .unwrap_or(false)
}

/// Whether the record carries a gate event of interest: an escalation, a
/// non-allow decision, or a human approval (spec §4 — approvals, denials,
/// escalations).
fn is_interesting(record: &RunRecord) -> bool {
    record.tool_calls.iter().any(|step| {
        step.escalated || step.decision != "allowed" || step.approved == Some(true)
    })
}

// ─────────────────────────────────────────────── Lock ────────────────────────

/// A cross-process mutation lock. Held (create-exclusive file) while the
/// hot layer, its hash sidecar and the rollup state are mutated; removed on
/// drop. A lock older than [`STALE_LOCK_MINUTES`] is crash debris and is
/// reclaimed on acquire.
struct RollupLock {
    path: PathBuf,
}

impl Drop for RollupLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

impl RollupLock {
    /// Try to take the lock. `Ok(None)` means it is held by another live
    /// process (or the reclaim lost a race); callers decide whether that
    /// is a silent skip (task-start auto rollup) or an error (operator
    /// commands).
    fn acquire(notebook_dir: &Path) -> Result<Option<Self>, String> {
        std::fs::create_dir_all(notebook_dir)
            .map_err(|e| format!("cannot create notebook directory {}: {e}", notebook_dir.display()))?;
        let path = notebook_dir.join(LOCK_FILE);
        match Self::try_create(&path) {
            Ok(lock) => Ok(Some(lock)),
            Err(TryCreateError::Held) => {
                let modified = std::fs::metadata(&path)
                    .and_then(|meta| meta.modified())
                    .map_err(|e| format!("cannot stat lock {}: {e}", path.display()))?;
                if is_stale_lock(modified, SystemTime::now()) {
                    let _ = std::fs::remove_file(&path);
                    match Self::try_create(&path) {
                        Ok(lock) => Ok(Some(lock)),
                        Err(_) => Ok(None),
                    }
                } else {
                    Ok(None)
                }
            }
            Err(TryCreateError::Io(e)) => Err(e),
        }
    }

    fn try_create(path: &Path) -> Result<Self, TryCreateError> {
        match std::fs::OpenOptions::new().write(true).create_new(true).open(path) {
            Ok(mut file) => {
                let stamped = writeln!(
                    file,
                    "pid={} at={}",
                    std::process::id(),
                    chrono::Utc::now().to_rfc3339()
                )
                .and_then(|_| file.flush());
                if let Err(e) = stamped {
                    let _ = std::fs::remove_file(path);
                    return Err(TryCreateError::Io(format!(
                        "cannot write lock {}: {e}",
                        path.display()
                    )));
                }
                Ok(Self {
                    path: path.to_path_buf(),
                })
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(TryCreateError::Held),
            Err(e) => Err(TryCreateError::Io(format!(
                "cannot create lock {}: {e}",
                path.display()
            ))),
        }
    }
}

enum TryCreateError {
    Held,
    Io(String),
}

/// Whether a lock whose mtime is `modified` is old enough to be crash
/// debris.
fn is_stale_lock(modified: SystemTime, now: SystemTime) -> bool {
    now.duration_since(modified)
        .map(|age| age >= Duration::from_secs(STALE_LOCK_MINUTES * 60))
        .unwrap_or(false)
}

// ─────────────────────────────────────────────── State / indexes ─────────────

/// Load the rollup state; a missing or corrupt file is a default state (a
/// rescan is wasteful but safe).
fn load_state(notebook_dir: &Path) -> RollupState {
    let path = notebook_dir.join(ROLLUP_STATE_FILE);
    match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
        Err(_) => RollupState::default(),
    }
}

/// Persist the rollup state atomically (temp file + rename).
fn save_state(notebook_dir: &Path, state: &RollupState) -> Result<(), String> {
    atomic_write_lines(
        &notebook_dir.join(ROLLUP_STATE_FILE),
        std::slice::from_ref(state),
    )
}

/// The `(tenant, tool_sequence_hash)` pairs already promoted into the hot
/// layer, from the dedupe sidecar.
fn load_hashes(notebook_dir: &Path) -> HashSet<(String, String)> {
    let path = notebook_dir.join(HOT_HASHES_FILE);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return HashSet::new();
    };
    text.lines()
        .filter_map(|line| serde_json::from_str::<HashRow>(line).ok())
        .map(|row| (row.tenant_id, row.tool_sequence_hash))
        .collect()
}

/// The set of operator-promoted cold record ids.
fn promoted_ids(notebook_dir: &Path) -> HashSet<String> {
    let path = notebook_dir.join(PROMOTED_FILE);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return HashSet::new();
    };
    text.lines()
        .filter_map(|line| serde_json::from_str::<PromotedRecord>(line).ok())
        .map(|row| row.record_id)
        .collect()
}

/// Parse one records-file line: the `MemoryEntry` wrapper (with its id and
/// created_at) plus the `RunRecord` inside `content`. Rows in any other
/// shape are skipped — the archive may carry test-seeded or legacy lines.
fn parse_entry_and_record(line: &str) -> Option<(MemoryEntry, RunRecord)> {
    let entry = serde_json::from_str::<MemoryEntry>(line).ok()?;
    let record = serde_json::from_str::<RunRecord>(&entry.content).ok()?;
    Some((entry, record))
}

/// Write `values` as JSONL to `path` atomically: a temp file is written
/// and renamed over the target, so a reader never sees a half-rewritten
/// file.
fn atomic_write_lines<T: Serialize>(path: &Path, values: &[T]) -> Result<(), String> {
    let tmp_name = format!(
        "{}.tmp",
        path.file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default()
    );
    let tmp = path.with_file_name(tmp_name);
    let mut text = String::new();
    for value in values {
        text.push_str(
            &serde_json::to_string(value).map_err(|e| format!("cannot serialize: {e}"))?,
        );
        text.push('\n');
    }
    std::fs::write(&tmp, text).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .map_err(|e| format!("cannot rename {} over {}: {e}", tmp.display(), path.display()))
}

// ─────────────────────────────────────────────── Promote / fold ──────────────

/// Promote the cold tail into the hot layer: everything written to
/// `records.jsonl` since `rollup.json`'s offset. A row is promoted when it
/// carries a gate event of interest or its (tenant, hash) pair is new to
/// the hot layer. Mid-file unparseable lines are skipped and the offset
/// advances past them; an unparseable *final* line stops the scan without
/// advancing (a torn sink append — retried next task start). The caller
/// holds the lock.
fn promote_tail(
    notebook_dir: &Path,
    max_bytes: usize,
    now: DateTime<Utc>,
) -> Result<PromoteReport, String> {
    let cold = notebook_dir.join("records.jsonl");
    let mut report = PromoteReport::default();
    let mut state = load_state(notebook_dir);
    let file = match File::open(&cold) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(report),
        Err(e) => {
            return Err(format!(
                "cannot open the cold archive {}: {e}",
                cold.display()
            ))
        }
    };
    let len = file
        .metadata()
        .map_err(|e| format!("cannot stat {}: {e}", cold.display()))?
        .len();
    let offset = if state.last_offset > len { 0 } else { state.last_offset };
    let mut hashes = load_hashes(notebook_dir);
    let mut reader = BufReader::new(file);
    reader
        .seek(SeekFrom::Start(offset))
        .map_err(|e| format!("cannot seek {}: {e}", cold.display()))?;

    let mut consumed = offset;
    let mut last_failed_start: Option<u64> = None;
    let mut buf = Vec::new();
    loop {
        buf.clear();
        let n = reader
            .read_until(b'\n', &mut buf)
            .map_err(|e| format!("cannot read {}: {e}", cold.display()))?;
        if n == 0 {
            break;
        }
        let line_start = consumed;
        consumed += n as u64;
        report.scanned += 1;
        let line = String::from_utf8_lossy(&buf);
        let line = line.trim_end_matches('\n').trim();
        if line.is_empty() {
            last_failed_start = None;
            continue;
        }
        match parse_entry_and_record(line) {
            Some((entry, record)) => {
                last_failed_start = None;
                let interesting = is_interesting(&record);
                let novel = hashes.insert((
                    record.tenant_id.clone(),
                    record.tool_sequence_hash.clone(),
                ));
                if interesting || novel {
                    let hot_entry = MemoryEntry {
                        id: entry.id,
                        created_at: entry.created_at,
                        content: serde_json::to_string(&hot_record(&record, max_bytes))
                            .map_err(|e| e.to_string())?,
                    };
                    append_line(&notebook_dir.join(HOT_FILE), &hot_entry)?;
                    append_line(
                        &notebook_dir.join(HOT_HASHES_FILE),
                        &HashRow {
                            tenant_id: record.tenant_id,
                            tool_sequence_hash: record.tool_sequence_hash,
                        },
                    )?;
                    report.promoted += 1;
                } else {
                    report.skipped += 1;
                }
            }
            None => last_failed_start = Some(line_start),
        }
    }
    state.last_offset = last_failed_start.unwrap_or(consumed);
    state.updated_at = now.to_rfc3339();
    save_state(notebook_dir, &state)?;
    Ok(report)
}

/// Fold the hot layer: rows older than `days` are pruned from hot (they
/// already live, complete, in the cold archive); operator-promoted rows
/// are exempt. Rows whose `started_at` does not parse fold unless
/// promoted. The hot file and its hash sidecar are rewritten atomically,
/// and `last_fold_at` is set. The caller holds the lock.
fn fold_hot(
    notebook_dir: &Path,
    days: u64,
    now: DateTime<Utc>,
) -> Result<FoldReport, String> {
    let hot_path = notebook_dir.join(HOT_FILE);
    let hashes_path = notebook_dir.join(HOT_HASHES_FILE);
    let promoted = promoted_ids(notebook_dir);
    let cutoff = now - chrono::Duration::days(days as i64);

    let mut report = FoldReport::default();
    let mut kept: Vec<MemoryEntry> = Vec::new();
    let mut kept_hashes: Vec<HashRow> = Vec::new();
    if let Ok(text) = std::fs::read_to_string(&hot_path) {
        for line in text.lines() {
            match parse_entry_and_record(line) {
                Some((entry, record)) => {
                    let recent = DateTime::parse_from_rfc3339(&record.started_at)
                        .map(|started| started >= cutoff)
                        .unwrap_or(false);
                    if promoted.contains(&entry.id) || recent {
                        kept_hashes.push(HashRow {
                            tenant_id: record.tenant_id.clone(),
                            tool_sequence_hash: record.tool_sequence_hash.clone(),
                        });
                        kept.push(entry);
                        report.kept += 1;
                    } else {
                        report.folded += 1;
                    }
                }
                None => report.folded += 1,
            }
        }
    }
    atomic_write_lines(&hot_path, &kept)?;
    atomic_write_lines(&hashes_path, &kept_hashes)?;

    let mut state = load_state(notebook_dir);
    state.last_fold_at = Some(now.to_rfc3339());
    state.updated_at = now.to_rfc3339();
    save_state(notebook_dir, &state)?;
    Ok(report)
}

// ─────────────────────────────────────────────── Public orchestrators ────────

/// The task-start rollup: promote the cold tail, and fold when the last
/// fold is at least 24 hours old (or has never run) — the defaults of
/// spec §4, applied automatically so a deployment without an operator
/// cron still keeps the hot layer small and fresh.
///
/// `Ok(None)` means the mutation lock is held by another process — the
/// caller skips silently and the next task start promotes. Errors are
/// reported by the caller and must never fail the task.
pub fn auto_rollup(
    notebook_dir: &Path,
    now: DateTime<Utc>,
) -> Result<Option<AutoReport>, String> {
    let Some(_lock) = RollupLock::acquire(notebook_dir)? else {
        return Ok(None);
    };
    let state = load_state(notebook_dir);
    let fold_due = state
        .last_fold_at
        .as_deref()
        .map(|at| {
            DateTime::parse_from_rfc3339(at)
                .map(|last| now.signed_duration_since(last) >= chrono::Duration::hours(AUTO_FOLD_HOURS))
                .unwrap_or(true)
        })
        .unwrap_or(true);
    let fold = if fold_due {
        fold_hot(notebook_dir, DEFAULT_ROLLUP_DAYS, now)?
    } else {
        FoldReport::default()
    };
    let promote = promote_tail(notebook_dir, DEFAULT_MAX_BYTES, now)?;
    Ok(Some(AutoReport {
        promoted: promote.promoted,
        folded: fold.folded,
        kept: fold.kept,
    }))
}

/// The forced rollup — the cron-able operator surface (`amparo notebook
/// rollup`): promote the tail and fold with explicit levers. Errors when
/// the lock is held (an operator action should know it did nothing).
pub fn rollup(
    notebook_dir: &Path,
    days: u64,
    max_bytes: usize,
    now: DateTime<Utc>,
) -> Result<RollupReport, String> {
    let Some(_lock) = RollupLock::acquire(notebook_dir)? else {
        return Err("the notebook is busy (another rollup holds the lock); retry shortly".to_string());
    };
    let fold = fold_hot(notebook_dir, days, now)?;
    let promote = promote_tail(notebook_dir, max_bytes.max(MAX_BYTES_FLOOR), now)?;
    Ok(RollupReport {
        promoted: promote.promoted,
        skipped: promote.skipped,
        folded: fold.folded,
        kept: fold.kept,
    })
}

/// The read-only [`rollup`] estimate — the same promoted/folded/kept
/// counts, without touching a single file (no lock, no appends, no state
/// write). The operator's `--dry-run`. `max_bytes` does not change *which*
/// rows promote, only the size of their hot copies, so it does not affect
/// the counts. Counts are an estimate: a concurrent sink append may land
/// after the scan.
pub fn rollup_dry_run(
    notebook_dir: &Path,
    days: u64,
    max_bytes: usize,
    now: DateTime<Utc>,
) -> Result<RollupReport, String> {
    let _ = max_bytes;
    let state = load_state(notebook_dir);
    let mut hashes = load_hashes(notebook_dir);
    let mut report = RollupReport::default();
    let cold = notebook_dir.join("records.jsonl");
    match File::open(&cold) {
        Ok(file) => {
            let len = file
                .metadata()
                .map_err(|e| format!("cannot stat {}: {e}", cold.display()))?
                .len();
            let offset = if state.last_offset > len { 0 } else { state.last_offset };
            let mut reader = BufReader::new(file);
            reader
                .seek(SeekFrom::Start(offset))
                .map_err(|e| format!("cannot seek {}: {e}", cold.display()))?;
            let mut buf = Vec::new();
            loop {
                buf.clear();
                let n = reader
                    .read_until(b'\n', &mut buf)
                    .map_err(|e| format!("cannot read {}: {e}", cold.display()))?;
                if n == 0 {
                    break;
                }
                let line = String::from_utf8_lossy(&buf);
                let line = line.trim_end_matches('\n').trim();
                if line.is_empty() {
                    continue;
                }
                if let Some((_entry, record)) = parse_entry_and_record(line) {
                    let interesting = is_interesting(&record);
                    let novel = hashes.insert((
                        record.tenant_id.clone(),
                        record.tool_sequence_hash.clone(),
                    ));
                    if interesting || novel {
                        report.promoted += 1;
                    } else {
                        report.skipped += 1;
                    }
                }
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(format!(
                "cannot open the cold archive {}: {e}",
                cold.display()
            ))
        }
    }
    let promoted = promoted_ids(notebook_dir);
    let cutoff = now - chrono::Duration::days(days as i64);
    if let Ok(text) = std::fs::read_to_string(notebook_dir.join(HOT_FILE)) {
        for line in text.lines() {
            match parse_entry_and_record(line) {
                Some((entry, record)) => {
                    let recent = DateTime::parse_from_rfc3339(&record.started_at)
                        .map(|started| started >= cutoff)
                        .unwrap_or(false);
                    if promoted.contains(&entry.id) || recent {
                        report.kept += 1;
                    } else {
                        report.folded += 1;
                    }
                }
                None => report.folded += 1,
            }
        }
    }
    Ok(report)
}

/// Pin one cold record into the hot layer (the operator review path from
/// spec §3.2): appends its capped hot copy (same id and created_at), its
/// hash row, and a [`PromotedRecord`] row making it fold-exempt. Idempotent
/// — promoting an already-promoted id writes nothing. Errors when the
/// record id is unknown, or when the lock is held.
pub fn promote_record(
    notebook_dir: &Path,
    record_id: &str,
    max_bytes: usize,
    now: DateTime<Utc>,
) -> Result<PromoteOutcome, String> {
    let Some(_lock) = RollupLock::acquire(notebook_dir)? else {
        return Err("the notebook is busy (another rollup holds the lock); retry shortly".to_string());
    };
    if promoted_ids(notebook_dir).contains(record_id) {
        return Ok(PromoteOutcome::AlreadyPromoted);
    }
    let cold = notebook_dir.join("records.jsonl");
    let text = match std::fs::read_to_string(&cold) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(format!("unknown record id {record_id} — the cold archive is empty"))
        }
        Err(e) => {
            return Err(format!(
                "cannot read the cold archive {}: {e}",
                cold.display()
            ))
        }
    };
    for line in text.lines() {
        let Some((entry, record)) = parse_entry_and_record(line) else {
            continue;
        };
        if entry.id != record_id {
            continue;
        }
        let hot_entry = MemoryEntry {
            id: entry.id,
            created_at: entry.created_at,
            content: serde_json::to_string(&hot_record(&record, max_bytes.max(MAX_BYTES_FLOOR)))
                .map_err(|e| e.to_string())?,
        };
        append_line(&notebook_dir.join(HOT_FILE), &hot_entry)?;
        append_line(
            &notebook_dir.join(HOT_HASHES_FILE),
            &HashRow {
                tenant_id: record.tenant_id.clone(),
                tool_sequence_hash: record.tool_sequence_hash.clone(),
            },
        )?;
        append_line(
            &notebook_dir.join(PROMOTED_FILE),
            &PromotedRecord {
                promoted_at: now.to_rfc3339(),
                tenant_id: record.tenant_id,
                record_id: record_id.to_string(),
            },
        )?;
        return Ok(PromoteOutcome::Promoted);
    }
    Err(format!(
        "unknown record id {record_id} in the cold archive"
    ))
}

/// Cold-archive summaries for `tenant_id`, newest first, capped at `limit`,
/// with the promotion flag from [`PROMOTED_FILE`]. Read-only.
pub fn list_records(
    notebook_dir: &Path,
    tenant_id: &str,
    limit: usize,
) -> Result<Vec<RecordSummary>, String> {
    let cold = notebook_dir.join("records.jsonl");
    let text = match std::fs::read_to_string(&cold) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(format!(
                "cannot read the cold archive {}: {e}",
                cold.display()
            ))
        }
    };
    let promoted = promoted_ids(notebook_dir);
    let mut rows = Vec::new();
    for line in text.lines() {
        let Some((entry, record)) = parse_entry_and_record(line) else {
            continue;
        };
        if record.tenant_id != tenant_id {
            continue;
        }
        let is_promoted = promoted.contains(&entry.id);
        rows.push(RecordSummary {
            id: entry.id,
            started_at: record.started_at,
            tenant_id: record.tenant_id,
            status: record.status,
            verification: record.verification.map(|v| v.decision),
            tool_sequence_hash: record.tool_sequence_hash,
            task_text: record.task_text,
            promoted: is_promoted,
        });
    }
    rows.sort_by(|a, b| b.started_at.cmp(&a.started_at));
    rows.truncate(limit);
    Ok(rows)
}

// ───────────────────────────────────────────────────────────── Tests ─────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::VerificationRecord;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("amparo-rollup-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn at(rfc3339: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(rfc3339)
            .unwrap()
            .with_timezone(&Utc)
    }

    fn record(tenant: &str, started_at: &str, hash: &str) -> RunRecord {
        RunRecord {
            version: 1,
            tenant_id: tenant.to_string(),
            started_at: started_at.to_string(),
            duration_ms: 10,
            task_text: format!("task {hash}"),
            tool_sequence_hash: hash.to_string(),
            tool_calls: vec![],
            verification: Some(VerificationRecord {
                decision: "complete".to_string(),
                feedback: None,
            }),
            status: "complete".to_string(),
            final_answer: Some("done".to_string()),
            token_cost_estimate: 4,
        }
    }

    fn entry(id: &str, created_at: &str, record: &RunRecord) -> MemoryEntry {
        MemoryEntry {
            id: id.to_string(),
            content: serde_json::to_string(record).unwrap(),
            created_at: created_at.to_string(),
        }
    }

    fn append_cold(dir: &Path, entry: &MemoryEntry) {
        append_line(&dir.join("records.jsonl"), entry).unwrap();
    }

    fn hot_rows(dir: &Path) -> Vec<MemoryEntry> {
        std::fs::read_to_string(dir.join(HOT_FILE))
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }

    fn interesting_record(tenant: &str, started_at: &str, hash: &str) -> RunRecord {
        let mut r = record(tenant, started_at, hash);
        r.tool_calls.push(crate::ToolStep {
            call_id: "call_1".to_string(),
            tool_name: "run_command".to_string(),
            target: "ls".to_string(),
            decision: "policy_denied".to_string(),
            reasons: vec!["blocked".to_string()],
            escalated: true,
            approved: Some(false),
            success: None,
            summary: None,
            duration_ms: None,
        });
        r
    }

    #[test]
    fn hot_record_keeps_small_records_unchanged() {
        let r = record("cli", "2026-08-29T00:00:00Z", "hash-1");
        assert_eq!(hot_record(&r, DEFAULT_MAX_BYTES), r);
    }

    #[test]
    fn hot_record_truncates_the_answer_first() {
        let mut r = record("cli", "2026-08-29T00:00:00Z", "hash-1");
        r.final_answer = Some("x".repeat(5000));
        let hot = hot_record(&r, DEFAULT_MAX_BYTES);
        assert!(
            hot.final_answer.as_deref().unwrap().chars().count() <= 301,
            "answer capped at 300 chars + marker"
        );
        assert!(fits(&hot, DEFAULT_MAX_BYTES));
    }

    #[test]
    fn hot_record_converges_on_pathological_input() {
        let mut r = record("cli", "2026-08-29T00:00:00Z", "hash-1");
        r.task_text = "t".repeat(5000);
        r.final_answer = Some("a".repeat(5000));
        r.verification = Some(VerificationRecord {
            decision: "complete".to_string(),
            feedback: Some("f".repeat(5000)),
        });
        for i in 0..200 {
            r.tool_calls.push(crate::ToolStep {
                call_id: format!("call-{i}"),
                tool_name: "run_command".to_string(),
                target: "target".repeat(100),
                decision: "allowed".to_string(),
                reasons: vec!["r".repeat(500); 20],
                escalated: false,
                approved: None,
                success: Some(true),
                summary: Some("s".repeat(500)),
                duration_ms: Some(1),
            });
        }
        for cap in [DEFAULT_MAX_BYTES, MAX_BYTES_FLOOR] {
            let hot = hot_record(&r, cap);
            let size = serde_json::to_string(&hot).unwrap().len();
            assert!(size <= cap, "cap {cap}: {size} bytes");
        }
    }

    #[test]
    fn is_interesting_detects_gate_events() {
        let plain = record("cli", "2026-08-29T00:00:00Z", "hash-1");
        assert!(!is_interesting(&plain));

        let mut escalated = plain.clone();
        escalated.tool_calls.push(crate::ToolStep {
            call_id: "c".to_string(),
            tool_name: "run_command".to_string(),
            target: String::new(),
            decision: "allowed".to_string(),
            reasons: vec![],
            escalated: true,
            approved: Some(true),
            success: Some(true),
            summary: None,
            duration_ms: Some(1),
        });
        assert!(is_interesting(&escalated));

        let mut denied = plain.clone();
        denied.tool_calls.push(crate::ToolStep {
            call_id: "c".to_string(),
            tool_name: "run_command".to_string(),
            target: String::new(),
            decision: "policy_denied".to_string(),
            reasons: vec![],
            escalated: false,
            approved: None,
            success: None,
            summary: None,
            duration_ms: None,
        });
        assert!(is_interesting(&denied));

        let mut approved = plain.clone();
        approved.tool_calls.push(crate::ToolStep {
            call_id: "c".to_string(),
            tool_name: "run_command".to_string(),
            target: String::new(),
            decision: "allowed".to_string(),
            reasons: vec![],
            escalated: false,
            approved: Some(true),
            success: Some(true),
            summary: None,
            duration_ms: Some(1),
        });
        assert!(is_interesting(&approved));
    }

    #[test]
    fn promote_tail_promotes_novel_and_interesting_and_skips_duplicates() {
        let dir = temp_dir("promote-basic.jsonl");
        // Two plain rows with the same hash — the first is novel, the
        // second is a duplicate and skipped.
        append_cold(&dir, &entry("rec-1", "2026-08-29T00:00:00Z", &record("cli", "2026-08-28T00:00:00Z", "hash-a")));
        append_cold(&dir, &entry("rec-2", "2026-08-29T00:00:01Z", &record("cli", "2026-08-28T00:00:01Z", "hash-a")));
        // An interesting row with the same hash is promoted anyway.
        append_cold(&dir, &entry("rec-3", "2026-08-29T00:00:02Z", &interesting_record("cli", "2026-08-28T00:00:02Z", "hash-a")));

        let report = promote_tail(&dir, DEFAULT_MAX_BYTES, at("2026-08-29T01:00:00Z")).unwrap();
        assert_eq!(report.scanned, 3);
        assert_eq!(report.promoted, 2);
        assert_eq!(report.skipped, 1);

        let rows = hot_rows(&dir);
        assert_eq!(rows.len(), 2);
        // The hot rows keep the cold ids and created_at.
        let ids: Vec<&str> = rows.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, vec!["rec-1", "rec-3"]);
        assert_eq!(rows[0].created_at, "2026-08-29T00:00:00Z");

        // The sidecar has two hash-a rows (one per promotion) — the set
        // view dedupes to a single pair.
        let hashes = load_hashes(&dir);
        assert_eq!(hashes.len(), 1);
        assert!(hashes.contains(&("cli".to_string(), "hash-a".to_string())));
        let hash_rows: Vec<HashRow> = std::fs::read_to_string(dir.join(HOT_HASHES_FILE))
            .unwrap()
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect();
        assert_eq!(hash_rows.len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn promote_tail_resumes_from_the_saved_offset() {
        let dir = temp_dir("promote-resume.jsonl");
        append_cold(&dir, &entry("rec-1", "2026-08-29T00:00:00Z", &record("cli", "2026-08-28T00:00:00Z", "hash-a")));
        append_cold(&dir, &entry("rec-2", "2026-08-29T00:00:01Z", &record("cli", "2026-08-28T00:00:01Z", "hash-b")));

        let first = promote_tail(&dir, DEFAULT_MAX_BYTES, at("2026-08-29T01:00:00Z")).unwrap();
        assert_eq!(first.scanned, 2);
        assert_eq!(first.promoted, 2);

        // A third row lands after the first scan.
        append_cold(&dir, &entry("rec-3", "2026-08-29T00:00:02Z", &record("cli", "2026-08-28T00:00:02Z", "hash-c")));
        let second = promote_tail(&dir, DEFAULT_MAX_BYTES, at("2026-08-29T02:00:00Z")).unwrap();
        assert_eq!(second.scanned, 1, "only the tail is re-read");
        assert_eq!(second.promoted, 1);

        assert_eq!(hot_rows(&dir).len(), 3);
        let state = load_state(&dir);
        assert_eq!(
            state.last_offset,
            std::fs::metadata(dir.join("records.jsonl")).unwrap().len()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn promote_tail_skips_garbage_and_stalls_on_a_torn_tail() {
        let dir = temp_dir("promote-torn.jsonl");
        append_cold(&dir, &entry("rec-1", "2026-08-29T00:00:00Z", &record("cli", "2026-08-28T00:00:00Z", "hash-a")));
        std::fs::OpenOptions::new()
            .append(true)
            .open(dir.join("records.jsonl"))
            .unwrap()
            .write_all(b"mid-file garbage\n")
            .unwrap();
        // A torn final line: no trailing newline, not valid JSON.
        std::fs::OpenOptions::new()
            .append(true)
            .open(dir.join("records.jsonl"))
            .unwrap()
            .write_all(b"{\"id\": \"rec-")
            .unwrap();

        let report = promote_tail(&dir, DEFAULT_MAX_BYTES, at("2026-08-29T01:00:00Z")).unwrap();
        assert_eq!(report.promoted, 1, "garbage mid-file is skipped");
        let state = load_state(&dir);
        let full_len = std::fs::metadata(dir.join("records.jsonl")).unwrap().len();
        assert!(state.last_offset < full_len, "the torn tail is not consumed");

        // The next scan starts at the torn line again — and still promotes
        // nothing new.
        let again = promote_tail(&dir, DEFAULT_MAX_BYTES, at("2026-08-29T02:00:00Z")).unwrap();
        assert_eq!(again.scanned, 1);
        assert_eq!(again.promoted, 0);
        assert_eq!(hot_rows(&dir).len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn promote_tail_missing_cold_is_an_empty_report() {
        let dir = temp_dir("promote-missing.jsonl");
        let report = promote_tail(&dir, DEFAULT_MAX_BYTES, at("2026-08-29T00:00:00Z")).unwrap();
        assert_eq!(report, PromoteReport::default());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fold_hot_keeps_recent_and_promoted_and_drops_old() {
        let dir = temp_dir("fold-basic.jsonl");
        let old = entry("rec-old", "2026-08-29T00:00:00Z", &record("cli", "2026-01-01T00:00:00Z", "hash-old"));
        let recent = entry("rec-recent", "2026-08-29T00:00:01Z", &record("cli", "2026-08-28T00:00:00Z", "hash-recent"));
        let pinned = entry("rec-pinned", "2026-08-29T00:00:02Z", &record("cli", "2026-01-01T00:00:01Z", "hash-pinned"));
        append_line(&dir.join(HOT_FILE), &old).unwrap();
        append_line(&dir.join(HOT_FILE), &recent).unwrap();
        append_line(&dir.join(HOT_FILE), &pinned).unwrap();
        append_line(
            &dir.join(PROMOTED_FILE),
            &PromotedRecord {
                promoted_at: "2026-08-29T00:00:03Z".to_string(),
                tenant_id: "cli".to_string(),
                record_id: "rec-pinned".to_string(),
            },
        )
        .unwrap();

        let report = fold_hot(&dir, DEFAULT_ROLLUP_DAYS, at("2026-08-29T00:00:00Z")).unwrap();
        assert_eq!(report.folded, 1);
        assert_eq!(report.kept, 2);

        let rows = hot_rows(&dir);
        let ids: Vec<&str> = rows.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, vec!["rec-recent", "rec-pinned"], "original order kept");

        // The hash sidecar is rewritten to match the kept rows.
        let hashes = load_hashes(&dir);
        assert_eq!(hashes.len(), 2);
        assert!(hashes.contains(&("cli".to_string(), "hash-recent".to_string())));
        assert!(hashes.contains(&("cli".to_string(), "hash-pinned".to_string())));

        let state = load_state(&dir);
        let folded_at = at("2026-08-29T00:00:00Z").to_rfc3339();
        assert_eq!(state.last_fold_at.as_deref(), Some(folded_at.as_str()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fold_hot_folds_unparseable_dates_unless_promoted() {
        let dir = temp_dir("fold-baddate.jsonl");
        let bad = entry("rec-bad", "2026-08-29T00:00:00Z", &record("cli", "not-a-date", "hash-bad"));
        let pinned = entry("rec-pinned", "2026-08-29T00:00:01Z", &record("cli", "not-a-date-either", "hash-pinned"));
        append_line(&dir.join(HOT_FILE), &bad).unwrap();
        append_line(&dir.join(HOT_FILE), &pinned).unwrap();
        append_line(
            &dir.join(PROMOTED_FILE),
            &PromotedRecord {
                promoted_at: "2026-08-29T00:00:03Z".to_string(),
                tenant_id: "cli".to_string(),
                record_id: "rec-pinned".to_string(),
            },
        )
        .unwrap();

        let report = fold_hot(&dir, DEFAULT_ROLLUP_DAYS, at("2026-08-29T00:00:00Z")).unwrap();
        assert_eq!(report.folded, 1);
        assert_eq!(report.kept, 1);
        assert_eq!(hot_rows(&dir)[0].id, "rec-pinned");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn auto_rollup_folds_at_most_once_a_day() {
        let dir = temp_dir("auto-daily.jsonl");
        // A never-folded state folds on the first auto rollup.
        let first = auto_rollup(&dir, at("2026-08-29T00:00:00Z")).unwrap().unwrap();
        assert_eq!(first.folded, 0, "empty hot folds nothing, but last_fold_at is set");
        let state = load_state(&dir);
        let folded_at = at("2026-08-29T00:00:00Z").to_rfc3339();
        assert_eq!(state.last_fold_at.as_deref(), Some(folded_at.as_str()));

        // An hour later: promote runs, the fold is gated off. The row is
        // old (January), so the next fold will claim it.
        append_cold(&dir, &entry("rec-1", "2026-08-29T00:01:00Z", &record("cli", "2026-01-01T00:00:00Z", "hash-a")));
        let second = auto_rollup(&dir, at("2026-08-29T01:00:00Z")).unwrap().unwrap();
        assert_eq!(second.promoted, 1);
        assert_eq!(second.folded, 0);

        // 25 hours later the fold runs again and the old row folds.
        let third = auto_rollup(&dir, at("2026-08-30T02:00:00Z")).unwrap().unwrap();
        assert_eq!(third.folded, 1, "the old hot row folds");
        assert_eq!(third.kept, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn auto_rollup_skips_silently_when_the_lock_is_held() {
        let dir = temp_dir("auto-locked.jsonl");
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(dir.join(LOCK_FILE))
            .unwrap();
        assert_eq!(auto_rollup(&dir, at("2026-08-29T00:00:00Z")).unwrap(), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stale_locks_are_reclaimed() {
        let now = SystemTime::now();
        assert!(!is_stale_lock(now, now));
        assert!(is_stale_lock(
            now - Duration::from_secs(STALE_LOCK_MINUTES * 60 + 1),
            now
        ));
        assert!(!is_stale_lock(
            now - Duration::from_secs(STALE_LOCK_MINUTES * 60 - 1),
            now
        ));
    }

    #[test]
    fn rollup_forces_fold_with_explicit_days() {
        let dir = temp_dir("rollup-forced.jsonl");
        let old = entry("rec-old", "2026-08-29T00:00:00Z", &record("cli", "2026-01-01T00:00:00Z", "hash-old"));
        let pinned = entry("rec-pinned", "2026-08-29T00:00:01Z", &record("cli", "2026-01-01T00:00:01Z", "hash-pinned"));
        append_line(&dir.join(HOT_FILE), &old).unwrap();
        append_line(&dir.join(HOT_FILE), &pinned).unwrap();
        append_line(
            &dir.join(PROMOTED_FILE),
            &PromotedRecord {
                promoted_at: "2026-08-29T00:00:03Z".to_string(),
                tenant_id: "cli".to_string(),
                record_id: "rec-pinned".to_string(),
            },
        )
        .unwrap();
        append_cold(&dir, &entry("rec-new", "2026-08-29T00:00:02Z", &record("cli", "2026-08-29T00:00:00Z", "hash-new")));

        // days = 0 folds everything except promoted rows.
        let report = rollup(&dir, 0, DEFAULT_MAX_BYTES, at("2026-08-29T00:00:00Z")).unwrap();
        assert_eq!(report.promoted, 1, "the cold tail is promoted too");
        assert_eq!(report.folded, 1);
        assert_eq!(report.kept, 1);
        let rows = hot_rows(&dir);
        let ids: Vec<&str> = rows.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, vec!["rec-pinned", "rec-new"], "kept rows first, then the promoted tail");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rollup_errors_when_the_lock_is_held() {
        let dir = temp_dir("rollup-locked.jsonl");
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(dir.join(LOCK_FILE))
            .unwrap();
        let err = rollup(&dir, DEFAULT_ROLLUP_DAYS, DEFAULT_MAX_BYTES, at("2026-08-29T00:00:00Z"))
            .unwrap_err();
        assert!(err.contains("busy"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rollup_dry_run_estimates_without_writing() {
        let dir = temp_dir("dry-run.jsonl");
        append_cold(&dir, &entry("rec-1", "2026-08-29T00:00:00Z", &record("cli", "2026-08-28T00:00:00Z", "hash-a")));
        append_cold(&dir, &entry("rec-2", "2026-08-29T00:00:01Z", &record("cli", "2026-08-28T00:00:01Z", "hash-a")));
        // One old hot row that a fold would claim.
        append_line(&dir.join(HOT_FILE), &entry("rec-old", "2026-08-29T00:00:02Z", &record("cli", "2026-01-01T00:00:00Z", "hash-old"))).unwrap();

        let report =
            rollup_dry_run(&dir, DEFAULT_ROLLUP_DAYS, DEFAULT_MAX_BYTES, at("2026-08-29T00:00:00Z"))
                .unwrap();
        assert_eq!(report.promoted, 1, "one novel tail row");
        assert_eq!(report.skipped, 1, "the duplicate hash row");
        assert_eq!(report.folded, 1);
        assert_eq!(report.kept, 0);

        // Nothing was written: no state, no hashes, hot untouched.
        assert!(!dir.join(ROLLUP_STATE_FILE).exists());
        assert!(!dir.join(HOT_HASHES_FILE).exists());
        assert_eq!(hot_rows(&dir).len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn promote_record_pins_a_case_and_is_idempotent() {
        let dir = temp_dir("promote-record.jsonl");
        append_cold(&dir, &entry("rec-1", "2026-08-29T00:00:00Z", &record("cli", "2026-08-28T00:00:00Z", "hash-a")));

        let outcome = promote_record(&dir, "rec-1", DEFAULT_MAX_BYTES, at("2026-08-29T01:00:00Z")).unwrap();
        assert_eq!(outcome, PromoteOutcome::Promoted);
        let rows = hot_rows(&dir);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "rec-1", "the hot copy keeps the cold id");
        assert_eq!(rows[0].created_at, "2026-08-29T00:00:00Z");
        assert!(load_hashes(&dir).contains(&("cli".to_string(), "hash-a".to_string())));

        let again = promote_record(&dir, "rec-1", DEFAULT_MAX_BYTES, at("2026-08-29T02:00:00Z")).unwrap();
        assert_eq!(again, PromoteOutcome::AlreadyPromoted);
        assert_eq!(hot_rows(&dir).len(), 1, "idempotent");
        assert_eq!(promoted_ids(&dir), HashSet::from(["rec-1".to_string()]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn promote_record_refuses_unknown_ids() {
        let dir = temp_dir("promote-unknown.jsonl");
        append_cold(&dir, &entry("rec-1", "2026-08-29T00:00:00Z", &record("cli", "2026-08-28T00:00:00Z", "hash-a")));
        let err = promote_record(&dir, "rec-404", DEFAULT_MAX_BYTES, at("2026-08-29T00:00:00Z"))
            .unwrap_err();
        assert!(err.contains("unknown record id rec-404"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn promote_record_errors_when_the_lock_is_held() {
        let dir = temp_dir("promote-locked.jsonl");
        append_cold(&dir, &entry("rec-1", "2026-08-29T00:00:00Z", &record("cli", "2026-08-28T00:00:00Z", "hash-a")));
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(dir.join(LOCK_FILE))
            .unwrap();
        let err = promote_record(&dir, "rec-1", DEFAULT_MAX_BYTES, at("2026-08-29T00:00:00Z"))
            .unwrap_err();
        assert!(err.contains("busy"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn list_records_is_newest_first_tenant_filtered_and_flags_promotions() {
        let dir = temp_dir("list.jsonl");
        append_cold(&dir, &entry("rec-1", "2026-08-29T00:00:00Z", &record("cli", "2026-08-27T00:00:00Z", "hash-1")));
        append_cold(&dir, &entry("rec-2", "2026-08-29T00:00:01Z", &record("telegram:111", "2026-08-28T00:00:00Z", "hash-2")));
        append_cold(&dir, &entry("rec-3", "2026-08-29T00:00:02Z", &record("cli", "2026-08-28T00:00:00Z", "hash-3")));
        append_line(
            &dir.join(PROMOTED_FILE),
            &PromotedRecord {
                promoted_at: "2026-08-29T00:00:03Z".to_string(),
                tenant_id: "cli".to_string(),
                record_id: "rec-3".to_string(),
            },
        )
        .unwrap();

        let rows = list_records(&dir, "cli", 10).unwrap();
        let ids: Vec<&str> = rows.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, vec!["rec-3", "rec-1"], "newest first, tenant filtered");
        assert!(rows[0].promoted);
        assert!(!rows[1].promoted);
        assert_eq!(rows[0].verification.as_deref(), Some("complete"));

        assert!(list_records(&dir, "cli", 1).unwrap().len() == 1);
        assert!(list_records(&dir, "nobody", 10).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn state_round_trips_through_the_atomic_save() {
        let dir = temp_dir("state.jsonl");
        let state = RollupState {
            last_offset: 1234,
            last_fold_at: Some("2026-08-29T00:00:00Z".to_string()),
            updated_at: "2026-08-29T01:00:00Z".to_string(),
        };
        save_state(&dir, &state).unwrap();
        assert_eq!(load_state(&dir), state);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
