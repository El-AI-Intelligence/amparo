//! Skills storage — the adopted-skill log and the distillation proposer
//! (M6c + M6d).
//!
//! [`SkillSet`] is the notebook's implementation of
//! [`amparo_tools::SkillLibrary`]: it loads a tenant's adopted skills from
//! the append-only `adopted.jsonl` log (last event per (tenant, name) wins
//! across `adopt`/`retire` kinds, I4-style audit semantics — retirement
//! disables, it never deletes history). [`Proposer`] distills *candidate*
//! skills from the run records — recurring, high-VERIFIED tool sequences,
//! written as **inert** proposals that never become executable on their
//! own.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};

use amparo_tools::{SkillLibrary, SkillSpec, USE_SKILL};
use serde::{Deserialize, Serialize};

use crate::RunRecord;

/// The adopted-skill log filename, under the workspace skills directory.
pub const ADOPTED_FILE: &str = "adopted.jsonl";
/// The proposal log filename, under the workspace skills directory.
pub const PROPOSALS_FILE: &str = "proposals.jsonl";

/// Cap for a distilled skill's suggested name.
const NAME_CAP: usize = 40;

// ─────────────────────────────────────────────── SkillSet ────────────────────

/// A tenant's adopted skills, loaded from the adoption log.
///
/// Loading folds the append-only log: the last event per (tenant, name)
/// wins across kinds — an `adopt` inserts its spec, a `retire` removes the
/// skill. Unparseable lines are skipped, and a missing file yields an empty
/// set — the same tolerance as [`crate::JsonlStore`]. Loading is O(file);
/// fine at M6d scale (M6e adds the index layer).
#[derive(Clone, Default)]
pub struct SkillSet {
    skills: HashMap<String, SkillSpec>,
}

impl SkillSet {
    /// Load `tenant_id`'s adopted skills from `path` (a [`SkillLogEvent`]
    /// JSONL file). A missing file is an empty set, not an error.
    pub fn load(path: &Path, tenant_id: &str) -> Self {
        let mut set = Self::default();
        let Ok(text) = std::fs::read_to_string(path) else {
            return set;
        };
        for line in text.lines() {
            let Ok(event) = serde_json::from_str::<SkillLogEvent>(line) else {
                continue;
            };
            if event.tenant_id() != tenant_id {
                continue;
            }
            match event {
                SkillLogEvent::Adopt { spec, .. } => {
                    set.skills.insert(spec.name.clone(), spec);
                }
                SkillLogEvent::Retire { name, .. } => {
                    set.skills.remove(&name);
                }
            }
        }
        set
    }

    /// Build a set from owned specs (tests, in-memory hosts).
    pub fn from_specs<I: IntoIterator<Item = SkillSpec>>(specs: I) -> Self {
        Self {
            skills: specs.into_iter().map(|s| (s.name.clone(), s)).collect(),
        }
    }
}

impl SkillLibrary for SkillSet {
    fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.skills.keys().cloned().collect();
        names.sort();
        names
    }

    fn get(&self, name: &str) -> Option<SkillSpec> {
        self.skills.get(name).cloned()
    }
}

// ─────────────────────────────────────────────── Adoption log ────────────────

/// One skill-log event. The log is append-only — adopting a skill again
/// with a new spec is a new event, and the last event per (tenant, name)
/// wins **across kinds** (the audit trail keeps every row, I4). The serde
/// tag is `event`: `adopt` | `retire`.
///
/// The `Adopt` variant declares its fields in the M6c `AdoptRecord` order,
/// so rows written before M6d stay byte-identical.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum SkillLogEvent {
    /// A skill was adopted.
    Adopt {
        /// The tenant the skill was adopted for (I2).
        tenant_id: String,
        /// The skill's name (redundant with `spec.name`; the log key).
        name: String,
        /// The full adopted spec, including the step plan as approved.
        spec: SkillSpec,
        /// RFC 3339 adoption timestamp.
        adopted_at: String,
    },
    /// A skill was retired (M6d) — disabled, never deleted.
    Retire {
        /// The tenant the skill was retired for (I2).
        tenant_id: String,
        /// The skill's name.
        name: String,
        /// RFC 3339 retirement timestamp.
        retired_at: String,
        /// Why it retired: a policy-drift or performance reason, or the
        /// operator's own.
        reason: String,
    },
}

