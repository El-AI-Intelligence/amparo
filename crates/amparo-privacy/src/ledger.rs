//! The privacy ledger — a local, always-on record of what leaves the
//! machine and what was stripped before it left.
//!
//! Two row kinds: [`LedgerKind::NetworkCall`] records every execution
//! attempt of a tool that reaches beyond the machine (`web_search`,
//! `fetch_url`, `run_command`) with the gate's human provenance, and
//! [`LedgerKind::PiiStrip`] records that PII was stripped before inference
//! — per-category counts, never the values.
//!
//! The ledger answers the audit question "who allowed this, and under what
//! policy": for every executed action that touched the network, the row
//! carries whether a human approved or denied it. It is deliberately NOT
//! behind the `Memory` trait — it is a keyword-free, append-only evidence
//! log, not a content store. Storage is [`LedgerStore`]: one JSON line per
//! row at `<workspace>/.amparo/privacy/ledger.jsonl`, audit-log style.
//! Under a [`LedgerQuota`] the file is bounded: an append that would
//! exceed the quota rotates the ledger — the oldest rows are dropped
//! until the survivors fit in half the quota, rewritten atomically, and
//! a [`LedgerKind::Rotated`] marker row records how many were dropped.
//! The quota itself is recorded in a sidecar file beside the ledger (see
//! [`recorded_quota`]), so the reviewer surface can report the bound
//! without the ledger file carrying any header.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use serde::{Deserialize, Serialize};

/// What kind of row this is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LedgerKind {
    /// An execution attempt of a tool that reaches beyond the machine.
    NetworkCall,
    /// PII was stripped before it left the machine.
    PiiStrip,
    /// A rotation marker: the file exceeded its quota and the `n`
    /// oldest rows were dropped ([`LedgerRow::dropped_rows`] says how
    /// many). Written as the newest row so the drop is itself part of
    /// the audit trail.
    Rotated,
}

/// One ledger row, appended as one JSON line.
///
/// # Privacy rule (the invariant this type enforces)
///
/// A row never contains the *value* of anything sensitive: `site` holds a
/// scheme + host at most (never a path or query), `run_command` rows carry
/// no command text at all, and `pii_counts` holds per-category counts of
/// what was stripped — never the stripped values. The placeholder map is
/// discarded at the strip site, so it cannot reach the ledger.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LedgerRow {
    /// RFC 3339 timestamp of the row.
    pub ts: String,
    /// Tenant tag — `cli` for runs, `platform:user_id` for chat tasks.
    pub tenant: String,
    /// The task that produced the row (M8): the agent's task id —
    /// `sess-123.1` for a spawned sub-agent — so every row answers
    /// "whose action was this". Absent on rows written before M8.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    /// The parent task's id when this row's task is a sub-agent (M8) —
    /// the delegation chain, explicit on the row. Absent for top-level
    /// tasks and pre-M8 rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_task_id: Option<String>,
    /// The row's kind.
    pub kind: LedgerKind,
    /// `NetworkCall`: the tool name. `PiiStrip`: absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    /// `NetworkCall` `fetch_url`: scheme + host only — path and query are
    /// dropped. `run_command` rows never carry the command. `PiiStrip`:
    /// absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub site: Option<String>,
    /// `NetworkCall`: `ok` when the execution succeeded, `error` when it
    /// failed, `denied` when the human denied it before execution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    /// `NetworkCall`: `human_approved` / `human_denied` when the human
    /// approval gate ran; absent when no human was asked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gate: Option<String>,
    /// `PiiStrip`: per-category counts of what was stripped (`email`,
    /// `phone`, `ssn`, …) — counts, never the values.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pii_counts: Vec<(String, usize)>,
    /// `Rotated`: how many oldest rows this rotation dropped. Every
    /// other kind: absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dropped_rows: Option<usize>,
}

/// The ledger's directory under a workspace root:
/// `<workspace>/.amparo/privacy/` — the host appends `ledger.jsonl`.
pub fn privacy_dir(workspace_root: &Path) -> PathBuf {
    workspace_root.join(".amparo").join("privacy")
}

/// The sidecar filename (beside the ledger) recording the last
/// configured quota, in bytes — see [`recorded_quota`].
const QUOTA_SIDECAR: &str = "quota";

