//! The QC council (M9 W1) — deterministic rule auditors beside policy.
//!
//! The council runs after the loop produces a candidate final answer and
//! before the self-verification prompt is built. Four net-new rules fire
//! on what the run itself recorded — the requested and executed tool
//! sets, the tool results still in context, the token accounting, and
//! PII shapes — never on policy semantics, and never by calling a model.
//!
//! The verdicts are **advisory**: findings are appended to the
//! verification prompt as candidate issues for the model to check, and
//! the verification turn — not the council — decides, exactly as before.
//! That boundary is invariant I1's: the council is beside policy, never
//! a gate, and nothing it produces auto-tunes anything. The pipeline
//! shape (one pass, named rules, a verdict plus per-rule counters in
//! memory) follows the axiom-qc pattern; the rule set is Amparo's own —
//! axiom-qc's keyword red-lines and LLM panel are excluded by design (a
//! model is never the approver).

use std::collections::HashMap;
use std::sync::Mutex;

use serde::Serialize;

/// Rule id — the final answer cites a tool that never executed.
pub const UNEXECUTED_TOOL_CLAIM: &str = "unexecuted_tool_claim";
/// Rule id — an executed call's result is missing from the context.
pub const EVIDENCE: &str = "evidence";
/// Rule id — an inference-cost figure in the answer diverges from the
/// run's token accounting.
pub const COST_HONESTY: &str = "cost_honesty";
/// Rule id — the answer contains PII-shaped values.
pub const PII_SHAPE: &str = "pii_shape";

/// One advisory finding from one rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct QcFinding {
    /// The rule that fired — one of the four rule-id constants.
    pub rule: &'static str,
    /// The finding text, shown to the model in the verification prompt.
    /// Counts never values (I6): PII findings name categories only.
    pub message: String,
}

/// The verdict of one council pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum QcVerdict {
    /// No rule fired.
    Approved,
    /// One or more rules fired — see the report's findings.
    WithFindings,
}

/// One council pass: the verdict plus the findings that produced it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct QcReport {
    /// The advisory verdict.
    pub verdict: QcVerdict,
    /// The findings, empty when approved.
    pub findings: Vec<QcFinding>,
}

/// What the council audits: the candidate answer plus the run's own
/// records. Every field is data the loop already holds — the council
/// adds no new observability surface of its own.
#[derive(Debug, Clone)]
pub struct QcInput {
    /// The candidate final answer text.
    pub final_answer: String,
    /// Tool names the model asked to run this task, blocked included.
    pub requested_tools: Vec<String>,
    /// Tool names that actually executed this task — the requested set
    /// minus gate blocks, plus expanded skill steps that ran.
    pub executed_tools: Vec<String>,
    /// Tool calls that actually executed (one per executed call; a
    /// `use_skill` call counts once — its steps carry no messages).
    pub executed_calls: usize,
    /// Tool-role results present in the conversation context.
    pub tool_results_in_context: usize,
    /// Estimated tokens consumed so far (`chars/4` across every turn).
    pub tokens_estimated: usize,
    /// The host's cost rate; `None` silences the cost rule.
    pub cost_per_million_tokens: Option<f64>,
    /// Tool names the registry knows — the citation vocabulary.
    pub known_tools: Vec<String>,
    /// Whether a sub-agent was spawned: a parent's context carries
    /// child cost figures legitimately, so the cost rule applies to
    /// leaf runs only.
    pub spawned_subagents: bool,
}

/// In-memory counters over every pass the council has run (the axiom-qc
/// pattern): the stats are logged beside each audit, never persisted.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct QcStats {
    /// Audits run so far.
    pub audits: usize,
    /// Audits with no findings.
    pub approved: usize,
    /// Audits with one or more findings.
    pub with_findings: usize,
    /// Findings per rule, in first-fire order.
    pub findings_by_rule: HashMap<&'static str, usize>,
}

/// The council. Construct one per agent; [`QcCouncil::audit`] is a
/// deterministic function of its input, and the counters accumulate in
/// memory only.
#[derive(Debug)]
pub struct QcCouncil {
    stats: Mutex<QcStats>,
}

impl QcCouncil {
    /// A council with zeroed counters.
    pub fn new() -> Self {
        Self {
            stats: Mutex::new(QcStats::default()),
        }
    }