impl SkillLogEvent {
    /// Build an [`SkillLogEvent::Adopt`] event.
    pub fn adopt(
        tenant_id: impl Into<String>,
        name: impl Into<String>,
        spec: SkillSpec,
        adopted_at: impl Into<String>,
    ) -> Self {
        Self::Adopt {
            tenant_id: tenant_id.into(),
            name: name.into(),
            spec,
            adopted_at: adopted_at.into(),
        }
    }

    /// Build a [`SkillLogEvent::Retire`] event.
    pub fn retire(
        tenant_id: impl Into<String>,
        name: impl Into<String>,
        retired_at: impl Into<String>,
        reason: impl Into<String>,
    ) -> Self {
        Self::Retire {
            tenant_id: tenant_id.into(),
            name: name.into(),
            retired_at: retired_at.into(),
            reason: reason.into(),
        }
    }

    /// The event's tenant id (I2).
    pub fn tenant_id(&self) -> &str {
        match self {
            Self::Adopt { tenant_id, .. } | Self::Retire { tenant_id, .. } => tenant_id,
        }
    }

    /// The event's skill name.
    pub fn name(&self) -> &str {
        match self {
            Self::Adopt { name, .. } | Self::Retire { name, .. } => name,
        }
    }
}

/// Append one skill-log event to the log, creating parent directories as
/// needed and flushing before returning (every row is an audit row, I4 —
/// it must survive process exit).
pub fn append_event(path: &Path, event: &SkillLogEvent) -> Result<(), String> {
    append_line(path, event)
}

/// Read every skill-log event from the log (unparseable lines skipped) —
/// for the CLI's `list`/`show`. Callers fold to get the effective set.
pub fn read_log(path: &Path) -> Vec<SkillLogEvent> {
    read_lines(path, |line| serde_json::from_str(line).ok())
}

// ─────────────────────────────────────────────── Proposals ───────────────────

/// One distilled skill proposal — a recurring, high-VERIFIED tool sequence
/// flagged to the operator. Inert by design: it names the sequence and its
/// evidence, never a runnable step plan (records store targets, not
/// arguments — the operator authors the candidate).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ProposalRecord {
    /// RFC 3339 timestamp of the proposal run.
    pub proposed_at: String,
    /// The tenant whose records produced it (I2).
    pub tenant_id: String,
    /// The recurring tool-sequence hash (the records' `tool_sequence_hash`).
    pub tool_sequence_hash: String,
    /// The tool names of the sequence, in order.
    pub tool_names: Vec<String>,
    /// Total matching runs (complete, every step allowed + succeeded).
    pub total_runs: usize,
    /// Matching runs whose verification decision was `complete`.
    pub verified_runs: usize,
    /// `verified_runs / total_runs`.
    pub verified_rate: f64,
    /// `started_at` of up to three example runs.
    pub example_run_ids: Vec<String>,
    /// A suggested skill name — the hyphenated tool names, 40-char cap.
    pub suggested_name: String,
}

/// Distills candidate skills from the run records.
///
/// A run qualifies when it is `complete`, every tool step was allowed and
/// succeeded, and it did not use `use_skill` (a candidate containing a
/// skill would be nested, which M6c rejects). Qualifying runs are grouped
/// by `tool_sequence_hash`; a group becomes a proposal when it has at least
/// `min_runs` VERIFIED runs and its verified rate clears
/// `min_verified_rate`.
pub struct Proposer {
    /// Minimum VERIFIED runs a sequence must have (default 3).
    pub min_runs: usize,
    /// Minimum verified fraction a sequence must hold (default 0.8).
    pub min_verified_rate: f64,
}

impl Default for Proposer {
    fn default() -> Self {
        Self {
            min_runs: 3,
            min_verified_rate: 0.8,
        }
    }
}

#[derive(Default)]
struct GroupStats {
    total_runs: usize,
    verified_runs: usize,
    examples: Vec<String>,
    tool_names: Vec<String>,
}