/// The quota last recorded beside the ledger at `path` — what
/// [`LedgerStore::open_with_quota`] wrote the last time a host opened the
/// file with a quota. `None` when no quota was ever recorded, or when the
/// sidecar is missing or unreadable. Reading is side-effect free: the
/// reviewer surface (`amparo privacy`) can report the bound without ever
/// touching the ledger.
pub fn recorded_quota(path: &Path) -> Option<LedgerQuota> {
    let sidecar = path.with_file_name(QUOTA_SIDECAR);
    let text = std::fs::read_to_string(&sidecar).ok()?;
    let bytes = text.trim().parse::<u64>().ok()?;
    Some(LedgerQuota::new(bytes))
}

/// Read every row of the ledger at `path` without opening a store — the
/// read-only surface behind `amparo privacy`, which must never create,
/// delete or rewrite anything (sidecar included) just by reading. Rows
/// whose lines fail to parse are skipped, exactly like
/// [`LedgerStore::read_all`].
pub fn read_ledger(path: &Path) -> Result<Vec<LedgerRow>, String> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read ledger {}: {e}", path.display()))?;
    Ok(content
        .lines()
        .filter_map(|line| serde_json::from_str::<LedgerRow>(line).ok())
        .collect())
}

/// Reduce a URL to `scheme://host[:port]` — the most a [`LedgerRow`] may
/// carry. Paths, queries and fragments are dropped; anything without a
/// `scheme://` prefix yields `None`.
pub fn site_host_only(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    let host_and_port = rest.split(['/', '?', '#']).next()?;
    if host_and_port.is_empty() {
        return None;
    }
    Some(format!("{}://{}", scheme, host_and_port))
}

/// A size bound on the ledger file, in bytes.
///
/// When an append would leave the file larger than
/// [`LedgerQuota::max_bytes`], [`LedgerStore`] rotates: the oldest rows
/// are dropped until the survivors fit in half the quota, the survivors
/// are rewritten atomically (tmp + rename), and a
/// [`LedgerKind::Rotated`] marker is appended as the newest row so the
/// drop is itself part of the audit trail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LedgerQuota {
    /// Maximum file size in bytes; exceeding it rotates the ledger.
    pub max_bytes: u64,
}

impl LedgerQuota {
    /// A quota of `max_bytes` bytes.
    pub fn new(max_bytes: u64) -> Self {
        Self { max_bytes }
    }
}

/// An append-only, file-backed ledger store.
///
/// `append` writes one JSON line and flushes, so a crash loses at most the
/// in-flight row's final flush; `read_all` reads the whole file, skipping
/// lines that fail to parse (an interrupted write can leave a partial
/// final line). File I/O is synchronous, matching the notebook's
/// `JsonlStore` precedent; the store is cheap to clone behind an `Arc`.
/// Appends hold the file mutex, and so does rotation — writers serialize,
/// and readers (who read via the path) see the old file or the new one,
/// never a mix.
pub struct LedgerStore {
    path: PathBuf,
    file: Mutex<File>,
    /// Size bound that triggers rotation; `None` = unbounded.
    quota: Option<u64>,
}

