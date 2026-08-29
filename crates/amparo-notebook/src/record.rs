//! The run record — the one document the notebook persists per task.
//!
//! Shape follows `docs/m6-controlled-growth.md` §3.1: task text (PII
//! stripped), tool-sequence hash, per-call gate log, verification outcome,
//! outcome (truncated answer, duration, token-cost estimate) and tenant tag.

use serde::{Deserialize, Serialize};

/// Cap for the task text and per-field short texts (targets, summaries,
/// feedback) — the same bound as the agent's event rendering
/// (`amparo_agent::TRUNCATE`), taken from the re-exported constant so the
/// two bounds cannot drift apart.
pub const TRUNCATE: usize = amparo_agent::TRUNCATE;

/// Cap for the final answer stored in a record. Answers are truncated more
/// generously than short fields but still capped: the notebook keeps the
/// shape of a task's outcome, not a full transcript.
pub const FINAL_ANSWER_CAP: usize = 500;

/// One run record — one task, from start to terminal event.
///
/// Serialized to JSON and stored as a single [`amparo_tools::Memory`]
/// payload; `version` is the forward-compatibility tag (currently `1`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunRecord {
    /// Record format version — `1` today; readers must accept unknown
    /// versions gracefully rather than fail.
    pub version: u8,
    /// The namespace this record belongs to (I2). The chat driver tags
    /// `platform:user_id`; the CLI tags `cli`.
    pub tenant_id: String,
    /// When the task began, RFC 3339 (UTC).
    pub started_at: String,
    /// Wall-clock duration of the whole task, in milliseconds, measured by
    /// the sink between the task-start and terminal events.
    pub duration_ms: u64,
    /// The task text, PII-stripped and truncated to `TRUNCATE` chars.
    pub task_text: String,
    /// FNV-1a 64-bit hash over the canonical `tool_name | stripped target`
    /// pairs, in call order — a stable identity digest for dedupe.
    /// Non-cryptographic by design (the doc mandates dependency-free
    /// hashing); it answers "is this the same task shape as before", not
    /// "is this content authentic".
    pub tool_sequence_hash: String,
    /// The per-call gate log, in call order.
    pub tool_calls: Vec<ToolStep>,
    /// The self-verification outcome — the *last*
    /// [`amparo_agent::AgentEvent::Verification`] of the task. `None` on the paths that never verify (one-shot and
    /// empty-turn completions).
    pub verification: Option<VerificationRecord>,
    /// `complete` | `failed` — from the terminal event.
    pub status: String,
    /// The truncated final answer (PII-stripped, `FINAL_ANSWER_CAP`
    /// chars); on failure, the failure message.
    pub final_answer: Option<String>,
    /// Rough token-cost estimate: chars/4 over the stripped task text and
    /// final answer. A character-based estimate, not provider-reported
    /// usage — labeled as such wherever it is shown.
    pub token_cost_estimate: usize,
}

/// One tool call in the gate log: what was asked, how the gate decided,
/// and what happened.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolStep {
    /// The model's call id.
    pub call_id: String,
    /// The tool's name.
    pub tool_name: String,
    /// The call's primary target (the policy-checked string), PII-stripped
    /// and truncated to `TRUNCATE` chars.
    pub target: String,
    /// The gate's decision: `allowed`, `trust_blocked`, `policy_denied`,
    /// `approval_denied` or `unknown_tool`.
    pub decision: String,
    /// The reasons behind the decision — policy rules that fired, or the
    /// block reason. Each reason is PII-stripped.
    pub reasons: Vec<String>,
    /// Whether the call was escalated to human approval
    /// ([`amparo_agent::AgentEvent::ApprovalRequested`] was seen).
    pub escalated: bool,
    /// The approval outcome — `None` when no approval was asked.
    pub approved: Option<bool>,
    /// Whether execution succeeded — `None` when the call was blocked and
    /// never executed.
    pub success: Option<bool>,
    /// The tool's display summary, PII-stripped and truncated to
    /// `TRUNCATE` chars. `None` when blocked.
    pub summary: Option<String>,
    /// Wall-clock execution time in milliseconds — `None` when blocked.
    pub duration_ms: Option<u64>,
}

/// The verification outcome carried by a record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VerificationRecord {
    /// `complete` | `incomplete`.
    pub decision: String,
    /// The model's feedback when incomplete — PII-stripped and truncated
    /// to `TRUNCATE` chars.
    pub feedback: Option<String>,
}

/// Strip PII with Secure Minions and discard the placeholder map — the
/// record keeps only sanitised text (I6: privacy enforced at capture time,
/// and records are archival, so there is no restore path).
pub(crate) fn strip(text: &str) -> String {
    amparo_privacy::secure_minions_strip(text).sanitised_text
}

