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

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// What kind of row this is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LedgerKind {
    /// An execution attempt of a tool that reaches beyond the machine.
    NetworkCall,
    /// PII was stripped before it left the machine.
    PiiStrip,
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
}

/// The ledger's directory under a workspace root:
/// `<workspace>/.amparo/privacy/` — the host appends `ledger.jsonl`.
pub fn privacy_dir(workspace_root: &Path) -> PathBuf {
    workspace_root.join(".amparo").join("privacy")
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

/// An append-only, file-backed ledger store.
///
/// `append` writes one JSON line and flushes, so a crash loses at most the
/// in-flight row's final flush; `read_all` reads the whole file, skipping
/// lines that fail to parse (an interrupted write can leave a partial
/// final line). File I/O is synchronous, matching the notebook's
/// `JsonlStore` precedent; the store is cheap to clone behind an `Arc`.
pub struct LedgerStore {
    path: PathBuf,
    file: Mutex<File>,
}

impl LedgerStore {
    /// Open (or create) the ledger at `path`, creating parent directories
    /// as needed — the workspace may not exist yet when a host opens its
    /// ledger.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, String> {
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
        Ok(Self { path, file: Mutex::new(file) })
    }

    /// The path this store appends to.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one row as one JSON line, flushed to disk.
    pub fn append(&self, row: &LedgerRow) -> Result<(), String> {
        let line = serde_json::to_string(row).map_err(|e| format!("ledger row serialization failed: {e}"))?;
        let mut file = self.file.lock().unwrap();
        file.write_all(line.as_bytes())
            .and_then(|_| file.write_all(b"\n"))
            .and_then(|_| file.flush())
            .map_err(|e| format!("ledger append failed: {e}"))
    }

    /// Read every row in order, skipping lines that fail to parse (a crash
    /// mid-append can leave a partial final line — the audit trail stays
    /// readable).
    pub fn read_all(&self) -> Result<Vec<LedgerRow>, String> {
        let content = std::fs::read_to_string(&self.path)
            .map_err(|e| format!("cannot read ledger {}: {e}", self.path.display()))?;
        Ok(content
            .lines()
            .filter_map(|line| serde_json::from_str::<LedgerRow>(line).ok())
            .collect())
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
            kind,
            tool: None,
            site: None,
            outcome: None,
            gate: None,
            pii_counts: Vec::new(),
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
}