    /// A snapshot of the cumulative counters.
    pub fn stats(&self) -> QcStats {
        self.stats.lock().unwrap().clone()
    }

    /// Run every rule over `input` and fold the findings into a report.
    /// Deterministic: the same input produces the same report. The
    /// verdict is advisory — the loop decides what to do with it (it
    /// appends the findings to the verification prompt).
    pub fn audit(&self, input: &QcInput) -> QcReport {
        let mut findings = Vec::new();
        findings.extend(unexecuted_tool_claims(input));
        findings.extend(evidence_gaps(input));
        findings.extend(cost_honesty(input));
        findings.extend(pii_shapes(input));

        let verdict = if findings.is_empty() {
            QcVerdict::Approved
        } else {
            QcVerdict::WithFindings
        };
        let mut stats = self.stats.lock().unwrap();
        stats.audits = stats.audits.saturating_add(1);
        match verdict {
            QcVerdict::Approved => stats.approved = stats.approved.saturating_add(1),
            QcVerdict::WithFindings => stats.with_findings = stats.with_findings.saturating_add(1),
        }
        for finding in &findings {
            *stats.findings_by_rule.entry(finding.rule).or_insert(0) += 1;
        }
        QcReport { verdict, findings }
    }
}

impl Default for QcCouncil {
    fn default() -> Self {
        Self::new()
    }
}

/// Render the council's findings as the verification-prompt section
/// (M9 W1): candidate issues for the model to check, never mandates —
/// the prompt says so, and the verdict never overrides the
/// verification decision.
pub(crate) fn qc_prompt_section(findings: &[QcFinding]) -> String {
    let mut section = String::from(
        "The QC council (deterministic rule auditors) raised advisory findings about \
         your final answer. Check each one against the conversation and correct your \
         answer where they are right; ignore any that are wrong:",
    );
    for (i, finding) in findings.iter().enumerate() {
        section.push_str(&format!(
            "\n{}. [{}] {}",
            i + 1,
            finding.rule,
            finding.message
        ));
    }
    section
}

/// (a) The final answer cites a tool that never executed. A citation is
/// a word-boundary mention of a registry tool name in the answer; the
/// finding is phrased as a claim to verify — a blocked call honestly
/// reported as blocked is for the verification turn to judge.
fn unexecuted_tool_claims(input: &QcInput) -> Vec<QcFinding> {
    let mut findings = Vec::new();
    for name in &input.known_tools {
        if input.executed_tools.iter().any(|e| e == name) {
            continue;
        }
        if cites(&input.final_answer, name) {
            findings.push(QcFinding {
                rule: UNEXECUTED_TOOL_CLAIM,
                message: format!(
                    "the final answer mentions `{name}`, which never executed this run — \
                     verify whether the answer claims a result from it"
                ),
            });
        }
    }
    findings
}

/// (b) Every executed call's result must still be present in the
/// conversation — a result that left the context cannot ground a claim.
/// Trimming (the 30-message tail) and dropped tool messages both show
/// up as a gap, which is exactly the finding: the model cannot cite
/// evidence it no longer has.
fn evidence_gaps(input: &QcInput) -> Vec<QcFinding> {
    if input.executed_calls > input.tool_results_in_context {
        vec![QcFinding {
            rule: EVIDENCE,
            message: format!(
                "{} tool call(s) executed but only {} tool result(s) remain in context — \
                 claims citing the missing results are ungrounded",
                input.executed_calls, input.tool_results_in_context
            ),
        }]
    } else {
        Vec::new()
    }
}

/// (c) Cost honesty: the answer's inference-cost claims must match the
/// run's accounting estimate — the same `chars/4` figure the host's
/// cost line renders. A claim is a `$` figure whose short tail mentions
/// "inference" (the cost line's own shape); other dollar figures (a
/// price lookup's answer, say) are not inference claims and are left
/// alone. Leaf runs only: a parent's context carries child figures
/// that belong to the children.
fn cost_honesty(input: &QcInput) -> Vec<QcFinding> {
    if input.spawned_subagents {
        return Vec::new();
    }
    let Some(rate) = input.cost_per_million_tokens else {
        return Vec::new();
    };
    let expected = input.tokens_estimated as f64 * rate / 1_000_000.0;
    // The rendered figure is what the host's own cost line would show
    // (`{:.2}` rounding) — a model quoting that rounded form is honest
    // even for tiny costs, where "~$0.00" is the true rendered value.
    let rendered = (expected * 100.0).round() / 100.0;
    let mut findings = Vec::new();
    for (figure, _) in inference_cost_claims(&input.final_answer) {
        let close = (figure - expected).abs() <= expected.max(figure).abs() * 0.5
            || (figure - rendered).abs() < 1e-6;
        if !close {
            findings.push(QcFinding {
                rule: COST_HONESTY,
                message: format!(
                    "the final answer cites ~${figure:.2} in inference, but this run's \
                     accounting estimate is ~${rendered:.2} (chars/4) — verify the claim"
                ),
            });
        }
    }
    findings
}

