//! Skill metrics and retirement — the M6d instruments (spec §3.4).
//!
//! Metrics are **derived at read time** from the run records (the single
//! source of truth, via the Proposer's double-parse pattern) — there is no
//! new per-task write path. The drift check dry-runs a skill's step plan
//! through the gate chain ([`amparo_agent::dry_run_gate`]): nothing
//! executes and no approval is asked.

use std::collections::BTreeMap;
use std::path::Path;

use amparo_agent::dry_run_gate;
use amparo_policy::PolicyEngine;
use amparo_tools::{SkillSpec, ToolCall, ToolRegistry, ToolTrustTier, USE_SKILL};
use serde::{Deserialize, Serialize};

use crate::RunRecord;

/// The re-check log filename, under the workspace skills directory.
pub const RECHECKS_FILE: &str = "rechecks.jsonl";

/// One attributed use of a skill: the `use_skill` call and the steps it
/// expanded into.
#[derive(Debug, Clone, PartialEq)]
pub struct SkillUse {
    /// RFC 3339 `started_at` of the run record containing the use.
    pub started_at: String,
    /// True when the run completed and its verification decision was
    /// `complete`.
    pub verified: bool,
    /// The expanded steps, in order. Denials count only attributed steps —
    /// a policy-denied `use_skill` call has zero steps.
    pub steps: Vec<StepOutcome>,
}

/// One expanded step's outcome.
#[derive(Debug, Clone, PartialEq)]
pub struct StepOutcome {
    /// The step's tool name.
    pub tool: String,
    /// The step's gate decision (`allowed`, `policy_denied`, …).
    pub decision: String,
}

/// Read every attributed use of `skill` by `tenant_id` from `records_path`
/// (a `RunRecord` JSONL file whose lines are MemoryEntry wrappers — the
/// CaseRetriever double-parse pattern). A missing file yields an empty
/// list, not an error.
///
/// A use is a `ToolStep` with `tool_name == "use_skill"` and a target
/// naming the skill — the skill name directly (records written after the
/// M6d sink fix) or the M6c-era compact-JSON `{"skill_name": …}` form. Its
/// steps are the following `ToolStep`s whose `call_id` starts with
/// `"{use.call_id}-step-"`.
pub fn read_uses(records_path: &Path, tenant_id: &str, skill: &str) -> Result<Vec<SkillUse>, String> {
    let text = match std::fs::read_to_string(records_path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("cannot read records {}: {e}", records_path.display())),
    };

    let mut uses = Vec::new();
    for line in text.lines() {
        let Ok(entry) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(content) = entry.get("content").and_then(|v| v.as_str()) else {
            continue;
        };
        let Ok(record) = serde_json::from_str::<RunRecord>(content) else {
            continue;
        };
        if record.tenant_id != tenant_id {
            continue;
        }
        let verified = record.status == "complete"
            && record
                .verification
                .as_ref()
                .map(|v| v.decision == "complete")
                .unwrap_or(false);
        let calls = &record.tool_calls;
        for (i, step) in calls.iter().enumerate() {
            if step.tool_name != USE_SKILL || !is_skill_target(&step.target, skill) {
                continue;
            }
            let prefix = format!("{}-step-", step.call_id);
            let steps: Vec<StepOutcome> = calls[i + 1..]
                .iter()
                .take_while(|s| s.call_id.starts_with(&prefix))
                .map(|s| StepOutcome {
                    tool: s.tool_name.clone(),
                    decision: s.decision.clone(),
                })
                .collect();
            uses.push(SkillUse {
                started_at: record.started_at.clone(),
                verified,
                steps,
            });
        }
    }
    Ok(uses)
}

/// True when `target` names `skill` — the skill name directly (post-M6d
/// records) or the M6c-era compact-JSON arguments form.
fn is_skill_target(target: &str, skill: &str) -> bool {
    if target == skill {
        return true;
    }
    serde_json::from_str::<serde_json::Value>(target)
        .ok()
        .and_then(|v| v.get("skill_name").and_then(|n| n.as_str()).map(|n| n == skill))
        .unwrap_or(false)
}

