//! [`CaseRetriever`] — the M6b case library over a [`amparo_tools::Memory`]
//! store.
//!
//! Retrieval is the read half of controlled growth: prior run records are
//! ranked by dependency-free keyword/tool-name matching and returned as
//! [`amparo_agent::EvidenceCase`]s for the verification prompt only (the
//! agent injects them — the retriever never touches a prompt). The
//! same-tenant filter (I2) is enforced here at retrieval time on the parsed
//! record's `tenant_id`, whatever the store matched: cross-tenant promotion
//! is an explicit operator action, never retrieval.

use std::sync::Arc;

use amparo_agent::{CaseLibrary, EvidenceCase};
use amparo_tools::Memory;

use crate::RunRecord;

/// Retrieves prior same-tenant run records as evidence cases.
///
/// Reads through the [`amparo_tools::Memory`] trait, so the built-in
/// [`crate::JsonlStore`], Engram or any other backend behaves identically.
/// The keyword score mirrors the stores' own matching; records that do not
/// parse as [`RunRecord`]s (or belong to another tenant) are skipped, and
/// ties break toward the most recent `started_at`.
#[derive(Clone)]
pub struct CaseRetriever {
    store: Arc<dyn Memory>,
    tenant_id: String,
}

impl CaseRetriever {
    /// Build the retriever scoped to `tenant_id` over `store`.
    ///
    /// The tenant filter (I2) is applied at retrieval time: only records
    /// whose parsed `tenant_id` matches are returned, regardless of what
    /// the store's keyword search matched.
    pub fn new(store: Arc<dyn Memory>, tenant_id: impl Into<String>) -> Self {
        Self {
            store,
            tenant_id: tenant_id.into(),
        }
    }

    /// The tenant id this retriever is scoped to.
    pub fn tenant_id(&self) -> &str {
        &self.tenant_id
    }
}