impl LedgerStore {
    /// Open (or create) the ledger at `path`, unbounded, creating parent
    /// directories as needed — the workspace may not exist yet when a
    /// host opens its ledger.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, String> {
        Self::open_with_quota(path, None)
    }

    /// Open (or create) the ledger at `path` under `quota`. `None` is the
    /// unbounded default — the file grows with every row, exactly as
    /// [`LedgerStore::open`]. `Some` bounds the file: the first append
    /// that would leave it larger than [`LedgerQuota::max_bytes`] rotates
    /// (see [`LedgerQuota`]).
    ///
    /// The configured bound is recorded beside the ledger (see
    /// [`recorded_quota`]): `Some` writes the sidecar, `None` removes it,
    /// so the reviewer surface never claims a bound that is not currently
    /// enforced. The sidecar write is best-effort — the bound itself is
    /// enforced by this store regardless.
    pub fn open_with_quota(
        path: impl AsRef<Path>,
        quota: Option<LedgerQuota>,
    ) -> Result<Self, String> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("cannot create ledger directory {}: {e}", parent.display()))?;
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| format!("cannot open ledger {}: {e}", path.display()))?;
        let bound = quota.map(|q| q.max_bytes);
        let sidecar = path.with_file_name(QUOTA_SIDECAR);
        match bound {
            Some(bytes) => {
                let _ = std::fs::write(&sidecar, bytes.to_string());
            }
            None => {
                let _ = std::fs::remove_file(&sidecar);
            }
        }
        Ok(Self { path, file: Mutex::new(file), quota: bound })
    }

    /// The path this store appends to.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one row as one JSON line, flushed to disk. Under a quota,
    /// an append that leaves the file over the bound rotates the ledger
    /// first (see [`LedgerQuota`]) — the appended row is never dropped
    /// by its own rotation.
    pub fn append(&self, row: &LedgerRow) -> Result<(), String> {
        let line = serde_json::to_string(row).map_err(|e| format!("ledger row serialization failed: {e}"))?;
        let mut file = self.file.lock().unwrap();
        file.write_all(line.as_bytes())
            .and_then(|_| file.write_all(b"\n"))
            .and_then(|_| file.flush())
            .map_err(|e| format!("ledger append failed: {e}"))?;
        if let Some(max_bytes) = self.quota {
            if file
                .metadata()
                .map_err(|e| format!("ledger stat failed: {e}"))?
                .len()
                > max_bytes
            {
                self.rotate(&mut file, max_bytes)?;
            }
        }
        Ok(())
    }

    /// Rotate the ledger: drop the oldest rows until the survivors fit
    /// in half of `max_bytes`, rewrite them atomically (a sibling tmp
    /// file + rename — readers never observe a partial file), and
    /// append a [`LedgerKind::Rotated`] marker as the newest row. The
    /// newest row survives even when it alone exceeds the half-size
    /// target, so the file never grows past a single oversized row.
    /// Callers hold `self.file`; the handle is reopened onto the
    /// renamed path before returning.
    fn rotate(&self, file: &mut MutexGuard<'_, File>, max_bytes: u64) -> Result<(), String> {
        let content = std::fs::read_to_string(&self.path)
            .map_err(|e| format!("cannot read ledger {}: {e}", self.path.display()))?;
        let rows: Vec<LedgerRow> = content
            .lines()
            .filter_map(|line| serde_json::from_str::<LedgerRow>(line).ok())
            .collect();
        if rows.len() <= 1 {
            // One oversized row is the best bound available; rewriting
            // it would change nothing.
            return Ok(());
        }
        // Walk from the newest backwards: keep rows while they fit in
        // half the quota; everything older is dropped as a prefix.
        let target = (max_bytes / 2).max(1);
        let mut survivors: Vec<&LedgerRow> = Vec::new();
        let mut kept_bytes = 0u64;
        let mut dropped = 0usize;
        for (index, row) in rows.iter().enumerate().rev() {
            let row_bytes = serde_json::to_string(row)
                .map_err(|e| format!("ledger row serialization failed: {e}"))?
                .len() as u64
                + 1; // the trailing newline
            if survivors.is_empty() || kept_bytes + row_bytes <= target {
                survivors.push(row);
                kept_bytes += row_bytes;
            } else {
                dropped = index + 1; // rows[0..=index] are all older
                break;
            }
        }
        if dropped == 0 {
            return Ok(());
        }
        survivors.reverse();

        let tmp = self.path.with_extension("jsonl.tmp");
        {
            let mut tmp_file = OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(&tmp)
                .map_err(|e| format!("cannot open ledger tmp {}: {e}", tmp.display()))?;
            let marker = LedgerRow {
                ts: chrono::Utc::now().to_rfc3339(),
                // The marker belongs to the file it describes — tag it
                // with the newest surviving row's tenant.
                tenant: survivors
                    .last()
                    .map(|row| row.tenant.clone())
                    .unwrap_or_else(|| "ledger".to_string()),
                task_id: None,
                parent_task_id: None,
                kind: LedgerKind::Rotated,
                tool: None,
                site: None,
                outcome: None,
                gate: None,
                pii_counts: Vec::new(),
                dropped_rows: Some(dropped),
            };
            for row in survivors.iter().copied().chain(std::iter::once(&marker)) {
                let serialized = serde_json::to_string(row)
                    .map_err(|e| format!("ledger row serialization failed: {e}"))?;
                tmp_file
                    .write_all(serialized.as_bytes())
                    .and_then(|_| tmp_file.write_all(b"\n"))
                    .map_err(|e| format!("ledger rotation write failed: {e}"))?;
            }
            tmp_file
                .flush()
                .map_err(|e| format!("ledger rotation flush failed: {e}"))?;
        }
        std::fs::rename(&tmp, &self.path)
            .map_err(|e| format!("cannot rotate ledger {}: {e}", self.path.display()))?;
        // The append handle still points at the pre-rename inode —
        // reopen onto the rotated file or the next append is lost.
        let reopened = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|e| format!("cannot reopen ledger {} after rotation: {e}", self.path.display()))?;
        **file = reopened;
        Ok(())
    }

    /// Read every row in order, skipping lines that fail to parse (a crash
    /// mid-append can leave a partial final line — the audit trail stays
    /// readable). Identical to [`read_ledger`].
    pub fn read_all(&self) -> Result<Vec<LedgerRow>, String> {
        read_ledger(&self.path)
    }

    /// Aggregate the ledger into one [`LedgerSummary`].
    pub fn summary(&self) -> Result<LedgerSummary, String> {
        Ok(LedgerSummary::compute(&self.read_all()?))
    }
}