/// A suggested skill name from the sequence's tool names: underscores
/// become hyphens, names join with `-`, and the result is capped at
/// [`NAME_CAP`] chars.
fn suggested_name(tool_names: &[String]) -> String {
    let joined = tool_names
        .iter()
        .map(|name| name.replace('_', "-"))
        .collect::<Vec<_>>()
        .join("-");
    let mut chars = joined.chars();
    let head: String = chars.by_ref().take(NAME_CAP).collect();
    head
}

impl Proposer {
    /// Parse `records_path` (a `RunRecord` JSONL file) and return every
    /// qualifying proposal for `tenant_id`. A missing file yields an empty
    /// list, not an error.
    pub fn propose(&self, records_path: &Path, tenant_id: &str) -> Result<Vec<ProposalRecord>, String> {
        let text = match std::fs::read_to_string(records_path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(format!("cannot read records {}: {e}", records_path.display())),
        };

        let mut groups: HashMap<String, GroupStats> = HashMap::new();
        for line in text.lines() {
            // records.jsonl holds MemoryEntry wrappers (the store's wire
            // format); the RunRecord is the entry's `content` — the
            // CaseRetriever double-parse pattern.
            let Ok(entry) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            let Some(content) = entry.get("content").and_then(|v| v.as_str()) else {
                continue;
            };
            let Ok(record) = serde_json::from_str::<RunRecord>(content) else {
                continue;
            };
            if record.tenant_id != tenant_id || record.status != "complete" {
                continue;
            }
            let mut tool_names = Vec::new();
            for step in &record.tool_calls {
                if step.decision != "allowed" || step.success != Some(true) {
                    tool_names.clear();
                    break;
                }
                if step.tool_name == USE_SKILL {
                    // A candidate containing a skill would be nested; M6c
                    // has no nested skills.
                    tool_names.clear();
                    break;
                }
                if !tool_names.contains(&step.tool_name) {
                    tool_names.push(step.tool_name.clone());
                }
            }
            if tool_names.is_empty() {
                continue;
            }
            let verified = record
                .verification
                .as_ref()
                .map(|v| v.decision == "complete")
                .unwrap_or(false);
            let group = groups
                .entry(record.tool_sequence_hash.clone())
                .or_default();
            group.total_runs += 1;
            if verified {
                group.verified_runs += 1;
            }
            if group.examples.len() < 3 {
                group.examples.push(record.started_at.clone());
            }
            if group.tool_names.is_empty() {
                group.tool_names = tool_names;
            }
        }

        let mut proposals: Vec<ProposalRecord> = groups
            .into_iter()
            .filter(|(_, group)| {
                group.verified_runs >= self.min_runs
                    && (group.verified_runs as f64 / group.total_runs as f64)
                        >= self.min_verified_rate
            })
            .map(|(hash, group)| ProposalRecord {
                proposed_at: chrono::Utc::now().to_rfc3339(),
                tenant_id: tenant_id.to_string(),
                tool_sequence_hash: hash,
                suggested_name: suggested_name(&group.tool_names),
                tool_names: group.tool_names,
                total_runs: group.total_runs,
                verified_runs: group.verified_runs,
                verified_rate: group.verified_runs as f64 / group.total_runs as f64,
                example_run_ids: group.examples,
            })
            .collect();
        proposals.sort_by(|a, b| b.verified_rate.partial_cmp(&a.verified_rate).unwrap_or(std::cmp::Ordering::Equal));
        Ok(proposals)
    }
}

/// Append proposals to `proposals.jsonl`, skipping any whose
/// (tenant, sequence hash) pair is already present. Returns the number of
/// proposals actually written.
pub fn append_proposals(path: &Path, proposals: &[ProposalRecord]) -> Result<usize, String> {
    let mut seen: HashSet<(String, String)> = read_lines(path, |line| {
        serde_json::from_str::<ProposalRecord>(line)
            .ok()
            .map(|p| (p.tenant_id, p.tool_sequence_hash))
    })
    .into_iter()
    .collect();
    let mut written = 0usize;
    for proposal in proposals {
        if !seen.insert((proposal.tenant_id.clone(), proposal.tool_sequence_hash.clone())) {
            continue;
        }
        append_line(path, proposal)?;
        written += 1;
    }
    Ok(written)
}