/// One skill's derived metrics.
#[derive(Debug, Clone, PartialEq)]
pub struct SkillMetrics {
    /// Total attributed uses.
    pub uses: usize,
    /// Uses from VERIFIED runs.
    pub verified_uses: usize,
    /// `verified_uses / uses` (0.0 with no uses).
    pub verified_rate: f64,
    /// Mean expanded steps per use (0.0 with no uses).
    pub mean_steps: f64,
    /// Non-`allowed` gate decisions of the expanded steps, counted.
    pub denials: BTreeMap<String, usize>,
    /// RFC 3339 `started_at` of the most recent use.
    pub last_use: Option<String>,
}

/// Summarize uses into a [`SkillMetrics`].
pub fn summarize(uses: &[SkillUse]) -> SkillMetrics {
    let total_steps: usize = uses.iter().map(|u| u.steps.len()).sum();
    let verified_uses = uses.iter().filter(|u| u.verified).count();
    let mut denials = BTreeMap::new();
    for use_ in uses {
        for step in &use_.steps {
            if step.decision != "allowed" {
                *denials.entry(step.decision.clone()).or_insert(0) += 1;
            }
        }
    }
    SkillMetrics {
        uses: uses.len(),
        verified_uses,
        verified_rate: if uses.is_empty() {
            0.0
        } else {
            verified_uses as f64 / uses.len() as f64
        },
        mean_steps: if uses.is_empty() {
            0.0
        } else {
            total_steps as f64 / uses.len() as f64
        },
        denials,
        last_use: uses.last().map(|u| u.started_at.clone()),
    }
}

/// The performance-retirement threshold (M6d locked defaults: 0.5 / 20 /
/// 3; the flags on `amparo skill check` override).
#[derive(Debug, Clone, PartialEq)]
pub struct RetirementThreshold {
    /// Minimum verified fraction over the window (default 0.5).
    pub min_verified_rate: f64,
    /// The most recent uses to evaluate (default 20).
    pub window: usize,
    /// Minimum uses before retirement can fire (default 3).
    pub min_uses: usize,
}

impl Default for RetirementThreshold {
    fn default() -> Self {
        Self {
            min_verified_rate: 0.5,
            window: 20,
            min_uses: 3,
        }
    }
}

/// The performance-retirement reason for `uses`, or `None` when the skill
/// keeps its adoption. Fewer than `min_uses` uses never retires; the LAST
/// `window` uses are judged against `min_verified_rate`, so a bad early
/// stretch fades out of the window.
pub fn retirement_reason(uses: &[SkillUse], t: &RetirementThreshold) -> Option<String> {
    if uses.len() < t.min_uses {
        return None;
    }
    let window: Vec<&SkillUse> = uses.iter().rev().take(t.window).collect();
    if window.len() < t.min_uses {
        return None;
    }
    let verified = window.iter().filter(|u| u.verified).count();
    let rate = verified as f64 / window.len() as f64;
    if rate < t.min_verified_rate {
        Some(format!(
            "performance: VERIFIED rate {:.0}% ({verified}/{}) over the last {} uses",
            rate * 100.0,
            window.len(),
            window.len()
        ))
    } else {
        None
    }
}

/// Dry-run a skill's step plan through the gate chain (registry lookup →
/// trust ceiling → policy engine) with synthetic call ids `check-step-{k}`.
/// Nothing executes and no approval is asked. Returns the first
/// would-block reason, or `None` when every step would still pass (a
/// policy Escalate is not drift — the live chain asks a human, it does not
/// deny).
pub async fn check_skill_drift(
    spec: &SkillSpec,
    registry: &ToolRegistry,
    ceiling: ToolTrustTier,
    policy: &dyn PolicyEngine,
) -> Option<String> {
    for (k, step) in spec.steps.iter().enumerate() {
        let call = ToolCall {
            id: format!("check-step-{k}"),
            name: step.tool.clone(),
            arguments: step.arguments.clone(),
        };
        let verdict = dry_run_gate(registry, ceiling, policy, &call).await;
        if verdict.would_block {
            let reason = match verdict.reasons.first() {
                Some(r) => format!("{} ({r})", verdict.decision),
                None => verdict.decision,
            };
            return Some(format!(
                "policy drift: step {} ({}) would be blocked: {reason}",
                k + 1,
                step.tool
            ));
        }
    }
    None
}