/// Truncate `s` to `cap` chars, appending `…` when anything was cut —
/// same semantics as `amparo_agent::events::truncate`, with a caller-chosen
/// cap (the agent's helper is fixed to its own constant).
pub(crate) fn truncate_to(s: &str, cap: usize) -> String {
    let mut chars = s.chars();
    let head: String = chars.by_ref().take(cap).collect();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

/// The canonical hash input for one call: `tool_name` and stripped target,
/// separated by a unit separator so field boundaries cannot blur.
pub(crate) fn hash_line(tool_name: &str, stripped_target: &str) -> String {
    format!("{tool_name}\u{1f}{stripped_target}")
}

/// FNV-1a 64-bit — small, stable across Rust releases, dependency-free.
/// Documented as a non-cryptographic identity hash (see
/// [`RunRecord::tool_sequence_hash`]).
pub(crate) fn fnv1a64(data: &[u8]) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET;
    for byte in data {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

/// The tool-sequence hash for a run: FNV-1a over the canonical
/// `name | stripped target` lines, in call order, as lowercase hex.
pub(crate) fn sequence_hash(pairs: &[(String, String)]) -> String {
    let canonical: String = pairs
        .iter()
        .map(|(name, target)| hash_line(name, target))
        .collect::<Vec<_>>()
        .join("\n");
    format!("{:016x}", fnv1a64(canonical.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_to_cuts_and_marks() {
        assert_eq!(truncate_to("abcdef", 6), "abcdef");
        assert_eq!(truncate_to("abcdef", 3), "abc…");
        assert_eq!(truncate_to("", 10), "");
    }

    #[test]
    fn truncate_to_respects_char_boundaries() {
        // Multi-byte characters count once each, and never split.
        assert_eq!(truncate_to("éééé", 2), "éé…");
    }

    #[test]
    fn strip_removes_pii_and_keeps_placeholders() {
        let stripped = strip("mail user@example.com about the plan");
        assert!(!stripped.contains("user@example.com"));
        assert!(stripped.contains("[EMAIL_1]"));
    }

    #[test]
    fn fnv1a64_is_stable_and_sensitive() {
        // Known FNV-1a 64 test vector: "" → the offset basis.
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        // And a well-known vector: "a".
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_ne!(fnv1a64(b"a"), fnv1a64(b"b"));
    }

    #[test]
    fn sequence_hash_is_deterministic_and_order_sensitive() {
        let a = sequence_hash(&[
            ("run_command".to_string(), "ls".to_string()),
            ("write_file".to_string(), "notes.txt".to_string()),
        ]);
        assert_eq!(
            a,
            sequence_hash(&[
                ("run_command".to_string(), "ls".to_string()),
                ("write_file".to_string(), "notes.txt".to_string()),
            ])
        );
        // A different tool name changes the digest.
        let b = sequence_hash(&[
            ("read_file".to_string(), "ls".to_string()),
            ("write_file".to_string(), "notes.txt".to_string()),
        ]);
        assert_ne!(a, b);
        // A different target changes the digest.
        let c = sequence_hash(&[
            ("run_command".to_string(), "ls -la".to_string()),
            ("write_file".to_string(), "notes.txt".to_string()),
        ]);
        assert_ne!(a, c);
        // Order matters.
        let d = sequence_hash(&[
            ("write_file".to_string(), "notes.txt".to_string()),
            ("run_command".to_string(), "ls".to_string()),
        ]);
        assert_ne!(a, d);
        // The empty sequence is a constant, not an error.
        assert_eq!(
            sequence_hash(&[]),
            format!("{:016x}", fnv1a64(b""))
        );
    }

    #[test]
    fn hash_line_separates_fields() {
        // The unit separator keeps "a|b,c" from colliding with "a,b|c".
        assert_ne!(
            hash_line("a|b", "c"),
            hash_line("a", "b|c"),
        );
    }

    #[test]
    fn run_record_round_trips_json() {
        let record = RunRecord {
            version: 1,
            tenant_id: "cli".to_string(),
            started_at: "2026-08-28T00:00:00Z".to_string(),
            duration_ms: 12,
            task_text: "do the thing".to_string(),
            tool_sequence_hash: "abc".to_string(),
            tool_calls: vec![ToolStep {
                call_id: "call_1".to_string(),
                tool_name: "run_command".to_string(),
                target: "ls".to_string(),
                decision: "allowed".to_string(),
                reasons: vec!["rule-a".to_string()],
                escalated: false,
                approved: None,
                success: Some(true),
                summary: Some("done".to_string()),
                duration_ms: Some(3),
            }],
            verification: Some(VerificationRecord {
                decision: "complete".to_string(),
                feedback: None,
            }),
            status: "complete".to_string(),
            final_answer: Some("all good".to_string()),
            token_cost_estimate: 5,
        };
        let json = serde_json::to_string(&record).unwrap();
        let back: RunRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back.tenant_id, "cli");
        assert_eq!(back.tool_calls.len(), 1);
        assert_eq!(back.verification.unwrap().decision, "complete");
    }
}