/// (d) Residual PII shapes in the final answer: the same pattern
/// heuristics the privacy policy uses (I6), reported as category
/// counts — the values themselves never enter the finding.
fn pii_shapes(input: &QcInput) -> Vec<QcFinding> {
    let stripped = amparo_privacy::secure_minions_strip(&input.final_answer);
    if !stripped.pii_found {
        return Vec::new();
    }
    let mut counts: Vec<(String, usize)> = Vec::new();
    for placeholder in &stripped.pii_map {
        match counts.iter_mut().find(|(c, _)| c == &placeholder.category) {
            Some((_, n)) => *n += 1,
            None => counts.push((placeholder.category.clone(), 1)),
        }
    }
    let parts: Vec<String> = counts.iter().map(|(c, n)| format!("{c} x{n}")).collect();
    vec![QcFinding {
        rule: PII_SHAPE,
        message: format!(
            "the final answer contains PII-shaped values ({}) — strip before publishing",
            parts.join(", ")
        ),
    }]
}

/// Whether `text` mentions `name` as a word: the characters before and
/// after each occurrence are not identifier characters, so `read_file`
/// in "the read_file tool" is a citation and "read_files" is not. Tool
/// names are ASCII — matching stays byte-safe.
fn cites(text: &str, name: &str) -> bool {
    text.match_indices(name).any(|(i, _)| {
        let before = i == 0
            || text[..i]
                .chars()
                .next_back()
                .map(|c| !(c.is_ascii_alphanumeric() || c == '_'))
                .unwrap_or(true);
        let end = i + name.len();
        let after = end >= text.len()
            || text[end..]
                .chars()
                .next()
                .map(|c| !(c.is_ascii_alphanumeric() || c == '_'))
                .unwrap_or(true);
        before && after
    })
}