/// Aggregated ledger statistics — the "summary first" view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LedgerSummary {
    /// Total [`LedgerKind::NetworkCall`] rows.
    pub network_calls: usize,
    /// Total [`LedgerKind::PiiStrip`] rows.
    pub pii_strips: usize,
    /// Network calls a human approved.
    pub human_approved: usize,
    /// Network calls a human denied.
    pub human_denied: usize,
    /// Network-call counts per tool, sorted by tool name.
    pub by_tool: Vec<(String, usize)>,
    /// Stripped PII counts per category, sorted by category name.
    pub pii_by_category: Vec<(String, usize)>,
    /// Total [`LedgerKind::Rotated`] marker rows — how many times the
    /// ledger has rotated.
    pub rotations: usize,
    /// Rows dropped across every rotation, summed from the markers.
    pub rows_dropped: usize,
}

impl LedgerSummary {
    /// Aggregate `rows` into a summary. Pure — unit-testable without a store.
    pub fn compute(rows: &[LedgerRow]) -> Self {
        let mut summary = Self {
            network_calls: 0,
            pii_strips: 0,
            human_approved: 0,
            human_denied: 0,
            by_tool: Vec::new(),
            pii_by_category: Vec::new(),
            rotations: 0,
            rows_dropped: 0,
        };
        let mut by_tool: BTreeMap<String, usize> = BTreeMap::new();
        let mut pii_by_category: BTreeMap<String, usize> = BTreeMap::new();
        for row in rows {
            match row.kind {
                LedgerKind::NetworkCall => {
                    summary.network_calls += 1;
                    if let Some(tool) = &row.tool {
                        *by_tool.entry(tool.clone()).or_default() += 1;
                    }
                    match row.gate.as_deref() {
                        Some("human_approved") => summary.human_approved += 1,
                        Some("human_denied") => summary.human_denied += 1,
                        _ => {}
                    }
                }
                LedgerKind::PiiStrip => {
                    summary.pii_strips += 1;
                    for (category, count) in &row.pii_counts {
                        *pii_by_category.entry(category.clone()).or_default() += count;
                    }
                }
                LedgerKind::Rotated => {
                    summary.rotations += 1;
                    summary.rows_dropped += row.dropped_rows.unwrap_or(0);
                }
            }
        }
        summary.by_tool = by_tool.into_iter().collect();
        summary.pii_by_category = pii_by_category.into_iter().collect();
        summary
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "amparo-privacy-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn row(kind: LedgerKind) -> LedgerRow {
        LedgerRow {
            ts: "2026-08-29T00:00:00+00:00".to_string(),
            tenant: "cli".to_string(),
            task_id: None,
            parent_task_id: None,
            kind,
            tool: None,
            site: None,
            outcome: None,
            gate: None,
            pii_counts: Vec::new(),
            dropped_rows: None,
        }
    }

    fn network_row(tool: &str) -> LedgerRow {
        LedgerRow {
            tool: Some(tool.to_string()),
            outcome: Some("ok".to_string()),
            ..row(LedgerKind::NetworkCall)
        }
    }

    #[test]
    fn append_and_read_round_trip() {
        let dir = temp_dir();
        let store = LedgerStore::open(dir.join("nested").join("ledger.jsonl")).unwrap();
        let network = LedgerRow {
            tool: Some("fetch_url".to_string()),
            site: Some("https://example.com".to_string()),
            outcome: Some("ok".to_string()),
            gate: Some("human_approved".to_string()),
            ..row(LedgerKind::NetworkCall)
        };
        let pii = LedgerRow {
            pii_counts: vec![("email".to_string(), 2), ("phone".to_string(), 1)],
            ..row(LedgerKind::PiiStrip)
        };
        store.append(&network).unwrap();
        store.append(&pii).unwrap();

        let rows = store.read_all().unwrap();
        assert_eq!(rows, vec![network, pii]);
    }