// ─────────────────────────────────────────────── File helpers ────────────────

/// Append one JSON line to `path`, creating parent directories and
/// flushing. Shared by the skill, proposal and re-check logs (all are
/// audit rows) and the notebook rollup sidecars.
pub(crate) fn append_line<T: Serialize>(path: &Path, value: &T) -> Result<(), String> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)
        .map_err(|e| format!("cannot create directory {}: {e}", parent.display()))?;
    let line = serde_json::to_string(value).map_err(|e| e.to_string())?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    writeln!(file, "{line}").map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    file.flush()
        .map_err(|e| format!("cannot flush {}: {e}", path.display()))
}

/// Parse every line of a JSONL file with `parse`, skipping unparseable
/// lines; a missing file yields an empty vector.
pub(crate) fn read_lines<T>(path: &Path, parse: impl Fn(&str) -> Option<T>) -> Vec<T> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines().filter_map(parse).collect()
}

/// The workspace skills directory for `workspace_root`.
pub fn skills_dir(workspace_root: &Path) -> PathBuf {
    workspace_root.join(".amparo").join("skills")
}

// ───────────────────────────────────────────────────────────── Tests ─────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ToolStep, VerificationRecord};
    use amparo_tools::{SkillOrigin, SkillStep};
    use std::io::Write;

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("amparo-skills-{}-{name}", std::process::id()))
    }

    fn spec(name: &str) -> SkillSpec {
        SkillSpec {
            name: name.to_string(),
            description: "does the thing".to_string(),
            preconditions: vec![],
            steps: vec![SkillStep {
                tool: "read_file".to_string(),
                arguments: serde_json::json!({"path": "x.txt"}),
            }],
            expected_outcome: "contents read".to_string(),
            origin: SkillOrigin::Operator,
            source_run_ids: vec![],
            adopted_at: None,
        }
    }

    fn adopt_record(tenant: &str, name: &str) -> SkillLogEvent {
        SkillLogEvent::adopt(tenant, name, spec(name), "2026-08-29T00:00:00Z")
    }

    #[test]
    fn adopt_rows_keep_the_m6c_wire_shape() {
        // The internally-tagged enum must serialize the M6c `AdoptRecord`
        // shape exactly — rows written before M6d parse with no change.
        let spec = spec("alpha");
        let event =
            SkillLogEvent::adopt("cli", "alpha", spec.clone(), "2026-08-29T00:00:00Z");
        let line = serde_json::to_string(&event).unwrap();
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["event"], "adopt");
        assert_eq!(value["tenant_id"], "cli");
        assert_eq!(value["name"], "alpha");
        assert_eq!(value["adopted_at"], "2026-08-29T00:00:00Z");
        assert_eq!(value["spec"], serde_json::to_value(&spec).unwrap());
        // And the same bytes deserialize back into the event.
        assert_eq!(serde_json::from_str::<SkillLogEvent>(&line).unwrap(), event);
    }

    #[test]
    fn load_folds_last_event_wins_and_skips_garbage() {
        let path = temp_path("fold.jsonl");
        std::fs::write(&path, "").unwrap();
        append_event(&path, &adopt_record("cli", "alpha")).unwrap();
        // Garbage line — skipped, not fatal.
        std::fs::OpenOptions::new().append(true).open(&path).unwrap()
            .write_all(b"not json\n").unwrap();
        // A second adoption of alpha (new spec) — the last one wins.
        let SkillLogEvent::Adopt {
            mut spec,
            tenant_id,
            name,
            adopted_at,
        } = adopt_record("cli", "alpha")
        else {
            unreachable!()
        };
        spec.description = "revised".to_string();
        append_event(
            &path,
            &SkillLogEvent::Adopt { tenant_id, name, spec, adopted_at },
        )
        .unwrap();
        // Another tenant's adoption is invisible to cli.
        append_event(&path, &adopt_record("telegram:111", "beta")).unwrap();

        let set = SkillSet::load(&path, "cli");
        assert_eq!(set.names(), vec!["alpha".to_string()]);
        assert_eq!(set.get("alpha").unwrap().description, "revised");

        let other = SkillSet::load(&path, "telegram:111");
        assert_eq!(other.names(), vec!["beta".to_string()]);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_missing_file_is_empty() {
        let set = SkillSet::load(&temp_path("missing.jsonl"), "cli");
        assert!(set.names().is_empty());
    }

    #[test]
    fn fold_retires_remove_and_readoption_wins_again() {
        let path = temp_path("retire.jsonl");
        std::fs::write(&path, "").unwrap();
        append_event(&path, &adopt_record("cli", "alpha")).unwrap();
        append_event(&path, &adopt_record("cli", "beta")).unwrap();
        append_event(
            &path,
            &SkillLogEvent::retire("cli", "alpha", "2026-08-29T01:00:00Z", "performance"),
        )
        .unwrap();
        // Another tenant's retirement is invisible to cli.
        append_event(
            &path,
            &SkillLogEvent::retire("telegram:111", "beta", "2026-08-29T01:00:00Z", "drift"),
        )
        .unwrap();

        let set = SkillSet::load(&path, "cli");
        assert_eq!(set.names(), vec!["beta".to_string()]);

        // Re-adoption after retirement wins again (last event per name).
        append_event(&path, &adopt_record("cli", "alpha")).unwrap();
        let set = SkillSet::load(&path, "cli");
        let mut names = set.names();
        names.sort();
        assert_eq!(names, vec!["alpha".to_string(), "beta".to_string()]);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn read_log_returns_events_in_order() {
        let path = temp_path("read.jsonl");
        std::fs::write(&path, "").unwrap();
        append_event(&path, &adopt_record("cli", "alpha")).unwrap();
        append_event(&path, &adopt_record("cli", "beta")).unwrap();
        append_event(
            &path,
            &SkillLogEvent::retire("cli", "alpha", "2026-08-29T01:00:00Z", "operator retired"),
        )
        .unwrap();
        let events = read_log(&path);
        assert_eq!(events.len(), 3);
        assert!(matches!(&events[0], SkillLogEvent::Adopt { .. }));
        assert_eq!(events[0].name(), "alpha");
        assert_eq!(events[1].name(), "beta");
        assert!(matches!(&events[2], SkillLogEvent::Retire { .. }));
        assert_eq!(events[2].name(), "alpha");
        let _ = std::fs::remove_file(&path);
    }

    /// Wrap a run record in the store's wire shape: records.jsonl holds
    /// MemoryEntry JSON whose `content` is the RunRecord JSON.
    fn entry(record: &RunRecord) -> serde_json::Value {
        serde_json::json!({"content": serde_json::to_string(record).unwrap()})
    }

    /// One qualifying run record: complete, every step allowed+success.
    fn run_record(
        tenant: &str,
        started_at: &str,
        hash: &str,
        tools: &[&str],
        verified: bool,
    ) -> RunRecord {
        RunRecord {
            version: 1,
            tenant_id: tenant.to_string(),
            started_at: started_at.to_string(),
            duration_ms: 10,
            task_text: "task".to_string(),
            tool_sequence_hash: hash.to_string(),
            tool_calls: tools
                .iter()
                .map(|name| ToolStep {
                    call_id: format!("call-{name}"),
                    tool_name: name.to_string(),
                    target: String::new(),
                    decision: "allowed".to_string(),
                    reasons: vec![],
                    escalated: false,
                    approved: None,
                    success: Some(true),
                    summary: Some("ok".to_string()),
                    duration_ms: Some(1),
                })
                .collect(),
            verification: Some(VerificationRecord {
                decision: if verified { "complete" } else { "incomplete" }.to_string(),
                feedback: None,
            }),
            status: "complete".to_string(),
            final_answer: Some("done".to_string()),
            token_cost_estimate: 4,
        }
    }

    #[test]
    fn proposer_keeps_only_high_verified_sequences() {
        let path = temp_path("propose.jsonl");
        std::fs::write(&path, "").unwrap();
        // hash-1: 3 verified runs of git_status,git_diff → qualifies.
        for i in 0..3 {
            append_line(&path, &entry(&run_record(
                "cli",
                &format!("2026-08-2{i}T10:00:00Z"),
                "hash-1",
                &["git_status", "git_diff"],
                true,
            )))
            .unwrap();
        }
        // hash-2: 4 runs but only 2 verified (rate 0.5) → rejected.
        for i in 0..4 {
            append_line(&path, &entry(&run_record(
                "cli",
                &format!("2026-08-2{i}T11:00:00Z"),
                "hash-2",
                &["read_file"],
                i < 2,
            )))
            .unwrap();
        }
        // hash-3: verified enough but another tenant → invisible.
        for i in 0..3 {
            append_line(&path, &entry(&run_record(
                "telegram:111",
                &format!("2026-08-2{i}T12:00:00Z"),
                "hash-3",
                &["run_tests"],
                true,
            )))
            .unwrap();
        }
        // hash-4: blocked or failed steps → excluded regardless of verification.
        let mut deny = run_record("cli", "2026-08-29T13:00:00Z", "hash-4", &["write_file"], true);
        deny.tool_calls[0].decision = "policy_denied".to_string();
        append_line(&path, &entry(&deny)).unwrap();
        let mut fail = run_record("cli", "2026-08-29T14:00:00Z", "hash-4", &["write_file"], true);
        fail.tool_calls[0].success = Some(false);
        append_line(&path, &entry(&fail)).unwrap();
        let mut unknown = run_record("cli", "2026-08-29T15:00:00Z", "hash-4", &["write_file"], true);
        unknown.tool_calls[0].decision = "unknown_tool".to_string();
        append_line(&path, &entry(&unknown)).unwrap();
        // hash-5: uses use_skill → excluded (no nested skills).
        for i in 0..3 {
            append_line(&path, &entry(&run_record(
                "cli",
                &format!("2026-08-2{i}T15:00:00Z"),
                "hash-5",
                &[USE_SKILL],
                true,
            )))
            .unwrap();
        }

        let proposals = Proposer::default().propose(&path, "cli").unwrap();
        assert_eq!(proposals.len(), 1, "only hash-1 qualifies: {proposals:?}");
        let p = &proposals[0];
        assert_eq!(p.tool_sequence_hash, "hash-1");
        assert_eq!(p.total_runs, 3);
        assert_eq!(p.verified_runs, 3);
        assert!((p.verified_rate - 1.0).abs() < f64::EPSILON);
        assert_eq!(p.tool_names, vec!["git_status".to_string(), "git_diff".to_string()]);
        assert_eq!(p.suggested_name, "git-status-git-diff");
        assert_eq!(p.example_run_ids.len(), 3);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn proposer_missing_file_is_empty() {
        let proposals = Proposer::default()
            .propose(&temp_path("absent.jsonl"), "cli")
            .unwrap();
        assert!(proposals.is_empty());
    }

    #[test]
    fn append_proposals_dedupes_by_tenant_and_hash() {
        let path = temp_path("props.jsonl");
        std::fs::write(&path, "").unwrap();
        let make = |tenant: &str, hash: &str| ProposalRecord {
            proposed_at: "2026-08-29T00:00:00Z".to_string(),
            tenant_id: tenant.to_string(),
            tool_sequence_hash: hash.to_string(),
            tool_names: vec!["git_status".to_string()],
            total_runs: 3,
            verified_runs: 3,
            verified_rate: 1.0,
            example_run_ids: vec!["2026-08-28T10:00:00Z".to_string()],
            suggested_name: "git-status".to_string(),
        };

        let first = vec![
            make("cli", "hash-a"),
            make("cli", "hash-b"),
            make("telegram:111", "hash-a"), // same hash, other tenant — kept
        ];
        assert_eq!(append_proposals(&path, &first).unwrap(), 3);
        // Re-running writes nothing new.
        let again = vec![make("cli", "hash-a"), make("cli", "hash-c")];
        assert_eq!(append_proposals(&path, &again).unwrap(), 1);
        let _ = std::fs::remove_file(&path);
    }
}
