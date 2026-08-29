//! Amparo's lab notebook — the M6a substrate.
//!
//! Every completed task becomes a **run record**: a PII-stripped,
//! tenant-tagged JSON document carrying the task text, the tool-call
//! sequence with its gate log, the verification outcome and the truncated
//! final answer. Records are written through the existing
//! [`amparo_agent::EventSink`] seam (no loop changes — the loop's terminal
//! events already carry everything) into the [`amparo_tools::Memory`] trait,
//! where the built-in [`JsonlStore`] persists them as an append-only local
//! file and Engram or any other store can sit behind the same trait.
//!
//! # The growth invariants this implements
//!
//! - **I6 — privacy at capture time.** Every free-text field (task text,
//!   per-call target, display summaries, verification feedback, final
//!   answer) passes `amparo_privacy::secure_minions_strip`; only the
//!   sanitised text is persisted and the placeholder map is discarded, so
//!   records are archival — there is no restore path.
//! - **I2 — per-tenant namespacing.** Each record carries the tenant id it
//!   belongs to (the chat driver tags `platform:user_id`; the CLI tags
//!   `cli`).
//! - **Append-only.** Records are written once, never edited or deleted —
//!   audit-log style. Retirement (M6d) disables and notifies; it never
//!   rewrites history.
//!
//! # Runtime requirement
//!
//! [`NotebookSink`] hands each finished record to
//! [`amparo_tools::Memory::store`] on a
//! spawned tokio task, so hosts must run the agent loop inside a tokio
//! runtime (every current host does). [`NotebookSink::flush`] awaits the
//! pending write — short-lived hosts (the CLI) call it after the run so a
//! record cannot lose the race with process exit.

#![warn(missing_docs)]

mod metrics;
mod record;
mod retrieve;
mod rollup;
mod sink;
mod skills;
mod store;

pub use metrics::{
    append_recheck, check_skill_drift, read_rechecks, read_uses, retirement_reason, summarize,
    CheckKind, CheckOutcome, CheckRecord, RetirementThreshold, SkillMetrics, SkillUse, StepOutcome,
    RECHECKS_FILE,
};
pub use record::{RunRecord, ToolStep, VerificationRecord};
pub use retrieve::CaseRetriever;
pub use rollup::{
    auto_rollup, hot_record, list_records, notebook_dir, promote_record, rollup, rollup_dry_run,
    AutoReport, FoldReport, HashRow, PromoteOutcome, PromoteReport, PromotedRecord, RecordSummary,
    RollupReport, RollupState, DEFAULT_MAX_BYTES, DEFAULT_ROLLUP_DAYS, HOT_FILE, HOT_HASHES_FILE,
    MAX_BYTES_FLOOR, PROMOTED_FILE, ROLLUP_STATE_FILE,
};
pub use sink::NotebookSink;
pub use skills::{
    append_event, append_proposals, read_log, skills_dir, ProposalRecord, Proposer, SkillLogEvent,
    SkillSet, ADOPTED_FILE, PROPOSALS_FILE,
};
pub use store::JsonlStore;