    #[test]
    fn open_creates_parent_directories() {
        let dir = temp_dir();
        let store = LedgerStore::open(dir.join("a").join("b").join("ledger.jsonl")).unwrap();
        store.append(&row(LedgerKind::PiiStrip)).unwrap();
        assert_eq!(store.read_all().unwrap().len(), 1);
    }

    #[test]
    fn partial_final_line_is_skipped() {
        let dir = temp_dir();
        let path = dir.join("ledger.jsonl");
        let store = LedgerStore::open(&path).unwrap();
        store.append(&row(LedgerKind::PiiStrip)).unwrap();
        // Simulate a crash mid-append: an unterminated partial line.
        let partial = serde_json::to_string(&row(LedgerKind::NetworkCall)).unwrap();
        let half: String = partial.chars().take(partial.len() / 2).collect();
        std::fs::OpenOptions::new().append(true).open(&path).unwrap().write_all(half.as_bytes()).unwrap();
        assert_eq!(store.read_all().unwrap().len(), 1);
    }

    #[test]
    fn summary_counts_rows_and_gates_and_categories() {
        let approved = LedgerRow {
            tool: Some("web_search".to_string()),
            outcome: Some("ok".to_string()),
            gate: Some("human_approved".to_string()),
            ..row(LedgerKind::NetworkCall)
        };
        let denied = LedgerRow {
            tool: Some("run_command".to_string()),
            gate: Some("human_denied".to_string()),
            ..row(LedgerKind::NetworkCall)
        };
        let gated_out = LedgerRow {
            tool: Some("fetch_url".to_string()),
            outcome: Some("error".to_string()),
            ..row(LedgerKind::NetworkCall)
        };
        let pii = LedgerRow {
            pii_counts: vec![("email".to_string(), 2), ("ssn".to_string(), 1)],
            ..row(LedgerKind::PiiStrip)
        };
        let summary = LedgerSummary::compute(&[approved, denied, gated_out, pii]);
        assert_eq!(summary.network_calls, 3);
        assert_eq!(summary.pii_strips, 1);
        assert_eq!(summary.human_approved, 1);
        assert_eq!(summary.human_denied, 1);
        assert_eq!(
            summary.by_tool,
            vec![
                ("fetch_url".to_string(), 1),
                ("run_command".to_string(), 1),
                ("web_search".to_string(), 1)
            ]
        );
        assert_eq!(
            summary.pii_by_category,
            vec![("email".to_string(), 2), ("ssn".to_string(), 1)]
        );
    }

    #[test]
    fn serialized_rows_never_contain_pii_shapes() {
        // Real strip output feeds the counts; the row must carry counts
        // only. Assert the serialized line contains none of the values or
        // value shapes the strip would have replaced.
        let text = "mail alice@example.com, call 555-123-4567, SSN 123-45-6789";
        let stripped = crate::secure_minions_strip(text);
        let mut counts: Vec<(String, usize)> = Vec::new();
        for p in &stripped.pii_map {
            match counts.iter_mut().find(|(c, _)| c == &p.category) {
                Some((_, n)) => *n += 1,
                None => counts.push((p.category.clone(), 1)),
            }
        }
        let row = LedgerRow { pii_counts: counts, ..row(LedgerKind::PiiStrip) };
        let line = serde_json::to_string(&row).unwrap();
        assert!(!line.contains("alice@example.com"));
        assert!(!line.contains("555-123-4567"));
        assert!(!line.contains("123-45-6789"));
        assert!(line.contains("\"email\""));
    }

    #[test]
    fn site_host_only_drops_path_query_and_fragment() {
        assert_eq!(
            site_host_only("https://example.com/a/b?q=1#frag"),
            Some("https://example.com".to_string())
        );
        assert_eq!(site_host_only("http://127.0.0.1:11434/api"), Some("http://127.0.0.1:11434".to_string()));
        assert_eq!(site_host_only("not a url"), None);
    }

