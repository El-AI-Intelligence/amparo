//! The verification case library seam (M6b).
//!
//! Retrieval over the lab notebook feeds **the verification prompt only** —
//! read-only evidence formatted as observations, never imperatives, never in
//! the action loop (design invariants I1/I5, `docs/m6-controlled-growth.md`
//! §3.2). The agent owns the seam and the prompt text; implementations live
//! behind the trait. The built-in default is `amparo_notebook::CaseRetriever`
//! over the `amparo_tools::Memory` store, wired by the CLI and chat hosts —
//! the agent itself never knows where records live.

use async_trait::async_trait;

/// Cap on cases injected into one verification prompt.
const MAX_EVIDENCE_CASES: usize = 3;
/// Cap on task text per case line.
const CASE_TASK_CAP: usize = 120;
/// Cap on the outcome snippet per case line.
const CASE_OUTCOME_CAP: usize = 120;
/// Cap on tool names listed per case line.
const CASE_TOOL_NAMES_CAP: usize = 5;

/// One retrieved prior case — the unit of evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvidenceCase {
    /// Store entry id of the record.
    pub id: String,
    /// Record start date, `YYYY-MM-DD`.
    pub date: String,
    /// `VERIFIED` | `INCOMPLETE` | `FAILED` | `UNVERIFIED`.
    pub verdict: String,
    /// The PII-stripped task text (already truncated by the record).
    pub task_text: String,
    /// Tool names the run called, in order, deduplicated.
    pub tool_names: Vec<String>,
    /// The truncated final answer (or failure message), when present.
    pub outcome: Option<String>,
}

/// The case library seam: hosts attach an implementation so the
/// self-verification step can weigh prior tenant evidence.
///
/// Retrieval is observational: implementations must never fail the task —
/// return what can be found, or an empty list. Evidence enters the
/// verification prompt only; it is never shown to the action loop, and a
/// retrieved case can never execute anything.
#[async_trait]
pub trait CaseLibrary: Send + Sync {
    /// Retrieve prior same-tenant cases resembling this task.
    ///
    /// `task_text` is the PII-stripped task; `tool_names` the tools this run
    /// has called so far, in first-use order (a sequence signal, not an
    /// instruction).
    async fn retrieve(
        &self,
        task_text: &str,
        tool_names: &[String],
        limit: usize,
    ) -> Vec<EvidenceCase>;
}

/// Render the evidence section for the verification prompt.
///
/// Observation format per `docs/m6-controlled-growth.md` §3.2: each line
/// describes what happened — it contains no instruction the model is told
/// to follow. Returns `None` when there is nothing to show. Fields are
/// truncated so a buggy backend cannot balloon the prompt.
pub fn evidence_section(cases: &[EvidenceCase]) -> Option<String> {
    if cases.is_empty() {
        return None;
    }
    let mut section = String::from("Prior cases in this tenant resembling the current task:");
    for case in cases.iter().take(MAX_EVIDENCE_CASES) {
        let tools = case
            .tool_names
            .iter()
            .take(CASE_TOOL_NAMES_CAP)
            .map(|name| format!("`{name}`"))
            .collect::<Vec<_>>()
            .join(", ");
        let outcome = match &case.outcome {
            Some(text) => format!("outcome: \"{}\"", truncate_to(text, CASE_OUTCOME_CAP)),
            None => "no final answer was recorded".to_string(),
        };
        section.push_str(&format!(
            "\n- Case {} ({}, {}): to achieve \"{}\" the agent ran {}; {}.",
            case.id,
            case.date,
            case.verdict,
            truncate_to(&case.task_text, CASE_TASK_CAP),
            tools,
            outcome
        ));
    }
    Some(section)
}

/// Char-based truncation with an ellipsis, like [`crate::events::truncate`]
/// but with a caller-chosen cap.
fn truncate_to(s: &str, cap: usize) -> String {
    let mut chars = s.chars();
    let head: String = chars.by_ref().take(cap).collect();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn case(id: &str) -> EvidenceCase {
        EvidenceCase {
            id: id.to_string(),
            date: "2026-08-14".to_string(),
            verdict: "VERIFIED".to_string(),
            task_text: "run the tests".to_string(),
            tool_names: vec!["run_command".to_string()],
            outcome: Some("all passed".to_string()),
        }
    }

    #[test]
    fn empty_input_is_no_section() {
        assert_eq!(evidence_section(&[]), None);
    }

    #[test]
    fn renders_observation_format() {
        let section = evidence_section(&[case("rec-1")]).expect("one case renders");
        assert!(section.starts_with("Prior cases in this tenant resembling the current task:"));
        assert!(section.contains("Case rec-1 (2026-08-14, VERIFIED)"));
        assert!(section.contains("the agent ran `run_command`"));
        assert!(section.contains("outcome: \"all passed\""));
        // Observations, not imperatives — the section never instructs.
        for imperative in ["you must", "please do", "should run"] {
            assert!(
                !section.to_lowercase().contains(imperative),
                "evidence section must not instruct: {section}"
            );
        }
    }

    #[test]
    fn truncates_long_fields() {
        let long = "x".repeat(500);
        let mut c = case("rec-1");
        c.task_text = long.clone();
        c.outcome = Some(long.clone());
        c.tool_names = (0..10).map(|i| format!("tool{i}")).collect();
        let section = evidence_section(&[c]).expect("renders");
        assert!(!section.contains(&long), "long fields must be truncated");
        assert!(!section.contains("tool5"), "tool list must be capped");
    }

    #[test]
    fn caps_cases_at_three() {
        let cases: Vec<EvidenceCase> = (0..5).map(|i| case(&format!("rec-{i}"))).collect();
        let section = evidence_section(&cases).expect("renders");
        assert_eq!(section.matches("- Case ").count(), 3);
        assert!(section.contains("rec-2"));
        assert!(!section.contains("rec-3"));
    }

    #[test]
    fn missing_outcome_is_honest() {
        let mut c = case("rec-1");
        c.outcome = None;
        let section = evidence_section(&[c]).expect("renders");
        assert!(section.contains("no final answer was recorded"));
    }
}
