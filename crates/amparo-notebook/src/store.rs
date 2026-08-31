//! [`JsonlStore`] — the notebook's built-in default store.
//!
//! One JSON line per entry (`amparo_tools::MemoryEntry`), appended to a
//! single file, audit-log style: writes never edit or delete earlier
//! lines. This is the M6a built-in — deliberately simple and quota-free
//! (growth must never silently consume a hosted vault's quota). Engram or
//! any other store can sit behind the same [`amparo_tools::Memory`] trait;
//! the M6e phase adds the rollup/index layer this append-only design is
//! meant to feed.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use amparo_tools::{Memory, MemoryEntry};

/// An append-only, file-backed [`Memory`] implementation.
///
/// `store` appends one JSON line and flushes; `search` reads the whole
/// file and scores entries with the same naive keyword matching as
/// [`amparo_tools::InMemoryStore`] — O(file), acceptable at notebook scale
/// (M6e's index layer addresses growth). File I/O is synchronous inside the
/// async methods, matching the workspace precedent for small
/// configuration-sized reads; the store itself is cheap to clone.
pub struct JsonlStore {
    path: PathBuf,
    file: Mutex<File>,
}

impl JsonlStore {
    /// Open (or create) the store at `path`, creating parent directories
    /// as needed. A task that fails before any tool runs may never create
    /// the workspace, so the store must not depend on it existing.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, String> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            let created = !parent.exists();
            std::fs::create_dir_all(parent).map_err(|e| {
                format!("cannot create notebook directory {}: {e}", parent.display())
            })?;
            // Harden only a directory this call created (audit
            // 2026-08-31 MED-6).
            if created {
                amparo_privacy::perms::owner_only(parent).map_err(|e| {
                    format!("cannot lock notebook directory {}: {e}", parent.display())
                })?;
            }
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| format!("cannot open notebook store {}: {e}", path.display()))?;
        amparo_privacy::perms::owner_only(&path)
            .map_err(|e| format!("cannot lock notebook store {}: {e}", path.display()))?;
        Ok(Self {
            path,
            file: Mutex::new(file),
        })
    }

    /// The path this store appends to.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// The same naive keyword score as `amparo_tools::memory::keyword_score`:
/// count query words contained in the entry, case-insensitively.
fn keyword_score(entry: &str, query: &str) -> usize {
    let q = query.to_lowercase();
    let e = entry.to_lowercase();
    let mut score = 0usize;
    for word in q.split_whitespace() {
        if e.contains(word) {
            score += 1;
        }
    }
    score
}

#[async_trait::async_trait]
impl Memory for JsonlStore {
    async fn search(&self, query: &str, limit: usize) -> Vec<MemoryEntry> {
        let Ok(text) = std::fs::read_to_string(&self.path) else {
            return Vec::new();
        };
        let mut scored: Vec<(usize, MemoryEntry)> = text
            .lines()
            .filter_map(|line| serde_json::from_str::<MemoryEntry>(line).ok())
            .map(|entry| (keyword_score(&entry.content, query), entry))
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0));
        scored
            .into_iter()
            .filter(|(score, _)| *score > 0)
            .take(limit)
            .map(|(_, entry)| entry)
            .collect()
    }

    async fn store(&self, content: String) -> Result<String, String> {
        let id = format!(
            "rec-{}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
            std::process::id()
        );
        let entry = MemoryEntry {
            id: id.clone(),
            content,
            created_at: chrono::Utc::now().to_rfc3339(),
        };
        let line = serde_json::to_string(&entry).map_err(|e| e.to_string())?;
        let mut file = self
            .file
            .lock()
            .map_err(|_| "notebook store lock poisoned".to_string())?;
        writeln!(file, "{line}").map_err(|e| format!("cannot write notebook store: {e}"))?;
        file.flush()
            .map_err(|e| format!("cannot flush notebook store: {e}"))?;
        Ok(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A unique dir per test — the sequence counter makes it unique by
    /// construction (concurrent tests can draw the same clock reading;
    /// M10 W6 hardening).
    fn temp_dir() -> PathBuf {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "amparo-notebook-test-{}-{}-{}",
            std::process::id(),
            n,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[cfg(unix)]
    #[test]
    fn open_creates_owner_only_store() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir();
        let path = dir.join("a/b/records.jsonl");
        JsonlStore::open(&path).unwrap();
        assert_eq!(
            std::fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700,
            "notebook dir must be owner-only"
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600,
            "notebook file must be owner-only"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn store_then_search_round_trips() {
        let dir = temp_dir();
        let store = JsonlStore::open(dir.join("a/b/records.jsonl")).unwrap();
        let id = store
            .store("run record with the word zephyr".to_string())
            .await
            .unwrap();
        assert!(id.starts_with("rec-"));

        let hits = store.search("zephyr", 10).await;
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, id);
        assert_eq!(hits[0].content, "run record with the word zephyr");
        // Parent directories were created by open().
        assert!(dir.join("a/b/records.jsonl").exists());
        // Non-matching queries return nothing.
        assert!(store.search("missing-term", 10).await.is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn entries_survive_reopen_and_append() {
        let dir = temp_dir();
        let path = dir.join("records.jsonl");
        let store = JsonlStore::open(&path).unwrap();
        store.store("first entry alpha".to_string()).await.unwrap();
        drop(store);

        let store = JsonlStore::open(&path).unwrap();
        store.store("second entry alpha".to_string()).await.unwrap();

        let hits = store.search("alpha", 10).await;
        assert_eq!(hits.len(), 2);
        // Best-first by keyword score, then insertion order is stable.
        assert_eq!(hits[0].content, "first entry alpha");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn garbage_lines_are_skipped() {
        let dir = temp_dir();
        let path = dir.join("records.jsonl");
        std::fs::write(&path, "not json\n").unwrap();
        let store = JsonlStore::open(&path).unwrap();
        store.store("a real record beta".to_string()).await.unwrap();

        let hits = store.search("beta", 10).await;
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].content, "a real record beta");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn missing_file_searches_empty() {
        let dir = temp_dir();
        let store = JsonlStore::open(dir.join("never-written.jsonl")).unwrap();
        assert!(store.search("anything", 10).await.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }
}