    #[test]
    fn run_command_rows_never_carry_a_command() {
        // The row shape itself has no field for a command — prove the
        // serialized form carries no command payload text (the tool name
        // itself legitimately contains the word "command").
        let row = LedgerRow {
            tool: Some("run_command".to_string()),
            outcome: Some("ok".to_string()),
            ..row(LedgerKind::NetworkCall)
        };
        let line = serde_json::to_string(&row).unwrap();
        assert!(!line.contains("sudo"));
        assert!(!line.contains("curl"));
        assert!(!line.contains("rm -rf"));
    }

    #[test]
    fn over_quota_append_rotates_and_the_marker_survives_as_newest() {
        let dir = temp_dir();
        let path = dir.join("ledger.jsonl");
        let store = LedgerStore::open_with_quota(&path, Some(LedgerQuota::new(600))).unwrap();

        // Append until the first rotation fires, recording every row.
        let mut written: Vec<LedgerRow> = Vec::new();
        for i in 0..1000 {
            let row = network_row(&format!("tool_{i:03}"));
            written.push(row.clone());
            store.append(&row).unwrap();
            if store.read_all().unwrap().iter().any(|r| r.kind == LedgerKind::Rotated) {
                break;
            }
        }
        assert!(written.len() < 1000, "the quota must fire well before 1000 rows");

        let rows = store.read_all().unwrap();
        assert_eq!(
            rows.iter().filter(|r| r.kind == LedgerKind::Rotated).count(),
            1,
            "exactly one rotation so far"
        );
        let marker = rows.last().unwrap();
        assert_eq!(marker.kind, LedgerKind::Rotated);
        assert_eq!(marker.tenant, "cli", "the marker carries the file's tenant");

        // The survivors are exactly a suffix of what was written, in
        // order — oldest dropped, newest kept.
        let survivors: Vec<&LedgerRow> =
            rows.iter().filter(|r| r.kind != LedgerKind::Rotated).collect();
        assert_eq!(
            survivors.len() + marker.dropped_rows.unwrap(),
            written.len(),
            "the dropped count accounts for every missing row"
        );
        for (index, survivor) in survivors.iter().enumerate() {
            let expected = &written[written.len() - survivors.len() + index];
            assert_eq!(*survivor, expected);
        }

        // The rewrite respects the quota.
        let size = std::fs::metadata(&path).unwrap().len();
        assert!(size <= 600, "file stays within quota: {size}");
    }

    #[test]
    fn rotation_drops_the_oldest_and_reports_the_count() {
        let dir = temp_dir();
        // Rows are ~110 bytes and the half-size target is 80, so each
        // rotation keeps only the newest row: every append past the
        // first triggers one, and each rotation drops the two rows that
        // came before (the previous row and the previous marker) —
        // `dropped_rows` reports exactly that.
        let store = LedgerStore::open_with_quota(dir.join("ledger.jsonl"), Some(LedgerQuota::new(160))).unwrap();
        let mut last = network_row("t0");
        store.append(&last).unwrap();
        for i in 1..6 {
            last = network_row(&format!("t{i}"));
            store.append(&last).unwrap();
        }
        let rows = store.read_all().unwrap();
        assert_eq!(rows.len(), 2, "newest row + one marker: {rows:?}");
        assert_eq!(rows[0], last, "the newest row survives alone");
        assert_eq!(rows[1].kind, LedgerKind::Rotated);
        assert_eq!(rows[1].dropped_rows, Some(2), "the previous row and the previous marker were dropped");
    }

    #[test]
    fn rotation_is_one_per_burst_not_one_per_append() {
        let dir = temp_dir();
        let path = dir.join("ledger.jsonl");
        let store = LedgerStore::open_with_quota(&path, Some(LedgerQuota::new(600))).unwrap();
        for i in 0..40 {
            store.append(&network_row(&format!("tool_{i:03}"))).unwrap();
        }
        let rows = store.read_all().unwrap();
        let rotations = rows.iter().filter(|r| r.kind == LedgerKind::Rotated).count();
        assert!(rotations >= 1, "the quota fires");
        assert!(
            rotations <= 40 / 2,
            "a rotation per append would be 40; the half-size target cycles in bursts: {rotations}"
        );
        assert!(
            std::fs::metadata(&path).unwrap().len() <= 600,
            "the file stays within quota after every append"
        );
    }