/// One policy re-check record (M6d). `amparo skill check` writes one per
/// skill per run; startup drift checks append only Retire events (no quiet
/// rows — the per-task chat log stays bounded).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct CheckRecord {
    /// RFC 3339 timestamp of the check.
    pub checked_at: String,
    /// The tenant whose skill was checked (I2).
    pub tenant_id: String,
    /// The skill name.
    pub name: String,
    /// What was checked.
    pub kind: CheckKind,
    /// What the check concluded.
    pub outcome: CheckOutcome,
    /// The finding: the retirement reason when retired, else `None`.
    pub reason: Option<String>,
}

/// The kind of re-check.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum CheckKind {
    /// Policy drift: the step plan dry-run through the gate chain.
    Drift,
    /// Performance: VERIFIED rate over the window.
    Performance,
}

/// The check's conclusion.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum CheckOutcome {
    /// The skill keeps its adoption.
    Ok,
    /// The skill was retired.
    Retired,
}

/// Append one re-check record to `path` (a JSONL file), creating parent
/// directories and flushing.
pub fn append_recheck(path: &Path, record: &CheckRecord) -> Result<(), String> {
    crate::skills::append_line(path, record)
}

/// Read every re-check record from `path` (unparseable lines skipped); a
/// missing file yields an empty vector.
pub fn read_rechecks(path: &Path) -> Vec<CheckRecord> {
    crate::skills::read_lines(path, |line| serde_json::from_str(line).ok())
}