/// `$`-prefixed figures whose 60-char tail mentions "inference" — the
/// shape of the cost line. Pure; returns `(figure, index after the
/// number)` pairs in first-seen order. The cost line's embedded rate
/// ("$3/1M tokens") has no "inference" in its tail and is skipped.
fn inference_cost_claims(text: &str) -> Vec<(f64, usize)> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'$' {
            i += 1;
            continue;
        }
        let mut j = i + 1;
        while j < bytes.len() && (bytes[j].is_ascii_digit() || bytes[j] == b'.') {
            j += 1;
        }
        if let Ok(figure) = text[i + 1..j].parse::<f64>() {
            let tail: String = text[j..].chars().take(60).collect();
            if tail.contains("inference") {
                out.push((figure, j));
            }
        }
        i = j;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(final_answer: &str) -> QcInput {
        QcInput {
            final_answer: final_answer.to_string(),
            requested_tools: Vec::new(),
            executed_tools: Vec::new(),
            executed_calls: 0,
            tool_results_in_context: 0,
            tokens_estimated: 0,
            cost_per_million_tokens: Some(3.0),
            known_tools: Vec::new(),
            spawned_subagents: false,
        }
    }

    #[test]
    fn cites_requires_word_boundaries() {
        assert!(cites("the run_command tool ran", "run_command"));
        assert!(cites("I ran run_command.", "run_command"));
        assert!(!cites("run_commands are listed", "run_command"));
        assert!(!cites("", "run_command"));
    }

    #[test]
    fn unexecuted_tool_claims_fire_only_for_known_unexecuted_citations() {
        let mut i = input("I ran read_file and the answer is 42.");
        i.known_tools = vec!["read_file".into(), "write_file".into()];
        let findings = unexecuted_tool_claims(&i);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].rule, UNEXECUTED_TOOL_CLAIM);
        assert!(findings[0].message.contains("read_file"));

        // Executed tools are not findings, and unknown names are not
        // citations.
        i.executed_tools = vec!["read_file".into()];
        assert!(unexecuted_tool_claims(&i).is_empty());
    }

    #[test]
    fn evidence_fires_when_results_left_the_context() {
        let mut i = input("done");
        i.executed_calls = 3;
        i.tool_results_in_context = 1;
        let findings = evidence_gaps(&i);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].rule, EVIDENCE);
        assert!(findings[0].message.contains("3 tool call(s)"));

        i.tool_results_in_context = 3;
        assert!(evidence_gaps(&i).is_empty());
        i.tool_results_in_context = 4;
        assert!(evidence_gaps(&i).is_empty());
    }

    #[test]
    fn cost_honesty_flags_divergent_claims_only() {
        // 13_500 estimated tokens at $3/1M → $0.0405, rendered "~$0.04".
        let mut i = input("The answer is 42.");
        i.tokens_estimated = 13_500;

        // A wildly divergent inference claim fires.
        i.final_answer = "The answer is 42. This run cost ~$9.99 in inference.".into();
        let findings = cost_honesty(&i);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].rule, COST_HONESTY);
        assert!(findings[0].message.contains("$0.04"));

        // The honest rendered figure passes.
        i.final_answer = "The answer is 42. This run cost ~$0.04 in inference.".into();
        assert!(cost_honesty(&i).is_empty());

        // A price-lookup answer's dollar figure is not an inference claim.
        i.final_answer = "The widget costs $25 USD.".into();
        assert!(cost_honesty(&i).is_empty());

        // No rate configured → rule silent.
        i.cost_per_million_tokens = None;
        i.final_answer = "This run cost ~$9.99 in inference.".into();
        assert!(cost_honesty(&i).is_empty());

        // A parent run's context carries child figures — rule silent.
        i.cost_per_million_tokens = Some(3.0);
        i.spawned_subagents = true;
        assert!(cost_honesty(&i).is_empty());
    }

    #[test]
    fn cost_honesty_accepts_the_rounded_zero_for_tiny_costs() {
        let mut i = input("Done. ~$0.00 in inference.");
        i.tokens_estimated = 100; // $0.0003 → renders "~$0.00"
        assert!(cost_honesty(&i).is_empty());
    }

    #[test]
    fn pii_shapes_report_category_counts_never_values() {
        let mut i = input("Contact jane@example.com or call 555-010-1234.");
        let findings = pii_shapes(&i);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].rule, PII_SHAPE);
        assert!(findings[0].message.contains("email x1"));
        assert!(findings[0].message.contains("phone x1"));
        // I6: the values themselves never enter the finding.
        assert!(!findings[0].message.contains("jane@example.com"));
        assert!(!findings[0].message.contains("555-010-1234"));

        i.final_answer = "The answer is 42.".into();
        assert!(pii_shapes(&i).is_empty());
    }

    #[test]
    fn audit_returns_approved_and_counts_stats() {
        let council = QcCouncil::new();
        let mut i = input("The answer is 42.");
        i.known_tools = vec!["web_search".into()];

        let report = council.audit(&i);
        assert_eq!(report.verdict, QcVerdict::Approved);
        assert!(report.findings.is_empty());

        i.final_answer = "I used web_search and the answer is 42.".into();
        let report = council.audit(&i);
        assert_eq!(report.verdict, QcVerdict::WithFindings);
        assert_eq!(report.findings.len(), 1);

        let stats = council.stats();
        assert_eq!(stats.audits, 2);
        assert_eq!(stats.approved, 1);
        assert_eq!(stats.with_findings, 1);
        assert_eq!(stats.findings_by_rule[UNEXECUTED_TOOL_CLAIM], 1);
    }

    #[test]
    fn prompt_section_names_each_finding_and_stays_advisory() {
        let findings = vec![QcFinding {
            rule: COST_HONESTY,
            message: "the figure diverges".into(),
        }];
        let section = qc_prompt_section(&findings);
        assert!(section.contains("advisory findings"));
        assert!(section.contains("1. [cost_honesty] the figure diverges"));
        assert!(section.contains("ignore any that are wrong"));
    }
}