/// The same naive keyword score as the stores' own search: count query
/// words contained in the entry, case-insensitively.
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
impl CaseLibrary for CaseRetriever {
    async fn retrieve(
        &self,
        task_text: &str,
        tool_names: &[String],
        limit: usize,
    ) -> Vec<EvidenceCase> {
        // Keyword query: the task's words plus the run's tool names (the
        // sequence signal). Fetch generously — the tenant filter discards
        // other tenants' matches — then rank and cap.
        let mut query = task_text.to_string();
        for name in tool_names {
            query.push(' ');
            query.push_str(name);
        }
        let fetch_limit = limit.saturating_mul(4).max(limit);
        let entries = self.store.search(&query, fetch_limit).await;

        let mut scored: Vec<(usize, EvidenceCase)> = entries
            .into_iter()
            .filter_map(|entry| {
                let record: RunRecord = serde_json::from_str(&entry.content).ok()?;
                if record.tenant_id != self.tenant_id {
                    return None;
                }
                let verdict = match record.status.as_str() {
                    "failed" => "FAILED".to_string(),
                    _ => match record.verification.as_ref().map(|v| v.decision.as_str()) {
                        Some("complete") => "VERIFIED".to_string(),
                        Some("incomplete") => "INCOMPLETE".to_string(),
                        _ => "UNVERIFIED".to_string(),
                    },
                };
                let mut tools: Vec<String> = Vec::new();
                for step in &record.tool_calls {
                    if !tools.contains(&step.tool_name) {
                        tools.push(step.tool_name.clone());
                    }
                }
                let case = EvidenceCase {
                    id: entry.id,
                    date: record.started_at.chars().take(10).collect(),
                    verdict,
                    task_text: record.task_text,
                    tool_names: tools,
                    outcome: record.final_answer,
                };
                let score = keyword_score(&entry.content, &query);
                Some((score, case))
            })
            .collect();

        // Best match first; ties break toward the most recent record
        // (RFC 3339 timestamps order lexicographically).
        scored.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then_with(|| b.1.date.cmp(&a.1.date))
        });
        scored
            .into_iter()
            .filter(|(score, _)| *score > 0)
            .take(limit)
            .map(|(_, case)| case)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ToolStep, VerificationRecord};
    use amparo_tools::InMemoryStore;

    fn record_json(
        tenant: &str,
        task: &str,
        status: &str,
        verification: Option<&str>,
        tool_names: &[&str],
        started_at: &str,
        answer: Option<&str>,
    ) -> String {
        let record = RunRecord {
            version: 1,
            tenant_id: tenant.to_string(),
            started_at: started_at.to_string(),
            duration_ms: 1000,
            task_text: task.to_string(),
            tool_sequence_hash: "0123456789abcdef".to_string(),
            tool_calls: tool_names
                .iter()
                .map(|name| ToolStep {
                    call_id: format!("call-{name}"),
                    tool_name: name.to_string(),
                    target: String::new(),
                    decision: "allowed".to_string(),
                    reasons: Vec::new(),
                    escalated: false,
                    approved: None,
                    success: Some(true),
                    summary: None,
                    duration_ms: None,
                })
                .collect(),
            verification: verification.map(|decision| VerificationRecord {
                decision: decision.to_string(),
                feedback: None,
            }),
            status: status.to_string(),
            final_answer: answer.map(str::to_string),
            token_cost_estimate: 10,
        };
        serde_json::to_string(&record).unwrap()
    }

    async fn seeded_store() -> Arc<InMemoryStore> {
        let store = Arc::new(InMemoryStore::new());
        let seed = |tenant: &str, task: &str| {
            record_json(tenant, task, "complete", Some("complete"), &["git_status"], "2026-08-14T10:00:00Z", Some("done"))
        };
        let _ = store.store(seed("cli", "deploy the staging site")).await;
        let _ = store.store(seed("telegram:111", "deploy the staging site")).await;
        store
    }

    #[tokio::test]
    async fn retrieves_only_matching_tenant_records() {
        let store = seeded_store().await;
        let retriever = CaseRetriever::new(store, "cli");
        let cases = retriever.retrieve("deploy the staging site", &[], 10).await;
        assert_eq!(cases.len(), 1);
        assert_eq!(cases[0].task_text, "deploy the staging site");
    }

    #[tokio::test]
    async fn other_tenant_matches_are_never_returned() {
        let store = seeded_store().await;
        let retriever = CaseRetriever::new(store, "telegram:111");
        let cases = retriever.retrieve("deploy", &[], 10).await;
        assert_eq!(cases.len(), 1);
        // And a tenant with no records sees nothing.
        let nobody = CaseRetriever::new(Arc::new(InMemoryStore::new()), "telegram:999");
        assert!(nobody.retrieve("deploy", &[], 10).await.is_empty());
    }

    #[tokio::test]
    async fn ranks_by_score_then_recency() {
        let store = Arc::new(InMemoryStore::new());
        // Older, but matches both query words.
        let _ = store
            .store(record_json(
                "cli",
                "deploy the staging site",
                "complete",
                Some("complete"),
                &["run_command"],
                "2026-08-10T10:00:00Z",
                Some("done"),
            ))
            .await;
        // Newer, matches only one query word.
        let _ = store
            .store(record_json(
                "cli",
                "staging",
                "complete",
                Some("complete"),
                &["git_status"],
                "2026-08-20T10:00:00Z",
                Some("done"),
            ))
            .await;
        let retriever = CaseRetriever::new(store, "cli");
        let cases = retriever.retrieve("deploy staging", &[], 10).await;
        assert_eq!(cases.len(), 2);
        assert_eq!(cases[0].task_text, "deploy the staging site", "score beats age");
        assert_eq!(cases[1].task_text, "staging");
    }

    #[tokio::test]
    async fn maps_verdicts_from_status_and_verification() {
        let store = Arc::new(InMemoryStore::new());
        let _ = store
            .store(record_json("cli", "alpha task", "complete", Some("incomplete"), &[], "2026-08-14T10:00:00Z", Some("x")))
            .await;
        let _ = store
            .store(record_json("cli", "alpha task", "failed", None, &[], "2026-08-15T10:00:00Z", Some("boom")))
            .await;
        let _ = store
            .store(record_json("cli", "alpha task", "complete", None, &[], "2026-08-16T10:00:00Z", Some("x")))
            .await;
        let retriever = CaseRetriever::new(store, "cli");
        let cases = retriever.retrieve("alpha task", &[], 10).await;
        let mut verdicts: Vec<&str> = cases.iter().map(|c| c.verdict.as_str()).collect();
        verdicts.sort_unstable();
        assert_eq!(verdicts, vec!["FAILED", "INCOMPLETE", "UNVERIFIED"]);
    }

    #[tokio::test]
    async fn limit_caps_results() {
        let store = Arc::new(InMemoryStore::new());
        for i in 0..5 {
            let _ = store
                .store(record_json(
                    "cli",
                    &format!("alpha task {i}"),
                    "complete",
                    Some("complete"),
                    &[],
                    "2026-08-14T10:00:00Z",
                    Some("x"),
                ))
                .await;
        }
        let retriever = CaseRetriever::new(store, "cli");
        assert_eq!(retriever.retrieve("alpha task", &[], 2).await.len(), 2);
        assert!(retriever.retrieve("alpha task", &[], 0).await.is_empty());
    }

    #[tokio::test]
    async fn unparseable_entries_are_skipped() {
        let store = Arc::new(InMemoryStore::new());
        let _ = store.store("not a run record".to_string()).await;
        let _ = store
            .store(record_json("cli", "beta task", "complete", Some("complete"), &[], "2026-08-14T10:00:00Z", Some("x")))
            .await;
        let retriever = CaseRetriever::new(store, "cli");
        let cases = retriever.retrieve("beta task", &[], 10).await;
        assert_eq!(cases.len(), 1);
        assert_eq!(cases[0].task_text, "beta task");
    }

    #[tokio::test]
    async fn tool_names_are_deduplicated_in_order() {
        let store = Arc::new(InMemoryStore::new());
        let _ = store
            .store(record_json(
                "cli",
                "gamma task",
                "complete",
                Some("complete"),
                &["git_status", "git_diff", "git_status"],
                "2026-08-14T10:00:00Z",
                Some("x"),
            ))
            .await;
        let retriever = CaseRetriever::new(store, "cli");
        let cases = retriever.retrieve("gamma task", &[], 10).await;
        assert_eq!(cases[0].tool_names, vec!["git_status".to_string(), "git_diff".to_string()]);
    }

    #[tokio::test]
    async fn query_combines_task_words_and_tool_names() {
        let store = Arc::new(InMemoryStore::new());
        // The record's task text shares no words with the new task, but its
        // tool name does.
        let _ = store
            .store(record_json(
                "cli",
                "run the linter on the repo",
                "complete",
                Some("complete"),
                &["cargo_fmt"],
                "2026-08-14T10:00:00Z",
                Some("formatted"),
            ))
            .await;
        let retriever = CaseRetriever::new(store, "cli");
        let cases = retriever
            .retrieve("check formatting again", &["cargo_fmt".to_string()], 10)
            .await;
        assert_eq!(cases.len(), 1);
        assert_eq!(cases[0].task_text, "run the linter on the repo");
    }
}