// ───────────────────────────────────────────────────────────── Tests ─────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ToolStep, VerificationRecord};
    use amparo_policy::{AllowAllPolicyEngine, DenyAllPolicyEngine};
    use amparo_tools::{SkillOrigin, SkillStep, ToolExecutor, ToolParam, ToolSchema, ToolResult};
    use async_trait::async_trait;
    use serde_json::json;
    use std::io::Write;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("amparo-metrics-{}-{name}", std::process::id()))
    }

    /// A stub tool whose schema carries `tier`.
    struct StubTool {
        name: String,
        tier: ToolTrustTier,
        calls: Arc<AtomicUsize>,
    }

    impl StubTool {
        fn new(name: &str, tier: ToolTrustTier) -> (Self, Arc<AtomicUsize>) {
            let calls = Arc::new(AtomicUsize::new(0));
            (Self { name: name.to_string(), tier, calls: calls.clone() }, calls)
        }
    }

    #[async_trait]
    impl ToolExecutor for StubTool {
        fn schema(&self) -> ToolSchema {
            ToolSchema {
                name: self.name.clone(),
                description: "stub".to_string(),
                parameters: vec![ToolParam {
                    name: "value".to_string(),
                    description: "value".to_string(),
                    param_type: "string".to_string(),
                    enum_values: None,
                    required: true,
                }],
                trust_tier: self.tier,
            }
        }

        async fn execute(&self, call: &ToolCall) -> ToolResult {
            self.calls.fetch_add(1, Ordering::SeqCst);
            ToolResult {
                tool_call_id: call.id.clone(),
                tool_name: call.name.clone(),
                success: true,
                output: json!({}),
                display_summary: "ok".to_string(),
                duration_ms: 0,
            }
        }
    }

    fn registry(tier: ToolTrustTier) -> (ToolRegistry, Arc<AtomicUsize>) {
        let (echo, calls) = StubTool::new("echo", tier);
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(echo));
        (registry, calls)
    }

    fn spec_with_step(tool: &str) -> SkillSpec {
        SkillSpec {
            name: "demo-skill".to_string(),
            description: "one step".to_string(),
            preconditions: vec![],
            steps: vec![SkillStep {
                tool: tool.to_string(),
                arguments: json!({"value": "x"}),
            }],
            expected_outcome: "it runs".to_string(),
            origin: SkillOrigin::Operator,
            source_run_ids: vec![],
            adopted_at: None,
        }
    }

    fn step(call_id: &str, tool: &str, decision: &str) -> ToolStep {
        ToolStep {
            call_id: call_id.to_string(),
            tool_name: tool.to_string(),
            target: String::new(),
            decision: decision.to_string(),
            reasons: vec![],
            escalated: false,
            approved: None,
            success: Some(decision == "allowed"),
            summary: None,
            duration_ms: None,
        }
    }

    fn record(
        tenant: &str,
        started_at: &str,
        verified: bool,
        calls: Vec<ToolStep>,
    ) -> RunRecord {
        RunRecord {
            version: 1,
            tenant_id: tenant.to_string(),
            started_at: started_at.to_string(),
            duration_ms: 1,
            task_text: "task".to_string(),
            tool_sequence_hash: "hash".to_string(),
            tool_calls: calls,
            verification: Some(VerificationRecord {
                decision: if verified { "complete" } else { "incomplete" }.to_string(),
                feedback: None,
            }),
            status: "complete".to_string(),
            final_answer: Some("done".to_string()),
            token_cost_estimate: 1,
        }
    }

    /// Wrap a run record in the store's wire shape (MemoryEntry whose
    /// `content` is the RunRecord JSON) and append it to `path`.
    fn append_record(path: &Path, record: &RunRecord) {
        let entry = json!({"content": serde_json::to_string(record).unwrap()});
        crate::skills::append_line(path, &entry).unwrap();
    }

    /// `n` records, each with one `use_skill` call for `demo-skill`, two
    /// attributed echo steps, and one unattributed trailing step (ignored).
    /// `new_target` picks the post-M6d target form; `false` the legacy
    /// compact-JSON form.
    fn use_records(n: usize, verified: &[bool], new_target: bool) -> Vec<RunRecord> {
        (0..n)
            .map(|i| {
                let target = if new_target {
                    "demo-skill".to_string()
                } else {
                    json!({"skill_name": "demo-skill"}).to_string()
                };
                let mut calls = vec![
                    ToolStep {
                        target,
                        ..step(&format!("call_{i}"), USE_SKILL, "allowed")
                    },
                    step(&format!("call_{i}-step-0"), "echo", "allowed"),
                    step(&format!("call_{i}-step-1"), "echo", "allowed"),
                ];
                calls.push(step("other", "read_file", "allowed"));
                record("cli", &format!("2026-08-2{i}T10:00:00Z"), verified[i], calls)
            })
            .collect()
    }

    /// Each caller passes a distinct name — tests run on parallel threads,
    /// and a shared file would race (append-only writes interleave).
    fn write_uses(name: &str, records: &[RunRecord]) -> PathBuf {
        let path = temp_path(name);
        std::fs::write(&path, "").unwrap();
        for record in records {
            append_record(&path, record);
        }
        path
    }

    #[test]
    fn read_uses_attributes_steps_with_the_new_target() {
        let path = write_uses("uses-new.jsonl", &use_records(2, &[true, false], true));
        let uses = read_uses(&path, "cli", "demo-skill").unwrap();
        assert_eq!(uses.len(), 2);
        assert!(uses[0].verified);
        assert!(!uses[1].verified);
        assert_eq!(uses[0].started_at, "2026-08-20T10:00:00Z");
        for use_ in &uses {
            assert_eq!(use_.steps.len(), 2, "only attributed steps count");
            assert_eq!(use_.steps[0].tool, "echo");
            assert_eq!(use_.steps[1].decision, "allowed");
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn read_uses_accepts_the_legacy_json_target() {
        let path = write_uses("uses-legacy.jsonl", &use_records(1, &[true], false));
        let uses = read_uses(&path, "cli", "demo-skill").unwrap();
        assert_eq!(uses.len(), 1);
        assert_eq!(uses[0].steps.len(), 2);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn read_uses_filters_tenant_skill_and_keeps_denied_uses() {
        let path = temp_path("filter.jsonl");
        std::fs::write(&path, "").unwrap();
        let mut first_use = step("call_0", USE_SKILL, "allowed");
        first_use.target = "demo-skill".to_string();
        append_record(&path, &record("cli", "2026-08-20T10:00:00Z", true, vec![
            first_use,
            step("call_0-step-0", "echo", "policy_denied"),
        ]));
        // A policy-denied use_skill: still a use, zero attributed steps.
        let mut denied = step("call_1", USE_SKILL, "policy_denied");
        denied.target = "demo-skill".to_string();
        append_record(&path, &record("cli", "2026-08-21T10:00:00Z", false, vec![denied]));
        // Another tenant and another skill are invisible.
        append_record(&path, &record("telegram:111", "2026-08-22T10:00:00Z", true, vec![
            step("call_2", USE_SKILL, "allowed"),
            step("call_2-step-0", "echo", "allowed"),
        ]));
        append_record(&path, &record("cli", "2026-08-23T10:00:00Z", true, vec![
            step("call_3", USE_SKILL, "allowed"),
        ]));
        let mut other = record("cli", "2026-08-23T10:00:00Z", true, vec![
            step("call_3", USE_SKILL, "allowed"),
        ]);
        other.tool_calls[0].target = "other-skill".to_string();
        append_record(&path, &other);

        let uses = read_uses(&path, "cli", "demo-skill").unwrap();
        assert_eq!(uses.len(), 2);
        assert_eq!(uses[0].steps.len(), 1);
        assert_eq!(uses[0].steps[0].decision, "policy_denied");
        assert!(!uses[1].verified);
        assert!(uses[1].steps.is_empty(), "a denied use_skill has no steps");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn read_uses_missing_file_is_empty() {
        let uses = read_uses(&temp_path("absent.jsonl"), "cli", "demo-skill").unwrap();
        assert!(uses.is_empty());
    }

    #[test]
    fn summarize_counts_uses_denials_and_means() {
        let uses = vec![
            SkillUse {
                started_at: "a".to_string(),
                verified: true,
                steps: vec![
                    StepOutcome { tool: "echo".to_string(), decision: "allowed".to_string() },
                    StepOutcome { tool: "write_file".to_string(), decision: "policy_denied".to_string() },
                    StepOutcome { tool: "echo".to_string(), decision: "policy_denied".to_string() },
                ],
            },
            SkillUse {
                started_at: "b".to_string(),
                verified: false,
                steps: vec![StepOutcome {
                    tool: "echo".to_string(),
                    decision: "trust_blocked".to_string(),
                }],
            },
        ];
        let metrics = summarize(&uses);
        assert_eq!(metrics.uses, 2);
        assert_eq!(metrics.verified_uses, 1);
        assert!((metrics.verified_rate - 0.5).abs() < f64::EPSILON);
        assert!((metrics.mean_steps - 2.0).abs() < f64::EPSILON);
        assert_eq!(metrics.denials.get("policy_denied"), Some(&2));
        assert_eq!(metrics.denials.get("trust_blocked"), Some(&1));
        assert!(!metrics.denials.contains_key("allowed"));
        assert_eq!(metrics.last_use.as_deref(), Some("b"));
    }

    fn bare_use(verified: bool) -> SkillUse {
        SkillUse {
            started_at: String::new(),
            verified,
            steps: Vec::new(),
        }
    }

    #[test]
    fn retirement_reason_floor_and_window() {
        let t = RetirementThreshold::default();
        // Two uses at 0% never retire — the floor is 3.
        assert!(retirement_reason(&[bare_use(false), bare_use(false)], &t).is_none());
        // Three uses at 1/3 retires.
        let reason = retirement_reason(&[bare_use(true), bare_use(false), bare_use(false)], &t);
        assert!(reason.is_some(), "1/3 below 0.5 retires");
        assert!(reason.unwrap().contains("33%"));
        // A bad early stretch fades out of the window: 5 bad of 25 uses,
        // the last 20 all good → keeps the adoption.
        let mut uses: Vec<SkillUse> = (0..5).map(|_| bare_use(false)).collect();
        uses.extend((0..20).map(|_| bare_use(true)));
        assert!(retirement_reason(&uses, &t).is_none());
        // And the window honors its own bound: 8 uses, window 3, all bad
        // early + good tail — the last 3 all good → keeps the adoption.
        let mut uses: Vec<SkillUse> = (0..5).map(|_| bare_use(false)).collect();
        uses.extend((0..3).map(|_| bare_use(true)));
        let narrow = RetirementThreshold { window: 3, ..t.clone() };
        assert!(retirement_reason(&uses, &narrow).is_none());
    }

    #[tokio::test]
    async fn drift_allow_all_passes_and_executes_nothing() {
        let (registry, calls) = registry(ToolTrustTier::SystemControl);
        let policy = AllowAllPolicyEngine;
        let drift = check_skill_drift(
            &spec_with_step("echo"),
            &registry,
            ToolTrustTier::SystemControl,
            &policy,
        )
        .await;
        assert!(drift.is_none());
        assert_eq!(calls.load(Ordering::SeqCst), 0, "the dry run executes nothing");
    }

    #[tokio::test]
    async fn drift_deny_all_blocks_the_first_step() {
        let (registry, _) = registry(ToolTrustTier::SystemControl);
        let policy = DenyAllPolicyEngine::new("no policy configured");
        let drift = check_skill_drift(
            &spec_with_step("echo"),
            &registry,
            ToolTrustTier::SystemControl,
            &policy,
        )
        .await
        .expect("a deny-all engine drifts the first step");
        assert!(drift.contains("step 1"), "{drift}");
        assert!(drift.contains("echo"), "{drift}");
    }

    #[tokio::test]
    async fn drift_unknown_step_tool_blocks() {
        let (registry, _) = registry(ToolTrustTier::SystemControl);
        let policy = AllowAllPolicyEngine;
        let drift = check_skill_drift(
            &spec_with_step("not_registered"),
            &registry,
            ToolTrustTier::SystemControl,
            &policy,
        )
        .await
        .expect("an unknown step tool drifts");
        assert!(drift.contains("unknown_tool"), "{drift}");
    }

    #[tokio::test]
    async fn drift_escalate_is_not_drift() {
        // A policy that escalates echo: the dry run reports
        // approval_required without would_block — the skill stays.
        struct EscalatePolicy;
        #[async_trait]
        impl PolicyEngine for EscalatePolicy {
            async fn judge_tool(
                &self,
                _tool: &str,
                _target: &str,
                _params: &[(&str, &str)],
            ) -> amparo_policy::PolicyDecision {
                amparo_policy::PolicyDecision::escalate("review me")
            }
        }
        let (registry, _) = registry(ToolTrustTier::SystemControl);
        let drift = check_skill_drift(
            &spec_with_step("echo"),
            &registry,
            ToolTrustTier::SystemControl,
            &EscalatePolicy,
        )
        .await;
        assert!(drift.is_none(), "escalation is not drift");
    }

    #[test]
    fn recheck_append_and_read_round_trip() {
        let path = temp_path("rechecks.jsonl");
        std::fs::write(&path, "").unwrap();
        let ok = CheckRecord {
            checked_at: "2026-08-29T10:00:00Z".to_string(),
            tenant_id: "cli".to_string(),
            name: "demo-skill".to_string(),
            kind: CheckKind::Drift,
            outcome: CheckOutcome::Ok,
            reason: None,
        };
        let retired = CheckRecord {
            checked_at: "2026-08-29T11:00:00Z".to_string(),
            tenant_id: "cli".to_string(),
            name: "demo-skill".to_string(),
            kind: CheckKind::Performance,
            outcome: CheckOutcome::Retired,
            reason: Some("performance: VERIFIED rate 25% (1/4) over the last 4 uses".to_string()),
        };
        append_recheck(&path, &ok).unwrap();
        append_recheck(&path, &retired).unwrap();

        let rows = read_rechecks(&path);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].outcome, CheckOutcome::Ok);
        assert!(rows[0].reason.is_none());
        assert_eq!(rows[1].outcome, CheckOutcome::Retired);
        assert_eq!(rows[1].kind, CheckKind::Performance);
        assert!(rows[1].reason.as_deref().unwrap().contains("25%"));
        assert_eq!(rows[1], retired);

        // Unparseable lines are skipped; a missing file is empty.
        std::fs::OpenOptions::new().append(true).open(&path).unwrap()
            .write_all(b"not json\n").unwrap();
        assert_eq!(read_rechecks(&path).len(), 2);
        assert!(read_rechecks(&temp_path("absent.jsonl")).is_empty());
        let _ = std::fs::remove_file(&path);
    }
}