    #[test]
    fn atomic_rotation_leaves_no_tmp_and_every_line_parses() {
        let dir = temp_dir();
        let path = dir.join("ledger.jsonl");
        let store = LedgerStore::open_with_quota(&path, Some(LedgerQuota::new(600))).unwrap();
        for i in 0..40 {
            store.append(&network_row(&format!("tool_{i:03}"))).unwrap();
        }
        // The rewrite went through the tmp sibling and rename — only
        // the ledger and the quota sidecar remain, and every line is a
        // complete row.
        let mut entries: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        entries.sort();
        assert_eq!(entries, vec!["ledger.jsonl".to_string(), QUOTA_SIDECAR.to_string()]);
        let content = std::fs::read_to_string(&path).unwrap();
        for line in content.lines() {
            assert!(
                serde_json::from_str::<LedgerRow>(line).is_ok(),
                "every line parses: {line}"
            );
        }
    }

    #[test]
    fn unbounded_default_never_rotates() {
        let dir = temp_dir();
        let store = LedgerStore::open(dir.join("ledger.jsonl")).unwrap();
        for i in 0..200 {
            store.append(&network_row(&format!("tool_{i:03}"))).unwrap();
        }
        let rows = store.read_all().unwrap();
        assert_eq!(rows.len(), 200);
        assert!(rows.iter().all(|r| r.kind != LedgerKind::Rotated));
    }

    #[test]
    fn summary_counts_rotations_and_dropped_rows() {
        let first = LedgerRow { dropped_rows: Some(3), ..row(LedgerKind::Rotated) };
        let call = LedgerRow {
            tool: Some("fetch_url".to_string()),
            outcome: Some("ok".to_string()),
            ..row(LedgerKind::NetworkCall)
        };
        let second = LedgerRow { dropped_rows: Some(1), ..row(LedgerKind::Rotated) };
        let pii = LedgerRow {
            pii_counts: vec![("email".to_string(), 1)],
            ..row(LedgerKind::PiiStrip)
        };
        let summary = LedgerSummary::compute(&[first, call, second, pii]);
        assert_eq!(summary.rotations, 2);
        assert_eq!(summary.rows_dropped, 4);
        assert_eq!(summary.network_calls, 1, "markers are not network calls");
        assert_eq!(summary.pii_strips, 1);
    }

    #[test]
    fn quota_sidecar_is_written_removed_and_read_back() {
        let dir = temp_dir();
        let path = dir.join("ledger.jsonl");

        // No ledger, no sidecar: nothing was ever recorded.
        assert_eq!(recorded_quota(&path), None);

        // A bounded open records the bound; the review surface reads it.
        LedgerStore::open_with_quota(&path, Some(LedgerQuota::new(4096))).unwrap();
        assert_eq!(recorded_quota(&path), Some(LedgerQuota::new(4096)));

        // An unbounded open clears the record — the reviewer must never
        // be shown a bound that is not currently enforced.
        LedgerStore::open(&path).unwrap();
        assert_eq!(recorded_quota(&path), None);

        // A garbage sidecar reads as "never recorded", not a wrong bound.
        std::fs::write(dir.join(QUOTA_SIDECAR), "not-a-number").unwrap();
        assert_eq!(recorded_quota(&path), None);
    }

    #[test]
    fn rows_written_before_task_ids_parse_with_none() {
        // A pre-M8 line carries no `task_id`/`parent_task_id` keys — the
        // serde defaults must read it, or old ledgers stop parsing.
        let dir = temp_dir();
        let path = dir.join("ledger.jsonl");
        std::fs::write(
            &path,
            r#"{"ts":"2026-08-28T00:00:00+00:00","tenant":"cli","kind":"network_call","tool":"fetch_url","site":"https://example.com","outcome":"ok","gate":"human_approved","pii_counts":[]}"#,
        )
        .unwrap();
        let rows = read_ledger(&path).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].task_id, None);
        assert_eq!(rows[0].parent_task_id, None);
        assert_eq!(rows[0].tool.as_deref(), Some("fetch_url"));
    }

    #[test]
    fn read_ledger_matches_read_all_without_opening() {
        let dir = temp_dir();
        let path = dir.join("ledger.jsonl");
        let store = LedgerStore::open(&path).unwrap();
        store.append(&network_row("fetch_url")).unwrap();
        store.append(&row(LedgerKind::PiiStrip)).unwrap();

        let via_free = read_ledger(&path).unwrap();
        let via_store = store.read_all().unwrap();
        assert_eq!(via_free, via_store);
        // The read-only surface never created the sidecar either.
        assert!(!dir.join(QUOTA_SIDECAR).exists());
    }
}
